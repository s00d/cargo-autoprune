//! Stories of `cargo autoprune` as a user runs it: the binary on simulated targets.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use support::sim::Sim;
use support::{now_secs, BIN};

struct Out {
    code: i32,
    out: String,
    err: String,
}

/// Run the binary with a clean environment: an empty home (no user config), no autoprune or
/// wrapper variables.
fn bin(s: &Sim, args: &[&str], env: &[(&str, &str)], cwd: &Path) -> Out {
    let home = s.root.join("home");
    fs::create_dir_all(home.join(".cargo")).unwrap();
    let mut c = Command::new(BIN);
    c.args(args).current_dir(cwd);
    for (k, _) in std::env::vars() {
        if k.starts_with("CARGO_AUTOPRUNE") || k == "RUSTC_WRAPPER" || k == "CARGO_TARGET_DIR" {
            c.env_remove(k);
        }
    }
    c.env("HOME", &home).env("CARGO_HOME", home.join(".cargo"));
    c.envs(env.iter().copied());
    let o = c.output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn p(s: &Path) -> &str {
    s.to_str().unwrap()
}

/// A target with garbage: app rebuilt with another hash after an hour.
fn superseded(tag: &str) -> Sim {
    let mut s = Sim::new(tag);
    s.build();
    s.advance(3600);
    s.conf.util_x = true;
    s.build();
    s
}

/// Given a user who mistypes
/// Then every bad option fails with exit code 2 and says what is wrong; help and version work.
#[test]
fn bad_options_are_refused() {
    let s = Sim::new("cli-opts");
    let r = &s.root;
    let help = bin(&s, &["--help"], &[], r);
    assert!(help.code == 0 && help.out.contains("rustc-wrapper = \"cargo-autoprune\""));
    assert_eq!(bin(&s, &["-n", "-h"], &[], r).out, help.out);
    let v = bin(&s, &["-V"], &[], r);
    assert_eq!(
        v.out,
        format!("cargo-autoprune {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(bin(&s, &["--version"], &[], r).out, v.out);
    for (args, msg) in [
        (&["--bogus"][..], "unknown option --bogus"),
        (&["--max-size", "20"], "below 1 MiB"),
        (&["--max-size", "lots"], "cannot parse size"),
        (&["--grace", "x"], "--grace takes seconds"),
        (&["--remove-idle"], "--remove-idle takes days"),
        (
            &["-n", "/nonexistent-autoprune-dir"],
            "no Cargo profile dirs under",
        ),
    ] {
        let o = bin(&s, args, &[], r);
        assert_eq!(o.code, 2, "{args:?}");
        assert!(
            o.err.starts_with("cargo-autoprune: ") && o.err.contains(msg),
            "{args:?}: {}",
            o.err
        );
    }
}

/// Given a target with superseded units
/// When the user asks for a dry run, then runs it for real
/// Then the dry run lists the paths and deletes nothing, the real run deletes what the dry run
/// listed and reports units, size and reasons; verbose adds the paths and phase timings.
#[test]
fn dry_run_then_prune() {
    let s = superseded("cli-dry");
    let dir = p(&s.root);
    let dry = bin(&s, &["-n", dir], &[], &s.root);
    assert_eq!(dry.code, 0, "{}", dry.err);
    let listed: Vec<&str> = dry
        .out
        .lines()
        .filter_map(|l| l.strip_prefix("  /"))
        .collect();
    assert!(!listed.is_empty() && listed.iter().all(|l| Path::new(&format!("/{l}")).exists()));
    assert!(
        dry.out
            .contains(": would remove 2 units, 2 incremental dirs, "),
        "{}",
        dry.out
    );
    let real = bin(&s, &["--verbose", dir], &[], &s.root);
    assert!(
        real.out.contains(": removed 2 units, 2 incremental dirs, "),
        "{}",
        real.out
    );
    assert!(
        real.out.contains("  stale incremental 2, superseded 2\n"),
        "{}",
        real.out
    );
    assert!(
        real.out.contains(" ms  plan\n"),
        "phase timings: {}",
        real.out
    );
    assert!(listed.iter().all(|l| !Path::new(&format!("/{l}")).exists()));
    assert!(real.out.contains("\ntotal: ") && !real.out.ends_with("total: 0B\n"));
    let again = bin(&s, &[dir], &[("CARGO_AUTOPRUNE_TRACE", "1")], &s.root);
    assert!(
        again.out.contains(" ms  plan\n") && again.out.ends_with("total: 0B\n"),
        "{}",
        again.out
    );
    // the log line `status` shows
    let st = bin(&s, &["status", dir], &[], &s.root).out;
    assert!(
        st.contains(" ago, units=0 incremental=0 freed=0B pinned=0 took="),
        "{st}"
    );
    let log = fs::read_to_string(s.prof.join(".autoprune/log")).unwrap();
    let first = log.lines().next().unwrap();
    assert!(
        first.contains(" units=2 incremental=2 freed=")
            && first.contains(" errors=0 [stale incremental 2, superseded 2]"),
        "{first}"
    );
}

/// Given files the user may not delete in the target
/// Then a manual run deletes the rest and warns about at most five failures.
#[cfg(unix)]
#[test]
fn undeletable_files_are_reported() {
    use std::os::unix::fs::PermissionsExt;
    let mut s = Sim::new("cli-perm");
    s.build();
    for i in 0..3 {
        s.advance(3600);
        s.conf.serde = i + 1; // every version leaves 6 outdated units in deps/
        s.build();
    }
    let deps = s.prof.join("deps");
    fs::set_permissions(&deps, fs::Permissions::from_mode(0o555)).unwrap();
    let o = bin(&s, &[p(&s.root)], &[], &s.root);
    fs::set_permissions(&deps, fs::Permissions::from_mode(0o755)).unwrap();
    let warnings = o
        .err
        .lines()
        .filter(|l| l.starts_with("  warning: could not delete "))
        .count();
    assert_eq!(warnings, 5, "{}", o.err);
    assert!(o.out.contains(": removed 18 units"), "{}", o.out);
}

/// Given the target found from the profile dir, the target dir, the project dir, the current dir
/// or CARGO_TARGET_DIR
/// Then each prunes the same profile.
#[test]
fn every_way_to_name_the_target() {
    let s = superseded("cli-dirs");
    let want = "debug: would remove 2 units";
    let r = &s.root;
    for (args, env, cwd) in [
        (vec!["-n", p(&s.prof)], vec![], r.clone()),
        (vec!["-n", p(&s.root.join("target"))], vec![], r.clone()),
        (vec!["-n", p(&s.root)], vec![], r.clone()),
        (vec!["autoprune", "-n"], vec![], r.clone()),
        (
            vec!["-n"],
            vec![("CARGO_TARGET_DIR", p(&s.root.join("target")))],
            PathBuf::from("/"),
        ),
    ] {
        let o = bin(&s, &args, &env, &cwd);
        assert!(o.out.contains(want), "{args:?}: {} {}", o.out, o.err);
    }
    let both = bin(&s, &["-n", p(r), p(&s.prof)], &[], r);
    assert_eq!(both.out.matches(want).count(), 2);
}

/// Given grace and cap settings in the environment (`[env]` of .cargo/config.toml)
/// Then a manual run uses them; settings that don't parse fall back to the defaults.
#[test]
fn settings_from_the_environment() {
    let mut s = Sim::new("cli-env");
    s.build();
    s.grow(&s.key("app", "lib-"), 3 << 19); // the target is over 1 MiB
    s.advance(300);
    s.conf.util_x = true;
    s.build(); // 5 min apart: within the default grace
    let dir = p(&s.root);
    let r = &s.root;
    assert!(bin(&s, &["-n", dir], &[], r)
        .out
        .contains("would remove 0 units"));
    let junk = bin(
        &s,
        &["-n", dir],
        &[
            ("CARGO_AUTOPRUNE_GRACE", "x"),
            ("CARGO_AUTOPRUNE_MAX_SIZE", "20"),
        ],
        r,
    );
    assert!(junk.out.contains("would remove 0 units"), "{}", junk.out);
    let grace = bin(&s, &["-n", dir], &[("CARGO_AUTOPRUNE_GRACE", " 60 ")], r);
    assert!(grace.out.contains("would remove 2 units"), "{}", grace.out);
    let cap = bin(&s, &["-n", dir], &[("CARGO_AUTOPRUNE_MAX_SIZE", "1M")], r);
    assert!(cap.out.contains("size cap"), "{}", cap.out);
}

/// Given a project the user checks on
/// Then `status` tells whether the wrapper is set up, warns about settings that don't parse,
/// shows size, what a prune would free (without deleting), the last prune, pins and notices.
#[test]
fn status_reports_the_setup_and_the_target() {
    let mut s = Sim::new("cli-status");
    s.build();
    // the copy about to be superseded weighs 1.5 MiB
    s.grow(&s.key("app", "lib-"), 3 << 19);
    s.advance(3600);
    s.conf.util_x = true;
    s.build();
    let r = &s.root;
    let st = |env: &[(&str, &str)], cwd: &Path| bin(&s, &["status", p(&s.root)], env, cwd).out;
    // a cwd with no `.cargo/config.toml` above it: the user's own config stays out
    let away = Path::new("/");
    let o = st(&[], away);
    assert!(o.starts_with("wrapper: not configured"), "{o}");
    assert!(
        o.contains(&format!("{}: ", s.prof.display())) && o.contains(" units\n"),
        "{o}"
    );
    assert!(
        o.contains(
            "  reclaimable now: 1.5MiB (2 units, 2 incremental dirs: stale incremental 2, superseded 2)"
        ),
        "{o}"
    );
    assert!(
        o.contains("  last prune: never") && !o.contains("pinned"),
        "{o}"
    );
    assert!(
        bin(&s, &["-n", p(r)], &[], r)
            .out
            .contains("would remove 2 units"),
        "status deleted nothing"
    );

    let o = st(
        &[
            ("RUSTC_WRAPPER", "/bin/cargo-autoprune"),
            ("CARGO_AUTOPRUNE_INTERVAL", "5m"),
            ("CARGO_AUTOPRUNE_MAX_SIZE", "20"),
        ],
        away,
    );
    assert!(
        o.starts_with("wrapper: RUSTC_WRAPPER=/bin/cargo-autoprune\n"),
        "{o}"
    );
    assert!(
        o.contains("warning: CARGO_AUTOPRUNE_INTERVAL=\"5m\" is not a number of seconds"),
        "{o}"
    );
    assert!(
        o.contains("warning: CARGO_AUTOPRUNE_MAX_SIZE ignored: size cap \"20\" is below 1 MiB"),
        "{o}"
    );
    let ok = [
        ("CARGO_AUTOPRUNE_INTERVAL", " 60 "),
        ("CARGO_AUTOPRUNE_GRACE", "0"),
        ("CARGO_AUTOPRUNE_MAX_SIZE", "1M"),
    ];
    assert!(!st(&ok, away).contains("warning:"), "valid settings");
    // every way to write a cap above what is left
    for cap in ["1M", "2048k", "20G", "1T", "1.5GiB"] {
        let capped = bin(&s, &["-n", "--max-size", cap, p(r)], &[], r);
        assert!(
            capped.code == 0 && capped.out.contains("would remove 2 units"),
            "{cap}: {}",
            capped.err
        );
    }

    let home_cfg = s.root.join("home/.cargo/config.toml");
    for (text, on) in [
        ("[build]\nrustc-wrapper = \"cargo-autoprune\"\n", true),
        ("  # rustc-wrapper = \"cargo-autoprune\"\n", false),
        ("rustc-wrapper = \"sccache\"\n", false),
        ("x = \"cargo-autoprune\"\n", false),
    ] {
        fs::write(&home_cfg, text).unwrap();
        let o = st(&[("RUSTC_WRAPPER", "sccache")], away);
        assert_eq!(
            o.starts_with(&format!("wrapper: enabled in {}", home_cfg.display())),
            on,
            "{text}: {o}"
        );
    }
    fs::remove_file(&home_cfg).unwrap();
    let proj = s.root.join("proj");
    fs::create_dir_all(proj.join(".cargo")).unwrap();
    fs::create_dir_all(proj.join("sub")).unwrap();
    fs::write(
        proj.join(".cargo/config.toml"),
        "rustc-wrapper = \"cargo-autoprune\"",
    )
    .unwrap();
    let o = st(&[], &proj.join("sub"));
    assert!(
        o.starts_with(&format!(
            "wrapper: enabled in {}",
            proj.join(".cargo/config.toml")
                .canonicalize()
                .unwrap()
                .display()
        )),
        "{o}"
    );

    let state = s.prof.join(".autoprune");
    fs::write(
        state.join("log"),
        format!("1 old\n{} units=3 [orphan 3]\n", now_secs() - 300),
    )
    .unwrap();
    fs::write(state.join("pinned"), "app|lib|x\t1\ntool|bin|y\t2\n").unwrap();
    fs::write(state.join("notice"), "freed 3.0GiB in somewhere\n").unwrap();
    let o = st(&[], away);
    assert!(
        o.contains("  last prune: 5 min ago, units=3 [orphan 3]\n"),
        "{o}"
    );
    assert!(
        o.contains("  pinned groups (alternating variants, 24 h): app, tool\n"),
        "{o}"
    );
    assert!(o.contains("  notice: freed 3.0GiB in somewhere\n"), "{o}");
    for (ago, text) in [
        (119, "119 s"),
        (120, "2 min"),
        (7199, "119 min"),
        (7200, "2 h"),
        (172_799, "47 h"),
        (172_800, "2 days"),
    ] {
        // on a fixed clock: a second passing mid-check would move the boundary
        let now = SystemTime::now();
        let secs = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        fs::write(state.join("log"), format!("{} x\n", secs - ago)).unwrap();
        let mut o = Vec::new();
        cargo_autoprune::cli::status_at(Some(s.root.clone()), now, &mut o).unwrap();
        let o = String::from_utf8(o).unwrap();
        assert!(
            o.contains(&format!("  last prune: {text} ago, x\n")),
            "{ago}: {o}"
        );
    }
}

/// Given a build running in the project
/// Then `status` does not wait for it: reclaimable space is unknown.
#[test]
fn status_does_not_wait_for_a_build() {
    let s = superseded("cli-busy");
    let f = fs::File::options()
        .read(true)
        .write(true)
        .open(s.prof.join(".cargo-lock"))
        .unwrap();
    f.lock().unwrap();
    let o = bin(&s, &["status", p(&s.root)], &[], &s.root);
    assert!(
        o.out
            .contains("reclaimable now: unknown, a build or a prune is running"),
        "{}",
        o.out
    );
    // another prune at work: a manual run skips the profile at once
    let other = fs::File::create(s.prof.join(".autoprune/lock")).unwrap();
    other.lock().unwrap();
    drop(f);
    let o = bin(&s, &[p(&s.root)], &[], &s.root);
    assert!(
        o.out
            .contains(": busy (cargo or another prune holds the lock), skipped"),
        "{}",
        o.out
    );
}

/// Given a variant that alternates (deleted and rebuilt twice under the same Cargo.lock)
/// Then the manual run says which groups it pinned.
#[test]
fn manual_run_reports_pins() {
    let mut s = Sim::new("cli-pins");
    let dir = s.root.display().to_string();
    let run = |s: &mut Sim, x: bool| {
        s.conf.util_x = x;
        s.advance(60);
        s.build();
        bin(s, &["--grace", "1", &dir], &[], &s.root.clone()).out
    };
    run(&mut s, false);
    run(&mut s, true);
    run(&mut s, false); // comeback 1
    let o = run(&mut s, true); // comeback 2
    assert!(o.contains("  pinned (alternating variants): "), "{o}");
    assert!(o.contains("app") && o.contains("tool"), "{o}");
}

/// Given a CI job about to save its cache
/// Then `--ci` keeps the build but drops every incremental dir and every superseded unit.
#[test]
fn ci_mode_trims_for_the_cache() {
    let mut s = Sim::new("cli-ci");
    s.build();
    s.conf.util_x = true;
    s.build(); // 5 s later
    let o = bin(&s, &["--ci", p(&s.root)], &[], &s.root);
    assert!(
        o.out.contains(": removed 2 units, 6 incremental dirs"),
        "{}",
        o.out
    );
    assert!(s.used.iter().all(|k| s.intact(k)));
    let o = bin(&s, &["--drop-incremental", p(&s.root)], &[], &s.root);
    assert!(
        o.out.contains(": removed 0 units, 0 incremental dirs"),
        "{}",
        o.out
    );
}

/// Given a server with many projects, some untouched for weeks
/// When the admin runs `-r --remove-idle DAYS` over them
/// Then every target dir is found (not inside hidden dirs or node_modules), idle ones are
/// removed whole (kept in a dry run, kept while a cargo holds them), the others pruned.
#[test]
fn server_mode_finds_targets_and_removes_idle_ones() {
    let root = support::scratch("srv");
    let work = root.join("projects");
    let busy = Sim::at(work.join("busy"));
    let mut idle = Sim::at(work.join("idle"));
    let mut hidden = Sim::at(root.join(".cache/x"));
    let mut npm = Sim::at(root.join("web/node_modules/x"));
    for s in [&mut idle, &mut hidden, &mut npm] {
        s.build(); // 400 days ago
    }
    let mut recent = Sim::at(work.join("recent"));
    recent.advance(390 * 24 * 3600);
    recent.build(); // 10 days ago
    let mut busy = busy;
    busy.build();
    busy.advance(3600);
    busy.conf.util_x = true;
    busy.build();
    let busy_prof = busy.prof.clone();
    // the wrapper renewed its stamp today
    fs::write(busy_prof.join(".autoprune/stamp"), "").unwrap();
    let work_idle = work.join("idle/target");
    let r = |args: &[&str]| bin(&idle, args, &[], &root);
    let dry = r(&["-r", "--remove-idle", "30", "-n", p(&root)]);
    assert!(
        dry.out.contains(&format!(
            "{}: idle for 30+ days, would remove the whole dir",
            work_idle.display()
        )),
        "{}",
        dry.out
    );
    assert!(
        dry.out
            .contains(&format!("{}: would remove 2 units", busy_prof.display())),
        "{}",
        dry.out
    );
    assert!(!dry.out.contains(".cache") && !dry.out.contains("node_modules"));
    assert!(work_idle.exists());
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .open(idle.prof.join(".cargo-lock"))
        .unwrap();
    lock.lock().unwrap();
    let held = std::thread::scope(|sc| {
        let run = sc.spawn(|| r(&["-r", "--remove-idle", "30", p(&root)]));
        std::thread::sleep(std::time::Duration::from_secs(2)); // the build ends; its prune follows
        drop(lock);
        run.join().unwrap()
    });
    assert!(
        held.out
            .contains("idle but locked by a running cargo, kept"),
        "{}",
        held.out
    );
    let real = r(&["-r", "--remove-idle", "30", p(&root)]);
    assert!(
        real.out.contains(&format!(
            "{}: idle for 30+ days, removed",
            work_idle.display()
        )),
        "{}",
        real.out
    );
    assert!(!work_idle.exists() && busy_prof.exists());
    assert!(recent.prof.exists(), "idle for 10 days, not 30");
    let none = r(&["-r", p(&root.join("web"))]);
    assert_eq!(none.out, "no Cargo target dirs found\ntotal: 0B\n");
    let _ = fs::remove_dir_all(&root);
}
