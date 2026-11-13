//! Stories of a prune next to a running cargo: Cargo's locks, batches, another prune.

mod support;

use cargo_autoprune::layout::STATE_DIR;
use cargo_autoprune::prune::{prune, Config, Outcome};
use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};
use support::sim::{set_mtime, Key, Sim, DAY};

fn cfg() -> Config {
    Config {
        lock_wait: Duration::ZERO,
        ..Config::default()
    }
}

fn lock(p: &Path) -> File {
    let f = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(p)
        .unwrap();
    f.lock().unwrap();
    f
}

/// Two dependency bumps: the units built against serde 1.0.0 and 1.0.1 are garbage (12).
fn garbage(tag: &str) -> (Sim, Vec<Key>) {
    let mut s = Sim::new(tag);
    s.build();
    let mut old = s.used.clone();
    s.conf.serde = 1;
    s.build();
    old.extend(s.wrote.clone());
    s.conf.serde = 2;
    s.build();
    old.retain(|k| !s.used.contains(k));
    old.dedup();
    (s, old)
}

/// Given a cargo build holding the profile's lock
/// When a prune starts that may not wait
/// Then it reports busy and deletes nothing
/// When it may wait and the build ends meanwhile
/// Then it prunes after it.
#[test]
fn prune_waits_for_the_running_build() {
    let (s, old) = garbage("busy");
    let held = lock(&s.prof.join(".cargo-lock"));
    assert!(matches!(prune(&s.prof, &cfg()).unwrap(), Outcome::Busy));
    assert!(old.iter().all(|k| s.present(k)));
    let t = Instant::now();
    let r = std::thread::scope(|sc| {
        sc.spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        prune(
            &s.prof,
            &Config {
                lock_wait: Duration::from_secs(10),
                ..cfg()
            },
        )
        .unwrap()
    });
    assert!(t.elapsed() >= Duration::from_millis(300));
    assert!(matches!(r, Outcome::Done(r) if r.removed_units == old.len()));
}

/// Given a prune already running on the profile
/// Then a second one reports busy at once.
#[test]
fn one_prune_at_a_time() {
    let (s, _) = garbage("twice");
    fs::create_dir_all(s.prof.join(STATE_DIR)).unwrap();
    let _running = lock(&s.prof.join(STATE_DIR).join("lock"));
    assert!(matches!(prune(&s.prof, &cfg()).unwrap(), Outcome::Busy));
}

/// Deletes the garbage while a "cargo" takes the lock whenever it is free and rebuilds (touches)
/// every garbage unit. Returns how many units were deleted.
fn prune_racing_cargo(tag: &str, lock_batch: Duration) -> usize {
    let (s, old) = garbage(tag);
    let stamps: Vec<_> = old
        .iter()
        .map(|k| s.units[k].fp.join("invoked.timestamp"))
        .collect();
    let prof = s.prof.clone();
    let r = std::thread::scope(|sc| {
        sc.spawn(|| {
            let trash = prof.join(STATE_DIR).join("trash");
            let start = Instant::now();
            while !trash.exists() && start.elapsed() < Duration::from_secs(20) {
                std::thread::yield_now();
            }
            let f = File::options()
                .read(true)
                .write(true)
                .open(prof.join(".cargo-lock"))
                .unwrap();
            while f.try_lock().is_err() && start.elapsed() < Duration::from_secs(20) {
                std::thread::yield_now();
            }
            for st in &stamps {
                if st.exists() {
                    set_mtime(st, SystemTime::now());
                }
            }
        });
        prune(
            &s.prof,
            &Config {
                lock_batch,
                lock_wait: Duration::from_secs(20),
                ..cfg()
            },
        )
        .unwrap()
    });
    let half = s.half_units();
    assert!(half.is_empty(), "half deleted: {half:?}");
    match r {
        Outcome::Done(r) => r.removed_units,
        Outcome::Busy => panic!("busy"),
    }
}

/// Given a big cleanup and a cargo waiting for the lock
/// When the prune works in batches (here: one unit each)
/// Then it lets cargo in between batches and skips every unit cargo rebuilt meanwhile, never
/// leaving a unit half deleted; in one long batch everything goes.
#[test]
fn batches_let_cargo_in_and_skip_what_it_rebuilt() {
    assert!(prune_racing_cargo("race0", Duration::ZERO) < 12);
    assert_eq!(prune_racing_cargo("race1", Duration::from_secs(60)), 12);
}

/// Given a dir that has a cargo lock file but is not inside a target dir (a profile dir is at
/// most three levels below one; this one is four below the project's `target/`)
/// Then the pruner refuses to touch it.
#[test]
fn refuses_a_dir_that_is_not_a_cargo_profile() {
    let root = support::scratch("notcargo");
    let dir = root.join("a/b");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(".cargo-lock"), "").unwrap();
    assert!(prune(&dir, &cfg()).is_err());
    assert!(!dir.join(STATE_DIR).exists());
    let _ = fs::remove_dir_all(&root);
}

/// Given garbage of every kind: units built against old dependency versions (their build
/// scripts ran after cargo stamped them), a crashed build's leftovers, stale incremental dirs
/// When the prune works in batches of one unit and no cargo comes in between
/// Then it re-validates every unit after relocking and still deletes all of it, as in one batch.
#[test]
fn batches_without_cargo_delete_everything() {
    let run = |tag: &str, lock_batch| {
        let (mut s, _) = garbage(tag);
        s.crash();
        s.advance(DAY);
        let r = s.run(&Config {
            lock_batch,
            ..cfg()
        });
        (r.removed_units, r.removed_incremental, r.by_reason)
    };
    let one = run("batch60", Duration::from_secs(60));
    assert!(one.2.contains_key("orphan"), "{one:?}");
    assert_eq!(run("batch0", Duration::ZERO), one);
}
