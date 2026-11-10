//! Real cargo builds of a throwaway workspace through the wrapper; after every change the pruned
//! target must equal a from-scratch target of the same commands, and nothing may recompile.

mod support;

use cargo_autoprune::prune::Config;
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};
use support::*;

/// Every kind of unit: lib/bin/test/example/build script, check vs build, release.
const CMDS: &[&str] = &[
    "build --workspace --all-targets",
    "test --workspace --no-run",
    "check --workspace",
    "build --release -p app",
];

const A10: &str = "dep_a = \"=1.0.0\"";
const A11: &str = "dep_a = \"=1.1.0\"";

fn step(ws: &Ws, deps: &str, cmds: &[&str]) {
    ws.deps(deps);
    ws.run_all(cmds);
    ws.prune();
    ws.assert_clean(cmds);
}

#[test]
fn dependency_version_bump() {
    let ws = Ws::new("bump");
    step(&ws, A10, CMDS);
    step(&ws, A11, CMDS);
    step(&ws, A10, CMDS); // quick revert
    step(&ws, A11, CMDS); // and back again: Cargo.lock changed each time, so no pin
    step(&ws, A10, CMDS);
    assert_eq!(ws.pins(), 0);
}

#[test]
fn added_and_removed_dependencies() {
    let ws = Ws::new("addrm");
    step(&ws, A10, CMDS);
    step(&ws, &format!("{A10}\ndep_b = \"1\"\ndep_m = \"1\""), CMDS); // build script + proc-macro
    step(&ws, A10, CMDS); // both removed: their units, build dirs and build-script runs go
    assert!(ws.deps_files("debug", "libdep_b-").is_empty());
    assert!(ws.deps_files("debug", "libdep_m-").is_empty());
}

#[test]
fn transitive_dependency_bump_through_build_script_crate() {
    let ws = Ws::new("transitive");
    step(&ws, &format!("{A10}\ndep_b = \"1\""), CMDS);
    step(&ws, &format!("{A11}\ndep_b = \"1\""), CMDS); // dep_b is rebuilt against dep_a 1.1
}

#[test]
fn registry_feature_change() {
    let ws = Ws::new("feature");
    step(&ws, "dep_c = \"1\"", CMDS);
    step(
        &ws,
        "dep_c = { version = \"1\", features = [\"fast\"] }",
        CMDS,
    );
    step(&ws, "", CMDS); // dependency dropped
}

#[test]
fn workspace_source_edits_keep_cache() {
    let ws = Ws::new("edit");
    step(&ws, A10, CMDS);
    for i in 0..3 {
        ws.write(
            "app/src/lib.rs",
            &format!("pub fn app() -> u32 {{ util::u() + {i} }}\n"),
        );
        ws.write(
            "util/src/lib.rs",
            &format!("pub fn u() -> u32 {{ {i} + 1 }}\n"),
        );
        ws.run_all(CMDS);
        ws.prune();
        ws.assert_clean(CMDS);
    }
}

#[test]
fn feature_variants_of_workspace_member_coexist() {
    // `-p util --features extra` and the plain workspace build are two live variants of util.
    let cmds = &[
        "build --workspace --all-targets",
        "build -p util --features extra",
        "test -p util --features extra --no-run",
    ];
    let ws = Ws::new("wsfeat");
    ws.deps(A10);
    for _ in 0..3 {
        ws.run_all(cmds);
        ws.prune();
    }
    assert_eq!(ws.run_all(cmds), 0);
    ws.assert_clean(cmds);
    assert_eq!(ws.pins(), 0, "told apart by features: no thrash, no pin");
}

#[test]
fn feature_unification_alternation_is_pinned_not_thrashed() {
    // `tool` turns on util/extra; `-p app` alone builds app against plain util, `--workspace`
    // against util+extra: same app config, different deps. Only timing tells these apart.
    let ws = Ws::new("unify");
    ws.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"util\", \"tool\"]\nresolver = \"2\"\n",
    );
    ws.write(
        "tool/Cargo.toml",
        "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nutil = { path = \"../util\", features = [\"extra\"] }\n",
    );
    ws.write("tool/src/lib.rs", "pub fn t() -> u32 { util::u() }\n");
    ws.deps(A10);
    let a = "build -p app --all-targets";
    let w = "build --workspace --all-targets";
    let mut compiled = Vec::new();
    for _ in 0..3 {
        compiled.push(ws.cargo(a));
        ws.prune();
        compiled.push(ws.cargo(w));
        ws.prune();
    }
    assert_eq!(&compiled[4..], &[0, 0], "still thrashing: {compiled:?}");
    assert!(ws.pins() > 0);
}

#[test]
fn check_build_test_release_and_doc() {
    let cmds = &[
        "bench --workspace --no-run",
        "check --workspace --all-targets",
        "doc --workspace --no-deps",
        "build --examples",
        "test --workspace --no-run --release",
    ];
    let ws = Ws::new("kinds");
    step(&ws, &format!("{A10}\ndep_m = \"1\""), cmds);
    step(&ws, &format!("{A11}\ndep_m = \"1\""), cmds);
}

#[test]
fn clippy_and_check_coexist() {
    if !has_clippy() {
        eprintln!("cargo clippy not installed, skipped");
        return;
    }
    let cmds = &[
        "check --workspace --all-targets",
        "clippy --workspace --all-targets",
    ];
    let ws = Ws::new("clippy");
    step(&ws, &format!("{A10}\ndep_b = \"1\""), cmds);
    step(&ws, &format!("{A11}\ndep_b = \"1\""), cmds);
    assert_eq!(ws.pins(), 0, "clippy units are tagged, no pins expected");
}

#[test]
fn revert_an_hour_later_is_deduplicated() {
    let ws = Ws::new("revert");
    step(&ws, A10, CMDS);
    step(&ws, A11, CMDS);
    ws.age_deleted_log(3600);
    step(&ws, A10, CMDS);
    assert_eq!(ws.pins(), 0);
}

#[test]
fn orphaned_objects_and_dep_info_removed() {
    let ws = Ws::new("orphan");
    ws.deps(A10);
    ws.run_all(&CMDS[..2]);
    // what an rm of test executables leaves behind: .d, .o and the fingerprint
    let fp = ws.target.join("debug/.fingerprint");
    for e in fs::read_dir(&fp).unwrap() {
        let p = e.unwrap().path();
        if fs::read_dir(&p).unwrap().any(|f| {
            f.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("test-integration")
        }) {
            fs::remove_dir_all(p).unwrap();
        }
    }
    let before = ws.deps_files("debug", "it-").len();
    assert!(before > 0);
    ws.prune();
    assert!(
        ws.deps_files("debug", "it-").is_empty(),
        "{:?}",
        ws.deps_files("debug", "it-")
    );
    ws.run_all(&CMDS[..2]); // rebuilds the test, nothing breaks
}

#[test]
fn size_cap_evicts_oldest_but_builds_still_work() {
    let ws = Ws::new("cap");
    ws.deps(&format!("{A10}\ndep_b = \"1\"\ndep_m = \"1\""));
    ws.run_all(CMDS);
    let debug = ws.target.join("debug");
    // what the cap counts: unit areas (the profile root holds hard links to deps files)
    let dir_size = |d: &std::path::Path| -> u64 {
        ["deps", ".fingerprint", "build", "incremental", "examples"]
            .iter()
            .map(|a| dir_size(&d.join(a)))
            .sum()
    };
    let cap = dir_size(&debug) / 2;
    ws.prune_with(Config {
        grace: Duration::ZERO,
        max_size: Some(cap),
        lock_wait: Duration::from_secs(60),
        ..Config::default()
    });
    // the cap counts unit files; a few profile files belong to no unit (Linux: ~6%)
    assert!(
        dir_size(&debug) <= cap + cap / 10,
        "{} > {cap}",
        dir_size(&debug)
    );
    ws.run_all(CMDS);
}

#[test]
fn shared_target_without_lockfile_still_deduplicates() {
    let ws = Ws::new("shared");
    let shared = ws.root.join("shared-target");
    let run = |deps: &str| {
        ws.deps(deps);
        for c in CMDS {
            ws.cargo_in(c, &shared, true, &[]);
        }
    };
    run(A10);
    run(A11);
    for p in cargo_autoprune::layout::profile_dirs(&shared) {
        cargo_autoprune::prune::prune(
            &p,
            &Config {
                grace: Duration::ZERO,
                ..Config::default()
            },
        )
        .unwrap();
    }
    let deps: Vec<String> = fs::read_dir(shared.join("debug/deps"))
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("libdep_a-") && n.ends_with(".rlib"))
        .collect();
    assert_eq!(deps.len(), 1, "superseded dep_a rlib should go: {deps:?}");
    for c in CMDS {
        assert_eq!(ws.cargo_in(c, &shared, true, &[]), 0);
    }
}

#[test]
fn background_prune_waits_for_the_build_and_cleans_after_it() {
    let ws = Ws::new("background");
    ws.deps(A10);
    ws.run_all(&CMDS[..1]);
    // a slow build script keeps cargo busy while the background pruner is already spawned
    ws.write("app/build.rs", "fn main() { std::thread::sleep(std::time::Duration::from_secs(3)); println!(\"cargo:rerun-if-changed=build.rs\"); }\n");
    ws.deps(A11);
    let env = [
        ("CARGO_AUTOPRUNE", "1"),
        ("CARGO_AUTOPRUNE_INTERVAL", "0"),
        ("CARGO_AUTOPRUNE_GRACE", "0"),
    ];
    let t0 = Instant::now();
    ws.cargo_in(CMDS[0], &ws.target.clone(), true, &env);
    let built = t0.elapsed();
    let log = ws.target.join("debug/.autoprune/log");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !log.exists()
        || Instant::now() < deadline
            && ws
                .deps_files("debug", "libdep_a-")
                .iter()
                .filter(|f| f.ends_with(".rlib"))
                .count()
                > 1
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(log.exists(), "background prune never ran");
    assert!(built >= Duration::from_secs(3));
    let rlibs: Vec<_> = ws
        .deps_files("debug", "libdep_a-")
        .into_iter()
        .filter(|f| f.ends_with(".rlib"))
        .collect();
    assert_eq!(rlibs.len(), 1, "{rlibs:?}");
    assert_eq!(ws.cargo(CMDS[0]), 0);
}

#[test]
fn wrapper_is_transparent() {
    // version probe without --out-dir, chained wrapper, failing compile keeps its exit code
    let out = Command::new(BIN).args(["rustc", "-vV"]).output().unwrap();
    assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("rustc"));
    let out = Command::new(BIN)
        .args(["rustc", "--version"])
        .env("CARGO_AUTOPRUNE_WRAPPER", "env")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("rustc "));
    let out = Command::new(BIN)
        .args(["rustc", "--definitely-not-a-flag"])
        .output()
        .unwrap();
    assert!(!out.status.success());

    let ws = Ws::new("broken");
    ws.write("util/src/lib.rs", "pub fn u() -> u32 { not_rust }\n");
    let mut c = Command::new("cargo");
    c.current_dir(&ws.dir)
        .args(["build", "--offline", "-p", "util"])
        .env("RUSTC_WRAPPER", BIN)
        .env("CARGO_TARGET_DIR", &ws.target);
    let out = c.output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not_rust"));
}

#[test]
fn cli_dry_run_and_prune() {
    let ws = Ws::new("cli");
    ws.deps(A10);
    ws.run_all(&CMDS[..1]);
    ws.deps(A11);
    ws.run_all(&CMDS[..1]);
    let run = |args: &[&str]| {
        let out = Command::new(BIN)
            .args(args)
            .arg(&ws.dir)
            .env("CARGO_AUTOPRUNE_GRACE", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let before = inventory(&ws.target);
    let dry = run(&["autoprune", "--dry-run"]);
    assert!(dry.contains("would remove"), "{dry}");
    assert_eq!(before, inventory(&ws.target), "dry run changed files");
    let st = run(&["autoprune", "status"]);
    assert!(
        st.contains("reclaimable now") && st.contains("superseded"),
        "{st}"
    );
    let real = run(&["autoprune"]);
    assert!(real.contains("superseded"), "breakdown missing: {real}");
    assert!(real.contains("removed"), "{real}");
    assert_eq!(
        ws.deps_files("debug", "libdep_a-")
            .iter()
            .filter(|f| f.ends_with(".rlib"))
            .count(),
        1
    );
    assert_eq!(ws.cargo(CMDS[0]), 0);
}

#[test]
fn custom_profile_and_explicit_target_triple() {
    let ws = Ws::new("triple");
    ws.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"util\"]\nresolver = \"2\"\n[profile.ci]\ninherits = \"dev\"\nopt-level = 1\n",
    );
    let host = host_triple();
    let t = format!("build --workspace --target {host}");
    let cmds = [
        "build --workspace --profile ci",
        t.as_str(),
        "check --workspace",
    ];
    step(&ws, &format!("{A10}\ndep_b = \"1\""), &cmds);
    step(&ws, &format!("{A11}\ndep_b = \"1\""), &cmds);
    let profiles = cargo_autoprune::layout::profile_dirs(&ws.target);
    assert!(profiles.iter().any(|p| p.ends_with("ci")), "{profiles:?}");
    assert!(
        profiles
            .iter()
            .any(|p| p.ends_with(format!("{host}/debug"))),
        "{profiles:?}"
    );
}

#[test]
fn rustflags_change() {
    let ws = Ws::new("rustflags");
    let cmds = &CMDS[..2];
    ws.deps(A10);
    ws.run_all(cmds);
    ws.prune();
    // same commands with RUSTFLAGS: compare against an oracle built with the same flags
    let env = [("RUSTFLAGS", "-C debug-assertions=off")];
    for c in cmds {
        ws.cargo_in(c, &ws.target.clone(), true, &env);
    }
    ws.prune();
    let oracle = ws.root.join("oracle-flags");
    for c in cmds {
        ws.cargo_in(c, &oracle, false, &env);
    }
    let (a, b) = (inventory(&ws.target), inventory(&oracle));
    let lost: Vec<_> = b.difference(&a).collect();
    assert!(lost.is_empty(), "{lost:#?}");
    for c in cmds {
        assert_eq!(ws.cargo_in(c, &ws.target.clone(), true, &env), 0);
    }
    // the build without flags stays as a variant; a week later, with the flagged one still in
    // use, it expires
    age_tree(&ws.target, 8 * 24 * 3600);
    ws.write("util/src/lib.rs", "pub fn u() -> u32 { 5 }\n");
    for c in cmds {
        ws.cargo_in(c, &ws.target.clone(), true, &env);
    }
    ws.prune();
    let (a, b) = (
        inventory(&ws.target),
        inventory(&oracle_rebuild(&ws, cmds, &env)),
    );
    let left: Vec<_> = a.difference(&b).collect();
    let lost: Vec<_> = b.difference(&a).collect();
    assert!(lost.is_empty(), "{lost:#?}");
    assert!(
        left.is_empty(),
        "the stale no-flags variant should expire: {left:#?}"
    );
}

#[test]
fn removed_workspace_member_and_renamed_bin() {
    let ws = Ws::new("member");
    ws.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"util\", \"tool\"]\nresolver = \"2\"\n",
    );
    ws.write("tool/Cargo.toml", "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\ndep_c = \"1\"\n");
    ws.write("tool/src/main.rs", "fn main() { dep_c::c(); }\n");
    step(&ws, A10, CMDS);
    assert!(!ws.deps_files("debug", "tool-").is_empty());
    ws.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"util\"]\nresolver = \"2\"\n",
    );
    ws.write("app/src/main.rs", "fn main() {}\n");
    fs::rename(
        ws.dir.join("app/src/main.rs"),
        ws.dir.join("app/src/bin_renamed.rs"),
    )
    .unwrap();
    fs::create_dir_all(ws.dir.join("app/src/bin")).unwrap();
    fs::rename(
        ws.dir.join("app/src/bin_renamed.rs"),
        ws.dir.join("app/src/bin/renamed.rs"),
    )
    .unwrap();
    ws.run_all(CMDS);
    ws.prune();
    let (left, lost) = ws.oracle_diff(CMDS);
    assert!(lost.is_empty(), "{lost:#?}");
    // uplifted outputs of the gone binaries in the profile root are a documented limitation
    let left: Vec<_> = left
        .into_iter()
        .filter(|f| {
            !f.starts_with("debug/app")
                && !f.starts_with("debug/tool")
                && !f.starts_with("release/app")
        })
        .collect();
    assert!(left.is_empty(), "{left:#?}");
    assert!(ws.deps_files("debug", "tool-").is_empty());
    assert!(ws.deps_files("debug", "libdep_c-").is_empty());
}

#[test]
fn toolchain_switch_and_retirement() {
    let Some(old) = other_toolchain_rustc() else {
        eprintln!("no second toolchain installed, skipped");
        return;
    };
    let ws = Ws::new("toolchain");
    let cmds = &CMDS[..2];
    let with_old = |ws: &Ws| {
        let target = ws.target.clone();
        cmds.iter()
            .map(|c| ws.cargo_in(c, &target, true, &[("RUSTC", old.as_str())]))
            .sum::<usize>()
    };
    ws.deps(A10);
    with_old(&ws);
    ws.run_all(cmds);
    ws.prune();
    // both toolchains stay usable side by side
    assert_eq!(with_old(&ws), 0);
    assert_eq!(ws.run_all(cmds), 0);
    // three weeks without the old toolchain: its units are retired, the current ones stay
    age_tree(&ws.target, 21 * 24 * 3600);
    ws.write("util/src/lib.rs", "pub fn u() -> u32 { 3 }\n");
    ws.run_all(cmds);
    ws.prune();
    ws.assert_clean(cmds);
}

fn other_toolchain_rustc() -> Option<String> {
    let current = String::from_utf8(Command::new("rustc").arg("-V").output().ok()?.stdout).ok()?;
    let list = String::from_utf8(
        Command::new("rustup")
            .args(["toolchain", "list"])
            .output()
            .ok()?
            .stdout,
    )
    .ok()?;
    for tc in list.lines().filter_map(|l| l.split_whitespace().next()) {
        let host = host_triple();
        if !tc.ends_with(&host) || tc.starts_with("nightly") {
            continue;
        }
        let out = Command::new("rustup")
            .args(["which", "rustc", "--toolchain", tc])
            .output()
            .ok()?;
        let rustc = String::from_utf8(out.stdout).ok()?.trim().to_owned();
        let v = Command::new(&rustc).arg("-V").output().ok()?;
        if v.status.success() && String::from_utf8_lossy(&v.stdout) != current {
            return Some(rustc);
        }
    }
    None
}

#[test]
fn separate_build_dir() {
    // `build.build-dir`: intermediates live outside target/, profile dirs have only .cargo-build-lock
    let ws = Ws::new("builddir");
    let bd = ws.root.join("build-dir");
    let bd_s = bd.display().to_string();
    let env = [("CARGO_BUILD_BUILD_DIR", bd_s.as_str())];
    let run = |deps: &str| -> usize {
        ws.deps(deps);
        CMDS.iter()
            .map(|c| ws.cargo_in(c, &ws.target.clone(), true, &env))
            .sum()
    };
    run(A10);
    run(A11);
    let profiles = cargo_autoprune::layout::profile_dirs(&bd);
    assert!(
        profiles.iter().any(|p| p.ends_with("debug")),
        "{profiles:?}"
    );
    for p in &profiles {
        cargo_autoprune::prune::prune(
            p,
            &Config {
                grace: Duration::ZERO,
                ..Config::default()
            },
        )
        .unwrap();
    }
    let rlibs: Vec<_> = fs::read_dir(bd.join("debug/deps"))
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("libdep_a-") && n.ends_with(".rlib"))
        .collect();
    assert_eq!(rlibs.len(), 1, "{rlibs:?}");
    assert!(
        bd.join("debug/.autoprune/units").exists(),
        "wrapper should record workspace units"
    );
    for c in CMDS {
        assert_eq!(ws.cargo_in(c, &ws.target.clone(), true, &env), 0);
    }
    // the background prune finds the build-dir profile through the wrapper too
    ws.deps(A10);
    let bg = [
        env[0],
        ("CARGO_AUTOPRUNE", "1"),
        ("CARGO_AUTOPRUNE_INTERVAL", "0"),
    ];
    ws.cargo_in(CMDS[0], &ws.target.clone(), true, &bg);
    assert!(bd.join("debug/.autoprune/stamp").exists());
}

#[test]
fn two_workspaces_share_one_target_dir() {
    // same package names (app, util) in both: they must not evict each other
    let a = Ws::new("share-a");
    let b = Ws::new("share-b");
    let shared = a.root.join("shared-target");
    let all = |ws: &Ws| -> usize {
        CMDS.iter()
            .map(|c| ws.cargo_in(c, &shared, true, &[]))
            .sum()
    };
    let prune_all = || {
        for p in cargo_autoprune::layout::profile_dirs(&shared) {
            cargo_autoprune::prune::prune(
                &p,
                &Config {
                    grace: Duration::ZERO,
                    ..Config::default()
                },
            )
            .unwrap();
        }
    };
    a.deps(&format!("{A10}\ndep_b = \"1\""));
    b.deps(A11);
    all(&a);
    all(&b);
    prune_all();
    assert_eq!(all(&a), 0, "workspace a lost units");
    assert_eq!(all(&b), 0, "workspace b lost units");
    a.deps(A10); // dep_b removed from a: no Cargo.lock next to the shared target, roots recorded
    all(&a);
    prune_all();
    let left: Vec<String> = fs::read_dir(shared.join("debug/deps"))
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("libdep_b-"))
        .collect();
    assert!(left.is_empty(), "removed dependency kept: {left:?}");
    assert_eq!(all(&a), 0);
    assert_eq!(all(&b), 0);
}

fn oracle_rebuild(ws: &Ws, cmds: &[&str], env: &[(&str, &str)]) -> std::path::PathBuf {
    let oracle = ws.root.join("oracle-again");
    let _ = fs::remove_dir_all(&oracle);
    for c in cmds {
        ws.cargo_in(c, &oracle, false, env);
    }
    oracle
}

fn nightly_new_layout() -> Option<[(&'static str, &'static str); 2]> {
    let ok = Command::new("rustup")
        .args(["run", "nightly", "cargo", "-V"])
        .output()
        .is_ok_and(|o| o.status.success());
    ok.then_some([
        ("RUSTUP_TOOLCHAIN", "nightly"),
        ("CARGO_UNSTABLE_BUILD_DIR_NEW_LAYOUT", "true"),
    ])
}

#[test]
fn new_build_dir_layout() {
    // cargo 1.100's layout: build/<pkg>/<hash>/{fingerprint,out,run}
    let Some(env) = nightly_new_layout() else {
        eprintln!("no nightly toolchain, skipped");
        return;
    };
    let ws = Ws::new("newlayout");
    let cmds = &CMDS[..3];
    let run = |deps: &str| {
        ws.deps(deps);
        for c in cmds {
            ws.cargo_in(c, &ws.target.clone(), true, &env);
        }
    };
    run(&format!("{A10}\ndep_b = \"1\""));
    assert!(
        ws.target.join("debug/build/dep_a").is_dir(),
        "not the new layout"
    );
    run(&format!("{A11}\ndep_b = \"1\""));
    let reports = ws.prune();
    assert!(reports.iter().map(|r| r.removed_units).sum::<usize>() > 0);
    let oracle = ws.root.join("oracle-new");
    for c in cmds {
        ws.cargo_in(c, &oracle, false, &env);
    }
    let (a, b) = (inventory(&ws.target), inventory(&oracle));
    let left: Vec<_> = a.difference(&b).collect();
    let lost: Vec<_> = b.difference(&a).collect();
    assert!(lost.is_empty(), "{lost:#?}");
    assert!(left.is_empty(), "{left:#?}");
    for c in cmds {
        assert_eq!(ws.cargo_in(c, &ws.target.clone(), true, &env), 0);
    }
}

#[test]
fn upgrade_from_the_legacy_layout() {
    // a target built by an older cargo, then by one with the new layout: the old units are dead
    let Some(env) = nightly_new_layout() else {
        eprintln!("no nightly toolchain, skipped");
        return;
    };
    let ws = Ws::new("upgrade");
    ws.deps(A10);
    ws.cargo(CMDS[0]); // the current cargo: legacy layout
    let legacy = ws.deps_files("debug", "libdep_a-").len();
    assert!(legacy > 0);
    age_tree(&ws.target, 2 * 24 * 3600); // two days later the toolchain is upgraded
    ws.cargo_in(CMDS[0], &ws.target.clone(), true, &env);
    ws.prune();
    assert!(
        ws.deps_files("debug", "libdep_a-").is_empty(),
        "legacy dep_a copies left"
    );
    assert!(
        !fs::read_dir(ws.target.join("debug/.fingerprint"))
            .map(|d| d.count() > 0)
            .unwrap_or(false),
        "legacy fingerprints left"
    );
    assert_eq!(ws.cargo_in(CMDS[0], &ws.target.clone(), true, &env), 0);
}

fn host_triple() -> String {
    let out = Command::new("rustc").arg("-vV").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .unwrap()
        .to_owned()
}

#[test]
fn cli_accepts_a_directory_without_subcommand() {
    // `cargo-autoprune DIR` must not be taken for a wrapper call (cargo passes rustc first)
    let ws = Ws::new("cli-dir");
    ws.deps(A10);
    ws.run_all(&CMDS[..1]);
    let out = Command::new(BIN)
        .arg(&ws.dir)
        .arg("--dry-run")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("would remove"));
    let out = Command::new(BIN)
        .args(["status"])
        .arg(&ws.dir)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("units"));
}

#[test]
fn background_failure_leaves_a_notice() {
    let ws = Ws::new("notice");
    ws.deps(A10);
    ws.run_all(&CMDS[..1]);
    // a profile dir the background pruner refuses (no CACHEDIR.TAG above it)
    let bogus = ws.root.join("bogus/debug");
    fs::create_dir_all(&bogus).unwrap();
    fs::write(bogus.join(".cargo-lock"), "").unwrap();
    let out = Command::new(BIN)
        .arg("--background")
        .arg(&bogus)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        !bogus.join(".autoprune").exists(),
        "wrote state into a non-cargo dir"
    );
    // nothing to report there (no state dir can be trusted), but a real profile reports errors:
    ws.deps(A11);
    ws.run_all(&CMDS[..1]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let debug = ws.target.join("debug");
        let deps = debug.join("deps");
        fs::set_permissions(&deps, fs::Permissions::from_mode(0o555)).unwrap(); // unlink fails
        let out = Command::new(BIN)
            .arg("--background")
            .arg(&debug)
            .env("CARGO_AUTOPRUNE_GRACE", "0")
            .output()
            .unwrap();
        fs::set_permissions(&deps, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(out.status.success());
        let notice = fs::read_to_string(debug.join(".autoprune/notice")).unwrap_or_default();
        assert!(notice.contains("could not delete"), "{notice:?}");
    }
}

#[test]
fn server_mode_recursive_and_remove_idle() {
    let a = Ws::new("srv-a");
    let b = Ws::new("srv-b");
    for ws in [&a, &b] {
        ws.deps(A10);
        ws.run_all(&CMDS[..1]);
        ws.deps(A11);
        ws.run_all(&CMDS[..1]);
    }
    age_tree(&b.target, 40 * 24 * 3600); // nothing compiled into b for 40 days
    let run = |extra: &[&str]| {
        let out = Command::new(BIN)
            .args(["autoprune", "-r", "--remove-idle", "30", "--grace", "0"])
            .args(extra)
            .arg(&a.root)
            .arg(&b.root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let dry = run(&["--dry-run"]);
    assert!(
        dry.contains("would remove the whole dir") && b.target.exists(),
        "{dry}"
    );
    let real = run(&[]);
    assert!(!b.target.exists(), "idle target should be gone: {real}");
    assert!(a.target.exists());
    assert!(
        real.contains("superseded"),
        "the active target is pruned as usual: {real}"
    );
    assert_eq!(a.cargo(CMDS[0]), 0);
}

#[test]
fn ci_mode_drops_incremental_and_keeps_the_build() {
    let ws = Ws::new("ci");
    ws.deps(A10);
    ws.run_all(&CMDS[..2]);
    ws.deps(A11);
    ws.run_all(&CMDS[..2]);
    let out = Command::new(BIN)
        .args(["autoprune", "--ci"])
        .arg(&ws.dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let inc = fs::read_dir(ws.target.join("debug/incremental"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(inc, 0, "incremental dirs left");
    assert_eq!(
        ws.run_all(&CMDS[..2]),
        0,
        "dropping incremental must not force a rebuild"
    );
}

fn dir_size(p: &std::path::Path) -> u64 {
    let Ok(m) = fs::symlink_metadata(p) else {
        return 0;
    };
    if !m.is_dir() {
        return m.len();
    }
    fs::read_dir(p)
        .unwrap()
        .map(|e| dir_size(&e.unwrap().path()))
        .sum()
}
