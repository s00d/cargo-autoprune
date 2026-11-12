//! Cargo's per-profile file locks.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

const CARGO_LOCKS: [&str; 3] = [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

pub(crate) fn lock_file(p: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(p)
}

/// Take every Cargo lock of the profile so no build runs while we delete. All-or-nothing with
/// retry, so we never hold one lock while waiting for another (no deadlock with cargo).
pub(crate) fn acquire_cargo_locks(profile: &Path, wait: Duration) -> io::Result<Option<Vec<File>>> {
    let deadline = Instant::now() + wait;
    loop {
        let mut held = Vec::new();
        let mut ok = true;
        for name in CARGO_LOCKS {
            let p = profile.join(name);
            if !p.exists() {
                continue;
            }
            let f = lock_file(&p)?;
            if f.try_lock().is_err() {
                ok = false;
                break;
            }
            held.push(f);
        }
        if ok {
            return Ok(Some(held));
        }
        drop(held);
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fx;

    #[test]
    fn waits_for_cargo_to_release_its_locks() {
        let fx = Fx::new("lock-wait");
        std::fs::write(fx.prof.join(".cargo-build-lock"), "").unwrap();
        let held = lock_file(&fx.prof.join(".cargo-build-lock")).unwrap();
        held.lock().unwrap();
        assert!(acquire_cargo_locks(&fx.prof, Duration::ZERO)
            .unwrap()
            .is_none());
        let t = Instant::now();
        std::thread::scope(|s| {
            s.spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                drop(held);
            });
            let got = acquire_cargo_locks(&fx.prof, Duration::from_secs(10)).unwrap();
            assert_eq!(got.map(|l| l.len()), Some(2), "both existing lock files");
        });
        assert!(t.elapsed() >= Duration::from_millis(300));
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}
