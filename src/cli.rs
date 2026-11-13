//! `cargo autoprune [--dry-run] [--max-size SIZE] [--grace SECS] [DIR]`.

use crate::fsutil::{entries, human, parse_size, size_of};
use crate::layout::{find_target_dirs, fingerprint_dirs, is_profile_dir, profile_dirs, STATE_DIR};
use crate::prune::{prune, Config, Outcome};
use crate::state::read_notices;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "\
cargo-autoprune: prune superseded build artifacts in Cargo target dirs

Install once, then every build keeps target/ trim:
  ~/.cargo/config.toml:  [build]
                         rustc-wrapper = \"cargo-autoprune\"

Manual run:
  cargo autoprune [OPTIONS] [DIR...]
    DIR                 profile, target or project dir (default: $CARGO_TARGET_DIR or ./target)
    -n, --dry-run       print what would be removed
    -v, --verbose       list removed paths and phase timings
    --max-size SIZE     cap each profile dir, e.g. 20G (oldest units go first)
    --grace SECS        keep variants built within SECS of the newest (default 600)
    -r, --recursive     find every Cargo target dir under DIR... (servers, runners, ~/projects)
    --remove-idle DAYS  with -r: delete whole target dirs nothing was compiled into for DAYS
    --drop-incremental  also delete all incremental dirs
    --ci                --grace 0 --drop-incremental: trim target/ before a CI cache is saved
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

/// `--remove-idle DAYS`: delete a whole target dir when nothing was compiled into it for DAYS
/// (the newest build of a unit, or the wrapper's stamp). Not the log or the fingerprint dir: a
/// prune writes those, and a nightly `-r --remove-idle` would never see a target idle.
/// Skipped while any of its profiles is locked by a running cargo.
fn remove_if_idle(target: &std::path::Path, days: u64, dry_run: bool) -> bool {
    let profiles = profile_dirs(target);
    let newest = profiles
        .iter()
        .flat_map(|p| {
            fingerprint_dirs(p)
                .into_iter()
                .map(|f| f.join("invoked.timestamp"))
                .chain([p.join(STATE_DIR).join("stamp")])
        })
        .filter_map(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
        .max();
    let idle = newest
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age.as_secs() >= days * 86400);
    if !idle {
        return false;
    }
    let size = human(size_of(target));
    if dry_run {
        println!(
            "{}: idle for {days}+ days, would remove the whole dir ({size})",
            target.display()
        );
        return true;
    }
    let mut held = Vec::new();
    for p in &profiles {
        match crate::lock::acquire_cargo_locks(p, Duration::ZERO) {
            Ok(Some(l)) => held.push(l),
            _ => {
                println!(
                    "{}: idle but locked by a running cargo, kept",
                    target.display()
                );
                return false;
            }
        }
    }
    if cfg!(windows) {
        drop(held); // open files can't be deleted there
    }
    match fs::remove_dir_all(target) {
        Ok(()) => println!(
            "{}: idle for {days}+ days, removed ({size})",
            target.display()
        ),
        Err(e) => eprintln!("{}: {e}", target.display()),
    }
    true
}

/// A size cap from the user. Tiny values are refused: `--max-size 20` (bytes, meant 20G) would
/// delete every unit.
pub(crate) fn parse_cap(v: &str) -> Result<u64, String> {
    match parse_size(v) {
        Some(b) if b >= 1 << 20 => Ok(b),
        Some(_) => Err(format!(
            "size cap {v:?} is below 1 MiB; use a unit, e.g. 20G"
        )),
        None => Err(format!("cannot parse size {v:?}; use e.g. 500M or 20G")),
    }
}

/// Environment settings that do not parse, so `status` can say they are ignored.
fn bad_env(var: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut out = Vec::new();
    for k in ["CARGO_AUTOPRUNE_INTERVAL", "CARGO_AUTOPRUNE_GRACE"] {
        if let Some(v) = var(k) {
            if v.trim().parse::<u64>().is_err() {
                out.push(format!(
                    "{k}={v:?} is not a number of seconds; the default is used"
                ));
            }
        }
    }
    if let Some(v) = var("CARGO_AUTOPRUNE_MAX_SIZE") {
        if let Err(e) = parse_cap(&v) {
            out.push(format!("CARGO_AUTOPRUNE_MAX_SIZE ignored: {e}"));
        }
    }
    out
}

/// `cargo autoprune status [DIR]`: size, reclaimable space, last prune, pins, notices, setup.
fn status(dir: Option<PathBuf>, out: &mut dyn Write) -> io::Result<()> {
    status_at(dir, std::time::SystemTime::now(), out)
}

/// `status` as of `now` (tests pass a fixed clock).
#[doc(hidden)]
pub fn status_at(
    dir: Option<PathBuf>,
    now: std::time::SystemTime,
    out: &mut dyn Write,
) -> io::Result<()> {
    let now = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let wrapper = configured_wrapper(|k| env::var_os(k), env::current_dir().ok());
    writeln!(out, "wrapper: {}", wrapper.as_deref().unwrap_or("not configured (add build.rustc-wrapper = \"cargo-autoprune\" to ~/.cargo/config.toml)"))?;
    for w in bad_env(|k| env::var(k).ok()) {
        writeln!(out, "warning: {w}")?;
    }
    let dry = Config {
        dry_run: true,
        lock_wait: Duration::ZERO,
        ..Config::from_env()
    };
    for p in resolve_profiles(dir) {
        let state = p.join(STATE_DIR);
        let units = entries(&p.join(".fingerprint")).len();
        writeln!(
            out,
            "{}: {} on disk, {units} units",
            p.display(),
            human(size_of(&p))
        )?;
        match prune(&p, &dry) {
            Ok(Outcome::Done(r)) => writeln!(
                out,
                "  reclaimable now: {} ({} units, {} incremental dirs{}{})",
                human(r.freed_bytes),
                r.removed_units,
                r.removed_incremental,
                if r.by_reason.is_empty() { "" } else { ": " },
                r.breakdown()
            )?,
            Ok(Outcome::Busy) => writeln!(
                out,
                "  reclaimable now: unknown, a build or a prune is running"
            )?,
            Err(e) => writeln!(out, "  error: {e}")?,
        }
        let log = fs::read_to_string(state.join("log")).unwrap_or_default();
        match log.lines().last().and_then(|l| l.split_once(' ')) {
            Some((ts, rest)) => {
                let ago = now.saturating_sub(ts.parse().unwrap_or(0));
                writeln!(out, "  last prune: {} ago, {rest}", ago_text(ago))?;
            }
            None => writeln!(out, "  last prune: never")?,
        }
        let pins = fs::read_to_string(state.join("pinned")).unwrap_or_default();
        let pins: Vec<&str> = pins.lines().filter_map(|l| l.split('|').next()).collect();
        if !pins.is_empty() {
            writeln!(
                out,
                "  pinned groups (alternating variants, 24 h): {}",
                pins.join(", ")
            )?;
        }
        for n in read_notices(&state) {
            writeln!(out, "  notice: {n}")?;
        }
    }
    Ok(())
}

/// `rustc-wrapper = "cargo-autoprune"` from `RUSTC_WRAPPER`, the Cargo home config or a
/// `.cargo/config.toml` above `cwd`. `var` reads the environment.
fn configured_wrapper(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    cwd: Option<PathBuf>,
) -> Option<String> {
    if let Some(w) =
        var("RUSTC_WRAPPER").filter(|w| w.to_string_lossy().contains("cargo-autoprune"))
    {
        return Some(format!("RUSTC_WRAPPER={}", w.to_string_lossy()));
    }
    let home = var("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
    let mut candidates = vec![home.join("config.toml"), home.join("config")];
    if let Some(cwd) = cwd {
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

/// A manual run's options.
#[derive(Debug)]
struct Opts {
    cfg: Config,
    dirs: Vec<PathBuf>,
    verbose: bool,
    recursive: bool,
    remove_idle: Option<u64>,
}

#[derive(Debug)]
enum Cmd {
    Help,
    Version,
    Status(Option<PathBuf>),
    Prune(Opts),
}

fn parse_args(args: &[OsString]) -> Result<Cmd, String> {
    let mut o = Opts {
        cfg: Config {
            lock_wait: Duration::from_secs(600),
            ..Config::from_env()
        },
        dirs: Vec::new(),
        verbose: false,
        recursive: false,
        remove_idle: None,
    };
    let mut it = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .peekable();
    if it.peek().map(String::as_str) == Some("status") {
        it.next();
        return Ok(Cmd::Status(it.next().map(PathBuf::from)));
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(Cmd::Help),
            "-V" | "--version" => return Ok(Cmd::Version),
            "-n" | "--dry-run" => o.cfg.dry_run = true,
            "-v" | "--verbose" => o.verbose = true,
            "--max-size" => o.cfg.max_size = Some(parse_cap(&it.next().unwrap_or_default())?),
            "--grace" => {
                let v = it.next().unwrap_or_default();
                let secs = v
                    .parse()
                    .map_err(|_| format!("--grace takes seconds, got {v:?}"))?;
                o.cfg.grace = Duration::from_secs(secs);
            }
            "-r" | "--recursive" => o.recursive = true,
            "--drop-incremental" => o.cfg.drop_incremental = true,
            "--ci" => {
                o.cfg.grace = Duration::ZERO;
                o.cfg.drop_incremental = true;
            }
            "--remove-idle" => {
                let v = it.next().unwrap_or_default();
                let days = v
                    .parse()
                    .map_err(|_| format!("--remove-idle takes days, got {v:?}"))?;
                o.remove_idle = Some(days);
            }
            _ if a.starts_with('-') => return Err(format!("unknown option {a}\n\n{HELP}")),
            _ => o.dirs.push(PathBuf::from(a)),
        }
    }
    Ok(Cmd::Prune(o))
}

pub fn run(args: &[OsString]) {
    let Opts {
        cfg,
        dirs,
        verbose,
        recursive,
        remove_idle,
    } = match parse_args(args).unwrap_or_else(|e| die(&e)) {
        Cmd::Help => return print!("{HELP}"),
        Cmd::Version => return println!("cargo-autoprune {}", env!("CARGO_PKG_VERSION")),
        Cmd::Status(dir) => {
            let _ = status(dir, &mut io::stdout().lock());
            return;
        }
        Cmd::Prune(o) => o,
    };
    let mut out = io::stdout().lock();
    let mut err = io::stderr().lock();
    let profiles = select_profiles(dirs, recursive, remove_idle, cfg.dry_run, &mut out);
    let mut total = 0;
    for p in profiles {
        let _ = match prune(&p, &cfg) {
            Ok(Outcome::Done(r)) => {
                total += r.freed_bytes;
                let trace = verbose || env::var_os("CARGO_AUTOPRUNE_TRACE").is_some();
                print_report(&mut out, &mut err, &p, &r, cfg.dry_run, verbose, trace)
            }
            Ok(Outcome::Busy) => writeln!(
                out,
                "{}: busy (cargo or another prune holds the lock), skipped",
                p.display()
            ),
            Err(e) => writeln!(err, "{}: {e}", p.display()),
        };
    }
    let _ = writeln!(out, "total: {}", human(total));
}

/// Profile dirs of a manual run: DIR arguments, or with `-r` every target dir found under them
/// (minus the idle ones `--remove-idle` deleted).
fn select_profiles(
    dirs: Vec<PathBuf>,
    recursive: bool,
    remove_idle: Option<u64>,
    dry_run: bool,
    out: &mut dyn Write,
) -> Vec<PathBuf> {
    if !recursive {
        if dirs.is_empty() {
            return resolve_profiles(None);
        }
        return dirs
            .into_iter()
            .flat_map(|d| resolve_profiles(Some(d)))
            .collect();
    }
    let roots = if dirs.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        dirs
    };
    let mut targets: Vec<PathBuf> = roots.iter().flat_map(|r| find_target_dirs(r)).collect();
    if let Some(days) = remove_idle {
        targets.retain(|t| !remove_if_idle(t, days, dry_run));
    }
    let profiles: Vec<PathBuf> = targets.iter().flat_map(|t| profile_dirs(t)).collect();
    if profiles.is_empty() {
        let _ = writeln!(out, "no Cargo target dirs found");
    }
    profiles
}

/// One profile's result of a manual run. `trace`: phase timings; `verbose` or a dry run: paths.
fn print_report(
    out: &mut dyn Write,
    err: &mut dyn Write,
    p: &std::path::Path,
    r: &crate::prune::Report,
    dry_run: bool,
    verbose: bool,
    trace: bool,
) -> io::Result<()> {
    if trace {
        for (name, d) in &r.phases {
            writeln!(out, "  {:>8.1} ms  {name}", d.as_secs_f64() * 1000.0)?;
        }
    }
    if verbose || dry_run {
        for f in &r.removed {
            writeln!(out, "  {}", f.display())?;
        }
    }
    let what = if dry_run { "would remove" } else { "removed" };
    writeln!(
        out,
        "{}: {what} {} units, {} incremental dirs, {} ({} ms, cargo blocked {} ms)",
        p.display(),
        r.removed_units,
        r.removed_incremental,
        human(r.freed_bytes),
        r.elapsed.as_millis(),
        r.locked.as_millis(),
    )?;
    if !r.by_reason.is_empty() {
        writeln!(out, "  {}", r.breakdown())?;
    }
    if !r.pinned_now.is_empty() {
        let names: Vec<&str> = r
            .pinned_now
            .iter()
            .map(|k| k.split('|').next().unwrap_or(k))
            .collect();
        writeln!(out, "  pinned (alternating variants): {}", names.join(", "))?;
    }
    for e in r.errors.iter().take(5) {
        writeln!(err, "  warning: could not delete {e}")?;
    }
    Ok(())
}

fn die(msg: &str) -> ! {
    eprintln!("cargo-autoprune: {msg}");
    std::process::exit(2)
}
