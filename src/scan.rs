//! Collect units (everything sharing one metadata hash) and incremental dirs of a profile.

use crate::fingerprint::{json_signature, parse_fingerprint_json, parse_le_hex};
use crate::fsutil::{mtime, read_dir};
use crate::layout::{out_kind, split_hash, STATE_DIR, TAGS_DIR};
use std::collections::{BTreeMap, BTreeSet};
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
    pub(crate) krate: Option<String>, // rustc crate name (stem of `deps/<crate>-H.d`), for incremental dirs
    pub(crate) fp_value: Option<u64>, // this unit's fingerprint value
    pub(crate) dep_values: Vec<u64>,  // fingerprint values of its dependencies
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

pub(crate) type Incremental = Vec<(String, PathBuf, Option<SystemTime>)>;

pub(crate) fn scan(profile: &Path, with_sig: bool) -> (BTreeMap<String, Unit>, Incremental) {
    let mut units: BTreeMap<String, Unit> = BTreeMap::new();
    let tags = profile.join(STATE_DIR).join(TAGS_DIR);
    for (name, path) in read_dir(&profile.join(".fingerprint")) {
        let Some((pkg, h, "")) = split_hash(&name) else {
            continue;
        };
        let u = units.entry(h.to_owned()).or_default();
        u.name = Some(pkg.to_owned());
        u.has_fingerprint = true;
        // file names encode the target kind: lib-x, test-lib-x, bin-x, build-script-..., run-build-script-...
        u.fp_kind = read_dir(&path)
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| n != "invoked.timestamp" && !n.starts_with("output-"))
            .collect();
        if let Some(json) = u.fp_kind.iter().find(|n| n.ends_with(".json")).cloned() {
            let text = fs::read_to_string(path.join(&json)).unwrap_or_default();
            (u.rustc, u.dep_values) = parse_fingerprint_json(&text);
            if with_sig {
                u.sig = json_signature(&text);
            }
            u.fp_value = fs::read_to_string(path.join(json.trim_end_matches(".json")))
                .ok()
                .and_then(|h| parse_le_hex(h.trim()));
        }
        u.tag = fs::read_to_string(tags.join(h)).unwrap_or_default();
        u.touch(mtime(&path));
        u.touch(mtime(&path.join("invoked.timestamp")));
        u.paths.push(path);
    }
    for (name, path) in read_dir(&profile.join("build")) {
        let Some((pkg, h, "")) = split_hash(&name) else {
            continue;
        };
        let u = units.entry(h.to_owned()).or_default();
        if u.stem.is_empty() {
            u.stem = pkg.to_owned();
        }
        if path.join("build-script-build").exists() {
            u.krate = Some("build_script_build".to_owned());
        }
        u.touch(mtime(&path));
        u.paths.push(path);
    }
    for area in ["deps", "examples"] {
        for (name, path) in read_dir(&profile.join(area)) {
            let Some((stem, h, rest)) = split_hash(&name) else {
                continue;
            };
            let u = units.entry(h.to_owned()).or_default();
            if u.stem.is_empty() {
                u.stem = stem.to_owned();
            }
            if let Some(k) = out_kind(rest, &path) {
                u.out_kind.insert(k);
            }
            if rest == ".d" {
                u.krate = Some(stem.to_owned());
            }
            u.touch(mtime(&path));
            u.paths.push(path);
        }
    }
    let incremental = read_dir(&profile.join("incremental"))
        .into_iter()
        .filter(|(n, p)| n.contains('-') && p.is_dir())
        .map(|(n, p)| {
            let t = mtime(&p);
            (n, p, t)
        })
        .collect();
    (units, incremental)
}
