//! `cargo autoprune [--dry-run] [--max-size SIZE] [--grace SECS] [DIR]`.

use crate::fsutil::{entries, human, parse_size, size_of};
use crate::layout::{find_target_dirs, is_profile_dir, profile_dirs, STATE_DIR};
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
/// (newest fingerprint dir). Skipped while any of its profiles is locked by a running cargo.
fn remove_if_idle(target: &std::path::Path, days: u64, dry_run: bool) -> bool {
    let profiles = profile_dirs(target);
    let newest = profiles
        .iter()
        .flat_map(|p| [p.join(".fingerprint"), p.join(".autoprune").join("log")])
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
                let ago = now_secs().saturating_sub(ts.parse().unwrap_or(0));
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

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testutil::{set_age, Fx};
    use std::ffi::OsString;

    fn parse(args: &[&str]) -> Result<Cmd, String> {
        parse_args(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }
    fn opts(args: &[&str]) -> Opts {
        match parse(args) {
            Ok(Cmd::Prune(o)) => o,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn options_parse() {
        let o = opts(&[]);
        assert!(!o.cfg.dry_run && !o.verbose && !o.recursive && !o.cfg.drop_incremental);
        assert_eq!((o.cfg.max_size, o.remove_idle), (None, None));
        assert_eq!(o.cfg.lock_wait, Duration::from_secs(600));
        let o = opts(&["-n", "-v", "-r", "--drop-incremental", "--max-size", "2G"]);
        assert!(o.cfg.dry_run && o.verbose && o.recursive && o.cfg.drop_incremental);
        assert_eq!(o.cfg.max_size, Some(2 << 30));
        let o = opts(&[
            "--dry-run",
            "--verbose",
            "--recursive",
            "a",
            "--grace",
            "5",
            "b",
        ]);
        assert!(o.cfg.dry_run && o.verbose && o.recursive);
        assert_eq!(o.cfg.grace, Duration::from_secs(5));
        assert_eq!(o.dirs, [PathBuf::from("a"), PathBuf::from("b")]);
        let o = opts(&["--grace", "9", "--ci", "--remove-idle", "30"]);
        assert_eq!(o.cfg.grace, Duration::ZERO);
        assert!(o.cfg.drop_incremental);
        assert_eq!(o.remove_idle, Some(30));
    }

    #[test]
    fn commands_and_bad_options() {
        assert!(matches!(parse(&["-h"]), Ok(Cmd::Help)));
        assert!(matches!(parse(&["x", "--help"]), Ok(Cmd::Help)));
        assert!(matches!(parse(&["-V"]), Ok(Cmd::Version)));
        assert!(matches!(parse(&["--version"]), Ok(Cmd::Version)));
        assert!(matches!(parse(&["status"]), Ok(Cmd::Status(None))));
        assert!(
            matches!(parse(&["status", "d"]), Ok(Cmd::Status(Some(d))) if d.as_os_str() == "d")
        );
        assert!(parse(&["--bogus"])
            .unwrap_err()
            .starts_with("unknown option --bogus"));
        assert!(parse(&["--max-size", "20"])
            .unwrap_err()
            .contains("below 1 MiB"));
        assert!(parse(&["--grace", "x"])
            .unwrap_err()
            .contains("--grace takes seconds"));
        assert!(parse(&["--remove-idle"])
            .unwrap_err()
            .contains("takes days"));
    }

    #[test]
    fn ago_text_units() {
        let cases = [
            (0, "0 s"),
            (119, "119 s"),
            (120, "2 min"),
            (7199, "119 min"),
            (7200, "2 h"),
            (172_799, "47 h"),
            (172_800, "2 days"),
        ];
        for (s, t) in cases {
            assert_eq!(ago_text(s), t, "{s}");
        }
        assert!(now_secs() > 1_700_000_000);
    }

    #[test]
    fn bad_env_lists_unparsable_settings() {
        fn env(pairs: &[(&str, &str)]) -> Vec<String> {
            bad_env(|k| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string()))
        }
        assert!(env(&[]).is_empty());
        let ok = [
            ("CARGO_AUTOPRUNE_INTERVAL", " 60 "),
            ("CARGO_AUTOPRUNE_GRACE", "0"),
            ("CARGO_AUTOPRUNE_MAX_SIZE", "1G"),
        ];
        assert!(env(&ok).is_empty());
        let bad = env(&[
            ("CARGO_AUTOPRUNE_INTERVAL", "5m"),
            ("CARGO_AUTOPRUNE_GRACE", "x"),
            ("CARGO_AUTOPRUNE_MAX_SIZE", "20"),
        ]);
        assert_eq!(bad.len(), 3, "{bad:?}");
        assert!(bad[0].starts_with("CARGO_AUTOPRUNE_INTERVAL=\"5m\""));
        assert!(bad[1].starts_with("CARGO_AUTOPRUNE_GRACE=\"x\""));
        assert!(bad[2].contains("MAX_SIZE ignored"));
    }

    #[test]
    fn wrapper_detection() {
        let fx = Fx::new("cli-wrapper");
        let home = fx.root.join("home");
        fs::create_dir_all(home.join(".cargo")).unwrap();
        let h = home.clone();
        let detect = |vars: &[(&str, &str)], cwd: Option<PathBuf>| {
            let vars: Vec<(String, OsString)> = vars
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        OsString::from(v.replace("$H", h.to_str().unwrap())),
                    )
                })
                .collect();
            configured_wrapper(|k| vars.iter().find(|p| p.0 == k).map(|p| p.1.clone()), cwd)
        };
        assert_eq!(
            detect(&[("RUSTC_WRAPPER", "/bin/cargo-autoprune")], None).as_deref(),
            Some("RUSTC_WRAPPER=/bin/cargo-autoprune")
        );
        assert_eq!(detect(&[], None), None);
        let cfg_file = home.join(".cargo/config.toml");
        let found = Some(format!("enabled in {}", cfg_file.display()));
        for (text, on) in [
            ("[build]\nrustc-wrapper = \"cargo-autoprune\"\n", true),
            ("  # rustc-wrapper = \"cargo-autoprune\"\n", false),
            ("rustc-wrapper = \"sccache\"\n", false),
            ("x = \"cargo-autoprune\"\n", false),
        ] {
            fs::write(&cfg_file, text).unwrap();
            let want = if on { found.clone() } else { None };
            assert_eq!(
                detect(&[("HOME", "$H"), ("RUSTC_WRAPPER", "sccache")], None),
                want,
                "{text}"
            );
            let cargo_home = &[("CARGO_HOME", "$H/.cargo")];
            assert_eq!(detect(cargo_home, None), want, "{text}");
        }
        fs::remove_file(&cfg_file).unwrap();
        // a project config above the working dir
        let proj = fx.root.join("proj");
        fs::create_dir_all(proj.join(".cargo")).unwrap();
        fs::create_dir_all(proj.join("sub")).unwrap();
        fs::write(
            proj.join(".cargo/config.toml"),
            "rustc-wrapper = \"cargo-autoprune\"",
        )
        .unwrap();
        assert_eq!(
            detect(&[("HOME", "$H")], Some(proj.join("sub"))),
            Some(format!(
                "enabled in {}",
                proj.join(".cargo/config.toml").display()
            ))
        );
    }

    fn status_text(fx: &Fx) -> String {
        let mut out = Vec::new();
        status(Some(fx.prof.clone()), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn status_reports_prune_log_and_pins() {
        let fx = Fx::new("cli-status");
        fx.lib("foo", crate::testutil::A, 7200, false);
        fx.lib("foo", crate::testutil::B, 0, false);
        let s = status_text(&fx);
        assert!(s.contains("2 units"), "{s}");
        assert!(
            s.contains("reclaimable now: ")
                && s.contains("(1 units, 0 incremental dirs: superseded 1)"),
            "{s}"
        );
        assert!(s.contains("last prune: never"), "{s}");
        assert!(!s.contains("pinned groups"), "{s}");
        assert!(
            fx.exists("deps/libfoo-aaaaaaaaaaaaaaaa.rlib"),
            "status is a dry run"
        );
        let state = fx.prof.join(STATE_DIR);
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join("log"),
            format!("1 old\n{} removed 3 units\n", now_secs() - 300),
        )
        .unwrap();
        fs::write(state.join("pinned"), "foo|lib|x 1\nbar|lib|y 2\n").unwrap();
        let s = status_text(&fx);
        assert!(s.contains("last prune: 5 min ago, removed 3 units"), "{s}");
        assert!(
            s.contains("pinned groups (alternating variants, 24 h): foo, bar"),
            "{s}"
        );
    }

    #[test]
    fn status_does_not_wait_for_a_running_build() {
        let fx = Fx::new("cli-status-busy");
        let _held = crate::lock::acquire_cargo_locks(&fx.prof, Duration::ZERO)
            .unwrap()
            .unwrap();
        let s = status_text(&fx);
        assert!(s.contains("reclaimable now: unknown"), "{s}");
    }

    #[test]
    fn idle_target_dirs() {
        let fx = Fx::new("cli-idle");
        let target = fx.root.join("target");
        set_age(&fx.prof.join(".fingerprint"), 36 * 3600);
        assert!(
            !remove_if_idle(&target, 2, true),
            "1.5 days old, 2 required"
        );
        assert!(remove_if_idle(&target, 1, true));
        assert!(target.exists(), "dry run");
        // a running cargo keeps it
        let held = crate::lock::acquire_cargo_locks(&fx.prof, Duration::ZERO).unwrap();
        assert!(!remove_if_idle(&target, 1, false));
        drop(held);
        assert!(remove_if_idle(&target, 1, false));
        assert!(!target.exists());
    }

    #[test]
    fn profiles_of_a_dir_argument() {
        let fx = Fx::new("cli-resolve");
        let want = vec![fx.prof.clone()];
        assert_eq!(resolve_profiles(Some(fx.prof.clone())), want, "profile dir");
        assert_eq!(
            resolve_profiles(Some(fx.root.join("target"))),
            want,
            "target dir"
        );
        assert_eq!(resolve_profiles(Some(fx.root.clone())), want, "project dir");
    }

    fn report_text(
        r: &crate::prune::Report,
        dry: bool,
        verbose: bool,
        trace: bool,
    ) -> (String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        print_report(
            &mut out,
            &mut err,
            std::path::Path::new("/t/debug"),
            r,
            dry,
            verbose,
            trace,
        )
        .unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn manual_run_report() {
        let mut r = crate::prune::Report {
            removed_units: 2,
            removed_incremental: 1,
            freed_bytes: 2048,
            elapsed: Duration::from_millis(7),
            locked: Duration::from_millis(3),
            phases: vec![("plan", Duration::from_micros(1500))],
            removed: vec![PathBuf::from("/t/debug/deps/x")],
            ..Default::default()
        };
        let (out, err) = report_text(&r, false, false, false);
        assert_eq!(
            out,
            "/t/debug: removed 2 units, 1 incremental dirs, 2.0KiB (7 ms, cargo blocked 3 ms)\n"
        );
        assert!(err.is_empty());
        let (out, _) = report_text(&r, true, false, false);
        assert_eq!(out, "  /t/debug/deps/x\n/t/debug: would remove 2 units, 1 incremental dirs, 2.0KiB (7 ms, cargo blocked 3 ms)\n");
        let (out, _) = report_text(&r, false, true, false);
        assert!(
            out.starts_with("  /t/debug/deps/x\n") && !out.contains(" ms  plan"),
            "{out}"
        );
        let (out, _) = report_text(&r, false, false, true);
        assert!(
            out.starts_with("       1.5 ms  plan\n/t/debug: removed"),
            "{out}"
        );
        r.by_reason.insert("orphan", 2);
        r.pinned_now = vec!["tokio|lib|x".into(), "serde|lib|y".into()];
        r.errors = (0..7).map(|i| format!("e{i}")).collect();
        let (out, err) = report_text(&r, false, false, false);
        assert!(
            out.ends_with("ms)\n  orphan 2\n  pinned (alternating variants): tokio, serde\n"),
            "{out}"
        );
        assert_eq!(err.lines().count(), 5);
        assert!(err.starts_with("  warning: could not delete e0\n"));
    }

    #[test]
    fn profiles_of_a_manual_run() {
        let fx = Fx::new("cli-select");
        let mut out = Vec::new();
        let target = fx.root.join("target");
        assert_eq!(
            select_profiles(vec![fx.root.clone()], false, None, false, &mut out),
            std::slice::from_ref(&fx.prof)
        );
        let two = select_profiles(
            vec![fx.root.clone(), target.clone()],
            false,
            None,
            false,
            &mut out,
        );
        assert_eq!(two, [fx.prof.clone(), fx.prof.clone()]);
        assert_eq!(
            select_profiles(vec![fx.root.clone()], true, None, false, &mut out),
            std::slice::from_ref(&fx.prof)
        );
        assert!(out.is_empty());
        // idle for 2 days: a dry run keeps the dir but prunes nothing in it
        set_age(&fx.prof.join(".fingerprint"), 2 * 86400);
        assert!(select_profiles(vec![fx.root.clone()], true, Some(1), true, &mut out).is_empty());
        assert_eq!(
            String::from_utf8(out.clone()).unwrap(),
            "no Cargo target dirs found\n"
        );
        assert!(target.exists());
        assert_eq!(
            select_profiles(vec![fx.root.clone()], true, Some(3), true, &mut out),
            std::slice::from_ref(&fx.prof)
        );
    }

    #[test]
    fn caps_need_a_sane_size() {
        assert_eq!(parse_cap("20G"), Ok(20 << 30));
        assert!(parse_cap("20").unwrap_err().contains("below 1 MiB"));
        assert!(parse_cap("lots").unwrap_err().contains("cannot parse"));
    }
}
