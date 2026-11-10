//! Decide what is garbage in one Cargo profile dir (`target/debug`, `target/<triple>/release`, ...)
//! and delete it while holding Cargo's own build locks.
//!
//! Model: every compiled unit has a 16-hex metadata hash `H` shared by
//! `.fingerprint/<pkg>-H/`, `build/<pkg>-H/` and every `deps/*-H*` / `examples/*-H*` file.
//! Units of the same package and the same *kind* (lib build vs `check` rmeta, test, bin,
//! build-script compile/run) form a group. Inside a group the newest unit is live; units that are
//! older than the newest by more than `grace` were superseded (dependency/feature/profile change)
//! and are deleted. Units without a fingerprint can never be fresh for Cargo, so they are orphans.
//! If a deleted unit comes back (two legit variants built alternately, e.g. `-p x --features y`),
//! its group is pinned and no longer deduplicated.
//!
//! When the workspace `Cargo.lock` is next to `target/`, only workspace (path) packages are
//! deduplicated; registry/git units stay only while reachable from a kept workspace unit through
//! the `deps` edges of `.fingerprint/*/*.json` (each edge names the dependency's fingerprint value,
//! the hex in the sibling file). That drops removed dependencies and deps of superseded units.
//! The `rustc` hash in the json separates `clippy-driver` from `rustc` builds; a hash with no
//! writes for `RUSTC_RETIRE` before the newest unit (old toolchain) is dropped entirely.

use crate::fingerprint::Workspace;
use crate::fsutil::size_of;
use crate::prune::Config;
use crate::scan::{Incremental, Unit};
use std::collections::{BTreeMap, HashMap, HashSet};

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// A rustc hash not used for this long (relative to the newest unit) is an old toolchain.
const RUSTC_RETIRE: Duration = Duration::from_secs(14 * 24 * 3600);

pub(crate) struct Candidate {
    pub(crate) krate: Option<String>,
    pub(crate) value: Option<u64>,
    pub(crate) id: String, // what goes into the `deleted` log: hash or `inc:<dir>`
    pub(crate) inodes: Vec<u64>,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) mtime: Option<SystemTime>,
    pub(crate) incremental: bool,
}

pub(crate) fn plan(
    units: BTreeMap<String, Unit>,
    incremental: Incremental,
    pinned: &HashMap<String, SystemTime>,
    ws: Option<&Workspace>,
    extra_roots: Vec<u64>,
    cfg: &Config,
) -> (Vec<Candidate>, Vec<Candidate>) {
    let mut doomed = Vec::new();
    let mut kept = Vec::new();
    let newest_all = units
        .values()
        .filter(|u| u.has_fingerprint)
        .filter_map(|u| u.mtime)
        .max();
    let mut rustc_newest: HashMap<u64, SystemTime> = HashMap::new();
    for u in units.values().filter(|u| u.has_fingerprint) {
        if let Some(t) = u.mtime {
            let e = rustc_newest.entry(u.rustc).or_insert(t);
            *e = (*e).max(t);
        }
    }
    let retired = |u: &Unit| match (newest_all, rustc_newest.get(&u.rustc)) {
        (Some(n), Some(&r)) => r + RUSTC_RETIRE < n,
        _ => false,
    };

    // Build-script *run* units never reach rustc, so they inherit the clippy tag of the compiled
    // build script they depend on.
    let tag_by_value: HashMap<u64, String> = units
        .values()
        .filter(|u| !u.tag.is_empty())
        .filter_map(|u| Some((u.fp_value?, u.tag.clone())))
        .collect();
    let mut units = units;
    for u in units.values_mut() {
        if u.tag.is_empty() && u.fp_kind.iter().any(|k| k.starts_with("run-build-script")) {
            if let Some(t) = u.dep_values.iter().find_map(|v| tag_by_value.get(v)) {
                u.tag = t.clone();
            }
        }
    }

    let mut groups: HashMap<String, Vec<(Candidate, Vec<u64>, bool)>> = HashMap::new();
    let mut registry: Vec<(Candidate, Option<u64>, Vec<u64>)> = Vec::new();
    for (h, u) in units {
        let local = ws.is_none_or(|w| w.is_local(u.name.as_deref().unwrap_or("")));
        let c = Candidate {
            krate: if local { u.krate.clone() } else { None }, // only workspace crates are incremental
            value: u.fp_value,
            id: h,
            inodes: u.inodes.clone(),
            paths: u.paths.clone(),
            mtime: u.mtime,
            incremental: false,
        };
        let removed = || match (ws, &u.dep_info) {
            (Some(w), Some(d)) if local => w.target_removed(d),
            _ => false,
        };
        if !u.has_fingerprint || retired(&u) || removed() {
            doomed.push(c); // no fingerprint: cargo rebuilds it anyway
        } else if local {
            let is_run = u.fp_kind.iter().any(|k| k.starts_with("run-build-script"));
            groups
                .entry(u.group())
                .or_default()
                .push((c, u.dep_values, is_run));
        } else {
            registry.push((c, u.fp_value, u.dep_values));
        }
    }

    // Dedup workspace units (all units when locality is unknown).
    let mut roots: Vec<u64> = extra_roots;
    let mut kept_local = Vec::new();
    for (key, members) in groups {
        let newest = members.iter().filter_map(|(c, _, _)| c.mtime).max();
        for (c, deps, is_run) in members {
            let stale = matches!((newest, c.mtime), (Some(n), Some(t)) if t + cfg.grace < n);
            if stale && !pinned.contains_key(&key) {
                doomed.push(c);
            } else {
                kept_local.push((c, deps, is_run));
            }
        }
    }
    // A build-script run whose compiled build script is going away goes with it.
    let doomed_vals: HashSet<u64> = doomed.iter().filter_map(|c| c.value).collect();
    let kept_vals: HashSet<u64> = kept_local.iter().filter_map(|(c, _, _)| c.value).collect();
    for (c, deps, is_run) in kept_local {
        let orphan_run = is_run
            && deps.iter().any(|v| doomed_vals.contains(v))
            && !deps.iter().any(|v| kept_vals.contains(v));
        if orphan_run {
            doomed.push(c);
        } else {
            roots.extend(deps);
            kept.push(c);
        }
    }

    // Registry units live only while a kept unit depends on them (transitively).
    if !registry.is_empty() {
        let by_value: HashMap<u64, usize> = registry
            .iter()
            .enumerate()
            .filter_map(|(i, (_, v, _))| v.map(|v| (v, i)))
            .collect();
        let mut live = vec![false; registry.len()];
        while let Some(v) = roots.pop() {
            if let Some(&i) = by_value.get(&v) {
                if !live[i] {
                    live[i] = true;
                    roots.extend(registry[i].2.iter().copied());
                }
            }
        }
        for ((c, _, _), live) in registry.into_iter().zip(live) {
            if live {
                kept.push(c)
            } else {
                doomed.push(c)
            }
        }
    }

    // `incremental/<crate>-<stable id>` cannot be mapped by hash. Its session files are hard links
    // to the unit's `.rmeta` / `.rcgu.o` in `deps/`, which identifies the owner exactly. Without a
    // shared inode (Linux drops a binary's objects) units and dirs of one crate are paired
    // one-to-one, closest mtimes first. A dir left without a unit goes.
    let mut owner: HashMap<u64, (&str, bool)> = HashMap::new();
    for (list, is_doomed) in [(&doomed, true), (&kept, false)] {
        for c in list.iter() {
            for &i in &c.inodes {
                owner.insert(i, (c.id.as_str(), is_doomed));
            }
        }
    }
    let mut keep_dir: Vec<Option<bool>> = vec![None; incremental.len()];
    let mut claimed: HashSet<&str> = HashSet::new();
    for (k, (_, _, _, inodes)) in incremental.iter().enumerate() {
        if let Some(&(id, is_doomed)) = inodes.iter().find_map(|i| owner.get(i)) {
            keep_dir[k] = Some(!is_doomed);
            claimed.insert(id);
        }
    }
    let mut by_crate: HashMap<&str, Vec<(SystemTime, bool)>> = HashMap::new();
    for (list, is_doomed) in [(&doomed, true), (&kept, false)] {
        for c in list.iter().filter(|c| !claimed.contains(c.id.as_str())) {
            if let (Some(k), Some(t)) = (c.krate.as_deref(), c.mtime) {
                by_crate.entry(k).or_default().push((t, is_doomed));
            }
        }
    }
    let gap = |a: SystemTime, b: SystemTime| a.duration_since(b).unwrap_or_else(|e| e.duration());
    let mut dirs_by_crate: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, (dir, _, _, _)) in incremental.iter().enumerate() {
        if keep_dir[i].is_none() {
            let krate = dir.rsplit_once('-').map_or(dir.as_str(), |(c, _)| c);
            dirs_by_crate.entry(krate.to_owned()).or_default().push(i);
        }
    }
    for (krate, dirs) in &dirs_by_crate {
        let Some(units) = by_crate.get(krate.as_str()) else {
            continue;
        };
        let mut pairs = Vec::new();
        for &d in dirs {
            if let Some(t) = incremental[d].2 {
                for (u, &(ut, _)) in units.iter().enumerate() {
                    pairs.push((gap(ut, t), d, u));
                }
            }
        }
        pairs.sort();
        let (mut dir_taken, mut unit_taken) = (HashSet::new(), HashSet::new());
        for (_, d, u) in pairs {
            if !dir_taken.contains(&d) && !unit_taken.contains(&u) {
                dir_taken.insert(d);
                unit_taken.insert(u);
                keep_dir[d] = Some(!units[u].1);
            }
        }
    }
    let keep_dir: Vec<bool> = keep_dir.into_iter().map(|k| k.unwrap_or(false)).collect();
    let mut inc_doomed = Vec::new();
    let mut inc_kept = Vec::new();
    for ((dir, path, t, _), keep) in incremental.into_iter().zip(keep_dir) {
        let c = Candidate {
            krate: None,
            value: None,
            inodes: Vec::new(),
            id: format!("inc:{dir}"),
            paths: vec![path],
            mtime: t,
            incremental: true,
        };
        if keep {
            inc_kept.push(c)
        } else {
            inc_doomed.push(c)
        }
    }
    doomed.extend(inc_doomed);
    kept.extend(inc_kept);
    (doomed, kept)
}

/// Over the cap: delete the oldest kept units (incremental first at equal age) until under it.
pub(crate) fn apply_cap(
    mut doomed: Vec<Candidate>,
    mut kept: Vec<Candidate>,
    cap: Option<u64>,
) -> Vec<Candidate> {
    let Some(cap) = cap else { return doomed };
    let mut total: u64 = kept.iter().flat_map(|c| &c.paths).map(|p| size_of(p)).sum();
    kept.sort_by_key(|c| (c.mtime, !c.incremental));
    for c in kept {
        if total <= cap {
            break;
        }
        total -= c.paths.iter().map(|p| size_of(p)).sum::<u64>().min(total);
        doomed.push(c);
    }
    doomed
}
