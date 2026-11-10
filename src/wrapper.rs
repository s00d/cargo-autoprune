//! The `rustc-wrapper` hot path: tag clippy units, maybe spawn a background prune, exec rustc.

use crate::layout::{has_cargo_lock, is_profile_dir, STATE_DIR, TAGS_DIR, UNITS_FILE};
use crate::prune::{env_secs, prune, Config, Outcome};
use crate::state::{add_notice, notices_for, read_notices, NOTICE_FILE};
use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

const STAMP: &str = ".autoprune/stamp";
const DEFAULT_INTERVAL: u64 = 300;

pub fn run_wrapper(args: Vec<OsString>) -> ! {
    let profile = out_dir(&args).as_deref().and_then(profile_of);
    if let Some(profile) = &profile {
        // both are needed even with the background prune off
        tag_workspace_wrapper(&args, profile);
        record_unit(&args, profile);
    }
    let enabled = env::var_os("CARGO_AUTOPRUNE").is_none_or(|v| v != "0");
    let mut cmd = match env::var_os("CARGO_AUTOPRUNE_WRAPPER").filter(|w| !w.is_empty()) {
        Some(w) => {
            let mut c = Command::new(w);
            c.args(&args);
            c
        }
        None => {
            let mut c = Command::new(&args[0]);
            c.args(&args[1..]);
            c
        }
    };
    run(&mut cmd, profile.filter(|_| enabled))
}

/// Unix: spawn the pruner, then `exec` rustc (the pruner gets no cargo pipe: stdio is null and
/// every other descriptor is close-on-exec).
#[cfg(unix)]
fn run(cmd: &mut Command, prune: Option<PathBuf>) -> ! {
    use std::os::unix::process::CommandExt;
    if let Some(p) = prune {
        maybe_trigger(&p);
    }
    let e = cmd.exec();
    eprintln!("cargo-autoprune: cannot exec {:?}: {e}", cmd.get_program());
    std::process::exit(127)
}

/// Windows: no exec. Run rustc first, then spawn the pruner with our std handles made
/// non-inheritable: Windows hands every inheritable handle to a child, and a pruner holding
/// cargo's output pipe would make cargo wait for it while it waits for cargo's lock.
#[cfg(not(unix))]
fn run(cmd: &mut Command, prune: Option<PathBuf>) -> ! {
    let code = match cmd.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("cargo-autoprune: cannot run {:?}: {e}", cmd.get_program());
            127
        }
    };
    if let Some(p) = prune {
        no_inherit_std_handles();
        maybe_trigger(&p);
    }
    std::process::exit(code)
}

#[cfg(windows)]
fn no_inherit_std_handles() {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetHandleInformation(h: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    let handles = [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ];
    for h in handles {
        if !h.is_null() {
            // SAFETY: a handle of this process; only its inherit flag changes
            unsafe { SetHandleInformation(h.cast(), HANDLE_FLAG_INHERIT, 0) };
        }
    }
}
#[cfg(all(not(unix), not(windows)))]
fn no_inherit_std_handles() {}

/// `--out-dir X` / `--out-dir=X` from a rustc command line.
fn out_dir(args: &[OsString]) -> Option<PathBuf> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--out-dir" {
            return it.next().map(PathBuf::from);
        }
        if let Some(v) = a.to_str().and_then(|s| s.strip_prefix("--out-dir=")) {
            return Some(PathBuf::from(v));
        }
    }
    None
}

/// The profile dir is the nearest ancestor of the out dir that holds a Cargo lock file
/// (`debug/deps`, `debug/build/x-<hash>`, `debug/examples`).
fn profile_of(out: &Path) -> Option<PathBuf> {
    // legacy: debug/deps, debug/build/x-<hash>; new layout: debug/build/<pkg>/<hash>/out
    out.ancestors()
        .take(5)
        .find(|a| has_cargo_lock(a))
        .map(Path::to_path_buf)
}

/// `-C extra-filename=-<hash>` → `<hash>`; in the new build-dir layout (no extra-filename for
/// binaries) the unit dir: `--out-dir .../build/<pkg>/<hash>/out`.
fn unit_hash(args: &[OsString]) -> Option<String> {
    extra_filename(args).or_else(|| {
        let out = out_dir(args)?;
        let h = out.parent()?.file_name()?.to_str()?;
        (out.file_name()? == "out" && is_hash(h)).then(|| h.to_owned())
    })
}

fn is_hash(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn extra_filename(args: &[OsString]) -> Option<String> {
    let mut prev_c = false;
    for a in args.iter().filter_map(|a| a.to_str()) {
        let v = if prev_c {
            Some(a)
        } else {
            a.strip_prefix("-C")
        };
        prev_c = a == "-C";
        if let Some(h) = v.and_then(|v| v.strip_prefix("extra-filename=-")) {
            return Some(h.to_owned());
        }
    }
    None
}

/// Record which package dir a unit belongs to, so the pruner knows its workspace (`Cargo.lock`)
/// even with a separate `build.build-dir` or a target dir shared by several workspaces, where
/// same-named packages would otherwise look alike. One append per path-package compile.
fn record_unit(args: &[OsString], profile: &Path) {
    let Some(dir) = env::var_os("CARGO_MANIFEST_DIR") else {
        return;
    };
    let Some(dir) = dir.to_str() else { return };
    let d = dir.replace('\\', "/"); // Windows separators
    if d.contains("/registry/src/") || d.contains("/git/checkouts/") {
        return;
    }
    let Some(h) = unit_hash(args) else { return };
    let file = profile.join(STATE_DIR).join(UNITS_FILE);
    let open = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
    };
    let f = open().or_else(|_| {
        let _ = std::fs::create_dir_all(profile.join(STATE_DIR));
        open()
    });
    if let Ok(mut f) = f {
        use std::io::Write;
        let _ = f.write_all(format!("{h}\t{dir}\n").as_bytes()); // one O_APPEND write: no interleaving
    }
}

/// clippy-driver (RUSTC_WORKSPACE_WRAPPER) units have the same fingerprint json as plain
/// `check` units; tag them so the pruner keeps both variants. Costs nothing for plain rustc.
fn tag_workspace_wrapper(args: &[OsString], profile: &Path) {
    // cargo-clippy exports RUSTC_WORKSPACE_WRAPPER; it changes the hash of every workspace unit,
    // including build scripts that cargo compiles with plain rustc.
    let ws = env::var_os("RUSTC_WORKSPACE_WRAPPER").filter(|w| !w.is_empty());
    let prog = match ws
        .as_deref()
        .map(Path::new)
        .unwrap_or(Path::new(&args[0]))
        .file_stem()
    {
        Some(p) => p.to_string_lossy().into_owned(),
        None => return,
    };
    if prog.starts_with("rustc") {
        return;
    }
    if let Some(h) = unit_hash(args) {
        let dir = profile.join(STATE_DIR).join(TAGS_DIR);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(h), prog);
    }
}

fn maybe_trigger(profile: &Path) {
    let stamp = profile.join(STAMP);
    let interval = env_secs("CARGO_AUTOPRUNE_INTERVAL", DEFAULT_INTERVAL);
    let fresh = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < interval);
    if fresh {
        return;
    }
    show_notices(profile);
    let _ = std::fs::create_dir_all(stamp.parent().unwrap_or(profile));
    if File::create(&stamp).is_err() {
        return;
    }
    let Ok(me) = env::current_exe() else { return };
    let mut cmd = Command::new(me);
    cmd.arg("--background")
        .arg(profile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // not killed with cargo's Ctrl-C
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    }
    let _ = cmd.spawn(); // never fail the build because of us
}

pub fn background(profile: Option<PathBuf>) {
    close_inherited_fds();
    let Some(profile) = profile else { return };
    let state = profile.join(STATE_DIR);
    let notices = match prune(&profile, &Config::from_env()) {
        Ok(Outcome::Done(r)) => notices_for(&profile, &r),
        Ok(Outcome::Busy) => Vec::new(), // another prune runs, or cargo kept the lock for an hour
        Err(_) if !is_profile_dir(&profile) => return, // not a cargo dir: write nothing there
        Err(e) => vec![format!(
            "background prune of {} failed: {e}",
            profile.display()
        )],
    };
    for n in notices {
        add_notice(&state, &n);
    }
}

/// Drop every descriptor inherited from cargo/rustc above stderr (pipes a CI step or a test
/// harness waits on, jobserver fds): the pruner must never keep someone else's pipe open.
#[cfg(unix)]
fn close_inherited_fds() {
    extern "C" {
        fn close(fd: i32) -> i32;
    }
    for fd in 3..1024 {
        // SAFETY: nothing in this process has opened descriptors yet at this point
        unsafe { close(fd) };
    }
}
#[cfg(not(unix))]
fn close_inherited_fds() {} // Windows: the spawn already made the std handles non-inheritable

/// Print pending notices once on the terminal. Not on stderr: cargo stores a crate's stderr and
/// replays it on every later build until the crate is recompiled.
fn show_notices(profile: &Path) {
    let file = profile.join(STATE_DIR).join(NOTICE_FILE);
    if !file.exists() || env::var_os("CARGO_AUTOPRUNE_QUIET").is_some() {
        return;
    }
    let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") else {
        return; // no terminal (IDE, CI): keep them for `cargo autoprune status`
    };
    use std::io::Write;
    for n in read_notices(&profile.join(STATE_DIR)) {
        let _ = writeln!(tty, "\x1b[1;33mwarning\x1b[0m: cargo-autoprune: {n}");
    }
    let _ = std::fs::remove_file(file);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_out_dir() {
        let a: Vec<OsString> = [
            "--crate-name",
            "x",
            "--out-dir",
            "/t/debug/deps",
            "-C",
            "opt-level=0",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(out_dir(&a), Some(PathBuf::from("/t/debug/deps")));
        let b: Vec<OsString> = vec!["--out-dir=/t/x".into()];
        assert_eq!(out_dir(&b), Some(PathBuf::from("/t/x")));
        assert_eq!(out_dir(&["-vV".into()]), None);
    }

    #[test]
    fn finds_unit_hash() {
        let a: Vec<OsString> = [
            "--crate-name",
            "x",
            "-C",
            "extra-filename=-0123456789abcdef",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(unit_hash(&a).as_deref(), Some("0123456789abcdef"));
        let b: Vec<OsString> = vec!["-Cextra-filename=-aa".into()];
        assert_eq!(unit_hash(&b).as_deref(), Some("aa"));
        assert_eq!(unit_hash(&["-C".into(), "opt-level=0".into()]), None);
    }

    #[test]
    fn unit_hash_from_new_layout_out_dir() {
        let a: Vec<OsString> = [
            "--crate-name",
            "probe",
            "--out-dir",
            "/t/debug/build/probe/860b9d940d21e6fc/out",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(unit_hash(&a).as_deref(), Some("860b9d940d21e6fc"));
        let b: Vec<OsString> = ["--out-dir", "/t/debug/deps"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(unit_hash(&b), None);
    }

    #[test]
    fn profile_from_build_script_out_dir() {
        let root = env::temp_dir().join(format!("autoprune-prof-{}", std::process::id()));
        let out = root.join("debug/build/libc-aaaaaaaaaaaaaaaa");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(root.join("debug/.cargo-lock"), "").unwrap();
        assert_eq!(profile_of(&out), Some(root.join("debug")));
        assert_eq!(
            profile_of(&root.join("debug/deps")),
            Some(root.join("debug"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
