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
//!
//! When the workspace `Cargo.lock` is next to `target/`, only workspace (path) packages are
//! deduplicated; registry/git units stay only while reachable from a kept workspace unit through
//! the `deps` edges of `.fingerprint/*/*.json` (each edge names the dependency's fingerprint value,
//! the hex in the sibling file). That drops removed dependencies and deps of superseded units.
//! The `rustc` hash in the json separates `clippy-driver` from `rustc` builds; a hash with no
//! writes for `RUSTC_RETIRE` before the newest unit (old toolchain) is dropped entirely.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const STATE_DIR: &str = ".autoprune";
const CARGO_LOCKS: [&str; 3] = [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];
const PIN_TTL: Duration = Duration::from_secs(24 * 3600);
/// A deleted unit rebuilt within this window was a live variant, not a revert done later.
const PIN_WINDOW: Duration = Duration::from_secs(30 * 60);
/// Units built through a workspace wrapper (clippy-driver) are tagged here by hash.
pub const TAGS_DIR: &str = "tags";
const MAX_LOG_BYTES: u64 = 256 * 1024;
/// A rustc/clippy-driver hash not used for this long (relative to the newest unit) is retired.
const RUSTC_RETIRE: Duration = Duration::from_secs(14 * 24 * 3600);

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
    rustc: u64,            // hash of `rustc -vV` (differs between toolchains)
    sig: String, // fingerprint json minus deps/local/path: features, profile, rustflags, ...
    tag: String, // workspace wrapper that built it (clippy-driver), from TAGS_DIR
    krate: Option<String>, // rustc crate name (stem of `deps/<crate>-H.d`), for incremental dirs
    fp_value: Option<u64>, // this unit's fingerprint value
    dep_values: Vec<u64>, // fingerprint values of its dependencies
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
        format!(
            "{name}|{}|{}|{}|{}|{}",
            fp.join(","),
            out.join(","),
            self.rustc,
            self.tag,
            self.sig
        )
    }
}

struct Candidate {
    krate: Option<String>,
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
    let state = profile.join(STATE_DIR);
    fs::create_dir_all(&state)?;
    let own = lock_file(&state.join("lock"))?;
    if own.try_lock().is_err() {
        return Ok(Outcome::Busy);
    }
    let Some(_cargo_locks) = acquire_cargo_locks(profile, cfg.lock_wait)? else {
        return Ok(Outcome::Busy);
    };
    let start = Instant::now(); // prune time only, not the wait for cargo

    let now = SystemTime::now();
    let deleted_before = read_deleted(&state.join("deleted"), now);
    let mut pinned = read_pins(&state.join("pinned"), now);
    let locals = local_packages(profile);
    let (units, incremental) = scan(profile, locals.is_some());

    let mut report = Report::default();
    // A previously deleted unit is back: its group holds two live variants.
    for (h, u) in &units {
        let local = locals
            .as_ref()
            .is_none_or(|l| l.contains(u.name.as_deref().unwrap_or("")));
        if u.has_fingerprint
            && local
            && deleted_before.contains_key(h.as_str())
            && !pinned.contains_key(&u.group())
        {
            pinned.insert(u.group(), now);
            report.pinned_now.push(u.group());
        }
    }
    let (doomed, kept) = plan(units, incremental, &pinned, locals.as_ref(), cfg);
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
        let mut deleted = deleted_before;
        let ts = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        deleted.extend(new_deleted.into_iter().map(|id| (id, ts)));
        write_deleted(&state.join("deleted"), &deleted)?;
        clean_tags(&state.join(TAGS_DIR), profile);
        write_pins(&state.join("pinned"), &pinned)?;
        log(&state.join("log"), &report);
    }
    Ok(Outcome::Done(report))
}

type Incremental = Vec<(String, PathBuf, Option<SystemTime>)>;

fn scan(profile: &Path, with_sig: bool) -> (BTreeMap<String, Unit>, Incremental) {
    let mut units: BTreeMap<String, Unit> = BTreeMap::new();
    let tags = profile.join(STATE_DIR).join(TAGS_DIR);
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
            .filter(|n| n != "invoked.timestamp" && !n.starts_with("output-"))
            .collect();
        if let Some(json) = u.fp_kind.iter().find(|n| n.ends_with(".json")).cloned() {
            let text = fs::read_to_string(path.join(&json)).unwrap_or_default();
            (u.rustc, u.dep_values) = parse_fingerprint_json(&text);
            if with_sig {
                u.sig = json_signature(&text);
            }
            u.fp_value = fs::read_to_string(path.join(json.trim_end_matches(".json")))
                .ok()
                .and_then(|h| parse_le_hex(h.trim()));
        }
        u.tag = fs::read_to_string(tags.join(h)).unwrap_or_default();
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
        if path.join("build-script-build").exists() {
            u.krate = Some("build_script_build".to_owned());
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
            if rest == ".d" {
                u.krate = Some(stem.to_owned());
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

fn plan(
    units: BTreeMap<String, Unit>,
    incremental: Incremental,
    pinned: &HashMap<String, SystemTime>,
    locals: Option<&HashSet<String>>,
    cfg: &Config,
) -> (Vec<Candidate>, Vec<Candidate>) {
    let mut doomed = Vec::new();
    let mut kept = Vec::new();
    let newest_all = units
        .values()
        .filter(|u| u.has_fingerprint)
        .filter_map(|u| u.mtime)
        .max();
    let mut rustc_newest: HashMap<u64, SystemTime> = HashMap::new();
    for u in units.values().filter(|u| u.has_fingerprint) {
        if let Some(t) = u.mtime {
            let e = rustc_newest.entry(u.rustc).or_insert(t);
            *e = (*e).max(t);
        }
    }
    let retired = |u: &Unit| match (newest_all, rustc_newest.get(&u.rustc)) {
        (Some(n), Some(&r)) => r + RUSTC_RETIRE < n,
        _ => false,
    };

    // Build-script *run* units never reach rustc, so they inherit the clippy tag of the compiled
    // build script they depend on.
    let tag_by_value: HashMap<u64, String> = units
        .values()
        .filter(|u| !u.tag.is_empty())
        .filter_map(|u| Some((u.fp_value?, u.tag.clone())))
        .collect();
    let mut units = units;
    for u in units.values_mut() {
        if u.tag.is_empty() && u.fp_kind.iter().any(|k| k.starts_with("run-build-script")) {
            if let Some(t) = u.dep_values.iter().find_map(|v| tag_by_value.get(v)) {
                u.tag = t.clone();
            }
        }
    }

    let mut groups: HashMap<String, Vec<(Candidate, Vec<u64>)>> = HashMap::new();
    let mut registry: Vec<(Candidate, Option<u64>, Vec<u64>)> = Vec::new();
    for (h, u) in units {
        let local = locals.is_none_or(|l| l.contains(u.name.as_deref().unwrap_or("")));
        let c = Candidate {
            krate: if local { u.krate.clone() } else { None }, // only workspace crates are incremental
            id: h,
            paths: u.paths.clone(),
            mtime: u.mtime,
            incremental: false,
        };
        if !u.has_fingerprint || retired(&u) {
            doomed.push(c); // no fingerprint: cargo rebuilds it anyway
        } else if local {
            groups.entry(u.group()).or_default().push((c, u.dep_values));
        } else {
            registry.push((c, u.fp_value, u.dep_values));
        }
    }

    // Dedup workspace units (all units when locality is unknown).
    let mut roots: Vec<u64> = Vec::new();
    for (key, members) in groups {
        let newest = members.iter().filter_map(|(c, _)| c.mtime).max();
        for (c, deps) in members {
            let stale = matches!((newest, c.mtime), (Some(n), Some(t)) if t + cfg.grace < n);
            if stale && !pinned.contains_key(&key) {
                doomed.push(c);
            } else {
                roots.extend(deps);
                kept.push(c);
            }
        }
    }

    // Registry units live only while a kept unit depends on them (transitively).
    if !registry.is_empty() {
        let by_value: HashMap<u64, usize> = registry
            .iter()
            .enumerate()
            .filter_map(|(i, (_, v, _))| v.map(|v| (v, i)))
            .collect();
        let mut live = vec![false; registry.len()];
        while let Some(v) = roots.pop() {
            if let Some(&i) = by_value.get(&v) {
                if !live[i] {
                    live[i] = true;
                    roots.extend(registry[i].2.iter().copied());
                }
            }
        }
        for ((c, _, _), live) in registry.into_iter().zip(live) {
            if live {
                kept.push(c)
            } else {
                doomed.push(c)
            }
        }
    }

    // `incremental/<crate>-<stable id>` cannot be mapped by hash. Its session is written when its
    // unit is compiled, so it follows the unit of the same crate with the closest mtime.
    let mut by_crate: HashMap<&str, Vec<(SystemTime, bool)>> = HashMap::new();
    for (list, is_doomed) in [(&doomed, true), (&kept, false)] {
        for c in list.iter() {
            if let (Some(k), Some(t)) = (c.krate.as_deref(), c.mtime) {
                by_crate.entry(k).or_default().push((t, is_doomed));
            }
        }
    }
    let gap = |a: SystemTime, b: SystemTime| a.duration_since(b).unwrap_or_else(|e| e.duration());
    let mut inc_doomed = Vec::new();
    let mut inc_kept = Vec::new();
    for (dir, path, t) in incremental {
        let krate = dir.rsplit_once('-').map_or(dir.as_str(), |(c, _)| c);
        let nearest = match (by_crate.get(krate), t) {
            (Some(units), Some(t)) => units
                .iter()
                .min_by_key(|(ut, _)| gap(*ut, t))
                .map(|&(_, d)| d),
            _ => None,
        };
        let c = Candidate {
            krate: None,
            id: format!("inc:{dir}"),
            paths: vec![path],
            mtime: t,
            incremental: true,
        };
        // no unit of that crate left (removed crate) or its unit is going away
        if nearest.unwrap_or(true) {
            inc_doomed.push(c)
        } else {
            inc_kept.push(c)
        }
    }
    doomed.extend(inc_doomed);
    kept.extend(inc_kept);
    (doomed, kept)
}

/// Package names without a `source` in the workspace `Cargo.lock` next to the target dir.
fn local_packages(profile: &Path) -> Option<HashSet<String>> {
    let target = profile
        .ancestors()
        .find(|a| a.join("CACHEDIR.TAG").is_file())?;
    let lock = fs::read_to_string(target.parent()?.join("Cargo.lock")).ok()?;
    let mut out = HashSet::new();
    for pkg in lock.split("[[package]]").skip(1) {
        let name = pkg
            .lines()
            .find_map(|l| l.strip_prefix("name = "))?
            .trim()
            .trim_matches('"');
        if !pkg.lines().any(|l| l.starts_with("source = ")) {
            out.insert(name.to_owned());
        }
    }
    (!out.is_empty()).then_some(out)
}

/// `rustc` hash and dependency fingerprint values from a fingerprint json:
/// `{"rustc":N,...,"deps":[[pkg_id,"name",bool,FP],...],...}`.
fn parse_fingerprint_json(s: &str) -> (u64, Vec<u64>) {
    let num = |t: &str| t.trim().parse::<u64>().ok();
    let rustc = s
        .split_once("\"rustc\":")
        .and_then(|(_, r)| num(r.split(|c: char| !c.is_ascii_digit()).next()?))
        .unwrap_or(0);
    let mut deps = Vec::new();
    if let Some((_, rest)) = s.split_once("\"deps\":[") {
        let mut start = None;
        for (i, ch) in rest.char_indices() {
            match ch {
                '[' => start = Some(i + 1),
                ']' => match start.take() {
                    Some(st) => deps.extend(rest[st..i].rsplit(',').next().and_then(num)),
                    None => break, // end of the deps array
                },
                _ => {}
            }
        }
    }
    (rustc, deps)
}

/// What makes two builds of the same package legitimately different: everything in the json
/// except `deps` and `local` (they embed the unit hash) and `path` (source path hash).
fn json_signature(s: &str) -> String {
    let head = s.split("\"deps\":").next().unwrap_or("");
    let tail = s.split_once("\"rustflags\":").map_or("", |(_, t)| t);
    let head: String = head
        .split(',')
        .filter(|f| !f.starts_with("\"path\":"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{head}{tail}")
}

/// Cargo writes fingerprint values as hex of the little-endian bytes.
fn parse_le_hex(h: &str) -> Option<u64> {
    if h.len() != 16 {
        return None;
    }
    let mut b = [0u8; 8];
    for (i, byte) in b.iter_mut().enumerate() {
        *byte = u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(u64::from_le_bytes(b))
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

/// `id\tunix_ts` of recent deletions; entries older than PIN_WINDOW are dropped.
fn read_deleted(p: &Path, now: SystemTime) -> HashMap<String, u64> {
    let horizon = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(PIN_WINDOW)
        .as_secs();
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (id, ts) = l.rsplit_once('\t')?;
            let ts: u64 = ts.parse().ok()?;
            (ts >= horizon).then(|| (id.to_owned(), ts))
        })
        .collect()
}

fn write_deleted(p: &Path, d: &HashMap<String, u64>) -> io::Result<()> {
    let s: String = d.iter().map(|(id, ts)| format!("{id}\t{ts}\n")).collect();
    atomic_write(p, &s)
}

/// Drop tag files of units that no longer exist.
fn clean_tags(tags: &Path, profile: &Path) {
    let live: HashSet<String> = read_dir(&profile.join(".fingerprint"))
        .into_iter()
        .filter_map(|(n, _)| split_hash(&n).map(|(_, h, _)| h.to_owned()))
        .collect();
    for (h, p) in read_dir(tags) {
        if !live.contains(&h) {
            let _ = fs::remove_file(p);
        }
    }
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

    impl Fx {
        /// A unit with an explicit fingerprint json: rustc hash, own value and dependency values.
        fn node(&self, pkg: &str, h: &str, age: u64, rustc: u64, value: u64, deps: &[u64]) {
            let fp = format!(".fingerprint/{pkg}-{h}");
            let d: Vec<String> = deps
                .iter()
                .map(|v| format!("[1,\"x\",false,{v}]"))
                .collect();
            let json = format!(
                "{{\"rustc\":{rustc},\"features\":\"[]\",\"deps\":[{}],\"local\":[]}}",
                d.join(",")
            );
            self.file(&format!("{fp}/lib-{pkg}.json"), age);
            fs::write(self.prof.join(format!("{fp}/lib-{pkg}.json")), json).unwrap();
            fs::write(self.prof.join(format!("{fp}/lib-{pkg}")), hex_le(value)).unwrap();
            set_age(&self.prof.join(format!("{fp}/lib-{pkg}")), age);
            set_age(&self.prof.join(format!("{fp}/lib-{pkg}.json")), age);
            set_age(&self.prof.join(&fp), age);
            self.file(&format!("deps/lib{}-{h}.rlib", pkg.replace('-', "_")), age);
        }
        fn lockfile(&self, locals: &[&str], registry: &[&str]) {
            let mut s = String::from("version = 4\n");
            for n in locals {
                s += &format!("\n[[package]]\nname = \"{n}\"\nversion = \"0.1.0\"\n");
            }
            for n in registry {
                s += &format!("\n[[package]]\nname = \"{n}\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n");
            }
            fs::write(self.root.join("Cargo.lock"), s).unwrap();
        }
    }

    fn hex_le(v: u64) -> String {
        v.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn removed_registry_dep_dropped_transitive_kept() {
        let fx = Fx::new("graph");
        fx.lockfile(&["app"], &["serde", "serde_derive", "regex"]);
        fx.node("app", A, 10, 1, 100, &[1]);
        fx.node("serde", B, 9000, 1, 1, &[3]);
        fx.node("serde_derive", C, 9000, 1, 3, &[]);
        fx.node("regex", "dddddddddddddddd", 9000, 1, 2, &[]); // no longer a dependency
        let r = fx.run(&cfg());
        assert_eq!(r.removed_units, 1);
        assert!(!fx.exists("deps/libregex-dddddddddddddddd.rlib"));
        assert!(fx.exists(&format!("deps/libserde-{B}.rlib")));
        assert!(fx.exists(&format!("deps/libserde_derive-{C}.rlib")));
    }

    #[test]
    fn superseded_workspace_unit_takes_its_deps_along() {
        let fx = Fx::new("graph2");
        fx.lockfile(&["app"], &["libc"]);
        fx.node("app", A, 7200, 1, 100, &[1]); // old app built against libc 0.2.190
        fx.node("libc", B, 7200, 1, 1, &[]);
        fx.node("app", C, 10, 1, 101, &[2]); // new app against libc 0.2.191
        fx.node("libc", "dddddddddddddddd", 10, 1, 2, &[]);
        assert_eq!(fx.run(&cfg()).removed_units, 2);
        assert!(!fx.exists(&format!("deps/liblibc-{B}.rlib")));
        assert!(fx.exists("deps/liblibc-dddddddddddddddd.rlib"));
    }

    #[test]
    fn different_rustc_hashes_are_separate_groups() {
        let fx = Fx::new("clippy");
        fx.node("app", A, 7200, 1, 100, &[]); // toolchain 1
        fx.node("app", B, 10, 2, 101, &[]); // toolchain 2
        assert_eq!(fx.run(&cfg()).removed_units, 0);
    }

    #[test]
    fn old_toolchain_retired() {
        let fx = Fx::new("toolchain");
        fx.node("app", A, 20 * 24 * 3600, 1, 100, &[]);
        fx.node("util", B, 20 * 24 * 3600, 1, 101, &[]);
        fx.node("app", C, 10, 2, 102, &[]);
        assert_eq!(fx.run(&cfg()).removed_units, 2);
        assert!(fx.exists(&format!("deps/libapp-{C}.rlib")));
    }

    #[test]
    fn revert_after_window_is_not_pinned() {
        let fx = Fx::new("revert");
        fx.lib("tokio", A, 10, false); // rebuilt after a revert
        fx.lib("tokio", B, 7200, false);
        let old = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3600;
        fs::create_dir_all(fx.prof.join(STATE_DIR)).unwrap();
        fs::write(
            fx.prof.join(STATE_DIR).join("deleted"),
            format!("{A}\t{old}\n"),
        )
        .unwrap();
        let r = fx.run(&cfg());
        assert!(r.pinned_now.is_empty());
        assert_eq!(r.removed_units, 1); // B superseded again
        assert!(fx.exists(&format!("deps/libtokio-{A}.rlib")));
    }

    #[test]
    fn clippy_tag_separates_groups() {
        let fx = Fx::new("tag");
        fx.node("app", A, 7200, 1, 100, &[]); // cargo check
        fx.node("app", B, 10, 1, 101, &[]); // cargo clippy: same json
        let tags = fx.prof.join(STATE_DIR).join(TAGS_DIR);
        fs::create_dir_all(&tags).unwrap();
        fs::write(tags.join(B), "clippy-driver").unwrap();
        fs::write(tags.join(C), "clippy-driver").unwrap(); // stale tag
        assert_eq!(fx.run(&cfg()).removed_units, 0);
        assert!(!tags.join(C).exists() && tags.join(B).exists());
    }

    #[test]
    fn fingerprint_json_parsing() {
        let j = r#"{"rustc":139,"features":"[]","deps":[[13,"build_script_build",false,49],[49,"util",false,15]],"local":[{"CheckDepInfo":{"dep_info":"x","checksum":false}}]}"#;
        assert_eq!(parse_fingerprint_json(j), (139, vec![49, 15]));
        assert_eq!(
            parse_fingerprint_json(r#"{"rustc":1,"deps":[],"local":[]}"#),
            (1, vec![])
        );
        assert_eq!(parse_le_hex("d5dd8855943b3fe2"), Some(16302814684380716501));
        assert_eq!(parse_le_hex("xyz"), None);
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
    fn incremental_follows_its_unit() {
        let fx = Fx::new("inc");
        fx.lib("gs_modem", A, 7200, false);
        fx.lib("gs_modem", B, 10, false);
        fx.lib("gs_at", C, 9000, false); // old but the only copy: live
        for (d, age) in [
            ("gs_modem-1abc", 7200),
            ("gs_modem-2def", 10),
            ("gs_at-3xyz", 9000),
            ("removed-4qqq", 10),
        ] {
            fx.dir(&format!("incremental/{d}/s-x"), age);
            set_age(&fx.prof.join(format!("incremental/{d}")), age);
        }
        let r = fx.run(&cfg());
        assert_eq!(r.removed_incremental, 2);
        assert!(!fx.exists("incremental/gs_modem-1abc") && !fx.exists("incremental/removed-4qqq"));
        assert!(fx.exists("incremental/gs_modem-2def") && fx.exists("incremental/gs_at-3xyz"));
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
