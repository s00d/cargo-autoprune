//! Cargo target-dir layout: profile dirs and unit hashes in file names.

use crate::fsutil::read_dir;
use std::path::{Path, PathBuf};

/// Per-profile state of cargo-autoprune: `<profile>/.autoprune/`.
pub const STATE_DIR: &str = ".autoprune";
/// Units built through a workspace wrapper (clippy-driver) are tagged here by hash.
pub const TAGS_DIR: &str = "tags";

/// Split `name` at the first `-<16 lowercase hex>` that is followed by `.` or the end.
pub fn split_hash(name: &str) -> Option<(&str, &str, &str)> {
    let b = name.as_bytes();
    let mut i = 0;
    while let Some(off) = name[i..].find('-') {
        let p = i + off;
        let h = p + 1..p + 17;
        if h.end <= b.len()
            && b[h.clone()]
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
            && (h.end == b.len() || b[h.end] == b'.')
        {
            return Some((&name[..p], &name[h.clone()], &name[h.end..]));
        }
        i = p + 1;
    }
    None
}

/// Kind of an output file in `deps/` from the part after the hash. Cargo's only extensionless
/// outputs are executables, so no permission check (and no `stat`) is needed.
pub(crate) fn out_kind(rest: &str, is_dir: bool) -> Option<&'static str> {
    Some(match rest {
        _ if is_dir => return None, // .dSYM
        ".rlib" => "rlib",
        ".rmeta" => "rmeta",
        ".dylib" | ".so" | ".dll" => "dylib",
        ".a" | ".lib" => "staticlib",
        ".exe" | ".wasm" | "" => "bin",
        _ => return None, // .d, .o, .pdb, ...
    })
}

/// Per-profile lock files. A separate `build.build-dir` has only `.cargo-build-lock`.
pub(crate) fn has_cargo_lock(p: &Path) -> bool {
    p.join(".cargo-lock").is_file() || p.join(".cargo-build-lock").is_file()
}

/// `<unit hash>\t<package dir>` lines appended by the wrapper for non-registry packages.
pub const UNITS_FILE: &str = "units";

/// A dir is a Cargo profile dir if it has a Cargo lock file and sits under a `CACHEDIR.TAG` dir.
pub fn is_profile_dir(p: &Path) -> bool {
    has_cargo_lock(p)
        && p.ancestors()
            .skip(1)
            .take(3)
            .any(|a| a.join("CACHEDIR.TAG").is_file())
}

/// Hashes of all units that have a fingerprint, in both layouts: `.fingerprint/<pkg>-<hash>` and
/// `build/<pkg>/<hash>/fingerprint` (cargo 1.100+).
pub(crate) fn fingerprinted_hashes(profile: &Path) -> std::collections::HashSet<String> {
    let mut out: std::collections::HashSet<String> =
        crate::fsutil::entries(&profile.join(".fingerprint"))
            .iter()
            .filter_map(|e| split_hash(&e.name).map(|(_, h, _)| h.to_owned()))
            .collect();
    for pkg in crate::fsutil::entries(&profile.join("build")) {
        if pkg.is_dir && split_hash(&pkg.name).is_none() {
            for u in crate::fsutil::entries(&pkg.path) {
                if u.is_dir && u.path.join("fingerprint").is_dir() {
                    out.insert(u.name);
                }
            }
        }
    }
    out
}

/// Fingerprint dirs of all units, in both layouts.
pub(crate) fn fingerprint_dirs(profile: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = crate::fsutil::entries(&profile.join(".fingerprint"))
        .into_iter()
        .map(|e| e.path)
        .collect();
    for pkg in crate::fsutil::entries(&profile.join("build")) {
        if pkg.is_dir && split_hash(&pkg.name).is_none() {
            out.extend(
                crate::fsutil::entries(&pkg.path)
                    .into_iter()
                    .map(|u| u.path.join("fingerprint"))
                    .filter(|f| f.is_dir()),
            );
        }
    }
    out
}

/// Cargo target dirs under `root` (dirs with `CACHEDIR.TAG` that hold profile dirs). Does not
/// descend into a found target dir, hidden dirs or `node_modules`, and never follows symlinks.
pub fn find_target_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        if d.join("CACHEDIR.TAG").is_file() && !profile_dirs(&d).is_empty() {
            out.push(d);
            continue;
        }
        for e in crate::fsutil::entries(&d) {
            if e.is_dir && !e.name.starts_with('.') && e.name != "node_modules" {
                stack.push(e.path);
            }
        }
    }
    out.sort();
    out
}

/// Profile dirs inside a target dir (`debug`, `release`, `<triple>/debug`, custom profiles).
pub fn profile_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (_, p) in read_dir(target) {
        if is_profile_dir(&p) {
            out.push(p);
        } else if p.is_dir() {
            out.extend(
                read_dir(&p)
                    .into_iter()
                    .map(|(_, q)| q)
                    .filter(|q| is_profile_dir(q)),
            );
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "aaaaaaaaaaaaaaaa";

    use crate::testutil::Fx;
    use std::fs;

    #[test]
    fn output_kinds() {
        for (rest, kind) in [
            (".rlib", Some("rlib")),
            (".rmeta", Some("rmeta")),
            (".dylib", Some("dylib")),
            (".so", Some("dylib")),
            (".dll", Some("dylib")),
            (".a", Some("staticlib")),
            (".lib", Some("staticlib")),
            (".exe", Some("bin")),
            (".wasm", Some("bin")),
            ("", Some("bin")),
            (".d", None),
            (".x.rcgu.o", None),
        ] {
            assert_eq!(out_kind(rest, false), kind, "{rest}");
        }
        assert_eq!(out_kind("", true), None, "a .dSYM-like dir");
        assert_eq!(out_kind(".rlib", true), None);
    }

    #[test]
    fn fingerprints_in_both_layouts() {
        let fx = Fx::new("layout-fp");
        let b = "bbbbbbbbbbbbbbbb";
        fx.dir(&format!(".fingerprint/old-{A}"), 0);
        fx.dir(&format!("build/new/{b}/fingerprint"), 0);
        fx.dir("build/new/cccccccccccccccc/out", 0); // no fingerprint dir
        fx.file("build/new/dddddddddddddddd/fingerprint", 0); // a file, not a dir
        fx.dir(&format!("build/legacy-{A}/out/fingerprint"), 0); // legacy build-script dir
        fx.file("build/stray", 0);
        let mut h: Vec<String> = fingerprinted_hashes(&fx.prof).into_iter().collect();
        h.sort();
        assert_eq!(h, [A, b]);
        let mut d = fingerprint_dirs(&fx.prof);
        d.sort();
        assert_eq!(
            d,
            [
                fx.prof.join(format!(".fingerprint/old-{A}")),
                fx.prof.join(format!("build/new/{b}/fingerprint")),
            ]
        );
    }

    #[test]
    fn target_and_profile_dirs() {
        let fx = Fx::new("layout-dirs");
        let t = fx.root.join("target");
        for p in ["release", "x86_64-unknown-linux-gnu/debug"] {
            fs::create_dir_all(t.join(p)).unwrap();
            fs::write(t.join(p).join(".cargo-lock"), "").unwrap();
        }
        fs::create_dir_all(t.join("tmp/sub")).unwrap();
        fs::write(t.join("file"), "").unwrap();
        assert_eq!(
            profile_dirs(&t),
            [
                t.join("debug"),
                t.join("release"),
                t.join("x86_64-unknown-linux-gnu/debug")
            ]
        );
        assert!(profile_dirs(&fx.root.join("none")).is_empty());

        // more target dirs: found, hidden, node_modules, a tag without profiles, nested
        let mk = |rel: &str, profile: bool| {
            let d = fx.root.join(rel);
            fs::create_dir_all(d.join("debug")).unwrap();
            fs::write(d.join("CACHEDIR.TAG"), "").unwrap();
            if profile {
                fs::write(d.join("debug/.cargo-lock"), "").unwrap();
            }
            d
        };
        let other = mk("p/q/target", true);
        mk(".hidden/target", true);
        mk("node_modules/x/target", true);
        mk("empty/target", false);
        mk("target/debug/deps/inner", true); // inside a found target: not descended
        fs::create_dir_all(fx.root.join("noisy_modules")).unwrap();
        let in_noisy = mk("noisy_modules/target", true);
        assert_eq!(find_target_dirs(&fx.root), [in_noisy, other, t.clone()]);
        assert_eq!(find_target_dirs(&t), [t]);
    }

    #[test]
    fn profile_dir_needs_lock_and_tag() {
        let fx = Fx::new("layout-prof");
        assert!(is_profile_dir(&fx.prof));
        let build_dir = fx.root.join("target/x/y/debug");
        fs::create_dir_all(&build_dir).unwrap();
        assert!(!is_profile_dir(&build_dir));
        fs::write(build_dir.join(".cargo-build-lock"), "").unwrap();
        assert!(is_profile_dir(&build_dir), "tag three levels up");
        let deep = fx.root.join("target/a/b/c/debug");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join(".cargo-lock"), "").unwrap();
        assert!(!is_profile_dir(&deep), "tag four levels up");
    }

    #[test]
    fn split_hash_cases() {
        assert_eq!(
            split_hash(&format!("libfoo-{A}.rlib")),
            Some(("libfoo", A, ".rlib"))
        );
        assert_eq!(split_hash(&format!("foo-{A}")), Some(("foo", A, "")));
        assert_eq!(
            split_hash(&format!("foo-{A}.x.rcgu.o")),
            Some(("foo", A, ".x.rcgu.o"))
        );
        assert_eq!(
            split_hash(&format!("proc-macro2-{A}")),
            Some(("proc-macro2", A, ""))
        );
        assert_eq!(split_hash("foo-ABCDEFabcdef0123"), None);
        assert_eq!(split_hash(&format!("foo-{A}x")), None);
        assert_eq!(split_hash("foo"), None);
        assert_eq!(
            split_hash(&format!("a-b-c-{A}.rlib")),
            Some(("a-b-c", A, ".rlib"))
        );
        let twice = format!("x-{A}-{A}");
        assert_eq!(split_hash(&twice), Some((&twice[..18], A, "")));
    }
}
