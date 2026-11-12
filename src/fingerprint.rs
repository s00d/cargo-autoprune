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
/// Backslash escapes spaces in rustc's dep-info except on Windows, where it separates paths.
pub(crate) fn first_dependency(text: &str) -> Option<String> {
    first_dependency_with(text, !cfg!(windows))
}

fn first_dependency_with(text: &str, escapes: bool) -> Option<String> {
    let (_, deps) = text.lines().next()?.split_once(": ")?;
    let mut out = String::new();
    let mut esc = false;
    for ch in deps.chars() {
        match ch {
            _ if esc => {
                out.push(ch);
                esc = false;
            }
            '\\' if escapes => esc = true,
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
        // semver `1.2.3[-pre]`: git checkouts are `<name>-<hash>`, which is no version
        let numeric = |p: Option<&str>| {
            p.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        };
        let mut parts = v.splitn(3, '.');
        let ok = numeric(parts.next())
            && numeric(parts.next())
            && parts
                .next()
                .is_some_and(|p| p.starts_with(|ch: char| ch.is_ascii_digit()));
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
        #[cfg(not(windows))] // a backslash is a path separator there, not an escape
        assert_eq!(
            first_dependency("/t/x: /a\\ b/lib.rs /c.rs").as_deref(),
            Some("/a b/lib.rs")
        );
        assert_eq!(
            first_dependency_with("C:\\t\\x.d: C:\\ws\\src\\main.rs C:\\ws\\Cargo.toml", false)
                .as_deref(),
            Some("C:\\ws\\src\\main.rs")
        );
        assert_eq!(
            first_dependency_with("/t/x: /a\\ b/lib.rs /c.rs", true).as_deref(),
            Some("/a b/lib.rs")
        );
        assert_eq!(first_dependency("/t/x:"), None);
        assert_eq!(
            first_dependency("/t/x:  a.rs b.rs").as_deref(),
            Some("a.rs")
        );
        assert_eq!(first_dependency("/t/x:   ").as_deref(), None);
    }

    #[test]
    fn removed_target_by_dep_info() {
        let fx = crate::testutil::Fx::new("fp-removed");
        let root = fx.root.join("ws");
        fs::create_dir_all(root.join("app/src")).unwrap();
        fs::write(root.join("app/src/main.rs"), "").unwrap();
        let ws = Workspace {
            roots: vec![fx.root.join("other"), root.clone()],
            locals: HashSet::new(),
            versions: HashMap::new(),
            lock_changed: None,
        };
        let d = fx.root.join("x.d");
        fs::write(&d, "/t/x.d: app/src/main.rs\n").unwrap();
        assert!(!ws.target_removed(&d));
        fs::write(&d, "/t/x.d: app/src/old.rs\n").unwrap();
        assert!(ws.target_removed(&d));
        fs::write(
            &d,
            format!("/t/x.d: {}\n", root.join("app/src/main.rs").display()),
        )
        .unwrap();
        assert!(!ws.target_removed(&d), "absolute path");
        assert!(!ws.target_removed(&fx.root.join("missing.d")));
    }

    #[test]
    fn workspaces_of_a_profile() {
        let fx = crate::testutil::Fx::new("fp-ws");
        fx.lockfile(&["app"], &["serde"]);
        let other = fx.root.join("elsewhere/proj");
        fs::create_dir_all(other.join("pkg")).unwrap();
        fs::write(
            other.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"tool\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"serde\"\nversion = \"2.0.0\"\nsource = \"registry+x\"\n",
        )
        .unwrap();
        let state = fx.prof.join(STATE_DIR);
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join(UNITS_FILE),
            format!(
                "{a}\t{p}\n{b}\t{p}\nbad line\n{a}\t{r}\n",
                a = crate::testutil::A,
                b = crate::testutil::B,
                p = other.join("pkg").display(),
                r = fx.root.display()
            ),
        )
        .unwrap();
        let rec = recorded_units(&fx.prof);
        assert_eq!(rec.len(), 2);
        assert_eq!(rec[crate::testutil::A], fx.root, "last record wins");
        let (ws, homes) = workspace(&fx.prof);
        let ws = ws.unwrap();
        assert_eq!(ws.roots, [fx.root.clone(), other.clone()]);
        assert!(ws.is_local("app") && ws.is_local("tool") && !ws.is_local("serde"));
        let mut v: Vec<&String> = ws.versions["serde"].iter().collect();
        v.sort();
        assert_eq!(v, ["1.0.0", "2.0.0"]);
        assert_eq!(ws.versions.len(), 1);
        assert_eq!(homes[crate::testutil::B], other);
        assert!(ws.lock_changed.is_some());
    }

    #[test]
    fn sibling_profiles_dep_values() {
        let fx = crate::testutil::Fx::new("fp-sibling");
        fx.node("own", crate::testutil::A, 0, 1, 10, &[11]);
        let rel = fx.root.join("target/release");
        fs::create_dir_all(rel.join(".fingerprint/x-cccccccccccccccc")).unwrap();
        fs::write(rel.join(".cargo-lock"), "").unwrap();
        fs::write(
            rel.join(".fingerprint/x-cccccccccccccccc/lib-x.json"),
            r#"{"rustc":1,"deps":[[1,"a",false,21],[2,"b",false,22]],"local":[]}"#,
        )
        .unwrap();
        fs::write(rel.join(".fingerprint/x-cccccccccccccccc/lib-x"), "ffff").unwrap();
        let mut v = sibling_dep_values(&fx.prof);
        v.sort();
        assert_eq!(v, [21, 22]);
        assert!(sibling_dep_values(&std::env::temp_dir().join("no-such-autoprune")).is_empty());
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
    fn version_from_source_path() {
        let d = std::env::temp_dir().join(format!("autoprune-ver-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let f = d.join("x.d");
        fs::write(&f, "/t/x.d: /h/registry/src/idx/memchr-2.8.2/src/lib.rs\n").unwrap();
        assert_eq!(source_version(&f, "memchr").as_deref(), Some("2.8.2"));
        fs::write(
            &f,
            "/t/x.d: /h/git/checkouts/dep_g-63d23441bb5565a0/abc1234/src/lib.rs\n",
        )
        .unwrap();
        assert_eq!(source_version(&f, "dep_g"), None);
        for bad in [
            "memchr-.8.2",
            "memchr-2.x.3",
            "memchr-x.1.2",
            "memchr-1.2.x",
            "memchr-1.2",
        ] {
            fs::write(
                &f,
                format!("/t/x.d: /h/registry/src/idx/{bad}/src/lib.rs\n"),
            )
            .unwrap();
            assert_eq!(source_version(&f, "memchr"), None, "{bad}");
        }
        fs::write(
            &f,
            "/t/x.d: /h/registry/src/idx/proc-macro2-1.0.95-rc.1/src/lib.rs\n",
        )
        .unwrap();
        assert_eq!(
            source_version(&f, "proc-macro2").as_deref(),
            Some("1.0.95-rc.1")
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn fingerprint_json_parsing() {
        let j = r#"{"rustc":139,"features":"[]","deps":[[13,"build_script_build",false,49],[49,"util",false,15]],"local":[{"CheckDepInfo":{"dep_info":"x","checksum":false}}]}"#;
        assert_eq!(parse_fingerprint_json(j), (139, vec![49, 15]));
        assert_eq!(
            parse_fingerprint_json(r#"{"rustc":1,"deps":[],"local":[]}"#),
            (1, vec![])
        );
        assert_eq!(
            parse_fingerprint_json(r#"{"deps":[[7],[8]]}"#),
            (0, vec![7, 8])
        );
        assert_eq!(parse_le_hex("d5dd8855943b3fe2"), Some(16302814684380716501));
        assert_eq!(parse_le_hex("xyz"), None);
    }
}
