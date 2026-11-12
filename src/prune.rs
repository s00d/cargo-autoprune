//! One prune pass over a profile dir: lock, scan, plan, delete, record.

use crate::fingerprint::{recorded_units, sibling_dep_values, workspace};
use crate::fsutil::{mtime, par_map, size_of};
use crate::layout::{is_profile_dir, STATE_DIR, TAGS_DIR, UNITS_FILE};
use crate::lock::{acquire_cargo_locks, lock_file};
use crate::plan::{apply_cap, plan};
use crate::scan::scan;
use crate::state::{clean_tags, log, read_deleted, read_pins, write_deleted, write_pins};
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

    let now = SystemTime::now();
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
    let extra_roots = sibling_dep_values(profile);
    ph.mark("sibling roots");
    let (doomed, kept) = plan(units, incremental, &pinned, ws.as_ref(), extra_roots, cfg);
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
        if files.len() >= 1000 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{STATE_DIR, TAGS_DIR};
    use crate::testutil::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn removed_registry_dep_dropped_transitive_kept() {
        let fx = Fx::new("graph");
        fx.lockfile(&["app"], &["serde", "serde_derive", "regex"]);
        fx.node("app", A, 10, 1, 100, &[1]);
        fx.node("serde", B, 9000, 1, 1, &[3]);
        fx.node("serde_derive", C, 9000, 1, 3, &[]);
        fx.node("regex", "dddddddddddddddd", 9000, 1, 2, &[]); // no longer a dependency
        let r = fx.run(&cfg());
        assert_eq!(r.removed_units, 1);
        assert!(!fx.exists("deps/libregex-dddddddddddddddd.rlib"));
        assert!(fx.exists(&format!("deps/libserde-{B}.rlib")));
        assert!(fx.exists(&format!("deps/libserde_derive-{C}.rlib")));
    }

    #[test]
    fn superseded_workspace_unit_takes_its_deps_along() {
        let fx = Fx::new("graph2");
        fx.lockfile(&["app"], &["libc"]);
        fx.node("app", A, 7200, 1, 100, &[1]); // old app built against libc 0.2.190
        fx.node("libc", B, 7200, 1, 1, &[]);
        fx.node("app", C, 10, 1, 101, &[2]); // new app against libc 0.2.191
        fx.node("libc", "dddddddddddddddd", 10, 1, 2, &[]);
        assert_eq!(fx.run(&cfg()).removed_units, 2);
        assert!(!fx.exists(&format!("deps/liblibc-{B}.rlib")));
        assert!(fx.exists("deps/liblibc-dddddddddddddddd.rlib"));
    }

    #[test]
    fn different_rustc_hashes_are_separate_groups() {
        let fx = Fx::new("clippy");
        fx.node("app", A, 7200, 1, 100, &[]); // toolchain 1
        fx.node("app", B, 10, 2, 101, &[]); // toolchain 2
        assert_eq!(fx.run(&cfg()).removed_units, 0);
    }

    #[test]
    fn old_toolchain_retired() {
        let fx = Fx::new("toolchain");
        fx.node("app", A, 20 * 24 * 3600, 1, 100, &[]);
        fx.node("util", B, 20 * 24 * 3600, 1, 101, &[]);
        fx.node("app", C, 10, 2, 102, &[]);
        assert_eq!(fx.run(&cfg()).removed_units, 2);
        assert!(fx.exists(&format!("deps/libapp-{C}.rlib")));
    }

    #[test]
    fn single_comeback_is_a_revert_not_pinned() {
        let fx = Fx::new("revert");
        fx.lib("tokio", A, 10, false); // rebuilt after a revert
        fx.lib("tokio", B, 7200, false);
        let old = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3600;
        fs::create_dir_all(fx.prof.join(STATE_DIR)).unwrap();
        fs::write(
            fx.prof.join(STATE_DIR).join("deleted"),
            format!("{A}\t{old}\n"),
        )
        .unwrap();
        let r = fx.run(&cfg());
        assert!(r.pinned_now.is_empty());
        assert_eq!(r.removed_units, 1); // B superseded again
        assert!(fx.exists(&format!("deps/libtokio-{A}.rlib")));
    }

    #[test]
    fn clippy_tag_separates_groups() {
        let fx = Fx::new("tag");
        fx.node("app", A, 7200, 1, 100, &[]); // cargo check
        fx.node("app", B, 10, 1, 101, &[]); // cargo clippy: same json
        let tags = fx.prof.join(STATE_DIR).join(TAGS_DIR);
        fs::create_dir_all(&tags).unwrap();
        fs::write(tags.join(B), "clippy-driver").unwrap();
        fs::write(tags.join(C), "clippy-driver").unwrap(); // stale tag
        assert_eq!(fx.run(&cfg()).removed_units, 0);
        assert!(!tags.join(C).exists() && tags.join(B).exists());
    }

    #[test]
    fn superseded_lib_removed_newest_kept() {
        let fx = Fx::new("superseded");
        fx.lib("serde", A, 7200, false);
        fx.lib("serde", B, 10, false);
        let r = fx.run(&cfg());
        assert_eq!(r.removed_units, 1);
        assert!(!fx.exists(&format!(".fingerprint/serde-{A}")));
        assert!(!fx.exists(&format!("deps/libserde-{A}.rlib")));
        assert!(!fx.exists(&format!("deps/serde-{A}.serde.cgu.0.rcgu.o")));
        assert!(fx.exists(&format!("deps/libserde-{B}.rlib")));
        assert!(fx.exists(&format!(".fingerprint/serde-{B}")));
    }

    #[test]
    fn check_and_build_variants_both_kept() {
        let fx = Fx::new("variants");
        fx.lib("libc", A, 7200, true); // cargo check (rmeta only)
        fx.lib("libc", B, 10, false); // cargo build
        let r = fx.run(&cfg());
        assert_eq!(r.removed_units, 0);
        assert!(fx.exists(&format!("deps/liblibc-{A}.rmeta")));
    }

    #[test]
    fn within_grace_kept() {
        let fx = Fx::new("grace");
        fx.lib("serde", A, 300, false);
        fx.lib("serde", B, 10, false);
        assert_eq!(fx.run(&cfg()).removed_units, 0);
    }

    #[test]
    fn orphans_removed_outside_untouched() {
        let fx = Fx::new("orphans");
        fx.file(&format!("deps/mytest-{A}.x.rcgu.o"), 10); // exe already gone, no fingerprint
        fx.file(&format!("deps/mytest-{A}.d"), 10);
        fx.file("deps/README", 10); // no hash: not ours
        fx.file("mybin", 10); // uplifted output in profile root
        fs::write(fx.root.join("target/keep.txt"), "x").unwrap();
        let r = fx.run(&cfg());
        assert_eq!(r.removed_units, 1);
        assert!(!fx.exists(&format!("deps/mytest-{A}.d")));
        assert!(fx.exists("deps/README") && fx.exists("mybin"));
        assert!(fx.root.join("target/keep.txt").exists());
    }

    #[test]
    fn build_script_dirs_follow_fingerprint() {
        let fx = Fx::new("buildscript");
        for (h, age) in [(A, 7200), (B, 10)] {
            fx.file(
                &format!(".fingerprint/libc-{h}/run-build-script-build-script-build"),
                age,
            );
            set_age(&fx.prof.join(format!(".fingerprint/libc-{h}")), age);
            fx.file(&format!("build/libc-{h}/output"), age);
            set_age(&fx.prof.join(format!("build/libc-{h}")), age);
        }
        fx.dir(&format!("build/libc-{C}"), 10); // no fingerprint
        assert_eq!(fx.run(&cfg()).removed_units, 2);
        assert!(!fx.exists(&format!("build/libc-{A}")) && !fx.exists(&format!("build/libc-{C}")));
        assert!(fx.exists(&format!("build/libc-{B}/output")));
    }

    #[test]
    fn test_exe_with_dsym_superseded() {
        let fx = Fx::new("exe");
        for (h, age) in [(A, 7200), (B, 10)] {
            fx.file(
                &format!(".fingerprint/gs-at-{h}/test-integration-test-urc"),
                age,
            );
            set_age(&fx.prof.join(format!(".fingerprint/gs-at-{h}")), age);
            let exe = fx.file(&format!("deps/urc-{h}"), age);
            let _ = &exe; // used by the unix-only permission bit below
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
            }
            fx.file(&format!("deps/urc-{h}.dSYM/Contents/Info.plist"), age);
            set_age(&fx.prof.join(format!("deps/urc-{h}.dSYM")), age);
        }
        assert_eq!(fx.run(&cfg()).removed_units, 1);
        assert!(!fx.exists(&format!("deps/urc-{A}.dSYM")) && !fx.exists(&format!("deps/urc-{A}")));
        assert!(fx.exists(&format!("deps/urc-{B}")));
    }

    #[test]
    fn returning_variant_gets_pinned() {
        let fx = Fx::new("pin");
        fx.lib("tokio", A, 7200, false);
        fx.lib("tokio", B, 3600, false);
        assert_eq!(fx.run(&cfg()).removed_units, 1);
        fx.lib("tokio", A, 100, false); // rebuilt once: could be a revert, B goes
        let r = fx.run(&cfg());
        assert!(r.pinned_now.is_empty());
        assert_eq!(r.removed_units, 1);
        fx.lib("tokio", B, 10, false); // rebuilt again: two live variants alternate
        let r = fx.run(&cfg());
        assert_eq!(r.removed_units, 0);
        assert_eq!(r.pinned_now.len(), 1);
        assert!(fx.exists(&format!("deps/libtokio-{A}.rlib")));
        assert!(fx.exists(&format!("deps/libtokio-{B}.rlib")));
        assert_eq!(fx.run(&cfg()).removed_units, 0); // pin persists
    }

    #[test]
    fn dependency_toggles_never_pin() {
        // the same unit comes back twice, but Cargo.lock changed each time: reverts, not variants
        let fx = Fx::new("toggle");
        fx.lockfile(&["tokio"], &["a"]);
        fx.lib("tokio", A, 7200, false);
        fx.lib("tokio", B, 3600, false);
        assert_eq!(fx.run(&cfg()).removed_units, 1); // A goes
        fx.lockfile(&["tokio"], &["b"]);
        fx.lib("tokio", A, 2000, false); // back after a lock change
        assert!(fx.run(&cfg()).pinned_now.is_empty()); // B goes
        fx.lockfile(&["tokio"], &["a"]);
        fx.lib("tokio", B, 10, false); // and again
        let r = fx.run(&cfg());
        assert!(r.pinned_now.is_empty(), "{:?}", r.pinned_now);
        assert_eq!(r.removed_units, 1);
    }

    #[test]
    #[cfg(unix)]
    fn incremental_owner_by_hard_link() {
        let fx = Fx::new("inclink");
        fx.lib("app", A, 10, false);
        fx.lib("app", B, 7200, false); // superseded
                                       // both dirs written at the same moment: only the hard links tell the owners apart
        for (d, h) in [("app-1aaa", B), ("app-2bbb", A)] {
            fx.dir(&format!("incremental/{d}/s-x"), 5);
            fs::hard_link(
                fx.prof.join(format!("deps/libapp-{h}.rmeta")),
                fx.prof.join(format!("incremental/{d}/s-x/metadata.rmeta")),
            )
            .unwrap();
            set_age(&fx.prof.join(format!("incremental/{d}")), 5);
        }
        let r = fx.run(&cfg());
        assert_eq!(r.removed_incremental, 1);
        assert!(!fx.exists("incremental/app-1aaa") && fx.exists("incremental/app-2bbb"));
    }

    #[test]
    fn incremental_follows_its_unit() {
        let fx = Fx::new("inc");
        fx.lib("gs_modem", A, 7200, false);
        fx.lib("gs_modem", B, 10, false);
        fx.lib("gs_at", C, 9000, false); // old but the only copy: live
        for (d, age) in [
            ("gs_modem-1abc", 7200),
            ("gs_modem-2def", 10),
            ("gs_at-3xyz", 9000),
            ("removed-4qqq", 10),
        ] {
            fx.dir(&format!("incremental/{d}/s-x"), age);
            set_age(&fx.prof.join(format!("incremental/{d}")), age);
        }
        let r = fx.run(&cfg());
        assert_eq!(r.removed_incremental, 2);
        assert!(!fx.exists("incremental/gs_modem-1abc") && !fx.exists("incremental/removed-4qqq"));
        assert!(fx.exists("incremental/gs_modem-2def") && fx.exists("incremental/gs_at-3xyz"));
    }

    #[test]
    fn size_cap_removes_oldest() {
        let fx = Fx::new("cap");
        fx.lib("a", A, 9000, false);
        fx.lib("b", B, 5000, false);
        fx.lib("c", C, 10, false);
        // each lib: 4 files of 1000 B in fingerprint + rlib + o + rmeta + d = ~7000 B
        let r = fx.run(&Config {
            max_size: Some(15_000),
            ..cfg()
        });
        assert_eq!(r.removed_units, 1);
        assert!(!fx.exists(&format!("deps/liba-{A}.rlib")));
        assert!(fx.exists(&format!("deps/libb-{B}.rlib")));
    }

    #[test]
    fn batched_delete_relocks_and_finishes() {
        // a zero batch releases and retakes the locks before every unit
        let fx = Fx::new("batch");
        for (i, h) in [A, B].iter().enumerate() {
            fx.lib(&format!("old{i}"), h, 7200, false);
        }
        fx.lib("old0", C, 10, false);
        fx.lib("old1", "dddddddddddddddd", 10, false);
        fx.file("deps/orphan-eeeeeeeeeeeeeeee.d", 10);
        let r = fx.run(&Config {
            lock_batch: Duration::ZERO,
            ..cfg()
        });
        assert_eq!(r.removed_units, 3);
        assert!(
            !fx.exists(&format!("deps/libold0-{A}.rlib"))
                && !fx.exists(&format!("deps/libold1-{B}.rlib"))
        );
        assert!(fx.exists(&format!("deps/libold0-{C}.rlib")));
    }

    #[test]
    fn dry_run_deletes_nothing() {
        let fx = Fx::new("dry");
        fx.lib("serde", A, 7200, false);
        fx.lib("serde", B, 10, false);
        let r = fx.run(&Config {
            dry_run: true,
            ..cfg()
        });
        assert_eq!(r.removed_units, 1);
        assert!(fx.exists(&format!("deps/libserde-{A}.rlib")));
    }

    #[test]
    fn busy_while_cargo_holds_lock() {
        let fx = Fx::new("busy");
        fx.lib("serde", A, 7200, false);
        fx.lib("serde", B, 10, false);
        let held = lock_file(&fx.prof.join(".cargo-lock")).unwrap();
        held.lock().unwrap();
        assert!(matches!(prune(&fx.prof, &cfg()).unwrap(), Outcome::Busy));
        assert!(fx.exists(&format!("deps/libserde-{A}.rlib")));
        drop(held);
        assert_eq!(fx.run(&cfg()).removed_units, 1);
    }

    fn hx(n: u64) -> String {
        format!("{n:016x}")
    }
    fn reasons(r: &Report) -> Vec<(&'static str, usize)> {
        r.by_reason.iter().map(|(k, v)| (*k, *v)).collect()
    }
    fn set_time(p: &Path, t: SystemTime) {
        fs::File::options()
            .read(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }
    const DAY: u64 = 24 * 3600;
    const LIB: &str = r#"{"rustc":1,"features":"[]","deps":[],"local":[]}"#;

    #[test]
    fn config_from_vars() {
        let c = Config::from_vars(|k| match k {
            "CARGO_AUTOPRUNE_GRACE" => Some(" 30 ".into()),
            "CARGO_AUTOPRUNE_MAX_SIZE" => Some("2G".into()),
            _ => None,
        });
        assert_eq!(
            (c.grace, c.max_size),
            (Duration::from_secs(30), Some(2 << 30))
        );
        let c = Config::from_vars(|_| Some("junk".into()));
        assert_eq!((c.grace, c.max_size), (Duration::from_secs(600), None));
        // cargo sets the package version for test processes: 0.1.x
        assert_eq!(
            env_secs("CARGO_PKG_VERSION_MINOR", 99),
            Duration::from_secs(1)
        );
        assert_eq!(
            env_secs("CARGO_AUTOPRUNE_NEVER_SET_IN_TESTS", 7),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn freed_bytes_trash_log_and_phases() {
        let fx = Fx::new("freed");
        fx.lib("serde", A, 7200, false);
        fx.lib("serde", B, 10, false);
        let state = fx.prof.join(STATE_DIR);
        // fingerprint dir (3 files, moved to the trash) + rlib, .o, .rmeta, .d: 1000 B each
        let dry = fx.run(&Config {
            dry_run: true,
            ..cfg()
        });
        assert_eq!((dry.freed_bytes, dry.removed.len()), (7000, 5));
        assert!(!state.join("log").exists() && !state.join("trash").exists());
        let r = fx.run(&cfg());
        assert_eq!((r.freed_bytes, r.removed.len()), (7000, 5));
        assert_eq!(reasons(&r), [("superseded", 1)]);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(!state.join("trash").exists());
        assert!(fs::read_to_string(state.join("log"))
            .unwrap()
            .contains("units=1 "));
        let phases: Vec<&str> = r.phases.iter().map(|p| p.0).collect();
        assert!(
            phases.contains(&"plan") && phases.contains(&"delete (locked)"),
            "{phases:?}"
        );
    }

    /// Deletes `n` superseded units while a "cargo" thread takes the lock whenever it is free
    /// and rebuilds (touches) every superseded unit. Returns how many units were deleted.
    fn prune_racing_cargo(tag: &str, lock_batch: Duration) -> usize {
        let fx = Fx::new(tag);
        let n = 20;
        for i in 0..n {
            fx.lib(&format!("p{i}"), &hx(i), 7200, false);
            fx.lib(&format!("p{i}"), &hx(100 + i), 10, false);
        }
        let prof = fx.prof.clone();
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                let trash = prof.join(STATE_DIR).join("trash");
                let start = Instant::now();
                while !trash.exists() && start.elapsed() < Duration::from_secs(20) {
                    std::thread::yield_now();
                }
                let lock = lock_file(&prof.join(".cargo-lock")).unwrap();
                while lock.try_lock().is_err() && start.elapsed() < Duration::from_secs(20) {
                    std::thread::yield_now();
                }
                for i in 0..n {
                    let stamp = prof.join(format!(".fingerprint/p{i}-{}/invoked.timestamp", hx(i)));
                    if stamp.exists() {
                        set_age(&stamp, 0);
                    }
                }
            });
            fx.run(&Config {
                lock_batch,
                lock_wait: Duration::from_secs(20),
                ..cfg()
            })
        });
        for i in 0..n {
            let fp = fx.exists(&format!(".fingerprint/p{i}-{}", hx(i)));
            let lib = fx.exists(&format!("deps/libp{i}-{}.rlib", hx(i)));
            assert_eq!(fp, lib, "unit {i} half deleted");
        }
        r.removed_units
    }

    #[test]
    fn relocks_between_batches_and_skips_units_cargo_touched() {
        assert!(prune_racing_cargo("race0", Duration::ZERO) < 20);
        assert_eq!(prune_racing_cargo("race1", Duration::from_secs(60)), 20);
    }

    #[test]
    fn units_file_keeps_live_records_only() {
        let fx = Fx::new("records");
        fx.lockfile(&["app"], &[]);
        fx.lib("app", A, 7200, false);
        fx.lib("app", B, 10, false);
        let state = fx.prof.join(STATE_DIR);
        fs::create_dir_all(&state).unwrap();
        let root = fx.root.display();
        fs::write(
            state.join(UNITS_FILE),
            format!("{A}\t{root}\n{B}\t{root}\n{C}\t/nonexistent-autoprune/x\n"),
        )
        .unwrap();
        assert_eq!(fx.run(&cfg()).removed_units, 1);
        assert_eq!(
            fs::read_to_string(state.join(UNITS_FILE)).unwrap(),
            format!("{B}\t{root}\n")
        );
    }

    #[test]
    fn every_reason_is_reported() {
        let fx = Fx::new("reasons");
        fx.lockfile(&["app", "util"], &["serde"]);
        fx.file(&format!("deps/x-{}.d", hx(1)), 10); // orphan
        fx.node("app", A, 7200, 1, 100, &[]); // superseded by B
        fx.node("app", B, 10, 1, 101, &[]);
        fx.node("util", C, 15 * DAY, 9, 102, &[]); // old toolchain
        fx.node("serde", &hx(2), 7200, 1, 103, &[]); // nobody uses it
        let r = fx.run(&cfg());
        assert_eq!(
            reasons(&r),
            [
                ("old toolchain", 1),
                ("orphan", 1),
                ("superseded", 1),
                ("unreachable", 1)
            ]
        );
    }

    #[test]
    fn toolchain_retires_after_two_weeks() {
        let fx = Fx::new("retire");
        fx.node("util", A, 13 * DAY, 9, 1, &[]);
        fx.node("app", B, 10, 1, 2, &[]);
        assert_eq!(fx.run(&cfg()).removed_units, 0);
    }

    #[test]
    fn legacy_layout_retires_a_day_after_the_new_one() {
        let run = |legacy_age: u64, with_new: bool| {
            let fx = Fx::new(&format!("legacy{legacy_age}{with_new}"));
            fx.lib("old", A, legacy_age, false);
            if with_new {
                fx.new_unit("new", B, 10, &["libnew.rlib"]);
            }
            let r = fx.run(&cfg());
            assert!(!with_new || fx.exists(&format!("build/new/{B}/out/libnew.rlib")));
            reasons(&r)
        };
        assert_eq!(run(23 * 3600, true), []);
        assert_eq!(run(25 * 3600, true), [("old layout", 1)]);
        assert_eq!(
            run(30 * DAY, false),
            [],
            "no new layout: nothing to compare with"
        );
    }

    #[test]
    fn stale_variant_expires_after_a_week() {
        let run = |age: u64| {
            let fx = Fx::new(&format!("variant{age}"));
            fx.lockfile(&["util"], &[]); // signatures need a workspace
            fx.unit_json("util", A, age, "lib-util", LIB, 1);
            let x = LIB.replace(r#""[]""#, r#""[\"x\"]""#);
            fx.unit_json("util", B, 10, "lib-util", &x, 2);
            reasons(&fx.run(&cfg()))
        };
        assert_eq!(run(6 * DAY), []);
        assert_eq!(run(8 * DAY), [("stale variant", 1)]);
    }

    #[test]
    fn lock_change_supersedes_within_grace() {
        let run = |old: u64, registry: u64| {
            let fx = Fx::new(&format!("lockchg{old}-{registry}"));
            fx.lockfile(&["app"], &["dep"]);
            set_age(&fx.root.join("Cargo.lock"), 100);
            fx.node("app", A, old, 1, 1, &[]);
            fx.node("app", B, 10, 1, 2, &[]);
            fx.node("dep", C, registry, 1, 3, &[]); // unused, within the grace period
            reasons(&fx.run(&cfg()))
        };
        assert_eq!(run(200, 200), [("superseded", 1), ("unreachable", 1)]);
        assert_eq!(run(50, 50), []);
    }

    #[test]
    fn outdated_registry_version_and_its_dependents_go() {
        let fx = Fx::new("outdated");
        fx.lockfile(&["app"], &["serde", "regex"]);
        let src = |p: &str, v: &str| format!("x.d: /h/registry/src/idx/{p}-{v}/src/lib.rs\n");
        fx.node("serde", B, 7200, 1, 1, &[]);
        fs::write(
            fx.file(&format!("deps/serde-{B}.d"), 7200),
            src("serde", "0.9.0"),
        )
        .unwrap();
        fx.node("regex", C, 7200, 1, 2, &[]);
        fs::write(
            fx.file(&format!("deps/regex-{C}.d"), 7200),
            src("regex", "1.0.0"),
        )
        .unwrap();
        fx.node("app", A, 10, 1, 100, &[1, 2]);
        // an orphan of the old version stays an orphan
        fs::write(
            fx.file(&format!("deps/serde-{}.d", hx(9)), 10),
            src("serde", "0.9.0"),
        )
        .unwrap();
        let r = fx.run(&cfg());
        assert_eq!(
            reasons(&r),
            [
                ("orphan", 1),
                ("outdated dependency", 2),
                ("unreachable", 1)
            ]
        );
    }

    #[test]
    fn build_scripts_live_while_used() {
        let fx = Fx::new("bscript");
        let kind = "build-script-build-script-build";
        fx.unit_json("app", A, 7200, kind, LIB, 5); // older, but the lib below uses it
        fx.unit_json("app", B, 10, kind, LIB, 6);
        fx.node("app", C, 10, 1, 7, &[5]);
        assert_eq!(fx.run(&cfg()).removed_units, 0);
    }

    #[test]
    fn size_cap_evicts_until_under_the_cap() {
        let fx = Fx::new("cap2");
        fx.lib("a", A, 9000, false);
        fx.lib("b", B, 5000, false);
        fx.lib("c", C, 10, false);
        let r = fx.run(&Config {
            max_size: Some(8000),
            ..cfg()
        });
        assert_eq!(reasons(&r), [("size cap", 2)]);
        assert!(fx.exists(&format!("deps/libc-{C}.rlib")));
    }

    #[test]
    fn size_cap_takes_incremental_first_at_equal_age() {
        let fx = Fx::new("capinc");
        fx.lib("x", A, 100, false);
        fx.file("incremental/x-1/s-x/a.bin", 100);
        let t = SystemTime::now() - Duration::from_secs(100);
        set_time(
            &fx.prof
                .join(format!(".fingerprint/x-{A}/invoked.timestamp")),
            t,
        );
        set_time(&fx.prof.join("incremental/x-1"), t);
        let r = fx.run(&Config {
            max_size: Some(7500),
            ..cfg()
        });
        assert_eq!((r.removed_units, r.removed_incremental), (0, 1));
        assert!(fx.exists(&format!("deps/libx-{A}.rlib")));
    }

    #[test]
    fn one_incremental_dir_per_unit() {
        let fx = Fx::new("incpair");
        fx.lib("app", B, 10, false);
        for (d, age) in [("app-1", 10), ("app-2", 7200)] {
            fx.dir(&format!("incremental/{d}/s-x"), age);
            set_age(&fx.prof.join(format!("incremental/{d}")), age);
        }
        assert_eq!(fx.run(&cfg()).removed_incremental, 1);
        assert!(fx.exists("incremental/app-1") && !fx.exists("incremental/app-2"));
    }

    #[test]
    fn orphan_build_dir_claims_its_incremental_dir() {
        let fx = Fx::new("orphaninc");
        fx.lib("app", B, 10, false);
        fx.file(&format!("build/app-{A}/output"), 7200);
        set_age(&fx.prof.join(format!("build/app-{A}")), 7200);
        fx.file(&format!("deps/app-{A}.d"), 7200);
        fx.dir("incremental/app-1/s-x", 7200);
        set_age(&fx.prof.join("incremental/app-1"), 7200);
        let r = fx.run(&cfg());
        assert_eq!((r.removed_units, r.removed_incremental), (1, 1));
    }

    #[test]
    fn output_files_are_not_a_kind() {
        let fx = Fx::new("outputfile");
        fx.lib("foo", A, 7200, false);
        fx.file(&format!(".fingerprint/foo-{A}/output-lib-foo"), 7200);
        fx.lib("foo", B, 10, false);
        assert_eq!(fx.run(&cfg()).removed_units, 1);
        assert!(!fx.exists(&format!("deps/libfoo-{A}.rlib")));
    }

    #[test]
    fn invoked_timestamp_dates_the_unit() {
        let fx = Fx::new("stampdate");
        fx.lib("foo", A, 7200, false);
        set_age(
            &fx.prof
                .join(format!(".fingerprint/foo-{A}/invoked.timestamp")),
            10,
        );
        fx.lib("foo", B, 10, false);
        set_age(
            &fx.prof
                .join(format!(".fingerprint/foo-{B}/invoked.timestamp")),
            7200,
        );
        assert_eq!(fx.run(&cfg()).removed_units, 1);
        assert!(
            fx.exists(&format!("deps/libfoo-{A}.rlib"))
                && !fx.exists(&format!("deps/libfoo-{B}.rlib"))
        );
    }

    #[test]
    fn new_layout_units_and_output_kinds() {
        let fx = Fx::new("newlayout");
        fx.new_unit("app", A, 7200, &["libapp.rlib", "libapp.rmeta"]);
        fx.new_unit("app", B, 10, &["libapp.rlib", "libapp.rmeta"]);
        fx.new_unit("app", C, 7200, &["libapp.rmeta"]); // check: another output kind
        fx.new_unit("tool", &hx(1), 7200, &["tool"]); // a binary
        fx.new_unit("tool", &hx(2), 10, &["tool.d"]); // no output kind
        fx.new_unit("lib2", &hx(3), 7200, &["liblib2.rlib"]);
        fx.dir(&format!("build/lib2/{}/out/sub", hx(3)), 7200); // a dir is no binary
        fx.new_unit("lib2", &hx(4), 10, &["liblib2.rlib"]);
        let r = fx.run(&cfg());
        assert_eq!(reasons(&r), [("superseded", 2)]);
        assert!(
            !fx.exists(&format!("build/app/{A}")) && !fx.exists(&format!("build/lib2/{}", hx(3)))
        );
        assert!(fx.exists(&format!("build/app/{B}")) && fx.exists(&format!("build/app/{C}")));
        assert!(fx.exists(&format!("build/tool/{}", hx(1))));
    }

    #[test]
    fn new_layout_dirs_without_fingerprint() {
        let fx = Fx::new("newempty");
        fx.dir(&format!("build/pkg/{}/out", hx(1)), 10); // prepared by cargo, empty: kept
        fx.file(&format!("build/pkg/{}/out/sub/f", hx(2)), 10); // leftovers: orphan
        fx.file("build/pkg/abcdef/f", 10); // not a unit dir
        let r = fx.run(&cfg());
        assert_eq!(reasons(&r), [("orphan", 1)]);
        assert!(fx.exists(&format!("build/pkg/{}/out", hx(1))) && fx.exists("build/pkg/abcdef/f"));
        assert!(!fx.exists(&format!("build/pkg/{}", hx(2))));
    }

    #[test]
    fn legacy_copy_of_a_new_layout_unit_is_an_orphan() {
        let fx = Fx::new("dual");
        fx.lib("dual", A, 7200, false);
        fx.new_unit("dual", A, 10, &["libdual.rlib"]);
        let r = fx.run(&cfg());
        assert_eq!(reasons(&r), [("orphan", 1)]);
        assert!(
            !fx.exists(&format!("deps/libdual-{A}.rlib"))
                && fx.exists(&format!("build/dual/{A}/out/libdual.rlib"))
        );
    }

    #[test]
    fn new_layout_dep_info_names_the_crate() {
        let fx = Fx::new("newkrate");
        fx.new_unit("app", A, 10, &[&format!("app-{A}.d"), "libapp.rlib"]);
        fx.dir("incremental/app-1/s-x", 10);
        set_age(&fx.prof.join("incremental/app-1"), 10);
        assert_eq!(fx.run(&cfg()).removed_incremental, 0);
    }

    #[test]
    #[cfg(unix)]
    fn new_layout_incremental_owner_by_hard_link() {
        let fx = Fx::new("newinclink");
        fx.new_unit("app", A, 7200, &["libapp.rmeta"]);
        fx.new_unit("app", B, 10, &["libapp.rmeta"]);
        for (d, h) in [("app-1", A), ("app-2", B)] {
            fx.dir(&format!("incremental/{d}/s-x"), 5);
            fs::hard_link(
                fx.prof.join(format!("build/app/{h}/out/libapp.rmeta")),
                fx.prof.join(format!("incremental/{d}/s-x/metadata.rmeta")),
            )
            .unwrap();
            set_age(&fx.prof.join(format!("incremental/{d}")), 5);
        }
        let r = fx.run(&cfg());
        assert_eq!(r.removed_incremental, 1);
        assert!(!fx.exists("incremental/app-1") && fx.exists("incremental/app-2"));
    }

    #[test]
    #[cfg(unix)]
    fn only_session_dirs_count_in_incremental() {
        let fx = Fx::new("incfilter");
        fx.lib("app", A, 7200, false); // superseded
        fx.lib("app", B, 10, false);
        fx.dir("incremental/app-1/s-x", 10);
        fx.dir("incremental/app-1/other", 10); // not a session: its links don't count
        fs::hard_link(
            fx.prof.join(format!("deps/libapp-{A}.rmeta")),
            fx.prof.join("incremental/app-1/other/metadata.rmeta"),
        )
        .unwrap();
        set_age(&fx.prof.join("incremental/app-1"), 10);
        fx.file("incremental/app-9.lock", 10); // a file
        fx.dir("incremental/nodash/s-x", 7200); // no crate-id name
        let r = fx.run(&cfg());
        assert_eq!((r.removed_units, r.removed_incremental), (1, 0));
        assert!(fx.exists("incremental/app-1") && fx.exists("incremental/app-9.lock"));
        assert!(fx.exists("incremental/nodash"));
    }

    /// Thresholds are strict: exactly at the limit is still live.
    #[test]
    fn exactly_at_a_threshold_is_kept() {
        let base = SystemTime::now() - Duration::from_secs(10);
        let fp = |fx: &Fx, pkg: &str, h: &str| fx.prof.join(format!(".fingerprint/{pkg}-{h}"));

        let fx = Fx::new("edge-toolchain");
        fx.node("util", A, 0, 9, 1, &[]);
        fx.node("app", B, 0, 1, 2, &[]);
        set_time(&fp(&fx, "app", B), base);
        set_time(&fp(&fx, "util", A), base - Duration::from_secs(14 * DAY));
        assert_eq!(reasons(&fx.run(&cfg())), []);

        let fx = Fx::new("edge-variant");
        fx.lockfile(&["util"], &[]);
        fx.unit_json("util", A, 0, "lib-util", LIB, 1);
        let x = LIB.replace(r#""[]""#, r#""[\"x\"]""#);
        fx.unit_json("util", B, 0, "lib-util", &x, 2);
        set_time(&fp(&fx, "util", B), base);
        set_time(&fp(&fx, "util", A), base - Duration::from_secs(7 * DAY));
        assert_eq!(reasons(&fx.run(&cfg())), []);

        let fx = Fx::new("edge-lock");
        fx.lockfile(&["app"], &["dep"]);
        let lock = base - Duration::from_secs(90);
        set_time(&fx.root.join("Cargo.lock"), lock);
        fx.node("app", A, 0, 1, 1, &[]);
        fx.node("app", B, 0, 1, 2, &[]);
        fx.node("dep", C, 0, 1, 3, &[]);
        set_time(&fp(&fx, "app", A), lock); // built in the same second the lock file changed
        set_time(&fp(&fx, "dep", C), lock);
        set_time(&fp(&fx, "app", B), base);
        assert_eq!(reasons(&fx.run(&cfg())), []);
    }

    #[test]
    fn units_of_a_removed_target_go() {
        let fx = Fx::new("removedtarget");
        fx.lockfile(&["app", "tool"], &[]);
        fs::create_dir_all(fx.root.join("app/src")).unwrap();
        fs::write(fx.root.join("app/src/main.rs"), "").unwrap();
        fx.lib("app", A, 10, false);
        fs::write(
            fx.prof.join(format!("deps/app-{A}.d")),
            "x: app/src/main.rs\n",
        )
        .unwrap();
        fx.lib("tool", B, 10, false);
        fs::write(
            fx.prof.join(format!("deps/tool-{B}.d")),
            "x: tool/src/main.rs\n",
        )
        .unwrap();
        assert_eq!(reasons(&fx.run(&cfg())), [("removed target", 1)]);
        assert!(
            fx.exists(&format!("deps/libapp-{A}.rlib"))
                && !fx.exists(&format!("deps/libtool-{B}.rlib"))
        );
    }

    #[test]
    fn legacy_build_dirs_are_not_package_dirs() {
        let fx = Fx::new("legacybuild");
        fx.file(
            &format!(".fingerprint/libc-{A}/run-build-script-build-script-build"),
            10,
        );
        set_age(&fx.prof.join(format!(".fingerprint/libc-{A}")), 10);
        fx.file(&format!("build/libc-{A}/{}/data", hx(5)), 10); // OUT_DIR-like content
        fx.file("build/stray-file", 10);
        assert_eq!(reasons(&fx.run(&cfg())), []);
        assert!(fx.exists(&format!("build/libc-{A}/{}/data", hx(5))));
    }

    #[test]
    fn refuses_non_cargo_dir() {
        let dir = std::env::temp_dir().join(format!("autoprune-notcargo-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".cargo-lock"), "").unwrap(); // lock but no CACHEDIR.TAG above
        assert!(prune(&dir, &cfg()).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
