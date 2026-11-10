//! Reading Cargo's `.fingerprint/*/*.json` and the workspace `Cargo.lock`.

use crate::fsutil::read_dir;
use crate::layout::profile_dirs;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The workspace that owns a target dir: its root and its path (non-registry) packages.
pub(crate) struct Workspace {
    pub(crate) root: PathBuf,
    pub(crate) locals: HashSet<String>,
}

impl Workspace {
    pub(crate) fn is_local(&self, pkg: &str) -> bool {
        self.locals.contains(pkg)
    }

    /// The crate root source named first in a rustc dep-info file is gone: the target
    /// (bin, test, example, build script) was removed or renamed.
    pub(crate) fn target_removed(&self, dep_info: &Path) -> bool {
        let Some(root) = fs::read_to_string(dep_info)
            .ok()
            .and_then(|t| first_dependency(&t))
        else {
            return false;
        };
        !self.root.join(root).exists() // join keeps absolute paths as they are
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

/// Package names without a `source` in the workspace `Cargo.lock` next to the target dir.
pub(crate) fn workspace(profile: &Path) -> Option<Workspace> {
    let target = profile
        .ancestors()
        .find(|a| a.join("CACHEDIR.TAG").is_file())?;
    let root = target.parent()?;
    let lock = fs::read_to_string(root.join("Cargo.lock")).ok()?;
    let mut out = HashSet::new();
    for pkg in lock.split("[[package]]").skip(1) {
        let name = pkg
            .lines()
            .find_map(|l| l.strip_prefix("name = "))?
            .trim()
            .trim_matches('"');
        if !pkg.lines().any(|l| l.starts_with("source = ")) {
            out.insert(name.to_owned());
        }
    }
    (!out.is_empty()).then(|| Workspace {
        root: root.to_path_buf(),
        locals: out,
    })
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

/// What makes two builds of the same package legitimately different: everything in the json
/// except `deps` and `local` (they embed the unit hash) and `path` (source path hash).
pub(crate) fn json_signature(s: &str) -> String {
    let head = s.split("\"deps\":").next().unwrap_or("");
    let tail = s.split_once("\"rustflags\":").map_or("", |(_, t)| t);
    let head: String = head
        .split(',')
        .filter(|f| !f.starts_with("\"path\":"))
        .collect::<Vec<_>>()
        .join(",");
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
