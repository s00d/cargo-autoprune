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
    let mut pins = read_times(p);
    pins.retain(|_, t| t.checked_add(PIN_TTL).is_some_and(|end| end > now));
    pins
}

/// `key\tunix_ts` lines (pins, suspects, last writes).
pub(crate) fn read_times(p: &Path) -> HashMap<String, SystemTime> {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, ts) = l.rsplit_once('\t')?;
            // checked: a corrupt timestamp must not panic every background prune
            let t = UNIX_EPOCH.checked_add(Duration::from_secs(ts.parse().ok()?))?;
            Some((k.to_owned(), t))
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
