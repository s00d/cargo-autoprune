//! Persistent state in `<profile>/.autoprune/`: recent deletions, pins, tags, log.

use crate::fsutil::{atomic_write, human, read_dir};
use crate::layout::split_hash;
use crate::prune::Report;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PIN_TTL: Duration = Duration::from_secs(24 * 3600);
/// A deleted unit rebuilt within this window was a live variant, not a revert done later.
const PIN_WINDOW: Duration = Duration::from_secs(30 * 60);
const MAX_LOG_BYTES: u64 = 256 * 1024;

/// `id\tunix_ts` of recent deletions; entries older than PIN_WINDOW are dropped.
pub(crate) fn read_deleted(p: &Path, now: SystemTime) -> HashMap<String, u64> {
    let horizon = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(PIN_WINDOW)
        .as_secs();
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (id, ts) = l.rsplit_once('\t')?;
            let ts: u64 = ts.parse().ok()?;
            (ts >= horizon).then(|| (id.to_owned(), ts))
        })
        .collect()
}

pub(crate) fn write_deleted(p: &Path, d: &HashMap<String, u64>) -> io::Result<()> {
    let s: String = d.iter().map(|(id, ts)| format!("{id}\t{ts}\n")).collect();
    atomic_write(p, &s)
}

/// Drop tag files of units that no longer exist.
pub(crate) fn clean_tags(tags: &Path, profile: &Path) {
    let live: HashSet<String> = read_dir(&profile.join(".fingerprint"))
        .into_iter()
        .filter_map(|(n, _)| split_hash(&n).map(|(_, h, _)| h.to_owned()))
        .collect();
    for (h, p) in read_dir(tags) {
        if !live.contains(&h) {
            let _ = fs::remove_file(p);
        }
    }
}

pub(crate) fn read_pins(p: &Path, now: SystemTime) -> HashMap<String, SystemTime> {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, ts) = l.rsplit_once('\t')?;
            let t = UNIX_EPOCH + Duration::from_secs(ts.parse().ok()?);
            (t + PIN_TTL > now).then(|| (k.to_owned(), t))
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
            "{ts} units={} incremental={} freed={} pinned={} took={}ms",
            r.removed_units,
            r.removed_incremental,
            human(r.freed_bytes),
            r.pinned_now.len(),
            r.elapsed.as_millis()
        );
    }
}
