//! Decide what is garbage in one Cargo profile dir (`target/debug`, `target/<triple>/release`, ...)
//! and delete it while holding Cargo's own build locks.
//!
//! Model: every compiled unit has a 16-hex metadata hash `H` shared by
//! `.fingerprint/<pkg>-H/`, `build/<pkg>-H/` and every `deps/*-H*` / `examples/*-H*` file.
//! Units of the same package and the same *kind* (lib build vs `check` rmeta, test, bin,
//! build-script compile/run) form a group. Inside a group the newest unit is live; units that are
//! older than the newest by more than `grace` were superseded (dependency/feature/profile change)
//! and are deleted. Units without a fingerprint can never be fresh for Cargo, so they are orphans.
//! If a deleted unit comes back (two legit variants built alternately, e.g. `-p x --features y`),
//! its group is pinned and no longer deduplicated.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const STATE_DIR: &str = ".autoprune";
const CARGO_LOCKS: [&str; 3] = [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];
const PIN_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const MAX_DELETED_LINES: usize = 50_000;
const MAX_LOG_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug)]
pub struct Config {
    /// Keep group members whose mtime is within `grace` of the newest member.
    pub grace: Duration,
    /// Optional cap for this profile dir; oldest units go first.
    pub max_size: Option<u64>,
    pub dry_run: bool,
    /// How long to wait for a running cargo to release the build lock.
    pub lock_wait: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            grace: Duration::from_secs(600),
            max_size: None,
            dry_run: false,
            lock_wait: Duration::from_secs(3600),
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub removed_units: usize,
    pub removed_incremental: usize,
    pub freed_bytes: u64,
    pub pinned_now: Vec<String>,
    pub removed: Vec<PathBuf>,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub enum Outcome {
    Done(Report),
    /// Another autoprune is already working on this dir, or cargo kept the lock past `lock_wait`.
    Busy,
}

#[derive(Debug, Default)]
struct Unit {
    name: Option<String>, // package name from .fingerprint
    stem: String,         // file stem from deps (fallback name)
    fp_kind: BTreeSet<String>,
    out_kind: BTreeSet<&'static str>,
    has_fingerprint: bool,
    paths: Vec<PathBuf>,
    mtime: Option<SystemTime>,
}

impl Unit {
    fn touch(&mut self, t: Option<SystemTime>) {
        if t > self.mtime {
            self.mtime = t;
        }
    }
    fn group(&self) -> String {
        let name = self.name.as_deref().unwrap_or(&self.stem);
        let fp: Vec<&str> = self.fp_kind.iter().map(String::as_str).collect();
        let out: Vec<&str> = self.out_kind.iter().copied().collect();
        format!("{name}|{}|{}", fp.join(","), out.join(","))
    }
}

struct Candidate {
    id: String, // what goes into the `deleted` log: hash or `inc:<dir>`
    paths: Vec<PathBuf>,
    mtime: Option<SystemTime>,
    incremental: bool,
}

/// Split `name` at the first `-<16 lowercase hex>` that is followed by `.` or the end.
pub fn split_hash(name: &str) -> Option<(&str, &str, &str)> {
    let b = name.as_bytes();
    let mut i = 0;
    while let Some(off) = name[i..].find('-') {
        let p = i + off;
        let h = p + 1..p + 17;
        if h.end <= b.len()
            && b[h.clone()]
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
            && (h.end == b.len() || b[h.end] == b'.')
        {
            return Some((&name[..p], &name[h.clone()], &name[h.end..]));
        }
        i = p + 1;
    }
    None
}

/// Kind of an output file in `deps/` from the part after the hash.
fn out_kind(rest: &str, path: &Path) -> Option<&'static str> {
    Some(match rest {
        ".rlib" => "rlib",
        ".rmeta" => "rmeta",
        ".dylib" | ".so" | ".dll" => "dylib",
        ".a" | ".lib" => "staticlib",
        ".exe" | ".wasm" => "bin",
        "" if is_executable(path) => "bin",
        _ => return None, // .d, .o, .dSYM, .pdb, ...
    })
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o100 != 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

fn mtime(p: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(p).and_then(|m| m.modified()).ok()
}

fn read_dir(p: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = fs::read_dir(p) else {
        return Vec::new();
    };
    rd.filter_map(|e| {
        let e = e.ok()?;
        Some((e.file_name().into_string().ok()?, e.path()))
    })
    .collect()
}

/// A dir is a Cargo profile dir if it has `.cargo-lock` and sits under a `CACHEDIR.TAG` dir.
pub fn is_profile_dir(p: &Path) -> bool {
    p.join(".cargo-lock").is_file()
        && p.ancestors()
            .skip(1)
            .take(3)
            .any(|a| a.join("CACHEDIR.TAG").is_file())
}

/// Profile dirs inside a target dir (`debug`, `release`, `<triple>/debug`, custom profiles).
pub fn profile_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (_, p) in read_dir(target) {
        if is_profile_dir(&p) {
            out.push(p);
        } else if p.is_dir() {
            out.extend(
                read_dir(&p)
                    .into_iter()
                    .map(|(_, q)| q)
                    .filter(|q| is_profile_dir(q)),
            );
        }
    }
    out.sort();
    out
}

pub fn prune(profile: &Path, cfg: &Config) -> io::Result<Outcome> {
    if !is_profile_dir(profile) {
        return Err(io::Error::other(format!(
            "{} is not a Cargo profile dir",
            profile.display()
        )));
    }
    let start = Instant::now();
    let state = profile.join(STATE_DIR);
    fs::create_dir_all(&state)?;
    let own = lock_file(&state.join("lock"))?;
    if own.try_lock().is_err() {
        return Ok(Outcome::Busy);
    }
    let Some(_cargo_locks) = acquire_cargo_locks(profile, cfg.lock_wait)? else {
        return Ok(Outcome::Busy);
    };

    let now = SystemTime::now();
    let deleted_before = read_lines(&state.join("deleted"));
    let mut pinned = read_pins(&state.join("pinned"), now);
    let (units, incremental) = scan(profile);

    let mut report = Report::default();
    // A previously deleted unit is back: its group holds two live variants.
    for (h, u) in &units {
        if u.has_fingerprint
            && deleted_before.contains(h.as_str())
            && !pinned.contains_key(&u.group())
        {
            pinned.insert(u.group(), now);
            report.pinned_now.push(u.group());
        }
    }
    for (dir, _, _) in &incremental {
        let key = inc_key(dir);
        if deleted_before.contains(&format!("inc:{dir}")) && !pinned.contains_key(&key) {
            pinned.insert(key.clone(), now);
            report.pinned_now.push(key);
        }
    }

    let (doomed, kept) = plan(units, incremental, &pinned, cfg);
    let doomed = apply_cap(doomed, kept, cfg.max_size);

    let mut new_deleted = Vec::new();
    for c in &doomed {
        for p in &c.paths {
            report.freed_bytes += size_of(p);
            if !cfg.dry_run {
                let r = if fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false) {
                    fs::remove_dir_all(p)
                } else {
                    fs::remove_file(p)
                };
                if let Err(e) = r {
                    if e.kind() != io::ErrorKind::NotFound {
                        eprintln!("cargo-autoprune: {}: {e}", p.display());
                    }
                }
            }
            report.removed.push(p.clone());
        }
        if c.incremental {
            report.removed_incremental += 1;
        } else {
            report.removed_units += 1;
        }
        new_deleted.push(c.id.clone());
    }
    report.elapsed = start.elapsed();
    if !cfg.dry_run {
        write_deleted(&state.join("deleted"), &deleted_before, &new_deleted)?;
        write_pins(&state.join("pinned"), &pinned)?;
        log(&state.join("log"), &report);
    }
    Ok(Outcome::Done(report))
}

type Incremental = Vec<(String, PathBuf, Option<SystemTime>)>;

fn scan(profile: &Path) -> (BTreeMap<String, Unit>, Incremental) {
    let mut units: BTreeMap<String, Unit> = BTreeMap::new();
    for (name, path) in read_dir(&profile.join(".fingerprint")) {
        let Some((pkg, h, "")) = split_hash(&name) else {
            continue;
        };
        let u = units.entry(h.to_owned()).or_default();
        u.name = Some(pkg.to_owned());
        u.has_fingerprint = true;
        // file names encode the target kind: lib-x, test-lib-x, bin-x, build-script-..., run-build-script-...
        u.fp_kind = read_dir(&path)
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| n != "invoked.timestamp")
            .collect();
        u.touch(mtime(&path));
        u.touch(mtime(&path.join("invoked.timestamp")));
        u.paths.push(path);
    }
    for (name, path) in read_dir(&profile.join("build")) {
        let Some((pkg, h, "")) = split_hash(&name) else {
            continue;
        };
        let u = units.entry(h.to_owned()).or_default();
        if u.stem.is_empty() {
            u.stem = pkg.to_owned();
        }
        u.touch(mtime(&path));
        u.paths.push(path);
    }
    for area in ["deps", "examples"] {
        for (name, path) in read_dir(&profile.join(area)) {
            let Some((stem, h, rest)) = split_hash(&name) else {
                continue;
            };
            let u = units.entry(h.to_owned()).or_default();
            if u.stem.is_empty() {
                u.stem = stem.to_owned();
            }
            if let Some(k) = out_kind(rest, &path) {
                u.out_kind.insert(k);
            }
            u.touch(mtime(&path));
            u.paths.push(path);
        }
    }
    let incremental = read_dir(&profile.join("incremental"))
        .into_iter()
        .filter(|(n, p)| n.contains('-') && p.is_dir())
        .map(|(n, p)| {
            let t = mtime(&p);
            (n, p, t)
        })
        .collect();
    (units, incremental)
}

fn inc_key(dir: &str) -> String {
    format!("inc|{}", dir.rsplit_once('-').map_or(dir, |(c, _)| c))
}

fn plan(
    units: BTreeMap<String, Unit>,
    incremental: Incremental,
    pinned: &HashMap<String, SystemTime>,
    cfg: &Config,
) -> (Vec<Candidate>, Vec<Candidate>) {
    let mut doomed = Vec::new();
    let mut groups: HashMap<String, Vec<Candidate>> = HashMap::new();
    for (h, u) in units {
        let c = Candidate {
            id: h,
            paths: u.paths.clone(),
            mtime: u.mtime,
            incremental: false,
        };
        if u.has_fingerprint {
            groups.entry(u.group()).or_default().push(c);
        } else {
            doomed.push(c); // no fingerprint: cargo rebuilds it anyway
        }
    }
    for (dir, path, t) in incremental {
        groups.entry(inc_key(&dir)).or_default().push(Candidate {
            id: format!("inc:{dir}"),
            paths: vec![path],
            mtime: t,
            incremental: true,
        });
    }
    let mut kept = Vec::new();
    for (key, members) in groups {
        let newest = members.iter().filter_map(|c| c.mtime).max();
        for c in members {
            let stale = match (newest, c.mtime) {
                (Some(n), Some(t)) => t + cfg.grace < n,
                _ => false,
            };
            if stale && !pinned.contains_key(&key) {
                doomed.push(c);
            } else {
                kept.push(c);
            }
        }
    }
    (doomed, kept)
}

/// Over the cap: delete the oldest kept units (incremental first at equal age) until under it.
fn apply_cap(
    mut doomed: Vec<Candidate>,
    mut kept: Vec<Candidate>,
    cap: Option<u64>,
) -> Vec<Candidate> {
    let Some(cap) = cap else { return doomed };
    let mut total: u64 = kept.iter().flat_map(|c| &c.paths).map(|p| size_of(p)).sum();
    kept.sort_by_key(|c| (c.mtime, !c.incremental));
    for c in kept {
        if total <= cap {
            break;
        }
        total -= c.paths.iter().map(|p| size_of(p)).sum::<u64>().min(total);
        doomed.push(c);
    }
    doomed
}

pub fn size_of(p: &Path) -> u64 {
    let Ok(m) = fs::symlink_metadata(p) else {
        return 0;
    };
    if !m.is_dir() {
        return m.len();
    }
    read_dir(p).into_iter().map(|(_, q)| size_of(&q)).sum()
}

fn lock_file(p: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(p)
}

/// Take every Cargo lock of the profile so no build runs while we delete. All-or-nothing with
/// retry, so we never hold one lock while waiting for another (no deadlock with cargo).
fn acquire_cargo_locks(profile: &Path, wait: Duration) -> io::Result<Option<Vec<File>>> {
    let deadline = Instant::now() + wait;
    loop {
        let mut held = Vec::new();
        let mut ok = true;
        for name in CARGO_LOCKS {
            let p = profile.join(name);
            if !p.exists() {
                continue;
            }
            let f = lock_file(&p)?;
            if f.try_lock().is_err() {
                ok = false;
                break;
            }
            held.push(f);
        }
        if ok {
            return Ok(Some(held));
        }
        drop(held);
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn read_lines(p: &Path) -> BTreeSet<String> {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn write_deleted(p: &Path, before: &BTreeSet<String>, new: &[String]) -> io::Result<()> {
    if new.is_empty() {
        return Ok(());
    }
    let mut lines: Vec<String> = fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    lines.extend(new.iter().filter(|n| !before.contains(*n)).cloned());
    let skip = lines.len().saturating_sub(MAX_DELETED_LINES);
    atomic_write(p, &(lines[skip..].join("\n") + "\n"))
}

fn read_pins(p: &Path, now: SystemTime) -> HashMap<String, SystemTime> {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, ts) = l.rsplit_once('\t')?;
            let t = UNIX_EPOCH + Duration::from_secs(ts.parse().ok()?);
            (t + PIN_TTL > now).then(|| (k.to_owned(), t))
        })
        .collect()
}

fn write_pins(p: &Path, pins: &HashMap<String, SystemTime>) -> io::Result<()> {
    let mut s = String::new();
    for (k, t) in pins {
        s += &format!(
            "{k}\t{}\n",
            t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
        );
    }
    atomic_write(p, &s)
}

fn atomic_write(p: &Path, s: &str) -> io::Result<()> {
    let tmp = p.with_extension("tmp");
    fs::write(&tmp, s)?;
    fs::rename(tmp, p)
}

fn log(p: &Path, r: &Report) {
    if fs::metadata(p)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = fs::rename(p, p.with_extension("old"));
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(
            f,
            "{ts} units={} incremental={} freed={} pinned={} took={}ms",
            r.removed_units,
            r.removed_incremental,
            human(r.freed_bytes),
            r.pinned_now.len(),
            r.elapsed.as_millis()
        );
    }
}

pub fn human(b: u64) -> String {
    let (v, u) = match b {
        b if b >= 1 << 30 => (b as f64 / (1u64 << 30) as f64, "GiB"),
        b if b >= 1 << 20 => (b as f64 / (1u64 << 20) as f64, "MiB"),
        b if b >= 1 << 10 => (b as f64 / 1024.0, "KiB"),
        b => return format!("{b}B"),
    };
    format!("{v:.1}{u}")
}

/// `20G`, `512M`, `100k`, plain bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s
        .trim()
        .trim_end_matches(['b', 'B'])
        .trim_end_matches(['i', 'I']);
    let (num, mul) = match s.chars().last()? {
        'k' | 'K' => (&s[..s.len() - 1], 1u64 << 10),
        'm' | 'M' => (&s[..s.len() - 1], 1 << 20),
        'g' | 'G' => (&s[..s.len() - 1], 1 << 30),
        't' | 'T' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|n| (n * mul as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccc";

    struct Fx {
        root: PathBuf,
        prof: PathBuf,
    }

    impl Fx {
        fn new(tag: &str) -> Fx {
            let root =
                std::env::temp_dir().join(format!("autoprune-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let prof = root.join("target/debug");
            for d in ["deps", ".fingerprint", "build", "incremental", "examples"] {
                fs::create_dir_all(prof.join(d)).unwrap();
            }
            fs::write(
                root.join("target/CACHEDIR.TAG"),
                "Signature: 8a477f597d28d172789f06886806bc55",
            )
            .unwrap();
            fs::write(prof.join(".cargo-lock"), "").unwrap();
            Fx { root, prof }
        }
        fn file(&self, rel: &str, age_secs: u64) -> PathBuf {
            let p = self.prof.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, vec![0u8; 1000]).unwrap();
            set_age(&p, age_secs);
            p
        }
        fn dir(&self, rel: &str, age_secs: u64) -> PathBuf {
            let p = self.prof.join(rel);
            fs::create_dir_all(&p).unwrap();
            set_age(&p, age_secs);
            p
        }
        /// A lib unit: fingerprint + rlib/rmeta (or rmeta only for `check`) + .d + .o.
        fn lib(&self, pkg: &str, h: &str, age: u64, check: bool) {
            let fp = format!(".fingerprint/{pkg}-{h}");
            self.file(&format!("{fp}/lib-{pkg}"), age);
            self.file(&format!("{fp}/lib-{pkg}.json"), age);
            self.file(&format!("{fp}/invoked.timestamp"), age);
            set_age(&self.prof.join(&fp), age);
            if !check {
                self.file(&format!("deps/lib{pkg}-{h}.rlib"), age);
                self.file(&format!("deps/{pkg}-{h}.{pkg}.cgu.0.rcgu.o"), age);
            }
            self.file(&format!("deps/lib{pkg}-{h}.rmeta"), age);
            self.file(&format!("deps/{pkg}-{h}.d"), age);
        }
        fn exists(&self, rel: &str) -> bool {
            self.prof.join(rel).exists()
        }
        fn run(&self, cfg: &Config) -> Report {
            match prune(&self.prof, cfg).unwrap() {
                Outcome::Done(r) => r,
                Outcome::Busy => panic!("busy"),
            }
        }
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn set_age(p: &Path, secs: u64) {
        let t = SystemTime::now() - Duration::from_secs(secs);
        File::options()
            .read(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn cfg() -> Config {
        Config {
            lock_wait: Duration::ZERO,
            ..Config::default()
        }
    }

    #[test]
    fn split_hash_cases() {
        assert_eq!(
            split_hash(&format!("libfoo-{A}.rlib")),
            Some(("libfoo", A, ".rlib"))
        );
        assert_eq!(split_hash(&format!("foo-{A}")), Some(("foo", A, "")));
        assert_eq!(
            split_hash(&format!("foo-{A}.x.rcgu.o")),
            Some(("foo", A, ".x.rcgu.o"))
        );
        assert_eq!(
            split_hash(&format!("proc-macro2-{A}")),
            Some(("proc-macro2", A, ""))
        );
        assert_eq!(split_hash("foo-ABCDEFabcdef0123"), None);
        assert_eq!(split_hash(&format!("foo-{A}x")), None);
        assert_eq!(split_hash("foo"), None);
    }

    #[test]
    fn superseded_lib_removed_newest_kept() {
        let fx = Fx::new("superseded");
        fx.lib("serde", A, 7200, false);
        fx.lib("serde", B, 10, false);
        let r = fx.run(&cfg());
        assert_eq!(r.removed_units, 1);
        assert!(!fx.exists(&format!(".fingerprint/serde-{A}")));
        assert!(!fx.exists(&format!("deps/libserde-{A}.rlib")));
        assert!(!fx.exists(&format!("deps/serde-{A}.serde.cgu.0.rcgu.o")));
        assert!(fx.exists(&format!("deps/libserde-{B}.rlib")));
        assert!(fx.exists(&format!(".fingerprint/serde-{B}")));
    }

    #[test]
    fn check_and_build_variants_both_kept() {
        let fx = Fx::new("variants");
        fx.lib("libc", A, 7200, true); // cargo check (rmeta only)
        fx.lib("libc", B, 10, false); // cargo build
        let r = fx.run(&cfg());
        assert_eq!(r.removed_units, 0);
        assert!(fx.exists(&format!("deps/liblibc-{A}.rmeta")));
    }

    #[test]
    fn within_grace_kept() {
        let fx = Fx::new("grace");
        fx.lib("serde", A, 300, false);
        fx.lib("serde", B, 10, false);
        assert_eq!(fx.run(&cfg()).removed_units, 0);
    }

    #[test]
    fn orphans_removed_outside_untouched() {
        let fx = Fx::new("orphans");
        fx.file(&format!("deps/mytest-{A}.x.rcgu.o"), 10); // exe already gone, no fingerprint
        fx.file(&format!("deps/mytest-{A}.d"), 10);
        fx.file("deps/README", 10); // no hash: not ours
        fx.file("mybin", 10); // uplifted output in profile root
        fs::write(fx.root.join("target/keep.txt"), "x").unwrap();
        let r = fx.run(&cfg());
        assert_eq!(r.removed_units, 1);
        assert!(!fx.exists(&format!("deps/mytest-{A}.d")));
        assert!(fx.exists("deps/README") && fx.exists("mybin"));
        assert!(fx.root.join("target/keep.txt").exists());
    }

    #[test]
    fn build_script_dirs_follow_fingerprint() {
        let fx = Fx::new("buildscript");
        for (h, age) in [(A, 7200), (B, 10)] {
            fx.file(
                &format!(".fingerprint/libc-{h}/run-build-script-build-script-build"),
                age,
            );
            set_age(&fx.prof.join(format!(".fingerprint/libc-{h}")), age);
            fx.file(&format!("build/libc-{h}/output"), age);
            set_age(&fx.prof.join(format!("build/libc-{h}")), age);
        }
        fx.dir(&format!("build/libc-{C}"), 10); // no fingerprint
        assert_eq!(fx.run(&cfg()).removed_units, 2);
        assert!(!fx.exists(&format!("build/libc-{A}")) && !fx.exists(&format!("build/libc-{C}")));
        assert!(fx.exists(&format!("build/libc-{B}/output")));
    }

    #[test]
    fn test_exe_with_dsym_superseded() {
        let fx = Fx::new("exe");
        for (h, age) in [(A, 7200), (B, 10)] {
            fx.file(
                &format!(".fingerprint/gs-at-{h}/test-integration-test-urc"),
                age,
            );
            set_age(&fx.prof.join(format!(".fingerprint/gs-at-{h}")), age);
            let exe = fx.file(&format!("deps/urc-{h}"), age);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
            }
            fx.file(&format!("deps/urc-{h}.dSYM/Contents/Info.plist"), age);
            set_age(&fx.prof.join(format!("deps/urc-{h}.dSYM")), age);
        }
        assert_eq!(fx.run(&cfg()).removed_units, 1);
        assert!(!fx.exists(&format!("deps/urc-{A}.dSYM")) && !fx.exists(&format!("deps/urc-{A}")));
        assert!(fx.exists(&format!("deps/urc-{B}")));
    }

    #[test]
    fn returning_variant_gets_pinned() {
        let fx = Fx::new("pin");
        fx.lib("tokio", A, 7200, false);
        fx.lib("tokio", B, 3600, false);
        assert_eq!(fx.run(&cfg()).removed_units, 1);
        fx.lib("tokio", A, 10, false); // rebuilt: it was a live variant
        let r = fx.run(&cfg());
        assert_eq!(r.removed_units, 0);
        assert_eq!(r.pinned_now.len(), 1);
        assert!(fx.exists(&format!("deps/libtokio-{B}.rlib")));
        assert_eq!(fx.run(&cfg()).removed_units, 0); // pin persists
    }

    #[test]
    fn incremental_dedup() {
        let fx = Fx::new("inc");
        fx.dir("incremental/gs_modem-1abc/s-old", 7200);
        set_age(&fx.prof.join("incremental/gs_modem-1abc"), 7200);
        fx.dir("incremental/gs_modem-2def/s-new", 10);
        set_age(&fx.prof.join("incremental/gs_modem-2def"), 10);
        fx.dir("incremental/other-3xyz", 7200);
        let r = fx.run(&cfg());
        assert_eq!(r.removed_incremental, 1);
        assert!(!fx.exists("incremental/gs_modem-1abc") && fx.exists("incremental/gs_modem-2def"));
        assert!(fx.exists("incremental/other-3xyz"));
    }

    #[test]
    fn size_cap_removes_oldest() {
        let fx = Fx::new("cap");
        fx.lib("a", A, 9000, false);
        fx.lib("b", B, 5000, false);
        fx.lib("c", C, 10, false);
        // each lib: 4 files of 1000 B in fingerprint + rlib + o + rmeta + d = ~7000 B
        let r = fx.run(&Config {
            max_size: Some(15_000),
            ..cfg()
        });
        assert_eq!(r.removed_units, 1);
        assert!(!fx.exists(&format!("deps/liba-{A}.rlib")));
        assert!(fx.exists(&format!("deps/libb-{B}.rlib")));
    }

    #[test]
    fn dry_run_deletes_nothing() {
        let fx = Fx::new("dry");
        fx.lib("serde", A, 7200, false);
        fx.lib("serde", B, 10, false);
        let r = fx.run(&Config {
            dry_run: true,
            ..cfg()
        });
        assert_eq!(r.removed_units, 1);
        assert!(fx.exists(&format!("deps/libserde-{A}.rlib")));
    }

    #[test]
    fn busy_while_cargo_holds_lock() {
        let fx = Fx::new("busy");
        fx.lib("serde", A, 7200, false);
        fx.lib("serde", B, 10, false);
        let held = lock_file(&fx.prof.join(".cargo-lock")).unwrap();
        held.lock().unwrap();
        assert!(matches!(prune(&fx.prof, &cfg()).unwrap(), Outcome::Busy));
        assert!(fx.exists(&format!("deps/libserde-{A}.rlib")));
        drop(held);
        assert_eq!(fx.run(&cfg()).removed_units, 1);
    }

    #[test]
    fn refuses_non_cargo_dir() {
        let dir = std::env::temp_dir().join(format!("autoprune-notcargo-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".cargo-lock"), "").unwrap(); // lock but no CACHEDIR.TAG above
        assert!(prune(&dir, &cfg()).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_sizes() {
        assert_eq!(parse_size("20G"), Some(20 << 30));
        assert_eq!(parse_size("512MiB"), Some(512 << 20));
        assert_eq!(parse_size("1.5g"), Some(3 << 29));
        assert_eq!(parse_size("1000"), Some(1000));
        assert_eq!(parse_size("x"), None);
    }
}
