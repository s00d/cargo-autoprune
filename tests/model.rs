//! Model test of the pruner over a simulated cargo (`support/sim.rs`): random developer sessions (builds in every mode, edits, dependency bumps and reverts,
//! feature unification flips, toolchain switches, the layout upgrade, removed targets, crashes,
//! waiting, prunes with and without a size cap) with the pruner's contract checked after every
//! prune, and at the end: after a month of only the current configuration, exactly its units are
//! left.

mod support;

use cargo_autoprune::layout::STATE_DIR;
use cargo_autoprune::prune::{Config, Report};
use proptest::prelude::*;
use proptest::test_runner::{Config as RunConfig, TestRunner};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use support::sim::{entries, Key, Mode, Sim, DAY};

fn cfg() -> Config {
    Config {
        lock_wait: Duration::ZERO,
        ..Config::default()
    }
}

#[derive(Clone, Debug)]
enum Act {
    Build,
    Edit(&'static str),
    Mode(Mode),
    Toolchain(u64),
    UpgradeLayout,
    Serde(u8),
    Regex(bool),
    UtilX(bool),
    Tool(bool),
    Crash,
    Wait(u64),
    Prune { cap: Option<u64>, ci: bool },
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        6 => Just(Act::Build),
        2 => prop::sample::select(vec!["util", "app", "tool"]).prop_map(Act::Edit),
        2 => prop::sample::select(vec![Mode::Build, Mode::Check, Mode::Clippy, Mode::Test]).prop_map(Act::Mode),
        1 => (1u64..5).prop_map(Act::Toolchain),
        1 => Just(Act::UpgradeLayout),
        1 => (0u8..3).prop_map(Act::Serde),
        1 => any::<bool>().prop_map(Act::Regex),
        2 => any::<bool>().prop_map(Act::UtilX),
        1 => any::<bool>().prop_map(Act::Tool),
        1 => Just(Act::Crash),
        3 => prop::sample::select(vec![5, 300, 700, 2 * 3600, 25 * 3600, 8 * DAY, 15 * DAY]).prop_map(Act::Wait),
        4 => (prop::option::weighted(0.2, prop::sample::select(vec![20_000u64, 60_000, 150_000])), prop::bool::weighted(0.15))
            .prop_map(|(cap, ci)| Act::Prune { cap, ci }),
    ]
}

/// Paths of a report as a set.
fn removed(r: &Report) -> BTreeSet<PathBuf> {
    r.removed.iter().cloned().collect()
}

/// One prune with its contract checked. `fresh`: nothing but waiting happened since the last build.
/// The pruner's own files (`.autoprune/`), contents included; the lock file it takes aside.
fn state(s: &Sim) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(p: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for e in entries(p) {
            let p = e.path();
            if p.is_dir() {
                out.push((p.clone(), Vec::new()));
                walk(&p, out);
            } else if e.file_name() != "lock" {
                out.push((p.clone(), fs::read(&p).unwrap_or_default()));
            }
        }
    }
    let mut out = Vec::new();
    walk(&s.prof.join(STATE_DIR), &mut out);
    out.sort();
    out
}

fn prune_and_check(s: &Sim, cfg: &Config, fresh: bool) -> Report {
    let before = s.cargo_bytes();
    let state_before = state(s);
    let had = s.present_keys();
    let had_inc: HashSet<PathBuf> = s
        .inc
        .values()
        .map(|(d, _)| d.clone())
        .filter(|d| s.exists(d))
        .collect();
    let uncapped = s.run(&Config {
        dry_run: true,
        max_size: None,
        ..cfg.clone()
    });
    let dry = s.run(&Config {
        dry_run: true,
        ..cfg.clone()
    });
    assert_eq!(s.cargo_bytes(), before, "a dry run deletes nothing");
    assert!(state(s) == state_before, "a dry run writes nothing");
    let r = s.run(cfg);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(
        !s.prof.join(STATE_DIR).join("trash").exists(),
        "the trash is emptied"
    );
    let names = |r: &Report| {
        let mut v: Vec<String> = r
            .removed
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    if removed(&dry) != removed(&r) {
        eprintln!(
            "DRY {:?} {:?}\nREAL {:?} {:?}\nUNCAPPED {:?}",
            dry.by_reason,
            names(&dry),
            r.by_reason,
            names(&r),
            names(&uncapped)
        );
    }
    assert_eq!(
        removed(&dry),
        removed(&r),
        "the dry run predicts the real one"
    );
    assert_eq!(
        (dry.freed_bytes, &dry.by_reason),
        (r.freed_bytes, &r.by_reason)
    );
    assert_eq!(
        before - s.cargo_bytes(),
        r.freed_bytes,
        "freed = what left the disk"
    );

    let half = s.half_units();
    assert!(half.is_empty(), "half-deleted units: {half:?}");
    for p in &s.leftovers {
        assert!(!s.exists(p), "crash debris left: {}", p.display());
    }
    for p in s.prepared.iter().chain(&s.foreign) {
        assert!(s.exists(p), "not ours, deleted: {}", p.display());
    }
    let hashes: HashSet<String> = s.present_keys().into_iter().map(|k| k.0).collect();
    assert!(s.tags().is_subset(&hashes), "tags of deleted units stay");
    assert!(
        s.recorded().is_subset(&hashes),
        "records of deleted units stay"
    );
    if cfg.drop_incremental {
        let left = entries(&s.prof.join("incremental"));
        assert!(
            left.iter()
                .all(|e| !e.path().is_dir() || s.is_foreign(&e.path())),
            "CI mode keeps no incremental dir"
        );
    }

    if let Some(cap) = cfg.max_size {
        assert!(
            s.cargo_bytes() <= cap,
            "{} over the cap {cap}",
            s.cargo_bytes()
        );
        // the cap evicts the oldest first
        let at = |p: &PathBuf| -> Option<SystemTime> {
            let unit = s.units.values().find(|w| w.tops.contains(p)).map(|w| w.at);
            unit.or_else(|| s.inc.values().find(|(d, _)| d == p).map(|(_, t)| *t))
        };
        let evicted: Vec<SystemTime> = removed(&r)
            .difference(&removed(&uncapped))
            .filter_map(at)
            .collect();
        let kept = s
            .units
            .values()
            .flat_map(|w| &w.tops)
            .chain(s.inc.values().map(|(d, _)| d))
            .filter(|p| s.exists(p))
            .filter_map(at);
        if let (Some(e), Some(k)) = (evicted.iter().max(), kept.min()) {
            assert!(e <= &k, "the cap evicted a newer unit");
        }
    } else {
        // registry units and build scripts a kept workspace unit depends on stay
        let mut todo: Vec<Key> = s
            .present_keys()
            .into_iter()
            .filter(|k| s.units[k].node.local && s.usable(k))
            .collect();
        let mut seen = HashSet::new();
        while let Some(k) = todo.pop() {
            for d in &s.units[&k].node.deps {
                let dk = (d.clone(), k.1);
                if !s.units[&dk].node.local && seen.insert(dk.clone()) {
                    let w = &s.units[&dk].node;
                    assert!(
                        s.present(&dk) || !had.contains(&dk),
                        "{} {} {} of kept {} {} {} deleted: {:?}",
                        w.kind,
                        w.pkg,
                        dk.0,
                        s.units[&k].node.kind,
                        s.units[&k].node.pkg,
                        k.0,
                        r.by_reason
                    );
                    if s.present(&dk) {
                        todo.push(dk);
                    }
                }
            }
        }
        if fresh {
            // what the last build wrote is the newest there is: it stays
            for k in &s.wrote {
                let w = &s.units[k];
                assert!(
                    s.intact(k) || !had.contains(k),
                    "fresh {} {} deleted",
                    w.node.kind,
                    w.node.pkg
                );
                if w.node.local && !cfg.drop_incremental && had_inc.contains(&s.inc[&k.0].0) {
                    assert!(
                        s.exists(&s.inc[&k.0].0),
                        "incremental dir of fresh {} gone",
                        w.node.pkg
                    );
                }
            }
        }
    }
    let again = s.run(cfg);
    if cfg.max_size.is_none() {
        assert_eq!(
            again.removed,
            Vec::<PathBuf>::new(),
            "a second prune finds nothing"
        );
    } else {
        // what the cap evicted can leave dependencies nobody uses: they go next time
        assert!(
            !again.by_reason.contains_key("size cap"),
            "still under the cap"
        );
    }
    r
}

/// A month of the current configuration only (edited, built in every mode used so far): what is
/// left is exactly what those builds use.
fn converge(s: &mut Sim) {
    s.advance(30 * DAY);
    for p in ["util", "app", "tool"] {
        s.edit(p);
    }
    let mode = s.conf.mode;
    let mut expected = BTreeSet::new();
    for m in s.modes.clone() {
        s.conf.mode = m;
        s.build();
        expected.extend(s.used.iter().cloned());
    }
    s.conf.mode = mode;
    prune_and_check(s, &cfg(), true);
    let left = s.present_keys();
    let describe = |ks: &BTreeSet<Key>| -> Vec<String> {
        ks.iter()
            .map(|k| {
                format!(
                    "{} {}-{} {}",
                    s.units[k].node.kind,
                    s.units[k].node.pkg,
                    k.0,
                    if k.1 { "new" } else { "legacy" }
                )
            })
            .collect()
    };
    assert_eq!(
        describe(&left),
        describe(&expected),
        "extra: {:?}, missing: {:?}",
        describe(&left.difference(&expected).cloned().collect()),
        describe(&expected.difference(&left).cloned().collect())
    );
    let want: BTreeSet<PathBuf> = expected
        .iter()
        .filter(|k| s.units[*k].node.local)
        .map(|k| s.inc[&k.0].0.clone())
        .collect();
    let inc: BTreeSet<PathBuf> = entries(&s.prof.join("incremental"))
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !s.is_foreign(p))
        .collect();
    assert_eq!(inc, want, "incremental dirs");
    let state = s.prof.join(STATE_DIR);
    assert!(!state.join("trash").exists());
}

fn session(acts: &[Act], tag: &str) {
    let mut s = Sim::new(tag);
    let mut fresh = false;
    for a in acts {
        match a {
            Act::Build => {
                s.build();
                fresh = true;
                continue;
            }
            Act::Wait(secs) => {
                s.advance(*secs);
                continue;
            }
            Act::Prune { cap, ci } => {
                s.advance(1);
                let c = Config {
                    max_size: *cap,
                    drop_incremental: *ci,
                    grace: if *ci { Duration::ZERO } else { cfg().grace },
                    ..cfg()
                };
                prune_and_check(&s, &c, fresh);
                continue;
            }
            Act::Edit(p) => s.edit(p),
            Act::Mode(m) => s.conf.mode = *m,
            Act::Toolchain(t) => s.conf.toolchain = *t,
            Act::UpgradeLayout => s.upgrade_layout(),
            Act::Serde(v) => s.conf.serde = *v,
            Act::Regex(b) => s.conf.regex = *b,
            Act::UtilX(b) => s.conf.util_x = *b,
            Act::Tool(b) => s.conf.tool = *b,
            Act::Crash => s.crash(),
        }
        fresh = false;
    }
    converge(&mut s);
}

/// Random sessions; a failure is shrunk to the shortest session that still fails and printed.
#[test]
fn model() {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(48);
    let mut runner = TestRunner::new(RunConfig {
        cases,
        failure_persistence: None,
        ..RunConfig::default()
    });
    let n = std::sync::atomic::AtomicU32::new(0);
    let r = runner.run(&prop::collection::vec(act(), 1..40), |acts| {
        let i = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        session(&acts, &format!("model{i}"));
        Ok(())
    });
    if let Err(e) = r {
        panic!("{e}");
    }
}
