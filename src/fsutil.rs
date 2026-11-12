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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes() {
        let cases = [
            (0, "0B"),
            (1023, "1023B"),
            (1024, "1.0KiB"),
            (1536, "1.5KiB"),
            ((1 << 20) - 1, "1024.0KiB"),
            (1 << 20, "1.0MiB"),
            (5 << 19, "2.5MiB"),
            ((1 << 30) - 1, "1024.0MiB"),
            (3 << 29, "1.5GiB"),
            (5 << 40, "5120.0GiB"),
        ];
        for (b, t) in cases {
            assert_eq!(human(b), t, "{b}");
        }
    }

    #[test]
    fn par_map_keeps_order_and_threads_only_big_inputs() {
        let me = std::thread::current().id();
        let run = |n: usize| par_map((0..n).collect(), |i| (i * 2, std::thread::current().id()));
        for n in [0, 10, 63] {
            let r = run(n);
            assert_eq!(
                r.iter().map(|x| x.0).collect::<Vec<_>>(),
                (0..n).map(|i| i * 2).collect::<Vec<_>>()
            );
            assert!(r.iter().all(|x| x.1 == me), "{n} items run inline");
        }
        let one = par_map_with(1, (0..1000).collect(), |_| std::thread::current().id());
        assert!(one.iter().all(|t| *t == me), "one thread: inline");
        let two = par_map_with(2, (0..1000).collect(), |_| std::thread::current().id());
        assert!(two.iter().all(|t| *t != me), "two threads: spawned");
        if std::thread::available_parallelism().map_or(4, |n| n.get()) >= 2 {
            for n in [64, 1000] {
                let r = run(n);
                assert_eq!(
                    r.iter().map(|x| x.0).collect::<Vec<_>>(),
                    (0..n).map(|i| i * 2).collect::<Vec<_>>()
                );
                assert!(r.iter().all(|x| x.1 != me), "{n} items go to threads");
            }
        }
    }

    #[test]
    fn dir_helpers() {
        let d = std::env::temp_dir().join(format!("autoprune-fsutil-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub/deeper")).unwrap();
        fs::write(d.join("a"), [0u8; 10]).unwrap();
        fs::write(d.join("sub/b"), [0u8; 20]).unwrap();
        fs::write(d.join("sub/deeper/c"), [0u8; 30]).unwrap();
        assert_eq!(size_of(&d), 60);
        assert_eq!(size_of(&d.join("a")), 10);
        assert_eq!(size_of(&d.join("none")), 0);
        let mut e: Vec<(String, bool, bool)> = entries(&d)
            .into_iter()
            .map(|e| (e.name.clone(), e.is_dir, e.path == d.join(&e.name)))
            .collect();
        e.sort();
        assert_eq!(e, [("a".into(), false, true), ("sub".into(), true, true)]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = entries(&d).into_iter().find(|e| e.name == "a").unwrap();
            assert_eq!(a.ino, Some(fs::metadata(d.join("a")).unwrap().ino()));
        }
        let mut r = read_dir(&d);
        r.sort();
        assert_eq!(
            r,
            [("a".into(), d.join("a")), ("sub".into(), d.join("sub"))]
        );
        assert!(entries(&d.join("none")).is_empty() && read_dir(&d.join("none")).is_empty());
        assert!(mtime(&d.join("a")).is_some() && mtime(&d.join("none")).is_none());
        atomic_write(&d.join("w.txt"), "x").unwrap();
        atomic_write(&d.join("w.txt"), "yz").unwrap();
        assert_eq!(fs::read_to_string(d.join("w.txt")).unwrap(), "yz");
        assert!(!d.join("w.tmp").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn parse_sizes() {
        assert_eq!(parse_size(" 100k "), Some(100 << 10));
        assert_eq!(parse_size("2T"), Some(2 << 40));
        assert_eq!(parse_size("3 MB"), Some(3 << 20));
        assert_eq!(parse_size("20G"), Some(20 << 30));
        assert_eq!(parse_size("512MiB"), Some(512 << 20));
        assert_eq!(parse_size("1.5g"), Some(3 << 29));
        assert_eq!(parse_size("1000"), Some(1000));
        assert_eq!(parse_size("x"), None);
    }
}
