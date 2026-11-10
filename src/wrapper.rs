//! The `rustc-wrapper` hot path: tag clippy units, maybe spawn a background prune, exec rustc.

use crate::layout::{STATE_DIR, TAGS_DIR};
use crate::prune::{env_secs, prune, Config};
use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

const STAMP: &str = ".autoprune/stamp";
const DEFAULT_INTERVAL: u64 = 300;

pub fn run_wrapper(args: Vec<OsString>) -> ! {
    if let Some(profile) = out_dir(&args).as_deref().and_then(profile_of) {
        tag_workspace_wrapper(&args, &profile); // needed even with the background prune off
        if env::var_os("CARGO_AUTOPRUNE").is_none_or(|v| v != "0") {
            maybe_trigger(&profile);
        }
    }
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
    run(&mut cmd)
}

#[cfg(unix)]
fn run(cmd: &mut Command) -> ! {
    use std::os::unix::process::CommandExt;
    let e = cmd.exec();
    eprintln!("cargo-autoprune: cannot exec {:?}: {e}", cmd.get_program());
    std::process::exit(127)
}

#[cfg(not(unix))]
fn run(cmd: &mut Command) -> ! {
    match cmd.status() {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("cargo-autoprune: cannot run {:?}: {e}", cmd.get_program());
            std::process::exit(127)
        }
    }
}

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

/// The profile dir is the nearest ancestor of the out dir that holds `.cargo-lock`
/// (`debug/deps`, `debug/build/x-<hash>`, `debug/examples`).
fn profile_of(out: &Path) -> Option<PathBuf> {
    out.ancestors()
        .take(3)
        .find(|a| a.join(".cargo-lock").is_file())
        .map(Path::to_path_buf)
}

/// `-C extra-filename=-<hash>` → `<hash>`.
fn unit_hash(args: &[OsString]) -> Option<String> {
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
    let Some(profile) = profile else { return };
    let _ = prune(&profile, &Config::from_env());
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
