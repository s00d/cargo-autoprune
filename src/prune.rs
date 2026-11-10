//! One prune pass over a profile dir: lock, scan, plan, delete, record.

use crate::fingerprint::local_packages;
use crate::fsutil::size_of;
use crate::layout::{is_profile_dir, STATE_DIR, TAGS_DIR};
use crate::lock::{acquire_cargo_locks, lock_file};
use crate::plan::{apply_cap, plan};
use crate::scan::scan;
use crate::state::{clean_tags, log, read_deleted, read_pins, write_deleted, write_pins};
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
}

impl Config {
    /// Defaults overridden by `CARGO_AUTOPRUNE_GRACE` / `CARGO_AUTOPRUNE_MAX_SIZE`.
    pub fn from_env() -> Self {
        let d = Config::default();
        Config {
            grace: env_secs("CARGO_AUTOPRUNE_GRACE", d.grace.as_secs()),
            max_size: std::env::var("CARGO_AUTOPRUNE_MAX_SIZE")
                .ok()
                .and_then(|s| crate::parse_size(&s)),
            ..d
        }
    }
}

pub(crate) fn env_secs(name: &str, default: u64) -> Duration {
    let v = std::env::var(name).ok().and_then(|v| v.trim().parse().ok());
    Duration::from_secs(v.unwrap_or(default))
}

impl Default for Config {
    fn default() -> Self {
        Config {
            grace: Duration::from_secs(600),
            max_size: None,
            dry_run: false,
            lock_wait: Duration::from_secs(3600),
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub removed_units: usize,
    pub removed_incremental: usize,
    pub freed_bytes: u64,
    pub pinned_now: Vec<String>,
    pub removed: Vec<PathBuf>,
    pub elapsed: Duration,
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
    let Some(_cargo_locks) = acquire_cargo_locks(profile, cfg.lock_wait)? else {
        return Ok(Outcome::Busy);
    };
    let start = Instant::now(); // prune time only, not the wait for cargo

    let now = SystemTime::now();
    let deleted_before = read_deleted(&state.join("deleted"), now);
    let mut pinned = read_pins(&state.join("pinned"), now);
    let locals = local_packages(profile);
    let (units, incremental) = scan(profile, locals.is_some());

    let mut report = Report::default();
    // A previously deleted unit is back: its group holds two live variants.
    for (h, u) in &units {
        let local = locals
            .as_ref()
            .is_none_or(|l| l.contains(u.name.as_deref().unwrap_or("")));
        if u.has_fingerprint
            && local
            && deleted_before.contains_key(h.as_str())
            && !pinned.contains_key(&u.group())
        {
            pinned.insert(u.group(), now);
            report.pinned_now.push(u.group());
        }
    }
    let (doomed, kept) = plan(units, incremental, &pinned, locals.as_ref(), cfg);
    let doomed = apply_cap(doomed, kept, cfg.max_size);

    let mut new_deleted = Vec::new();
    for c in &doomed {
        for p in &c.paths {
            report.freed_bytes += size_of(p);
            if !cfg.dry_run {
                let r = if fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false) {
                    fs::remove_dir_all(p)
                } else {
                    fs::remove_file(p)
                };
                if let Err(e) = r {
                    if e.kind() != io::ErrorKind::NotFound {
                        eprintln!("cargo-autoprune: {}: {e}", p.display());
                    }
                }
            }
            report.removed.push(p.clone());
        }
        if c.incremental {
            report.removed_incremental += 1;
        } else {
            report.removed_units += 1;
        }
        new_deleted.push(c.id.clone());
    }
    report.elapsed = start.elapsed();
    if !cfg.dry_run {
        let mut deleted = deleted_before;
        let ts = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        deleted.extend(new_deleted.into_iter().map(|id| (id, ts)));
        write_deleted(&state.join("deleted"), &deleted)?;
        clean_tags(&state.join(TAGS_DIR), profile);
        write_pins(&state.join("pinned"), &pinned)?;
        log(&state.join("log"), &report);
    }
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
    fn revert_after_window_is_not_pinned() {
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
        fx.lib("tokio", A, 10, false); // rebuilt: it was a live variant
        let r = fx.run(&cfg());
        assert_eq!(r.removed_units, 0);
        assert_eq!(r.pinned_now.len(), 1);
        assert!(fx.exists(&format!("deps/libtokio-{B}.rlib")));
        assert_eq!(fx.run(&cfg()).removed_units, 0); // pin persists
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

    #[test]
    fn refuses_non_cargo_dir() {
        let dir = std::env::temp_dir().join(format!("autoprune-notcargo-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".cargo-lock"), "").unwrap(); // lock but no CACHEDIR.TAG above
        assert!(prune(&dir, &cfg()).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
