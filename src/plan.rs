//! Decide what is garbage in one Cargo profile dir (`target/debug`, `target/<triple>/release`, ...).
//!
//! Model: every compiled unit has a 16-hex metadata hash `H` shared by `.fingerprint/<pkg>-H/`,
//! `build/<pkg>-H/` and every `deps/*-H*` / `examples/*-H*` file.
//!
//! - Units without a fingerprint can never be fresh for Cargo: orphans, deleted.
//! - Workspace units (path packages from the `Cargo.lock` next to `target/`) are grouped by
//!   package, target kind, outputs, fingerprint signature (features, profile, rustflags, ...),
//!   rustc hash and clippy tag. Inside a group, units older than the newest by more than `grace`
//!   were superseded and are deleted, unless the group is pinned (alternating variants, see
//!   `prune`). Units whose crate root source is gone (removed/renamed target) are deleted.
//! - Registry/git units stay only while reachable from a kept workspace unit (or from any unit of
//!   a sibling profile dir) through the `deps` edges of `.fingerprint/*/*.json`; each edge names
//!   the dependency's fingerprint value, the hex in the sibling file.
//! - A rustc hash with no writes for `RUSTC_RETIRE` before the newest unit (old toolchain) goes.
//!   Its last write is remembered (`seen`), so deleting its newest units does not age it.
//! - Without a `Cargo.lock` every unit is grouped like a workspace unit.

use crate::fingerprint::Workspace;
use crate::fsutil::{par_map, size_of};
use crate::prune::Config;
use crate::scan::{Incremental, Unit};
use std::collections::{BTreeMap, HashMap, HashSet};

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// A rustc hash not used for this long (relative to the newest unit) is an old toolchain.
const RUSTC_RETIRE: Duration = Duration::from_secs(14 * 24 * 3600);
/// Legacy-layout units not written for this long beyond the newest new-layout unit are dead.
const LAYOUT_RETIRE: Duration = Duration::from_secs(24 * 3600);
/// A profile/host variant of a package not rebuilt for this long while the package was.
const VARIANT_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

pub(crate) struct Candidate {
    /// Why it is deleted (report breakdown): orphan, superseded, unreachable, removed target,
    /// old toolchain, size cap. Empty while kept.
    pub(crate) reason: &'static str,
    pub(crate) krate: Option<String>,
    pub(crate) id: String, // what goes into the `deleted` log: hash or `inc:<dir>`
    pub(crate) inodes: Vec<u64>,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) mtime: Option<SystemTime>,
    pub(crate) incremental: bool,
    /// File whose mtime was `mtime` at scan time (fingerprint stamp, incremental dir). When the
    /// lock was released between delete batches, a changed guard means cargo touched the unit:
    /// it is skipped. `None`: an orphan, valid while no fingerprint for its hash appears.
    pub(crate) guard: Option<PathBuf>,
}

pub(crate) fn plan(
    units: BTreeMap<String, Unit>,
    incremental: Incremental,
    pinned: &HashMap<String, SystemTime>,
    seen: &HashMap<String, SystemTime>,
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
    let retired = |u: &Unit| {
        let last = rustc_newest
            .get(&u.rustc)
            .copied()
            .max(seen.get(&format!("rustc {}", u.rustc)).copied());
        matches!((newest_all, last), (Some(n), Some(r)) if r + RUSTC_RETIRE < n)
    };
    // After a cargo upgrade to the new build-dir layout the legacy units are dead once nothing
    // wrote them for LAYOUT_RETIRE longer than the new ones (an old cargo still in use writes them).
    let newest_of = |new: bool| {
        units
            .values()
            .filter(|u| u.has_fingerprint && u.new_layout == new)
            .filter_map(|u| u.mtime)
            .max()
    };
    let legacy = newest_of(false).map(|l| l.max(seen.get("legacy").copied().unwrap_or(l)));
    let legacy_dead =
        matches!((legacy, newest_of(true)), (Some(l), Some(n)) if l + LAYOUT_RETIRE < n);

    // Build-script *run* units never reach rustc, so they inherit the clippy tag of the compiled
    // build script they depend on.
    let by_value: HashMap<u64, (String, String)> = units
        .values()
        .filter_map(|u| Some((u.fp_value?, (u.tag.clone(), u.home.clone()))))
        .collect();
    let mut units = units;
    for u in units.values_mut() {
        if u.fp_kind.iter().any(|k| k.starts_with("run-build-script")) {
            if let Some((tag, home)) = u.dep_values.iter().find_map(|v| by_value.get(v)) {
                (u.tag, u.home) = (tag.clone(), home.clone());
            }
        }
    }

    type Member = (Candidate, Vec<u64>, Option<PathBuf>);
    // Registry units of a version that is not in Cargo.lock any more are dead, and so is every
    // unit built against one (transitively): after a revert cargo reuses the *older* copies, so
    // "newest wins" alone would keep the dead ones.
    let dead = ws.map(|w| outdated_units(&units, w)).unwrap_or_default();
    let lock_changed = ws.and_then(|w| w.lock_changed);
    let before_lock =
        |t: SystemTime, newest: SystemTime| lock_changed.is_some_and(|l| t < l && newest >= l);
    let mut groups: HashMap<String, Vec<Member>> = HashMap::new();
    let mut registry: Vec<(Candidate, Option<u64>, Vec<u64>)> = Vec::new();
    for (h, u) in units {
        let is_dead = dead.contains(&h);
        // an orphan has no package name: its file stem is the crate name
        let local = ws.is_none_or(|w| w.is_local(u.name.as_deref().unwrap_or(&u.stem)));
        let c = Candidate {
            krate: if local { u.krate.clone() } else { None }, // only workspace crates are incremental
            id: h,
            inodes: u.inodes.clone(),
            paths: u.paths.clone(),
            mtime: u.mtime,
            incremental: false,
            reason: "",
            guard: u.guard.clone(),
        };
        if is_dead {
            doomed.push(Candidate {
                reason: "outdated dependency",
                ..c
            });
        } else if !u.has_fingerprint {
            doomed.push(Candidate {
                reason: "orphan",
                ..c
            }); // cargo rebuilds it anyway
        } else if legacy_dead && !u.new_layout {
            doomed.push(Candidate {
                reason: "old layout",
                ..c
            });
        } else if retired(&u) {
            doomed.push(Candidate {
                reason: "old toolchain",
                ..c
            });
        } else if local && !is_build_script(&u) {
            groups
                .entry(u.group())
                .or_default()
                .push((c, u.dep_values, u.dep_info));
        } else {
            // registry units and every build script (compile and run): their json carries no
            // features, so feature variants look alike; they live while a kept unit uses them
            registry.push((c, u.fp_value, u.dep_values));
        }
    }

    // Dedup workspace units (all units when locality is unknown).
    let mut roots: Vec<u64> = extra_roots;
    let mut kept_local = Vec::new();
    // A family is a group without its signature: one package/kind built with another profile or
    // for the host. A variant not rebuilt for VARIANT_TTL while its family was is dead.
    let family = |key: &str| key.rsplit_once('|').map_or(key, |(f, _)| f).to_owned();
    let mut family_newest: HashMap<String, SystemTime> = HashMap::new();
    for (key, members) in &groups {
        if let Some(t) = members.iter().filter_map(|(c, ..)| c.mtime).max() {
            let e = family_newest.entry(family(key)).or_insert(t);
            *e = (*e).max(t);
        }
    }
    for (key, members) in groups {
        let newest = members.iter().filter_map(|(c, ..)| c.mtime).max();
        let dead_variant = match (newest, family_newest.get(&family(&key))) {
            (Some(n), Some(&f)) => n + VARIANT_TTL < f,
            _ => false,
        };
        for (c, deps, dep_info) in members {
            // older than the newest by more than the grace period, or built before the last
            // Cargo.lock change while the newest was built after it (a dependency change)
            let stale = matches!((newest, c.mtime), (Some(n), Some(t)) if t + cfg.grace < n || before_lock(t, n));
            if (stale || dead_variant) && !pinned.contains_key(&key) {
                let reason = if stale { "superseded" } else { "stale variant" };
                doomed.push(Candidate { reason, ..c });
            } else {
                kept_local.push((c, deps, dep_info));
            }
        }
    }
    // Units of removed or renamed targets: their crate root source is gone. Only kept units need
    // the check (one dep-info read each), done on a few threads.
    let checked = par_map(kept_local, |(c, deps, dep_info)| {
        let gone = matches!((ws, &dep_info), (Some(w), Some(d)) if w.target_removed(d));
        (c, deps, gone)
    });
    let mut kept_local = Vec::new();
    for (c, deps, gone) in checked {
        if gone {
            doomed.push(Candidate {
                reason: "removed target",
                ..c
            });
        } else {
            kept_local.push((c, deps));
        }
    }
    for (c, deps) in kept_local {
        roots.extend(deps);
        kept.push(c);
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
            // defense in depth: a unit written within the grace period is never "unreachable"
            let recent = matches!((newest_all, c.mtime), (Some(n), Some(t)) if t + cfg.grace >= n && !before_lock(t, n));
            if live || recent {
                kept.push(c)
            } else {
                doomed.push(Candidate {
                    reason: "unreachable",
                    ..c
                })
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
        // one dir can hold sessions of an old and a new unit (same `-C metadata`, e.g. after a
        // RUSTFLAGS change): any live owner keeps it
        let owners: Vec<(&str, bool)> = inodes
            .iter()
            .filter_map(|i| owner.get(i).copied())
            .collect();
        if let Some(&(id, _)) = owners.iter().find(|(_, d)| !d).or(owners.first()) {
            keep_dir[k] = Some(owners.iter().any(|(_, d)| !d));
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
    let keep_dir: Vec<bool> = keep_dir
        .into_iter()
        .map(|k| k.unwrap_or(false) && !cfg.drop_incremental)
        .collect();
    let mut inc_doomed = Vec::new();
    let mut inc_kept = Vec::new();
    for ((dir, path, t, _), keep) in incremental.into_iter().zip(keep_dir) {
        let c = Candidate {
            krate: None,
            inodes: Vec::new(),
            id: format!("inc:{dir}"),
            guard: Some(path.clone()),
            paths: vec![path],
            mtime: t,
            incremental: true,
            reason: "stale incremental",
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

/// Hashes of units whose registry version is not in the lock files, plus every unit that depends
/// on one of them, transitively.
fn outdated_units(units: &BTreeMap<String, Unit>, ws: &Workspace) -> HashSet<String> {
    let registry: Vec<(&String, &Unit)> = units
        .iter()
        .filter(|(_, u)| !ws.is_local(u.name.as_deref().unwrap_or("")))
        .collect();
    let versions = par_map(registry, |(h, u)| {
        let name = u.name.as_deref().unwrap_or("");
        let v = crate::fingerprint::source_version(u.dep_info.as_ref()?, name)?;
        let current = ws.versions.get(name)?;
        (!current.contains(&v)).then(|| h.clone())
    });
    let mut dead: HashSet<String> = versions.into_iter().flatten().collect();
    let mut dead_values: HashSet<u64> = dead.iter().filter_map(|h| units[h].fp_value).collect();
    loop {
        let more: Vec<&String> = units
            .iter()
            .filter(|(h, u)| {
                !dead.contains(*h) && u.dep_values.iter().any(|v| dead_values.contains(v))
            })
            .map(|(h, _)| h)
            .collect();
        if more.is_empty() {
            return dead;
        }
        for h in more {
            dead_values.extend(units[h].fp_value);
            dead.insert(h.clone());
        }
    }
}

/// Build-script compile or run unit (`build-script-build-script-build`, `run-build-script-...`).
fn is_build_script(u: &Unit) -> bool {
    u.fp_kind
        .iter()
        .any(|k| k.starts_with("build-script-") || k.starts_with("run-build-script"))
}

/// Over the cap: delete the oldest kept units (incremental first at equal age) until under it.
pub(crate) fn apply_cap(
    mut doomed: Vec<Candidate>,
    mut kept: Vec<Candidate>,
    cap: Option<u64>,
) -> Vec<Candidate> {
    let Some(cap) = cap else { return doomed };
    let mut total: u64 = kept.iter().flat_map(|c| &c.paths).map(|p| size_of(p)).sum();
    // ties (one build writes many units in the same second) broken by id: a dry run then names
    // exactly what the real run deletes
    kept.sort_by(|a, b| (a.mtime, !a.incremental, &a.id).cmp(&(b.mtime, !b.incremental, &b.id)));
    for c in kept {
        if total <= cap {
            break;
        }
        total -= c.paths.iter().map(|p| size_of(p)).sum::<u64>().min(total);
        doomed.push(Candidate {
            reason: "size cap",
            ..c
        });
    }
    doomed
}
