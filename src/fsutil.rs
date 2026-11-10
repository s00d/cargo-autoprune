//! Small filesystem and formatting helpers.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) fn mtime(p: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(p).and_then(|m| m.modified()).ok()
}

pub(crate) fn read_dir(p: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = fs::read_dir(p) else {
        return Vec::new();
    };
    rd.filter_map(|e| {
        let e = e.ok()?;
        Some((e.file_name().into_string().ok()?, e.path()))
    })
    .collect()
}

pub fn size_of(p: &Path) -> u64 {
    let Ok(m) = fs::symlink_metadata(p) else {
        return 0;
    };
    if !m.is_dir() {
        return m.len();
    }
    read_dir(p).into_iter().map(|(_, q)| size_of(&q)).sum()
}

pub(crate) fn atomic_write(p: &Path, s: &str) -> io::Result<()> {
    let tmp = p.with_extension("tmp");
    fs::write(&tmp, s)?;
    fs::rename(tmp, p)
}

pub fn human(b: u64) -> String {
    let (v, u) = match b {
        b if b >= 1 << 30 => (b as f64 / (1u64 << 30) as f64, "GiB"),
        b if b >= 1 << 20 => (b as f64 / (1u64 << 20) as f64, "MiB"),
        b if b >= 1 << 10 => (b as f64 / 1024.0, "KiB"),
        b => return format!("{b}B"),
    };
    format!("{v:.1}{u}")
}

/// `20G`, `512M`, `100k`, plain bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s
        .trim()
        .trim_end_matches(['b', 'B'])
        .trim_end_matches(['i', 'I']);
    let (num, mul) = match s.chars().last()? {
        'k' | 'K' => (&s[..s.len() - 1], 1u64 << 10),
        'm' | 'M' => (&s[..s.len() - 1], 1 << 20),
        'g' | 'G' => (&s[..s.len() - 1], 1 << 30),
        't' | 'T' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|n| (n * mul as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sizes() {
        assert_eq!(parse_size("20G"), Some(20 << 30));
        assert_eq!(parse_size("512MiB"), Some(512 << 20));
        assert_eq!(parse_size("1.5g"), Some(3 << 29));
        assert_eq!(parse_size("1000"), Some(1000));
        assert_eq!(parse_size("x"), None);
    }
}
