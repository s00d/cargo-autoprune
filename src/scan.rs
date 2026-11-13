//! Collect units (everything sharing one metadata hash) and incremental dirs of a profile.

use crate::fingerprint::{json_signature, parse_fingerprint_json, parse_le_hex};
use crate::fsutil::{entries, mtime, par_map};
use crate::layout::{is_codegen, split_hash, STATE_DIR, TAGS_DIR};
use crate::prune::Phases;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Default)]
pub(crate) struct Unit {
    pub(crate) name: Option<String>, // package name from .fingerprint
    pub(crate) stem: String,         // file stem from deps (fallback name)
    pub(crate) fp_kind: BTreeSet<String>,
    pub(crate) codegen: bool, // has a codegen output (not only `rmeta`: a `check` build)
    pub(crate) has_fingerprint: bool,
    pub(crate) rustc: u64,   // hash of `rustc -vV` (differs between toolchains)
    pub(crate) sig: String, // fingerprint json minus deps/local/path: features, profile, rustflags, ...
    pub(crate) tag: String, // workspace wrapper that built it (clippy-driver), from TAGS_DIR
    pub(crate) home: String, // workspace root recorded by the wrapper (shared target dirs)
    pub(crate) krate: Option<String>,
    pub(crate) dep_info: Option<PathBuf>, // rustc dep-info (`deps/<crate>-H.d`, build script's `.d`) // rustc crate name (stem of `deps/<crate>-H.d`), for incremental dirs
    pub(crate) fp_value: Option<u64>,     // this unit's fingerprint value
    pub(crate) dep_values: Vec<u64>,      // fingerprint values of its dependencies
    pub(crate) inodes: Vec<u64>, // of `.rmeta` / `.rcgu.o` files, shared with incremental sessions
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) mtime: Option<SystemTime>,
    pub(crate) guard: Option<PathBuf>, // the file `mtime` was read from (fingerprinted units)
    pub(crate) new_layout: bool,       // build/<pkg>/<hash>/ (cargo 1.100+)
}

impl Unit {
    pub(crate) fn group(&self) -> String {
        let name = self.name.as_deref().unwrap_or(&self.stem);
        let fp: Vec<&str> = self.fp_kind.iter().map(String::as_str).collect();
        format!(
            "{name}|{}|{}|{}|{}|{}|{}",
            fp.join(","),
            self.codegen,
            self.rustc,
            self.tag,
            self.home,
            self.sig
        )
    }
}

/// Fill a unit from its fingerprint dir (`.fingerprint/<pkg>-<hash>` or `build/<pkg>/<hash>/fingerprint`).
fn read_fingerprint(
    u: &mut Unit,
    pkg: &str,
    dir: &Path,
    with_sig: bool,
    tags: &HashMap<String, String>,
    h: &str,
) {
    let files = entries(dir);
    if files.is_empty() {
        return;
    }
    u.name = Some(pkg.to_owned());
    u.has_fingerprint = true;
    // file names encode the target kind: lib-x, test-lib-x, bin-x, build-script-..., run-build-script-...
    u.fp_kind = files
        .iter()
        .map(|f| f.name.clone())
        .filter(|n| n != "invoked.timestamp" && !n.starts_with("output-"))
        .collect();
    if let Some(json) = u.fp_kind.iter().find(|n| n.ends_with(".json")).cloned() {
        let text = fs::read_to_string(dir.join(&json)).unwrap_or_default();
        (u.rustc, u.dep_values) = parse_fingerprint_json(&text);
        if with_sig {
            u.sig = json_signature(&text);
        }
        u.fp_value = fs::read_to_string(dir.join(json.trim_end_matches(".json")))
            .ok()
            .and_then(|h| parse_le_hex(h.trim()));
    }
    u.tag = tags.get(h).cloned().unwrap_or_default();
    // cargo writes invoked.timestamp every time it builds the unit
    let stamp = files.iter().find(|f| f.name == "invoked.timestamp");
    let guard = stamp.map_or(dir, |f| &f.path).to_path_buf();
    u.mtime = mtime(&guard);
    u.guard = Some(guard);
}

fn has_files(dir: &Path) -> bool {
    entries(dir)
        .into_iter()
        .any(|e| !e.is_dir || has_files(&e.path))
}

fn is_hash(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// Start time encoded in an incremental session dir name: `s-<micros, base 36>-<random>[-<svh>]`.
pub(crate) fn session_start(name: &str) -> Option<SystemTime> {
    let ts = name.strip_prefix("s-")?.split('-').next()?;
    let micros = u64::from_str_radix(ts, 36)
        .ok()
        .filter(|&m| m >= 946_684_800_000_000)?; // >= 2000
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_micros(micros))
}

/// `incremental/<crate>-<id>` dirs: name, path, mtime, inodes of their session files.
pub(crate) type Incremental = Vec<(String, PathBuf, Option<SystemTime>, Vec<u64>)>;

pub(crate) fn scan(
    profile: &Path,
    with_sig: bool,
    ph: &mut Phases,
) -> (BTreeMap<String, Unit>, Incremental) {
    let mut units: BTreeMap<String, Unit> = BTreeMap::new();
    // clippy tags: one listing instead of a failed open per unit
    let tags: HashMap<String, String> = entries(&profile.join(STATE_DIR).join(TAGS_DIR))
        .into_iter()
        .map(|e| (e.name, fs::read_to_string(e.path).unwrap_or_default()))
        .collect();
    let fingerprints = par_map(entries(&profile.join(".fingerprint")), |e| {
        let (pkg, h, "") = split_hash(&e.name)? else {
            return None;
        };
        let mut u = Unit::default();
        read_fingerprint(&mut u, pkg, &e.path, with_sig, &tags, h);
        let h = h.to_owned();
        u.paths.push(e.path);
        Some((h, u))
    });
    units.extend(fingerprints.into_iter().flatten());
    ph.mark("scan .fingerprint");
    for e in entries(&profile.join("build")) {
        let Some((pkg, h, "")) = split_hash(&e.name) else {
            continue;
        };
        let u = units.entry(h.to_owned()).or_default();
        if u.stem.is_empty() {
            u.stem = pkg.to_owned();
        }
        let script = e.path.join(format!("build_script_build-{h}.d"));
        if script.exists() {
            u.krate = Some("build_script_build".to_owned());
            u.dep_info = Some(script);
        }
        if !u.has_fingerprint {
            u.mtime = u.mtime.max(mtime(&e.path));
        }
        u.paths.push(e.path);
    }
    ph.mark("scan build");
    // deps/ holds most files (objects, dep-info): names and inodes only, no stat per file
    for area in ["deps", "examples"] {
        for e in entries(&profile.join(area)) {
            let Some((stem, h, rest)) = split_hash(&e.name) else {
                continue;
            };
            let u = units.entry(h.to_owned()).or_default();
            if u.stem.is_empty() {
                u.stem = stem.to_owned();
            }
            if is_codegen(rest.trim_start_matches('.')) {
                u.codegen = true;
            }
            if rest == ".rmeta" || rest.ends_with(".rcgu.o") {
                u.inodes.extend(e.ino);
            }
            if rest == ".d" {
                u.krate = Some(stem.to_owned());
                u.dep_info = Some(e.path.clone());
            }
            u.paths.push(e.path);
        }
    }
    // a unit not dated yet (an orphan; one stat each, they are few) is dated by its dep-info, so
    // it claims the incremental dir of the interrupted session that wrote it
    for u in units.values_mut().filter(|u| u.mtime.is_none()) {
        u.mtime = u.dep_info.as_deref().and_then(mtime);
    }
    // new build-dir layout (cargo 1.100+): build/<pkg>/<hash>/{fingerprint,out,run}, one dir per
    // unit. A hash also present in the legacy layout above is then a dead copy of the old layout.
    for pkg in entries(&profile.join("build")) {
        if !pkg.is_dir || split_hash(&pkg.name).is_some() {
            continue;
        }
        for e in entries(&pkg.path)
            .into_iter()
            .filter(|e| e.is_dir && is_hash(&e.name))
        {
            if !e.path.join("fingerprint").is_dir() && !has_files(&e.path) {
                continue; // an empty out/ cargo prepared: costs nothing, may be needed
            }
            let legacy = units.remove(&e.name);
            let u = units.entry(e.name.clone()).or_default();
            read_fingerprint(
                u,
                &pkg.name,
                &e.path.join("fingerprint"),
                with_sig,
                &tags,
                &e.name,
            );
            u.stem = pkg.name.clone();
            for f in entries(&e.path.join("out")) {
                let rest = f.name.split_once('.').map_or("", |(_, r)| r);
                if is_codegen(rest) {
                    u.codegen = true;
                }
                if rest == "rmeta" || f.name.ends_with(".rcgu.o") {
                    u.inodes.extend(f.ino);
                }
                if rest == "d" {
                    let stem = f.name.trim_end_matches(".d");
                    let krate = split_hash(stem).map_or(stem, |(s, _, _)| s);
                    u.krate = Some(krate.to_owned());
                    u.dep_info = Some(f.path.clone());
                }
            }
            u.paths = vec![e.path];
            u.new_layout = true;
            if let Some(old) = legacy {
                // same hash in the old layout: cargo uses only the new dir now
                units.insert(
                    format!("{}~legacy", e.name),
                    Unit {
                        has_fingerprint: false,
                        ..old
                    },
                );
            }
        }
    }
    ph.mark("scan deps");
    let dirs: Vec<_> = entries(&profile.join("incremental"))
        .into_iter()
        .filter(|e| e.is_dir && e.name.contains('-'))
        .collect();
    let incremental = par_map(dirs, |e| {
        let sessions: Vec<_> = entries(&e.path)
            .into_iter()
            .filter(|s| s.is_dir && s.name.starts_with("s-"))
            .collect();
        // when the newest session started: `s-<micros since epoch, base 36>-...`, the same moment
        // cargo writes the unit's invoked.timestamp (closer than the dir's mtime, which is the end)
        let started = sessions.iter().filter_map(|s| session_start(&s.name)).max();
        let inodes = sessions
            .iter()
            .flat_map(|s| entries(&s.path))
            .filter(|f| f.name.ends_with(".o") || f.name == "metadata.rmeta")
            .filter_map(|f| f.ino)
            .collect();
        let t = started.or_else(|| mtime(&e.path));
        (e.name, e.path, t, inodes)
    });
    ph.mark("scan incremental");
    (units, incremental)
}
