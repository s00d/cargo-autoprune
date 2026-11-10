//! `cargo autoprune [--dry-run] [--max-size SIZE] [--grace SECS] [DIR]`.

use crate::fsutil::{entries, human, parse_size, size_of};
use crate::layout::{is_profile_dir, profile_dirs, STATE_DIR};
use crate::prune::{prune, Config, Outcome};
use crate::state::read_notices;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "\
cargo-autoprune: prune superseded build artifacts in Cargo target dirs

Install once, then every build keeps target/ trim:
  ~/.cargo/config.toml:  [build]
                         rustc-wrapper = \"cargo-autoprune\"

Manual run:
  cargo autoprune [--dry-run] [--verbose] [--max-size SIZE] [--grace SECS] [DIR]
    DIR        profile, target or project dir (default: $CARGO_TARGET_DIR or ./target)
    --dry-run  print what would be removed
    --verbose  list removed paths and phase timings
  cargo autoprune status [DIR]
    size, reclaimable space, last prune, pinned groups, notices, whether the wrapper is set up

Environment (also settable in [env] of .cargo/config.toml):
  CARGO_AUTOPRUNE=0            disable the background prune (wrapper stays transparent)
  CARGO_AUTOPRUNE_INTERVAL     min seconds between background prunes (default 300)
  CARGO_AUTOPRUNE_GRACE        keep variants built within SECS of the newest (default 600)
  CARGO_AUTOPRUNE_MAX_SIZE     cap per profile dir, e.g. 20G (default: none)
  CARGO_AUTOPRUNE_WRAPPER      chain another wrapper, e.g. sccache
  CARGO_AUTOPRUNE_QUIET=1      don't print notices (big frees, errors) on the terminal
  CARGO_AUTOPRUNE_TRACE=1      print phase timings of manual runs
";

/// Profile dirs for a CLI DIR argument: a profile dir, a target dir, or a project dir.
fn resolve_profiles(dir: Option<PathBuf>) -> Vec<PathBuf> {
    let dir = dir
        .or_else(|| env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("target"));
    if is_profile_dir(&dir) {
        return vec![dir];
    }
    let target = if dir.join("CACHEDIR.TAG").is_file() {
        dir.clone()
    } else {
        dir.join("target")
    };
    let profiles = profile_dirs(&target);
    if profiles.is_empty() {
        die(&format!("no Cargo profile dirs under {}", dir.display()));
    }
    profiles
}

/// `cargo autoprune status [DIR]`: size, reclaimable space, last prune, pins, notices, setup.
fn status(dir: Option<PathBuf>) {
    let wrapper = configured_wrapper();
    println!("wrapper: {}", wrapper.as_deref().unwrap_or("not configured (add build.rustc-wrapper = \"cargo-autoprune\" to ~/.cargo/config.toml)"));
    let dry = Config {
        dry_run: true,
        lock_wait: Duration::ZERO,
        ..Config::from_env()
    };
    for p in resolve_profiles(dir) {
        let state = p.join(STATE_DIR);
        let units = entries(&p.join(".fingerprint")).len();
        println!(
            "{}: {} on disk, {units} units",
            p.display(),
            human(size_of(&p))
        );
        match prune(&p, &dry) {
            Ok(Outcome::Done(r)) => println!(
                "  reclaimable now: {} ({} units, {} incremental dirs{}{})",
                human(r.freed_bytes),
                r.removed_units,
                r.removed_incremental,
                if r.by_reason.is_empty() { "" } else { ": " },
                r.breakdown()
            ),
            Ok(Outcome::Busy) => {
                println!("  reclaimable now: unknown, a build or a prune is running")
            }
            Err(e) => println!("  error: {e}"),
        }
        let log = fs::read_to_string(state.join("log")).unwrap_or_default();
        match log.lines().last().and_then(|l| l.split_once(' ')) {
            Some((ts, rest)) => {
                let ago = now_secs().saturating_sub(ts.parse().unwrap_or(0));
                println!("  last prune: {} ago, {rest}", ago_text(ago));
            }
            None => println!("  last prune: never"),
        }
        let pins = fs::read_to_string(state.join("pinned")).unwrap_or_default();
        let pins: Vec<&str> = pins.lines().filter_map(|l| l.split('|').next()).collect();
        if !pins.is_empty() {
            println!(
                "  pinned groups (alternating variants, 24 h): {}",
                pins.join(", ")
            );
        }
        for n in read_notices(&state) {
            println!("  notice: {n}");
        }
    }
}

fn configured_wrapper() -> Option<String> {
    if let Some(w) =
        env::var_os("RUSTC_WRAPPER").filter(|w| w.to_string_lossy().contains("cargo-autoprune"))
    {
        return Some(format!("RUSTC_WRAPPER={}", w.to_string_lossy()));
    }
    let home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
    let mut candidates = vec![home.join("config.toml"), home.join("config")];
    if let Ok(cwd) = env::current_dir() {
        candidates.extend(cwd.ancestors().map(|a| a.join(".cargo/config.toml")));
    }
    candidates.into_iter().find_map(|f| {
        let text = fs::read_to_string(&f).ok()?;
        text.lines()
            .any(|l| {
                !l.trim_start().starts_with('#')
                    && l.contains("rustc-wrapper")
                    && l.contains("cargo-autoprune")
            })
            .then(|| format!("enabled in {}", f.display()))
    })
}

fn ago_text(secs: u64) -> String {
    match secs {
        s if s < 120 => format!("{s} s"),
        s if s < 7200 => format!("{} min", s / 60),
        s if s < 172_800 => format!("{} h", s / 3600),
        s => format!("{} days", s / 86400),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn run(args: &[OsString]) {
    let mut cfg = Config::from_env();
    cfg.lock_wait = Duration::from_secs(600);
    let mut dir: Option<PathBuf> = None;
    let mut verbose = false;
    let mut it = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .peekable();
    if it.peek().map(String::as_str) == Some("status") {
        it.next();
        return status(it.next().map(PathBuf::from));
    }
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
            "--max-size" => cfg.max_size = it.next().and_then(|s| parse_size(&s)),
            "--grace" => {
                cfg.grace =
                    Duration::from_secs(it.next().and_then(|s| s.parse().ok()).unwrap_or(600))
            }
            _ if a.starts_with('-') => die(&format!("unknown option {a}\n\n{HELP}")),
            _ => dir = Some(PathBuf::from(a)),
        }
    }
    let profiles = resolve_profiles(dir);
    let mut total = 0;
    for p in profiles {
        match prune(&p, &cfg) {
            Ok(Outcome::Done(r)) => {
                if verbose || env::var_os("CARGO_AUTOPRUNE_TRACE").is_some() {
                    for (name, d) in &r.phases {
                        println!("  {:>8.1} ms  {name}", d.as_secs_f64() * 1000.0);
                    }
                }
                if verbose || cfg.dry_run {
                    for f in &r.removed {
                        println!("  {}", f.display());
                    }
                }
                let what = if cfg.dry_run {
                    "would remove"
                } else {
                    "removed"
                };
                println!(
                    "{}: {what} {} units, {} incremental dirs, {} ({} ms, cargo blocked {} ms)",
                    p.display(),
                    r.removed_units,
                    r.removed_incremental,
                    human(r.freed_bytes),
                    r.elapsed.as_millis(),
                    r.locked.as_millis(),
                );
                if !r.by_reason.is_empty() {
                    println!("  {}", r.breakdown());
                }
                if !r.pinned_now.is_empty() {
                    let names: Vec<&str> = r
                        .pinned_now
                        .iter()
                        .map(|k| k.split('|').next().unwrap_or(k))
                        .collect();
                    println!("  pinned (alternating variants): {}", names.join(", "));
                }
                for e in r.errors.iter().take(5) {
                    eprintln!("  warning: could not delete {e}");
                }
                total += r.freed_bytes;
            }
            Ok(Outcome::Busy) => println!(
                "{}: busy (cargo or another prune holds the lock), skipped",
                p.display()
            ),
            Err(e) => eprintln!("{}: {e}", p.display()),
        }
    }
    println!("total: {}", human(total));
}

fn die(msg: &str) -> ! {
    eprintln!("cargo-autoprune: {msg}");
    std::process::exit(2)
}
