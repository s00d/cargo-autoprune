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
pub(crate) fn first_dependency(text: &str) -> Option<String> {
    let (_, deps) = text.lines().next()?.split_once(": ")?;
    let mut out = String::new();
    let mut esc = false;
    for ch in deps.chars() {
        match ch {
            _ if esc => {
                out.push(ch);
                esc = false;
            }
            '\\' if !cfg!(windows) => esc = true,
            ' ' if !out.is_empty() => break,
            ' ' => {}
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
        for (_, unit) in read_dir(&p.join(".fingerprint")) {
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
    for r in &roots {
        locals.extend(local_names(
            &fs::read_to_string(r.join("Cargo.lock")).unwrap_or_default(),
        ));
    }
    let ws = (!locals.is_empty()).then_some(Workspace { roots, locals });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dep_info_first_dependency() {
        assert_eq!(
            first_dependency("/t/deps/app-1.d: app/src/main.rs Cargo.toml\n\napp/src/main.rs:\n")
                .as_deref(),
            Some("app/src/main.rs")
        );
        assert_eq!(
            first_dependency("/t/x: /a\\ b/lib.rs /c.rs").as_deref(),
            Some("/a b/lib.rs")
        );
        assert_eq!(first_dependency("/t/x:"), None);
    }

    #[test]
    fn signature_ignores_deps_but_keeps_variant_fields() {
        let a = r#"{"rustc":1,"features":"[\"x\"]","declared_features":"[]","target":5,"profile":7,"path":9,"deps":[],"local":[{"CheckDepInfo":{"dep_info":"a-1/dep"}}],"rustflags":[],"config":3,"compile_kind":0}"#;
        let b = a
            .replace("\"deps\":[]", "\"deps\":[[1,\"d\",false,2]]")
            .replace("a-1/dep", "a-2/dep");
        assert_eq!(json_signature(a), json_signature(&b));
        for (from, to) in [
            ("[\\\"x\\\"]", "[]"),
            ("\"profile\":7", "\"profile\":8"),
            ("\"rustflags\":[]", "\"rustflags\":[\"-Cx\"]"),
        ] {
            assert_ne!(
                json_signature(a),
                json_signature(&a.replace(from, to)),
                "{from}"
            );
        }
    }

    #[test]
    fn fingerprint_json_parsing() {
        let j = r#"{"rustc":139,"features":"[]","deps":[[13,"build_script_build",false,49],[49,"util",false,15]],"local":[{"CheckDepInfo":{"dep_info":"x","checksum":false}}]}"#;
        assert_eq!(parse_fingerprint_json(j), (139, vec![49, 15]));
        assert_eq!(
            parse_fingerprint_json(r#"{"rustc":1,"deps":[],"local":[]}"#),
            (1, vec![])
        );
        assert_eq!(parse_le_hex("d5dd8855943b3fe2"), Some(16302814684380716501));
        assert_eq!(parse_le_hex("xyz"), None);
    }
}
