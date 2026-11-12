//! Persistent state in `<profile>/.autoprune/`: recent deletions, pins, tags, log.

use crate::fsutil::{atomic_write, human};
use crate::prune::Report;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PIN_TTL: Duration = Duration::from_secs(24 * 3600);
/// How long a deletion is remembered to notice the unit being rebuilt ("comeback").
const PIN_WINDOW: Duration = Duration::from_secs(24 * 3600);
const MAX_LOG_BYTES: u64 = 256 * 1024;

/// `id\tunix_ts` of recent deletions; entries older than PIN_WINDOW are dropped.
/// A recent deletion: when, and the workspace `Cargo.lock` hash at that moment.
pub(crate) type Deleted = HashMap<String, (u64, String)>;

/// `id\tunix_ts\tlock_hash` of recent deletions; entries older than PIN_WINDOW are dropped.
pub(crate) fn read_deleted(p: &Path, now: SystemTime) -> Deleted {
    let horizon = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(PIN_WINDOW)
        .as_secs();
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (id, ts, lock) = (
                f.next()?,
                f.next()?.parse::<u64>().ok()?,
                f.next().unwrap_or(""),
            );
            (ts >= horizon).then(|| (id.to_owned(), (ts, lock.to_owned())))
        })
        .collect()
}

pub(crate) fn write_deleted(p: &Path, d: &Deleted) -> io::Result<()> {
    let s: String = d
        .iter()
        .map(|(id, (ts, lock))| format!("{id}\t{ts}\t{lock}\n"))
        .collect();
    atomic_write(p, &s)
}

/// Drop tag files of units that no longer exist.
pub(crate) fn clean_tags(tags: &Path, profile: &Path) {
    let live = crate::layout::fingerprinted_hashes(profile);
    for e in crate::fsutil::entries(tags) {
        if !live.contains(&e.name) {
            let _ = fs::remove_file(e.path);
        }
    }
}

pub(crate) fn read_pins(p: &Path, now: SystemTime) -> HashMap<String, SystemTime> {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, ts) = l.rsplit_once('\t')?;
            // checked: a corrupt timestamp must not panic every background prune
            let t = UNIX_EPOCH.checked_add(Duration::from_secs(ts.parse().ok()?))?;
            (t.checked_add(PIN_TTL)? > now).then(|| (k.to_owned(), t))
        })
        .collect()
}

pub(crate) fn write_pins(p: &Path, pins: &HashMap<String, SystemTime>) -> io::Result<()> {
    let mut s = String::new();
    for (k, t) in pins {
        s += &format!(
            "{k}\t{}\n",
            t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
        );
    }
    atomic_write(p, &s)
}

pub(crate) fn log(p: &Path, r: &Report) {
    if fs::metadata(p)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = fs::rename(p, p.with_extension("old"));
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(
            f,
            "{ts} units={} incremental={} freed={} pinned={} took={}ms locked={}ms errors={} [{}]",
            r.removed_units,
            r.removed_incremental,
            human(r.freed_bytes),
            r.pinned_now.len(),
            r.elapsed.as_millis(),
            r.locked.as_millis(),
            r.errors.len(),
            r.breakdown()
        );
    }
}

/// One-line messages for the user, written by the background prune and printed once by the
/// wrapper on the terminal (`/dev/tty`): cargo would cache and replay anything on stderr.
pub(crate) const NOTICE_FILE: &str = "notice";
const MAX_NOTICE_BYTES: u64 = 4096;

pub(crate) fn add_notice(state: &Path, msg: &str) {
    let p = state.join(NOTICE_FILE);
    if fs::metadata(&p).is_ok_and(|m| m.len() > MAX_NOTICE_BYTES) {
        return; // nobody reads them (no terminal): don't grow
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "{msg}");
    }
}

pub(crate) fn read_notices(state: &Path) -> Vec<String> {
    fs::read_to_string(state.join(NOTICE_FILE))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Notices worth telling the user about a background prune.
pub(crate) fn notices_for(profile: &Path, r: &Report) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(e) = r.errors.first() {
        out.push(format!(
            "could not delete {} path(s) in {}, e.g. {e}",
            r.errors.len(),
            profile.display()
        ));
    }
    if r.freed_bytes >= 1 << 30 {
        out.push(format!(
            "freed {} in {} ({})",
            human(r.freed_bytes),
            profile.display(),
            r.breakdown()
        ));
    }
    if r.locked.as_secs() >= 5 {
        out.push(format!(
            "pruning {} held cargo's build lock for {} s (a first cleanup of a large target)",
            profile.display(),
            r.locked.as_secs()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn notices_for_big_frees_errors_and_long_locks() {
        let p = Path::new("/t/debug");
        let mut r = Report::default();
        assert!(notices_for(p, &r).is_empty(), "a normal prune is silent");
        r.freed_bytes = 3 << 30;
        r.by_reason.insert("superseded", 7);
        r.errors.push("/t/debug/deps/x: Permission denied".into());
        r.locked = Duration::from_secs(9);
        let n = notices_for(p, &r);
        assert_eq!(n.len(), 3, "{n:?}");
        assert!(n[0].contains("could not delete 1 path(s)"));
        assert!(n[1].contains("freed 3.0GiB") && n[1].contains("superseded 7"));
        assert!(n[2].contains("9 s"));
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("autoprune-state-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn pins_and_deletions_expire_after_a_day() {
        let d = tmp("ttl");
        let now = SystemTime::now();
        let ago = |h: u64| {
            (now - Duration::from_secs(h * 3600))
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        fs::write(
            d.join("pinned"),
            format!("a|1\t{}\nb|2\t{}\nbroken\n", ago(2), ago(25)),
        )
        .unwrap();
        let pins = read_pins(&d.join("pinned"), now);
        assert_eq!(pins.keys().collect::<Vec<_>>(), ["a|1"]);
        write_pins(&d.join("pinned2"), &pins).unwrap();
        assert_eq!(
            fs::read_to_string(d.join("pinned2")).unwrap(),
            format!("a|1\t{}\n", ago(2))
        );

        fs::write(
            d.join("deleted"),
            format!(
                "x\t{}\tL1\ny\t{}\tL2\nz\t{}\nbad\tts\n",
                ago(2),
                ago(25),
                ago(1)
            ),
        )
        .unwrap();
        let del = read_deleted(&d.join("deleted"), now);
        assert_eq!(del.len(), 2, "{del:?}");
        assert_eq!(del["x"], (ago(2), "L1".to_owned()));
        assert_eq!(del["z"], (ago(1), String::new()));
        write_deleted(&d.join("deleted2"), &del).unwrap();
        assert_eq!(read_deleted(&d.join("deleted2"), now), del);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn expiry_at_exactly_one_day() {
        let d = tmp("edge");
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let day_ago = 1_800_000_000 - 24 * 3600;
        fs::write(
            d.join("pinned"),
            format!("gone\t{day_ago}\nlive\t{}\n", day_ago + 1),
        )
        .unwrap();
        assert_eq!(
            read_pins(&d.join("pinned"), now).keys().collect::<Vec<_>>(),
            ["live"]
        );
        fs::write(
            d.join("deleted"),
            format!("kept\t{day_ago}\ngone\t{}\n", day_ago - 1),
        )
        .unwrap();
        assert_eq!(
            read_deleted(&d.join("deleted"), now)
                .keys()
                .collect::<Vec<_>>(),
            ["kept"]
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn log_lines_and_rotation() {
        let d = tmp("log");
        let mut r = Report {
            removed_units: 3,
            removed_incremental: 1,
            freed_bytes: 2048,
            ..Report::default()
        };
        r.by_reason.insert("orphan", 3);
        log(&d.join("log"), &r);
        let line = fs::read_to_string(d.join("log")).unwrap();
        assert!(line.ends_with("units=3 incremental=1 freed=2.0KiB pinned=0 took=0ms locked=0ms errors=0 [orphan 3]\n"), "{line}");
        fs::write(d.join("log"), vec![b'x'; 256 * 1024]).unwrap();
        log(&d.join("log"), &r);
        assert_eq!(
            fs::metadata(d.join("log")).unwrap().len() as usize,
            256 * 1024 + line.len(),
            "not rotated at the limit"
        );
        fs::write(d.join("log"), vec![b'x'; 256 * 1024 + 1]).unwrap();
        log(&d.join("log"), &r);
        assert_eq!(
            fs::metadata(d.join("log.old")).unwrap().len(),
            256 * 1024 + 1
        );
        assert_eq!(fs::read_to_string(d.join("log")).unwrap().len(), line.len());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn notice_thresholds() {
        let p = Path::new("/t/debug");
        let r = Report {
            freed_bytes: (1 << 30) - 1,
            locked: Duration::from_millis(4999),
            ..Report::default()
        };
        assert!(notices_for(p, &r).is_empty());
        let r = Report {
            freed_bytes: 1 << 30,
            locked: Duration::from_secs(5),
            ..Report::default()
        };
        assert_eq!(notices_for(p, &r).len(), 2);
    }

    #[test]
    fn stale_tags_are_dropped() {
        let fx = crate::testutil::Fx::new("state-tags");
        fx.lib("app", crate::testutil::A, 0, false);
        let tags = fx.prof.join("tags");
        fs::create_dir_all(&tags).unwrap();
        fs::write(tags.join(crate::testutil::A), "clippy-driver").unwrap();
        fs::write(tags.join(crate::testutil::B), "clippy-driver").unwrap();
        clean_tags(&tags, &fx.prof);
        assert!(tags.join(crate::testutil::A).exists() && !tags.join(crate::testutil::B).exists());
    }

    #[test]
    fn corrupt_pin_timestamps_are_ignored() {
        let d = std::env::temp_dir().join(format!("autoprune-pins-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let f = d.join("pinned");
        fs::write(
            &f,
            format!("a|b\t{}\nc|d\tnot-a-number\ne|f\t42\n", u64::MAX),
        )
        .unwrap();
        assert!(read_pins(&f, SystemTime::now()).is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn notice_file_is_capped() {
        let d = std::env::temp_dir().join(format!("autoprune-notice-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        for i in 0..1000 {
            add_notice(&d, &format!("message number {i}"));
        }
        let len = fs::metadata(d.join(NOTICE_FILE)).unwrap().len();
        assert!(
            len > MAX_NOTICE_BYTES && len < MAX_NOTICE_BYTES + 100,
            "{len}"
        );
        assert_eq!(read_notices(&d)[0], "message number 0");
        let mut full = vec![b'x'; MAX_NOTICE_BYTES as usize];
        full[MAX_NOTICE_BYTES as usize - 1] = b'\n';
        fs::write(d.join(NOTICE_FILE), full).unwrap();
        add_notice(&d, "at the limit");
        assert_eq!(read_notices(&d).last().unwrap(), "at the limit");
        let _ = fs::remove_dir_all(&d);
    }
}
