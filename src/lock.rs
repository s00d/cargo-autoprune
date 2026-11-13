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
