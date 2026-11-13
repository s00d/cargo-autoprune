//! Stories of the pruning rules over a simulated cargo, at the exact thresholds random sessions
//! never hit. One rule per story; each prune is a background prune right after a build.

mod support;

use cargo_autoprune::layout::{STATE_DIR, UNITS_FILE};
use cargo_autoprune::prune::{Config, Report};
use std::fs;
use std::time::Duration;
use support::sim::{set_mtime, Key, Mode, Sim, DAY};

const GRACE: u64 = 600;

fn cfg() -> Config {
    Config {
        lock_wait: Duration::ZERO,
        ..Config::default()
    }
}

fn reasons(r: &Report) -> Vec<(&'static str, usize)> {
    r.by_reason.iter().map(|(k, v)| (*k, *v)).collect()
}

/// Reasons of the workspace and registry units only (incremental dirs left out).
fn unit_reasons(r: &Report) -> Vec<(&'static str, usize)> {
    reasons(r)
        .into_iter()
        .filter(|(k, _)| *k != "stale incremental")
        .collect()
}

/// Build, wait `gap` seconds, build again with util's feature flipped (feature unification:
/// app gets a new hash in the same group).
fn flip_after(tag: &str, gap: u64) -> (Sim, Key) {
    let mut s = Sim::new(tag);
    s.build();
    let old = s.key("app", "lib-");
    s.advance(gap - 5); // a build takes 5 s
    s.conf.util_x = true;
    s.build();
    (s, old)
}

/// Given app rebuilt with another hash
/// When the old copy is exactly `grace` older than the new one
/// Then it is kept; one second more and it is superseded.
#[test]
fn superseded_after_the_grace_period() {
    let (s, old) = flip_after("grace-edge", GRACE);
    assert_eq!(unit_reasons(&s.run(&cfg())), []);
    assert!(s.present(&old));
    let (s, old) = flip_after("grace-over", GRACE + 1);
    // app and tool (built on app)
    assert_eq!(unit_reasons(&s.run(&cfg())), [("superseded", 2)]);
    assert!(!s.present(&old) && s.present(&s.key("app", "lib-")));
}

/// Given app built with rustc warnings (cargo saves them next to the fingerprint)
/// When an edit fixes them and app is rebuilt with another hash
/// Then the old copy is still superseded: saved warnings make no new kind of unit.
#[test]
fn fixed_warnings_do_not_keep_the_old_copy() {
    let mut s = Sim::new("warnings");
    s.build();
    let old = s.key("app", "lib-");
    s.advance(GRACE + 1);
    s.edit("app");
    s.conf.util_x = true;
    s.build();
    assert_eq!(unit_reasons(&s.run(&cfg())), [("superseded", 2)]);
    assert!(!s.present(&old));
}

/// Given two variants of app that alternate under the same Cargo.lock (`-p util` vs `--workspace`)
/// When the deleted variant comes back once
/// Then that may be a revert: nothing is pinned and the other variant goes
/// When it comes back a second time
/// Then the group is pinned for a day: both variants stay, and after the day the old one goes.
#[test]
fn alternating_variants_get_pinned_for_a_day() {
    let (mut s, a) = flip_after("pin", 3600);
    let b = s.key("app", "lib-");
    let r = s.run(&cfg());
    assert!(!s.present(&a) && r.pinned_now.is_empty());
    s.conf.util_x = false;
    s.advance(GRACE + 1);
    s.build(); // A again: comeback 1
    let r = s.run(&cfg());
    assert!(r.pinned_now.is_empty(), "one comeback is a revert");
    assert!(s.present(&a) && !s.present(&b));
    s.conf.util_x = true;
    s.advance(GRACE + 1);
    s.build(); // B again: comeback 2
    let r = s.run(&cfg());
    assert_eq!(r.pinned_now.len(), 2, "app and tool");
    assert!(s.present(&a) && s.present(&b));
    s.advance(DAY - 60);
    assert_eq!(unit_reasons(&s.run(&cfg())), [], "still pinned");
    s.advance(120);
    assert_eq!(unit_reasons(&s.run(&cfg())), [("superseded", 2)]);
    assert!(s.present(&b) && !s.present(&a));
}

/// Given app flipping between two dependency sets, each flip a Cargo.lock change
/// When the deleted copy comes back again and again
/// Then these are reverts, never alternation: no pin, the stale copy always goes.
#[test]
fn dependency_toggles_never_pin() {
    let mut s = Sim::new("toggle");
    for i in 0..4 {
        s.conf.regex = i % 2 == 1;
        s.build();
        s.advance(10);
        let r = s.run(&cfg());
        assert!(r.pinned_now.is_empty(), "flip {i}");
        if i > 0 {
            assert!(r.by_reason.contains_key("superseded"), "flip {i}: {r:?}");
        }
        if i == 2 {
            // regex dropped: nothing uses it any more
            assert_eq!(r.by_reason.get("unreachable"), Some(&1), "{r:?}");
        }
    }
}

/// Given serde bumped in Cargo.lock and the workspace rebuilt
/// Then the units built against the old version go at once, without waiting for the grace
/// period: serde's old lib, build script and everything built on them
/// And the same in a target dir shared via `CARGO_TARGET_DIR`, with no `Cargo.lock` next to it:
/// the workspace is found through the package dirs the wrapper recorded.
#[test]
fn bumped_dependency_goes_with_its_dependents() {
    bump(Sim::new("bump"));
    bump(Sim::shared("bump-shared"));
}

fn bump(mut s: Sim) {
    s.build();
    let old: Vec<Key> = s.used.clone();
    s.advance(60);
    s.conf.serde = 1;
    s.build();
    let r = s.run(&cfg());
    let gone: Vec<&Key> = old.iter().filter(|k| !s.present(k)).collect();
    // serde lib, build script, run, util, app, tool: cfg-if stays
    assert_eq!(gone.len(), 6);
    assert!(s.present(&s.key("cfg-if", "lib-")));
    assert_eq!(r.by_reason.get("outdated dependency"), Some(&6), "{r:?}");
}

/// Given serde bumped and built, then reverted before any prune
/// When cargo builds again it reuses the older copies (fresh, untouched)
/// Then the newer copies built against the bumped version go, the reused ones stay.
#[test]
fn revert_keeps_the_reused_older_copies() {
    let mut s = Sim::new("revert");
    s.build();
    let old = s.used.clone();
    s.advance(3600);
    s.conf.serde = 1;
    s.build();
    let bumped = s.wrote.clone();
    s.advance(3600);
    s.conf.serde = 0;
    s.build();
    assert!(s.wrote.is_empty(), "everything reused");
    s.run(&cfg());
    assert!(old.iter().all(|k| s.present(k)));
    assert!(bumped.iter().all(|k| !s.present(k)));
}

/// Given toolchain 1 last used, then toolchain 2
/// Then toolchain 1 stays while it was used within 14 days of the newest build, exactly 14
/// included, and retires one second later.
#[test]
fn old_toolchain_retires_after_two_weeks() {
    let mut s = Sim::new("retire");
    s.build();
    let old = s.key("app", "lib-");
    s.conf.toolchain = 2;
    s.advance(14 * DAY - 5);
    s.build();
    assert_eq!(unit_reasons(&s.run(&cfg())), []);
    s.advance(1);
    s.edit("app");
    s.build(); // 5 s later: 14 days + 6 s
    let r = s.run(&cfg());
    assert!(r.by_reason["old toolchain"] >= 7, "{r:?}");
    assert!(!s.present(&old));
}

/// Given toolchain 1 used two days ago with a dependency set Cargo.lock no longer has
/// When those newest units of toolchain 1 go (outdated) and toolchain 2 builds 15 days after
/// toolchain 1's older units
/// Then toolchain 1 is not retired: its last use is remembered, not read from what is left.
#[test]
fn deleting_a_toolchains_newest_units_does_not_age_it() {
    let mut s = Sim::new("evidence");
    s.build(); // day 0, toolchain 1
    let day0 = s.key("util", "lib-");
    s.advance(13 * DAY);
    s.conf.serde = 1;
    s.build(); // day 13, toolchain 1, serde 1.0.1
    s.conf = support::sim::Conf {
        toolchain: 2,
        serde: 0,
        ..s.conf.clone()
    };
    s.build(); // toolchain 2, serde 1.0.0: toolchain 1's day-13 units are outdated
    s.advance(10);
    assert!(s.run(&cfg()).by_reason.contains_key("outdated dependency"));
    assert!(s.present(&day0));
    s.advance(2 * DAY);
    s.edit("app");
    s.build();
    let r = s.run(&cfg());
    assert!(!r.by_reason.contains_key("old toolchain"), "{r:?}");
    assert!(s.present(&day0));
}

/// Given the workspace built with the legacy layout, then a cargo that writes the new build-dir
/// layout (same toolchain)
/// Then the legacy copies of units the new layout rebuilt are orphans at once; the other legacy
/// units stay while an old cargo may still use them, and go a day after the newest new-layout
/// build.
#[test]
fn legacy_layout_retires_a_day_after_the_upgrade() {
    let mut s = Sim::new("layout");
    s.conf.mode = Mode::Check;
    s.build();
    let check = s.key("app", "lib-");
    s.conf.mode = Mode::Build;
    s.build();
    let legacy_build = s.key("app", "lib-");
    s.upgrade_layout();
    s.advance(60);
    s.build();
    let r = s.run(&cfg());
    assert!(r.by_reason["orphan"] >= 7, "{r:?}");
    assert!(s.present(&check), "a legacy unit nobody rebuilt yet stays");
    assert!(!s.present(&legacy_build) && s.present(&s.key("app", "lib-")));
    s.advance(DAY / 2);
    s.edit("app");
    s.build();
    s.run(&cfg());
    assert!(s.present(&check), "half a day after the upgrade");
    s.advance(DAY / 2);
    s.edit("app");
    s.build();
    let r = s.run(&cfg());
    assert!(r.by_reason["old layout"] >= 1, "{r:?}");
    assert!(!s.present(&check));
}

/// Given util built with and without its feature `x`
/// When only the `x` variant is rebuilt
/// Then the other variant stays a week (exactly 7 days included) and then goes.
#[test]
fn unused_variant_expires_after_a_week() {
    let run = |tag: &str, extra: u64| {
        let mut s = Sim::new(tag);
        s.build();
        let plain = s.key("util", "lib-");
        s.conf.util_x = true;
        s.advance(7 * DAY - 5 + extra);
        s.build();
        let r = s.run(&cfg());
        (r.by_reason.get("stale variant").copied(), s.present(&plain))
    };
    assert_eq!(run("variant-edge", 0), (None, true));
    assert_eq!(run("variant-over", 1), (Some(1), false));
}

/// Given the `tool` bin target removed from the workspace
/// Then its units go on the next build's prune, the rest stays.
#[test]
fn removed_target_goes() {
    let mut s = Sim::new("removed");
    s.build();
    let tool = s.key("tool", "bin-");
    s.conf.tool = false;
    s.build();
    let r = s.run(&cfg());
    assert_eq!(unit_reasons(&r), [("removed target", 1)]);
    assert!(!s.present(&tool));
    assert!(s.used.iter().all(|k| s.present(k)));
}

/// Given one build that writes every unit in the same second
/// When a cap a little below the total is set
/// Then incremental dirs go first, and the dry run names exactly what the real run deletes.
#[test]
fn size_cap_takes_incremental_dirs_first_at_equal_age() {
    let mut s = Sim::new("cap");
    s.build();
    let cap = s.cargo_bytes() - 100;
    let c = Config {
        max_size: Some(cap),
        ..cfg()
    };
    let dry = s.run(&Config {
        dry_run: true,
        ..c.clone()
    });
    let r = s.run(&c);
    assert_eq!(dry.removed, r.removed);
    assert_eq!((r.removed_units, r.removed_incremental), (0, 1));
    assert_eq!(reasons(&r), [("size cap", 1)]);
    assert!(s.used.iter().all(|k| s.present(k)));
}

/// Given state files a crashed or older version left corrupt
/// Then a prune ignores the bad lines and rewrites them.
#[test]
fn corrupt_state_files_are_ignored() {
    let mut s = Sim::new("corrupt");
    s.build();
    let state = s.prof.join(STATE_DIR);
    for f in ["pinned", "suspects", "deleted", "seen"] {
        fs::write(state.join(f), "x\t99999999999999999999\nno tab\ny\t\n").unwrap();
    }
    s.advance(10);
    let r = s.run(&cfg());
    assert!(r.errors.is_empty() && r.pinned_now.is_empty());
    assert!(!fs::read_to_string(state.join("pinned"))
        .unwrap()
        .contains('x'));
}

/// Given a target pruned for years: a log over 256 KiB
/// Then the log is rotated to `log.old` and starts over.
#[test]
fn log_rotates() {
    let mut s = Sim::new("log");
    s.build();
    let log = s.prof.join(STATE_DIR).join("log");
    fs::write(&log, "x".repeat(300 * 1024)).unwrap();
    s.run(&cfg());
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
    assert_eq!(
        fs::metadata(log.with_extension("old")).unwrap().len(),
        300 * 1024
    );
}

/// Given a build for `--target`: build scripts live in the host profile, used from
/// `<triple>/debug`
/// When the host profile is pruned
/// Then the build scripts the target profile uses stay.
#[test]
fn host_build_scripts_used_by_a_target_profile_stay() {
    let mut s = Sim::new("cross");
    s.build();
    let script = s.key("serde", "build-script-");
    let run = s.key("serde", "run-");
    let serde = s.key("serde", "lib-");
    // the target profile: serde's lib there depends on the host build script run
    let other = s.root.join("target/x86_64-unknown-none/debug");
    let fp = &s.units[&serde].fp;
    let dst = other.join(".fingerprint").join(fp.file_name().unwrap());
    fs::create_dir_all(&dst).unwrap();
    fs::write(other.join(".cargo-lock"), "").unwrap();
    for f in ["lib-serde", "lib-serde.json"] {
        fs::copy(fp.join(f), dst.join(f)).unwrap();
    }
    // a day later the host builds something newer (regex), so the build scripts are no longer
    // recent there and live only while something uses them
    s.advance(DAY);
    s.conf.regex = true;
    s.build();
    // the host keeps only what the target needs: no workspace unit uses serde any more there
    let gone: Vec<Key> = s
        .units
        .iter()
        .filter(|(k, w)| w.node.local || **k == serde)
        .map(|(k, _)| k.clone())
        .collect();
    for k in gone {
        for t in &s.units[&k].tops {
            let _ = fs::remove_dir_all(t).or_else(|_| fs::remove_file(t));
        }
    }
    s.advance(DAY);
    s.run(&cfg());
    assert!(s.present(&run) && s.present(&script));
}

/// Given `cargo test` (each lib and its test binary written in the same second), then util's
/// feature flipped and `cargo build`
/// When app's old lib is superseded
/// Then its incremental dir goes and the test binaries' stay: two dirs of one crate written at
/// once are told apart by the hard link to the lib's `.rmeta`, not by the clock. Four toolchains
/// give other hashes, so other dir names and another order on disk.
#[test]
fn incremental_dirs_follow_their_unit_not_the_clock() {
    for toolchain in 1..=4 {
        let mut s = Sim::new(&format!("inc-owner-{toolchain}"));
        s.conf.toolchain = toolchain;
        s.conf.mode = Mode::Test;
        s.build();
        let tests: Vec<Key> = s
            .used
            .iter()
            .filter(|k| s.units[*k].node.kind.starts_with("test-"))
            .cloned()
            .collect();
        let lib = s.key("app", "lib-");
        s.advance(3600);
        s.conf.mode = Mode::Build;
        s.conf.util_x = true;
        s.build();
        s.run(&cfg());
        assert!(!s.present(&lib) && !s.inc[&lib.0].0.exists(), "{toolchain}");
        assert_eq!(tests.len(), 2);
        for k in &tests {
            assert!(s.present(k) && s.inc[&k.0].0.exists(), "{toolchain}: {k:?}");
        }
    }
}

/// Given a target dir nothing ties to a workspace (shared, no `Cargo.lock` next to it, built
/// without the wrapper)
/// When `cargo check` and, a day later, `cargo build` run
/// Then the check units stay next to the build units: they are told apart by their outputs
/// (`rmeta` only, or code).
#[test]
fn check_and_build_keep_each_other_without_a_workspace() {
    let mut s = Sim::shared("anon");
    s.conf.mode = Mode::Check;
    s.build();
    let check = [s.key("app", "lib-"), s.key("tool", "bin-")];
    s.advance(DAY);
    s.conf.mode = Mode::Build;
    s.build();
    fs::remove_file(s.prof.join(STATE_DIR).join(UNITS_FILE)).unwrap();
    s.run(&cfg());
    assert!(check.iter().all(|k| s.present(k)));
    assert!(s.used.iter().all(|k| s.present(k)));
}

/// Given tool built, then serde bumped and tool rebuilt 5 s later, each compile taking a while:
/// an incremental dir's mtime is when its session ended (30 s and 40 s after it started)
/// When the old tool goes (outdated dependency)
/// Then its incremental dir goes and the new tool's stays: a dir is dated by when its session
/// started (in the session's name), the moment cargo stamped the unit.
#[test]
fn incremental_dirs_are_dated_by_their_session_start() {
    let mut s = Sim::new("inc-start");
    s.build();
    let old = s.key("tool", "bin-");
    s.conf.serde = 1;
    s.build();
    let new = s.key("tool", "bin-");
    for (k, took) in [(&old, 30), (&new, 40)] {
        let (dir, at) = s.inc[&k.0].clone();
        set_mtime(&dir, at + Duration::from_secs(took));
    }
    s.run(&cfg());
    assert!(!s.present(&old) && !s.inc[&old.0].0.exists());
    assert!(s.present(&new) && s.inc[&new.0].0.exists());
}
