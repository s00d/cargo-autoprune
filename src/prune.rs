//! One prune pass over a profile dir: lock, scan, plan, delete, record.

use crate::fingerprint::{recorded_units, sibling_dep_values, workspace};
use crate::fsutil::{mtime, par_map, size_of};
use crate::layout::{is_profile_dir, STATE_DIR, TAGS_DIR, UNITS_FILE};
use crate::lock::{acquire_cargo_locks, lock_file};
use crate::plan::{apply_cap, plan};
use crate::scan::scan;
use crate::state::{
    clean_tags, log, read_deleted, read_pins, read_times, write_deleted, write_pins,
};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct Config {
    /// Keep group members whose mtime is within `grace` of the newest member.
    pub grace: Duration,
    /// Optional cap for this profile dir; oldest units go first.
    pub max_size: Option<u64>,
    pub dry_run: bool,
    /// How long to wait for a running cargo to release the build lock.
    pub lock_wait: Duration,
    /// How long one delete batch may hold Cargo's locks before letting a waiting cargo in.
    pub lock_batch: Duration,
    /// Delete every incremental dir (CI: incremental state is never reused there).
    pub drop_incremental: bool,
}

impl Config {
    /// Defaults overridden by `CARGO_AUTOPRUNE_GRACE` / `CARGO_AUTOPRUNE_MAX_SIZE`.
    pub fn from_env() -> Self {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let d = Config::default();
        Config {
            grace: secs(var("CARGO_AUTOPRUNE_GRACE"), d.grace.as_secs()),
            max_size: var("CARGO_AUTOPRUNE_MAX_SIZE").and_then(|s| crate::cli::parse_cap(&s).ok()),
            ..d
        }
    }
}

/// Phase timer: `mark(name)` records the time since the previous mark.
pub(crate) struct Phases {
    last: Instant,
    pub(crate) list: Vec<(&'static str, Duration)>,
}

impl Phases {
    pub(crate) fn new() -> Self {
        Phases {
            last: Instant::now(),
            list: Vec::new(),
        }
    }
    pub(crate) fn mark(&mut self, name: &'static str) {
        let now = Instant::now();
        self.list.push((name, now - self.last));
        self.last = now;
    }
}

/// Hash of the workspaces' `Cargo.lock` files: tells a dependency change from variants that
/// alternate under the same lock file.
fn lockfile_hash(ws: &crate::fingerprint::Workspace) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut roots = ws.roots.clone();
    roots.sort();
    for r in roots {
        fs::read(r.join("Cargo.lock"))
            .unwrap_or_default()
            .hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

fn remove_path(p: &Path) -> Option<String> {
    let r = match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(_) => return None,
    };
    r.err().map(|e| format!("{}: {e}", p.display()))
}

pub(crate) fn env_secs(name: &str, default: u64) -> Duration {
    secs(std::env::var(name).ok(), default)
}

fn secs(v: Option<String>, default: u64) -> Duration {
    Duration::from_secs(v.and_then(|v| v.trim().parse().ok()).unwrap_or(default))
}

impl Default for Config {
    fn default() -> Self {
        Config {
            grace: Duration::from_secs(600),
            max_size: None,
            dry_run: false,
            lock_wait: Duration::from_secs(3600),
            lock_batch: Duration::from_millis(1000),
            drop_incremental: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Removed units and incremental dirs per reason (orphan, superseded, unreachable, ...).
    pub by_reason: std::collections::BTreeMap<&'static str, usize>,
    /// Paths that could not be deleted (first few errors).
    pub errors: Vec<String>,
    /// Wall time of each phase, for `--verbose` / `CARGO_AUTOPRUNE_TRACE`.
    pub phases: Vec<(&'static str, Duration)>,
    pub removed_units: usize,
    pub removed_incremental: usize,
    pub freed_bytes: u64,
    pub pinned_now: Vec<String>,
    pub removed: Vec<PathBuf>,
    pub elapsed: Duration,
    /// Longest single hold of Cargo's locks: the most a cargo started meanwhile waits.
    pub locked: Duration,
}

impl Report {
    /// `superseded 8, unreachable 3, orphan 1`
    pub fn breakdown(&self) -> String {
        let parts: Vec<String> = self
            .by_reason
            .iter()
            .map(|(r, n)| format!("{r} {n}"))
            .collect();
        parts.join(", ")
    }
}

#[derive(Debug)]
pub enum Outcome {
    Done(Report),
    /// Another autoprune is already working on this dir, or cargo kept the lock past `lock_wait`.
    Busy,
}

pub fn prune(profile: &Path, cfg: &Config) -> io::Result<Outcome> {
    prune_at(profile, cfg, SystemTime::now())
}

/// Unlinks are collected and done on a few threads; this many make a batch.
fn unlink_batch_full(files: &[PathBuf]) -> bool {
    files.len() >= 1000
}

/// `prune` with the clock injected: `now` dates the recorded deletions and expires pins.
/// For tests that simulate weeks of builds.
#[doc(hidden)]
pub fn prune_at(profile: &Path, cfg: &Config, now: SystemTime) -> io::Result<Outcome> {
    if !is_profile_dir(profile) {
        return Err(io::Error::other(format!(
            "{} is not a Cargo profile dir",
            profile.display()
        )));
    }
    let state = profile.join(STATE_DIR);
    fs::create_dir_all(&state)?;
    let own = lock_file(&state.join("lock"))?;
    if own.try_lock().is_err() {
        return Ok(Outcome::Busy);
    }
    let trash = state.join("trash");
    let _ = fs::remove_dir_all(&trash); // left by an interrupted prune
    let Some(cargo_locks) = acquire_cargo_locks(profile, cfg.lock_wait)? else {
        return Ok(Outcome::Busy);
    };
    let start = Instant::now(); // prune time only, not the wait for cargo
    let mut ph = Phases::new();

    let mut deleted_before = read_deleted(&state.join("deleted"), now);
    let mut pinned = read_pins(&state.join("pinned"), now);
    let mut suspects = read_pins(&state.join("suspects"), now);
    let (ws, homes) = workspace(profile);
    ph.mark("state+workspace");
    let (mut units, incremental) = scan(profile, ws.is_some(), &mut ph);
    for (h, u) in units.iter_mut() {
        if let Some(home) = homes.get(h) {
            u.home = home.display().to_string();
        }
    }

    let mut report = Report::default();
    // A deleted unit was rebuilt. If Cargo.lock changed since it was deleted, that is a dependency
    // revert or toggle and does not count. Otherwise (same lock file: `-p x` vs `--workspace`
    // feature unification) it counts; a second such comeback in a group pins it.
    let lock_hash = ws.as_ref().map(lockfile_hash).unwrap_or_default();
    for (h, u) in &units {
        let local = ws
            .as_ref()
            .is_none_or(|w| w.is_local(u.name.as_deref().unwrap_or("")));
        if u.has_fingerprint
            && local
            && deleted_before.remove(h.as_str()).is_some_and(|(_, lock)| lock == lock_hash) // once
            && !pinned.contains_key(&u.group())
        {
            if suspects.remove(&u.group()).is_some() {
                pinned.insert(u.group(), now);
                report.pinned_now.push(u.group());
            } else {
                suspects.insert(u.group(), now);
            }
        }
    }
    // Last write of every toolchain and of the legacy layout, also of units deleted since: the
    // retirement rules must not see a toolchain as unused because its newest units were deleted
    // (an outdated dependency, a removed target).
    let mut seen = read_times(&state.join("seen"));
    let mut present = HashSet::new();
    for u in units.values().filter(|u| u.has_fingerprint) {
        let t = u.mtime.unwrap_or(UNIX_EPOCH);
        let legacy = (!u.new_layout).then(|| "legacy".to_owned());
        for k in [format!("rustc {}", u.rustc)].into_iter().chain(legacy) {
            let e = seen.entry(k.clone()).or_insert(t);
            *e = (*e).max(t);
            present.insert(k);
        }
    }
    seen.retain(|k, _| present.contains(k)); // nothing left to retire
    let extra_roots = sibling_dep_values(profile);
    ph.mark("sibling roots");
    let (doomed, kept) = plan(
        units,
        incremental,
        &pinned,
        &seen,
        ws.as_ref(),
        extra_roots,
        cfg,
    );
    ph.mark("plan");
    let doomed = apply_cap(doomed, kept, cfg.max_size);
    ph.mark("size cap");

    // Under Cargo's locks: unlink doomed files, move doomed directories into the trash (one rename
    // each; their contents are deleted after unlocking). A big cleanup goes in batches of about
    // `Config::lock_batch` (1 s): between batches the locks are released so a waiting cargo gets in, and every
    // unit is re-validated before it is touched again.
    let mut moved = Vec::new();
    let mut freed = 0u64;
    let mut new_deleted = Vec::new();
    if !cfg.dry_run && !doomed.is_empty() {
        fs::create_dir_all(&trash)?;
    }
    let mut locks = Some(cargo_locks);
    let mut hold = Instant::now();
    let mut longest = Duration::ZERO;
    let mut fp_hashes: Option<HashSet<String>> = None; // set after a relock
    let mut files: Vec<PathBuf> = Vec::new();
    let flush = |files: &mut Vec<PathBuf>, errors: &mut Vec<String>| {
        errors.extend(
            par_map(std::mem::take(files), |p| remove_path(&p))
                .into_iter()
                .flatten(),
        );
    };
    for c in &doomed {
        if !cfg.dry_run && hold.elapsed() >= cfg.lock_batch {
            flush(&mut files, &mut report.errors);
            longest = longest.max(hold.elapsed());
            drop(locks.take());
            std::thread::sleep(Duration::from_millis(20)); // a waiting cargo takes the lock now
            match acquire_cargo_locks(profile, cfg.lock_wait)? {
                Some(l) => locks = Some(l),
                None => break, // the rest waits for the next prune
            }
            hold = Instant::now();
            fp_hashes = Some(crate::layout::fingerprinted_hashes(profile));
        }
        if let Some(fps) = &fp_hashes {
            let unchanged = match &c.guard {
                Some(g) => mtime(g) == c.mtime,
                None => c.incremental || !fps.contains(&c.id),
            };
            if !unchanged {
                continue; // cargo rebuilt or touched it while the lock was free
            }
        }
        for p in &c.paths {
            if cfg.dry_run {
                freed += size_of(p);
            } else {
                match fs::symlink_metadata(p) {
                    Ok(m) if m.is_dir() => {
                        let dst = trash.join(moved.len().to_string());
                        match fs::rename(p, &dst) {
                            Ok(()) => moved.push(dst),
                            Err(_) => report.errors.extend(remove_path(p)),
                        }
                    }
                    Ok(m) => {
                        freed += m.len();
                        files.push(p.clone()); // unlink touches one directory, rename two
                    }
                    Err(_) => {}
                }
            }
            report.removed.push(p.clone());
        }
        if unlink_batch_full(&files) {
            flush(&mut files, &mut report.errors); // so the batch clock sees the unlinks
        }
        *report.by_reason.entry(c.reason).or_default() += 1;
        if c.incremental {
            report.removed_incremental += 1;
        } else {
            report.removed_units += 1;
        }
        new_deleted.push(c.id.clone());
    }
    flush(&mut files, &mut report.errors);
    ph.mark("delete (locked)");
    if !cfg.dry_run {
        let mut deleted = deleted_before;
        let ts = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        deleted.extend(
            new_deleted
                .iter()
                .map(|id| (id.clone(), (ts, lock_hash.clone()))),
        );
        write_deleted(&state.join("deleted"), &deleted)?;
        clean_tags(&state.join(TAGS_DIR), profile);
        write_pins(&state.join("pinned"), &pinned)?;
        write_pins(&state.join("suspects"), &suspects)?;
        write_pins(&state.join("seen"), &seen)?;
        let gone: HashSet<&str> = new_deleted.iter().map(String::as_str).collect();
        let records: String = recorded_units(profile)
            .into_iter()
            .filter(|(h, _)| !gone.contains(h.as_str()) && homes.contains_key(h))
            .map(|(h, d)| format!("{h}\t{}\n", d.display()))
            .collect();
        if locks.is_some() {
            // only while no rustc can append to it
            let _ = crate::fsutil::atomic_write(&state.join(UNITS_FILE), &records);
        }
        ph.mark("write state");
    }
    longest = longest.max(hold.elapsed());
    drop(locks);
    report.locked = longest;
    report.freed_bytes = freed + moved.iter().map(|p| size_of(p)).sum::<u64>();
    if !cfg.dry_run {
        report.errors.extend(remove_path(&trash));
    }
    ph.mark("delete dirs (unlocked)");
    report.elapsed = start.elapsed();
    if !cfg.dry_run {
        log(&state.join("log"), &report);
    }
    report.phases = ph.list;
    Ok(Outcome::Done(report))
}
