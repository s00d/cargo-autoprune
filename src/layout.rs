//! Cargo target-dir layout: profile dirs and unit hashes in file names.

use crate::fsutil::read_dir;
use std::fs;
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

/// Kind of an output file in `deps/` from the part after the hash.
pub(crate) fn out_kind(rest: &str, path: &Path) -> Option<&'static str> {
    Some(match rest {
        ".rlib" => "rlib",
        ".rmeta" => "rmeta",
        ".dylib" | ".so" | ".dll" => "dylib",
        ".a" | ".lib" => "staticlib",
        ".exe" | ".wasm" => "bin",
        "" if is_executable(path) => "bin",
        _ => return None, // .d, .o, .dSYM, .pdb, ...
    })
}

#[cfg(unix)]
pub(crate) fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o100 != 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
pub(crate) fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// A dir is a Cargo profile dir if it has `.cargo-lock` and sits under a `CACHEDIR.TAG` dir.
pub fn is_profile_dir(p: &Path) -> bool {
    p.join(".cargo-lock").is_file()
        && p.ancestors()
            .skip(1)
            .take(3)
            .any(|a| a.join("CACHEDIR.TAG").is_file())
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
    }
}
