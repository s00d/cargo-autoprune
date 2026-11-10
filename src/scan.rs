//! Collect units (everything sharing one metadata hash) and incremental dirs of a profile.

use crate::fingerprint::{json_signature, parse_fingerprint_json, parse_le_hex};
use crate::fsutil::{entries, mtime, par_map};
use crate::layout::{out_kind, split_hash, STATE_DIR, TAGS_DIR};
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
    pub(crate) out_kind: BTreeSet<&'static str>,
    pub(crate) has_fingerprint: bool,
    pub(crate) rustc: u64,  // hash of `rustc -vV` (differs between toolchains)
    pub(crate) sig: String, // fingerprint json minus deps/local/path: features, profile, rustflags, ...
    pub(crate) tag: String, // workspace wrapper that built it (clippy-driver), from TAGS_DIR
    pub(crate) krate: Option<String>,
    pub(crate) dep_info: Option<PathBuf>, // rustc dep-info (`deps/<crate>-H.d`, build script's `.d`) // rustc crate name (stem of `deps/<crate>-H.d`), for incremental dirs
    pub(crate) fp_value: Option<u64>,     // this unit's fingerprint value
    pub(crate) dep_values: Vec<u64>,      // fingerprint values of its dependencies
    pub(crate) inodes: Vec<u64>, // of `.rmeta` / `.rcgu.o` files, shared with incremental sessions
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) mtime: Option<SystemTime>,
}

impl Unit {
    pub(crate) fn touch(&mut self, t: Option<SystemTime>) {
        if t > self.mtime {
            self.mtime = t;
        }
    }
    pub(crate) fn group(&self) -> String {
        let name = self.name.as_deref().unwrap_or(&self.stem);
        let fp: Vec<&str> = self.fp_kind.iter().map(String::as_str).collect();
        let out: Vec<&str> = self.out_kind.iter().copied().collect();
        format!(
            "{name}|{}|{}|{}|{}|{}",
            fp.join(","),
            out.join(","),
            self.rustc,
            self.tag,
            self.sig
        )
    }
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
        let mut u = Unit {
            name: Some(pkg.to_owned()),
            has_fingerprint: true,
            ..Unit::default()
        };
        // file names encode the target kind: lib-x, test-lib-x, bin-x, build-script-..., run-build-script-...
        let files = entries(&e.path);
        u.fp_kind = files
            .iter()
            .map(|f| f.name.clone())
            .filter(|n| n != "invoked.timestamp" && !n.starts_with("output-"))
            .collect();
        if let Some(json) = u.fp_kind.iter().find(|n| n.ends_with(".json")).cloned() {
            let text = fs::read_to_string(e.path.join(&json)).unwrap_or_default();
            (u.rustc, u.dep_values) = parse_fingerprint_json(&text);
            if with_sig {
                u.sig = json_signature(&text);
            }
            u.fp_value = fs::read_to_string(e.path.join(json.trim_end_matches(".json")))
                .ok()
                .and_then(|h| parse_le_hex(h.trim()));
        }
        u.tag = tags.get(h).cloned().unwrap_or_default();
        // cargo writes invoked.timestamp every time it builds the unit
        let stamp = files.iter().find(|f| f.name == "invoked.timestamp");
        u.mtime = mtime(stamp.map_or(&e.path, |f| &f.path));
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
            u.touch(mtime(&e.path));
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
            if let Some(k) = out_kind(rest, e.is_dir) {
                u.out_kind.insert(k);
            }
            if rest == ".rmeta" || rest.ends_with(".rcgu.o") {
                u.inodes.push(e.ino);
            }
            if rest == ".d" {
                u.krate = Some(stem.to_owned());
                u.dep_info = Some(e.path.clone());
            }
            u.paths.push(e.path);
        }
    }
    ph.mark("scan deps");
    let dirs: Vec<_> = entries(&profile.join("incremental"))
        .into_iter()
        .filter(|e| e.is_dir && e.name.contains('-'))
        .collect();
    let incremental = par_map(dirs, |e| {
        let inodes = entries(&e.path)
            .into_iter()
            .filter(|s| s.is_dir && s.name.starts_with("s-"))
            .flat_map(|s| entries(&s.path))
            .filter(|f| f.name.ends_with(".o") || f.name == "metadata.rmeta")
            .map(|f| f.ino)
            .collect();
        let t = mtime(&e.path);
        (e.name, e.path, t, inodes)
    });
    ph.mark("scan incremental");
    (units, incremental)
}
