//! Reading Cargo's `.fingerprint/*/*.json` and the workspace `Cargo.lock`.

use crate::fsutil::read_dir;
use crate::layout::{profile_dirs, STATE_DIR, UNITS_FILE};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// The workspace that owns a target dir: its root and its path (non-registry) packages.
pub(crate) struct Workspace {
    /// Dirs holding a workspace `Cargo.lock` (several when a target dir is shared).
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) locals: HashSet<String>,
    /// Versions of every registry/git package in the lock files: `serde` -> {"1.0.228"}.
    pub(crate) versions: HashMap<String, HashSet<String>>,
    /// When a `Cargo.lock` last changed (cargo rewrites it only when its content changes).
    pub(crate) lock_changed: Option<std::time::SystemTime>,
}

impl Workspace {
    pub(crate) fn is_local(&self, pkg: &str) -> bool {
        self.locals.contains(pkg)
    }

    /// The crate root source named first in a rustc dep-info file is gone: the target
    /// (bin, test, example, build script) was removed or renamed. Relative paths are relative to
    /// the workspace root cargo ran rustc in.
    pub(crate) fn target_removed(&self, dep_info: &Path) -> bool {
        let Some(src) = fs::read_to_string(dep_info)
            .ok()
            .and_then(|t| first_dependency(&t))
        else {
            return false;
        };
        !self.roots.iter().any(|r| r.join(&src).exists()) // join keeps absolute paths as they are
    }
}

/// First prerequisite of the first rule in a make-style dep-info file (`out: src/main.rs ...`).
/// rustc escapes only spaces (`\ `), on every platform; any other backslash is part of the path
/// (a Windows separator).
pub(crate) fn first_dependency(text: &str) -> Option<String> {
    let (_, deps) = text.lines().next()?.split_once(": ")?;
    let mut out = String::new();
    let mut chars = deps.trim_start().chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.next_if_eq(&' ').is_some() => out.push(' '),
            ' ' => break,
            _ => out.push(ch),
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Dependency fingerprint values of every unit in the *other* profile dirs of the same target:
/// with `--target`, build scripts and proc-macros live in the host profile but are used from
/// `<triple>/<profile>`.
pub(crate) fn sibling_dep_values(profile: &Path) -> Vec<u64> {
    let Some(target) = profile
        .ancestors()
        .find(|a| a.join("CACHEDIR.TAG").is_file())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for p in profile_dirs(target).into_iter().filter(|p| p != profile) {
        for unit in crate::layout::fingerprint_dirs(&p) {
            for (name, f) in read_dir(&unit) {
                if name.ends_with(".json") {
                    out.extend(
                        parse_fingerprint_json(&fs::read_to_string(f).unwrap_or_default()).1,
                    );
                }
            }
        }
    }
    out
}

/// Workspaces using this profile dir: the `Cargo.lock` next to the target dir, plus the
/// workspaces of the package dirs the wrapper recorded. Path packages (no `source` in a lock) are
/// local. Also returns each recorded unit's workspace root.
pub(crate) fn workspace(profile: &Path) -> (Option<Workspace>, HashMap<String, PathBuf>) {
    let mut lock_root: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    let mut find = |dir: &Path| -> Option<PathBuf> {
        lock_root
            .entry(dir.to_path_buf())
            .or_insert_with(|| {
                dir.ancestors()
                    .find(|a| a.join("Cargo.lock").is_file())
                    .map(Path::to_path_buf)
            })
            .clone()
    };
    let mut homes = HashMap::new();
    for (h, dir) in recorded_units(profile) {
        if let Some(r) = find(&dir) {
            homes.insert(h, r);
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(target) = profile
        .ancestors()
        .find(|a| a.join("CACHEDIR.TAG").is_file())
    {
        if let Some(p) = target.parent().filter(|p| p.join("Cargo.lock").is_file()) {
            roots.push(p.to_path_buf());
        }
    }
    for r in homes.values() {
        if !roots.contains(r) {
            roots.push(r.clone());
        }
    }
    let mut locals = HashSet::new();
    let mut versions: HashMap<String, HashSet<String>> = HashMap::new();
    for r in &roots {
        let lock = fs::read_to_string(r.join("Cargo.lock")).unwrap_or_default();
        locals.extend(local_names(&lock));
        for (name, version) in source_versions(&lock) {
            versions.entry(name).or_default().insert(version);
        }
    }
    let lock_changed = roots
        .iter()
        .filter_map(|r| {
            fs::metadata(r.join("Cargo.lock"))
                .and_then(|m| m.modified())
                .ok()
        })
        .max();
    let ws = (!locals.is_empty()).then_some(Workspace {
        roots,
        locals,
        versions,
        lock_changed,
    });
    (ws, homes)
}

/// `(unit hash, package dir)` pairs recorded by the wrapper; the last record of a hash wins.
pub(crate) fn recorded_units(profile: &Path) -> HashMap<String, PathBuf> {
    fs::read_to_string(profile.join(STATE_DIR).join(UNITS_FILE))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(h, d)| (h.to_owned(), PathBuf::from(d)))
        .collect()
}

/// `(name, version)` of packages with a `source` (registry, git) in a `Cargo.lock`.
fn source_versions(lock: &str) -> Vec<(String, String)> {
    let field = |pkg: &str, key: &str| {
        pkg.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim().trim_matches('"').to_owned())
    };
    lock.split("[[package]]")
        .skip(1)
        .filter(|pkg| pkg.lines().any(|l| l.starts_with("source = ")))
        .filter_map(|pkg| Some((field(pkg, "name = ")?, field(pkg, "version = ")?)))
        .collect()
}

/// Version of a registry package from where its sources live, read from the unit's rustc
/// dep-info: `.../registry/src/<index>/memchr-2.8.2/src/lib.rs` -> `2.8.2`.
pub(crate) fn source_version(dep_info: &Path, pkg: &str) -> Option<String> {
    let src = first_dependency(&fs::read_to_string(dep_info).ok()?)?;
    let prefix = format!("{pkg}-");
    Path::new(&src).components().find_map(|c| {
        let v = c.as_os_str().to_str()?.strip_prefix(&prefix)?;
        // semver `1.2.3[-pre][+build]`: git checkouts are `<name>-<hash>`, which is no version
        let core: Vec<&str> = v.split(['-', '+']).next()?.split('.').collect();
        let ok = core.len() == 3 && core.iter().all(|n| n.parse::<u64>().is_ok());
        ok.then(|| v.to_owned())
    })
}

fn local_names(lock: &str) -> Vec<String> {
    lock.split("[[package]]")
        .skip(1)
        .filter(|pkg| !pkg.lines().any(|l| l.starts_with("source = ")))
        .filter_map(|pkg| pkg.lines().find_map(|l| l.strip_prefix("name = ")))
        .map(|n| n.trim().trim_matches('"').to_owned())
        .collect()
}

/// `rustc` hash and dependency fingerprint values from a fingerprint json:
/// `{"rustc":N,...,"deps":[[pkg_id,"name",bool,FP],...],...}`.
pub(crate) fn parse_fingerprint_json(s: &str) -> (u64, Vec<u64>) {
    let num = |t: &str| t.trim().parse::<u64>().ok();
    let rustc = s
        .split_once("\"rustc\":")
        .and_then(|(_, r)| num(r.split(|c: char| !c.is_ascii_digit()).next()?))
        .unwrap_or(0);
    let mut deps = Vec::new();
    if let Some((_, rest)) = s.split_once("\"deps\":[") {
        let mut start = None;
        for (i, ch) in rest.char_indices() {
            match ch {
                '[' => start = Some(i + 1),
                ']' => match start.take() {
                    Some(st) => deps.extend(rest[st..i].rsplit(',').next().and_then(num)),
                    None => break, // end of the deps array
                },
                _ => {}
            }
        }
    }
    (rustc, deps)
}

/// What lets two builds of one workspace package be live at the same time: everything in the
/// fingerprint json except `deps` and `local` (they embed the unit hash). Features, rustflags,
/// config, profile and host/target stay in, so `-p x --features y` next to a plain build never
/// thrash. A variant that is dead (after an edit of `[features]` or `RUSTFLAGS`) expires through
/// the stale-variant rule in `plan`.
pub(crate) fn json_signature(s: &str) -> String {
    let head = s.split("\"deps\":").next().unwrap_or("");
    let tail = s.split_once("\"rustflags\":").map_or("", |(_, t)| t);
    format!("{head}{tail}")
}

/// Cargo writes fingerprint values as hex of the little-endian bytes.
pub(crate) fn parse_le_hex(h: &str) -> Option<u64> {
    if h.len() != 16 {
        return None;
    }
    let mut b = [0u8; 8];
    for (i, byte) in b.iter_mut().enumerate() {
        *byte = u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(u64::from_le_bytes(b))
}
