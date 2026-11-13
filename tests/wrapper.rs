//! Stories of the binary as cargo's `rustc-wrapper`: invoked with a rustc command line, as cargo
//! does, with `true` standing in for rustc.
#![cfg(unix)]

mod support;

use cargo_autoprune::layout::{STATE_DIR, TAGS_DIR, UNITS_FILE};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use support::sim::Sim;
use support::BIN;

const H: &str = "0123456789abcdef";

/// One rustc invocation through the wrapper, as cargo makes it, from a shell that exits right
/// after (the background pruner waits for its parent, cargo, to exit).
fn rustc(args: &[&str], env: &[(&str, &str)]) {
    let mut c = Command::new("sh");
    // `; exit`: sh must not exec the last command, it plays cargo and exits after rustc
    c.arg("-c")
        .arg("\"$@\"; exit $?")
        .arg("sh")
        .arg(BIN)
        .arg(fake_rustc())
        .args(args);
    for (k, _) in std::env::vars() {
        if k.starts_with("CARGO_AUTOPRUNE")
            || k == "RUSTC_WORKSPACE_WRAPPER"
            || k == "CARGO_MANIFEST_DIR"
        {
            c.env_remove(k);
        }
    }
    c.envs(env.iter().copied());
    let st = c.status().unwrap();
    assert!(st.success());
}

/// A `rustc` that does nothing.
fn fake_rustc() -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = support::tmp("rustc"); // shared by the tests: never emptied
    let p = dir.join("rustc");
    if !p.exists() {
        fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join(format!("rustc.{:?}", std::thread::current().id()));
        fs::write(&tmp, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(tmp, &p).unwrap();
    }
    p
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_default()
}

/// Poll until `f` holds, up to 20 s.
fn eventually(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Given cargo compiling path packages and registry crates
/// Then the wrapper records which package dir each workspace unit belongs to (both ways cargo
/// passes the hash, both layouts), and nothing for registry or git crates or without a hash.
#[test]
fn records_the_package_dir_of_workspace_units() {
    let s = Sim::new("wrap-record");
    let deps = s.prof.join("deps");
    let out = format!("--out-dir={}", deps.display());
    let units = s.prof.join(STATE_DIR).join(UNITS_FILE);
    let off = [("CARGO_AUTOPRUNE", "0")];
    let extra = format!("extra-filename=-{H}");
    for dir in [
        "/h/.cargo/registry/src/idx/x-1.0.0",
        "C:\\h\\.cargo\\git\\checkouts\\y-1",
    ] {
        rustc(
            &["-C", &extra, &out],
            &[off[0], ("CARGO_MANIFEST_DIR", dir)],
        );
    }
    rustc(&[&out], &[off[0], ("CARGO_MANIFEST_DIR", "/ws/nohash")]);
    rustc(&["-C", &extra, &out], &off); // no package dir
    assert!(!units.exists());
    rustc(
        &["-C", &extra, &out],
        &[off[0], ("CARGO_MANIFEST_DIR", "/ws/app")],
    );
    rustc(
        &[
            &format!("-Cextra-filename=-{H}"),
            "--out-dir",
            deps.to_str().unwrap(),
        ],
        &[off[0], ("CARGO_MANIFEST_DIR", "/ws/util")],
    );
    // new layout: no extra-filename for a build script, the unit dir names the hash
    let unit_out = s.prof.join(format!("build/probe/{H}/out"));
    fs::create_dir_all(&unit_out).unwrap();
    rustc(
        &["--out-dir", unit_out.to_str().unwrap()],
        &[off[0], ("CARGO_MANIFEST_DIR", "/ws/probe")],
    );
    for bad in [
        format!("build/probe/{H}/x"),
        "build/probe/0123456789abcdez/out".into(),
    ] {
        let d = s.prof.join(bad);
        fs::create_dir_all(&d).unwrap();
        rustc(
            &["--out-dir", d.to_str().unwrap()],
            &[off[0], ("CARGO_MANIFEST_DIR", "/ws/bad")],
        );
    }
    // legacy build script out dir
    let script = s.prof.join(format!("build/probe-{H}"));
    fs::create_dir_all(&script).unwrap();
    rustc(
        &["-C", &extra, "--out-dir", script.to_str().unwrap()],
        &[off[0], ("CARGO_MANIFEST_DIR", "/ws/script")],
    );
    assert_eq!(
        read(&units),
        format!("{H}\t/ws/app\n{H}\t/ws/util\n{H}\t/ws/probe\n{H}\t/ws/script\n")
    );
    rustc(&["-vV"], &off); // no out dir: nothing to do, rustc still runs
}

/// Given `cargo clippy` (clippy-driver as the workspace wrapper, or as the program)
/// Then its units are tagged, so they don't supersede the plain `check` units; plain rustc, or a
/// workspace wrapper named rustc*, tags nothing.
#[test]
fn tags_clippy_units() {
    let s = Sim::new("wrap-tag");
    let tags = s.prof.join(STATE_DIR).join(TAGS_DIR);
    let a = [
        "-C".to_owned(),
        format!("extra-filename=-{H}"),
        format!("--out-dir={}", s.prof.join("deps").display()),
    ];
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    let off = ("CARGO_AUTOPRUNE", "0");
    rustc(&a, &[off]);
    rustc(&a, &[off, ("RUSTC_WORKSPACE_WRAPPER", "")]);
    rustc(&a, &[off, ("RUSTC_WORKSPACE_WRAPPER", "/x/rustc-1.80")]);
    assert!(!tags.exists());
    rustc(&a, &[off, ("RUSTC_WORKSPACE_WRAPPER", "/x/clippy-driver")]);
    assert_eq!(read(&tags.join(H)), "clippy-driver");
}

/// Given a target with garbage and a build going through the wrapper
/// Then the first rustc invocation starts a background prune that runs after cargo exits; the
/// next ones within the interval start none; `CARGO_AUTOPRUNE=0` turns it off.
#[test]
fn builds_start_a_background_prune_once_per_interval() {
    let mut s = Sim::new("wrap-bg");
    s.build();
    let old = s.key("app", "lib-");
    s.advance(3600);
    s.conf.util_x = true;
    s.build();
    let out = format!("--out-dir={}", s.prof.join("deps").display());
    let log = s.prof.join(STATE_DIR).join("log");
    let stamp = s.prof.join(STATE_DIR).join("stamp");

    rustc(&[&out], &[("CARGO_AUTOPRUNE", "0")]);
    assert!(!stamp.exists());
    rustc(&[&out], &[]);
    eventually("the background prune", || read(&log).lines().count() == 1);
    assert!(!s.present(&old) && s.used.iter().all(|k| s.present(k)));
    rustc(&[&out], &[]);
    rustc(&[&out], &[("CARGO_AUTOPRUNE_INTERVAL", "x")]); // junk: the default (300 s)
    rustc(&[&out], &[("CARGO_AUTOPRUNE_INTERVAL", "0")]);
    eventually("a second prune", || read(&log).lines().count() == 2);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        read(&log).lines().count(),
        2,
        "within the interval: no prune"
    );
}

/// Given a background prune that could not delete something
/// When the next build is quiet (`CARGO_AUTOPRUNE_QUIET`)
/// Then the notice is not printed but kept for `status`.
#[test]
fn quiet_builds_keep_notices_for_status() {
    let mut s = Sim::new("wrap-notice");
    s.build();
    let state = s.prof.join(STATE_DIR);
    fs::write(state.join("notice"), "could not delete 1 path(s)\n").unwrap();
    let out = format!("--out-dir={}", s.prof.join("deps").display());
    rustc(&[&out], &[("CARGO_AUTOPRUNE_QUIET", "1")]);
    assert!(read(&state.join("notice")).contains("could not delete"));
    let st = Command::new(BIN)
        .args(["status", s.root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&st.stdout).contains("  notice: could not delete 1 path(s)"));
}

/// Given a background prune that freed a lot (here 2 GiB of garbage, sparse files)
/// Then it leaves a notice saying so, and the notice file stops growing past 4 KiB when nobody
/// reads it.
#[test]
fn big_frees_leave_a_notice() {
    let mut s = Sim::new("wrap-big");
    s.build();
    s.grow(&s.key("app", "lib-"), 2 << 30);
    s.advance(3600);
    s.conf.util_x = true;
    s.build();
    let state = s.prof.join(STATE_DIR);
    let out = format!("--out-dir={}", s.prof.join("deps").display());
    rustc(&[&out], &[("CARGO_AUTOPRUNE_QUIET", "1")]);
    let notice = state.join("notice");
    eventually("the notice", || read(&notice).contains("freed 2.0GiB in "));
    assert!(read(&notice).contains("superseded 2"), "{}", read(&notice));

    fs::write(&notice, "x".repeat(4097) + "\n").unwrap();
    let before = read(&notice).len();
    let mut s2 = s;
    s2.grow(&s2.key("app", "lib-"), 2 << 30);
    s2.advance(3600);
    s2.conf.util_x = false;
    s2.build();
    let log = state.join("log");
    let n = read(&log).lines().count();
    rustc(
        &[&out],
        &[
            ("CARGO_AUTOPRUNE_QUIET", "1"),
            ("CARGO_AUTOPRUNE_INTERVAL", "0"),
        ],
    );
    eventually("the second prune", || read(&log).lines().count() == n + 1);
    assert_eq!(read(&notice).len(), before, "capped");
}
