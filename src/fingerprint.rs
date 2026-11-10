//! Reading Cargo's `.fingerprint/*/*.json` and the workspace `Cargo.lock`.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Package names without a `source` in the workspace `Cargo.lock` next to the target dir.
pub(crate) fn local_packages(profile: &Path) -> Option<HashSet<String>> {
    let target = profile
        .ancestors()
        .find(|a| a.join("CACHEDIR.TAG").is_file())?;
    let lock = fs::read_to_string(target.parent()?.join("Cargo.lock")).ok()?;
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
    (!out.is_empty()).then_some(out)
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
