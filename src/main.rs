//! cargo-autoprune: a `rustc-wrapper` that keeps `target/` from growing.
//!
//! As a wrapper (`build.rustc-wrapper = "cargo-autoprune"`) it costs one `stat` per rustc call,
//! then `exec`s rustc (or the chained wrapper from `CARGO_AUTOPRUNE_WRAPPER`). At most once per
//! interval it spawns a detached copy of itself that waits for cargo to release the build lock
//! and prunes superseded artifacts of that profile dir.
//!
//! By hand: `cargo autoprune [--dry-run] [--max-size 20G] [--grace SECS] [DIR]`.

mod prune;

use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

const STAMP: &str = ".autoprune/stamp";
const DEFAULT_INTERVAL: u64 = 300;

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let first = args.first().and_then(|a| a.to_str());
    match first {
        Some("autoprune") => cli(&args[1..]), // `cargo autoprune ...`
        Some("--background") => background(args.get(1).map(PathBuf::from)),
        None => cli(&[]),
        Some(a) if a.starts_with('-') => cli(&args),
        _ => wrapper(args),
    }
}

fn env_secs(name: &str, default: u64) -> Duration {
    Duration::from_secs(
        env::var(name)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(default),
    )
}

fn config_from_env() -> prune::Config {
    let d = prune::Config::default();
    prune::Config {
        grace: env_secs("CARGO_AUTOPRUNE_GRACE", d.grace.as_secs()),
        max_size: env::var("CARGO_AUTOPRUNE_MAX_SIZE")
            .ok()
            .and_then(|s| prune::parse_size(&s)),
        ..d
    }
}

// ---------------------------------------------------------------- wrapper

fn wrapper(args: Vec<OsString>) -> ! {
    if env::var_os("CARGO_AUTOPRUNE").is_none_or(|v| v != "0") {
        maybe_trigger(&args);
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
        let dir = profile.join(prune::STATE_DIR).join(prune::TAGS_DIR);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(h), prog);
    }
}

fn maybe_trigger(args: &[OsString]) {
    let Some(profile) = out_dir(args).as_deref().and_then(profile_of) else {
        return;
    };
    tag_workspace_wrapper(args, &profile);
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
    let _ = std::fs::create_dir_all(stamp.parent().unwrap_or(&profile));
    if File::create(&stamp).is_err() {
        return;
    }
    let Ok(me) = env::current_exe() else { return };
    let mut cmd = Command::new(me);
    cmd.arg("--background")
        .arg(&profile)
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

fn background(profile: Option<PathBuf>) {
    let Some(profile) = profile else { return };
    let _ = prune::prune(&profile, &config_from_env());
}

// ---------------------------------------------------------------- CLI

const HELP: &str = "\
cargo-autoprune: prune superseded build artifacts in Cargo target dirs

Install once, then every build keeps target/ trim:
  ~/.cargo/config.toml:  [build]
                         rustc-wrapper = \"cargo-autoprune\"

Manual run:
  cargo autoprune [--dry-run] [--max-size SIZE] [--grace SECS] [DIR]
    DIR        target dir or project dir (default: $CARGO_TARGET_DIR or ./target)
    --dry-run  print what would be removed

Environment (also settable in [env] of .cargo/config.toml):
  CARGO_AUTOPRUNE=0            disable the background prune (wrapper stays transparent)
  CARGO_AUTOPRUNE_INTERVAL     min seconds between background prunes (default 300)
  CARGO_AUTOPRUNE_GRACE        keep variants built within SECS of the newest (default 600)
  CARGO_AUTOPRUNE_MAX_SIZE     cap per profile dir, e.g. 20G (default: none)
  CARGO_AUTOPRUNE_WRAPPER      chain another wrapper, e.g. sccache
";

fn cli(args: &[OsString]) {
    let mut cfg = config_from_env();
    cfg.lock_wait = Duration::from_secs(600);
    let mut dir: Option<PathBuf> = None;
    let mut verbose = false;
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("cargo-autoprune {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-n" | "--dry-run" => cfg.dry_run = true,
            "-v" | "--verbose" => verbose = true,
            "--max-size" => cfg.max_size = it.next().and_then(|s| prune::parse_size(&s)),
            "--grace" => {
                cfg.grace =
                    Duration::from_secs(it.next().and_then(|s| s.parse().ok()).unwrap_or(600))
            }
            _ if a.starts_with('-') => die(&format!("unknown option {a}\n\n{HELP}")),
            _ => dir = Some(PathBuf::from(a)),
        }
    }
    let dir = dir
        .or_else(|| env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("target"));
    let target = if dir.join("CACHEDIR.TAG").is_file() {
        dir.clone()
    } else {
        dir.join("target")
    };
    let profiles = if prune::is_profile_dir(&dir) {
        vec![dir.clone()]
    } else {
        prune::profile_dirs(&target)
    };
    if profiles.is_empty() {
        die(&format!("no Cargo profile dirs under {}", dir.display()));
    }
    let mut total = 0;
    for p in profiles {
        match prune::prune(&p, &cfg) {
            Ok(prune::Outcome::Done(r)) => {
                if verbose || cfg.dry_run {
                    for f in &r.removed {
                        println!("  {}", f.display());
                    }
                }
                println!(
                    "{}: {} {} units, {} incremental dirs, {} ({} ms){}",
                    p.display(),
                    if cfg.dry_run {
                        "would remove"
                    } else {
                        "removed"
                    },
                    r.removed_units,
                    r.removed_incremental,
                    prune::human(r.freed_bytes),
                    r.elapsed.as_millis(),
                    if r.pinned_now.is_empty() {
                        String::new()
                    } else {
                        format!(", pinned {:?}", r.pinned_now)
                    }
                );
                total += r.freed_bytes;
            }
            Ok(prune::Outcome::Busy) => println!(
                "{}: busy (cargo or another prune holds the lock), skipped",
                p.display()
            ),
            Err(e) => eprintln!("{}: {e}", p.display()),
        }
    }
    println!("total: {}", prune::human(total));
}

fn die(msg: &str) -> ! {
    eprintln!("cargo-autoprune: {msg}");
    std::process::exit(2)
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
