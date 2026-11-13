//! Small filesystem and formatting helpers.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A directory entry with what `readdir` already knows: no `stat` needed.
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
    /// Inode (unix, from `d_ino`); incremental work products are hard links to `deps/` files.
    pub(crate) ino: Option<u64>, // None off unix: no inode, owners fall back to mtime pairing
}

pub(crate) fn entries(p: &Path) -> Vec<Entry> {
    let Ok(rd) = fs::read_dir(p) else {
        return Vec::new();
    };
    rd.filter_map(|e| {
        let e = e.ok()?;
        Some(Entry {
            is_dir: e.file_type().ok()?.is_dir(),
            ino: ino(&e),
            name: e.file_name().into_string().ok()?,
            path: e.path(),
        })
    })
    .collect()
}

fn ino(e: &fs::DirEntry) -> Option<u64> {
    #[cfg(unix)]
    return Some(std::os::unix::fs::DirEntryExt::ino(e));
    #[cfg(not(unix))]
    return {
        let _ = e;
        None
    };
}

/// Map `f` over `items` on a few threads (the work is syscall-bound: open/read/readdir).
pub(crate) fn par_map<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    par_map_with(threads, items, f)
}

fn par_map_with<T: Send, R: Send>(
    threads: usize,
    items: Vec<T>,
    f: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    if items.len() < 64 || threads < 2 {
        return items.into_iter().map(f).collect();
    }
    let chunk = items.len().div_ceil(threads);
    let mut chunks: Vec<Vec<T>> = Vec::new();
    let mut it = items.into_iter().peekable();
    while it.peek().is_some() {
        chunks.push(it.by_ref().take(chunk).collect());
    }
    let f = &f;
    std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|c| s.spawn(move || c.into_iter().map(f).collect::<Vec<R>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    })
}

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
