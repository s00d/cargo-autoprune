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

/// An output of code generation (rlib, dylib, staticlib, executable) by its extension, e.g.
/// `rlib`; a unit with only `rmeta` is a `check` build. Cargo's only extensionless outputs are
/// executables, so no permission check (and no `stat`) is needed.
pub(crate) fn is_codegen(ext: &str) -> bool {
    matches!(
        ext,
        "rlib" | "dylib" | "so" | "dll" | "a" | "lib" | "exe" | "wasm" | ""
    )
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
    fingerprint_dirs(profile)
        .iter()
        .filter_map(|d| {
            let name = d.file_name()?.to_str()?;
            match split_hash(name) {
                Some((_, h, _)) => Some(h.to_owned()),
                None => Some(d.parent()?.file_name()?.to_str()?.to_owned()), // <hash>/fingerprint
            }
        })
        .collect()
}

/// Fingerprint dirs of all units, in both layouts.
pub(crate) fn fingerprint_dirs(profile: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = crate::fsutil::entries(&profile.join(".fingerprint"))
        .into_iter()
        .map(|e| e.path)
        .collect();
    for pkg in crate::fsutil::entries(&profile.join("build")) {
        if split_hash(&pkg.name).is_none() {
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
