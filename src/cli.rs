//! `cargo autoprune [--dry-run] [--max-size SIZE] [--grace SECS] [DIR]`.

use crate::fsutil::{human, parse_size};
use crate::layout::{is_profile_dir, profile_dirs};
use crate::prune::{prune, Config, Outcome};
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

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

pub fn run(args: &[OsString]) {
    let mut cfg = Config::from_env();
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
            "--max-size" => cfg.max_size = it.next().and_then(|s| parse_size(&s)),
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
    let profiles = if is_profile_dir(&dir) {
        vec![dir.clone()]
    } else {
        profile_dirs(&target)
    };
    if profiles.is_empty() {
        die(&format!("no Cargo profile dirs under {}", dir.display()));
    }
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
                println!(
                    "{}: {} {} units, {} incremental dirs, {} ({} ms, cargo blocked {} ms){}",
                    p.display(),
                    if cfg.dry_run {
                        "would remove"
                    } else {
                        "removed"
                    },
                    r.removed_units,
                    r.removed_incremental,
                    human(r.freed_bytes),
                    r.elapsed.as_millis(),
                    r.locked.as_millis(),
                    if r.pinned_now.is_empty() {
                        String::new()
                    } else {
                        format!(", pinned {:?}", r.pinned_now)
                    }
                );
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
