//! A simulated cargo for scenario tests: one workspace whose configuration changes over time,
//! built into a real target dir with cargo's file layout, on a virtual clock. Scenarios drive it
//! with what a developer does (build, edit, bump a dependency, switch toolchains, ...) and check
//! the pruner against what cargo needs.
//!
//! Workspace: `util` (lib, feature `x`) <- `app` (lib) <- `tool` (bin, optional); registry crates
//! `cfg-if`, `serde` (with a build script, version switchable) and `regex` (optional dep of app,
//! from git: its checkout dir looks numeric, yet it has no version).

#![allow(dead_code)]

use cargo_autoprune::layout::{STATE_DIR, TAGS_DIR, UNITS_FILE};
use cargo_autoprune::prune::{prune_at, Config, Outcome, Report};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DAY: u64 = 24 * 3600;
// a cargo home with a space: rustc escapes it in dep-info
const REGISTRY: &str = "/nonexistent cargo home/registry/src/index.crates.io-1949cf8c6b5b557f";
/// `<name>-<url hash>/<short rev>`; a hash can be all digits
const GIT: &str = "/nonexistent cargo home/git/checkouts/regex-1234567890123456/abcdef0";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mode {
    Build,
    Check,
    Clippy,
    Test,
}

/// What the next cargo invocation builds with.
#[derive(Clone, Debug)]
pub struct Conf {
    pub mode: Mode,
    /// rustc hash. A toolchain from `new_layout_from` on writes the new build-dir layout.
    pub toolchain: u64,
    /// `serde 1.0.<n>` in Cargo.lock
    pub serde: u8,
    /// app depends on regex
    pub regex: bool,
    /// util is built with its feature `x` (feature unification: `-p util` vs `--workspace`)
    pub util_x: bool,
    /// the `tool` package has its bin target (`src/main.rs`)
    pub tool: bool,
}

/// Unit identity: metadata hash and layout (true: new build-dir layout).
pub type Key = (String, bool);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    Lib,
    Rmeta,
    Exe,
    Script,
    ScriptRun,
}

/// A unit as cargo builds it for the current configuration.
#[derive(Clone, Debug)]
pub struct Node {
    pub pkg: &'static str,
    /// fingerprint file name: lib-util, bin-tool, test-lib-app, build-script-build-script-build, ...
    pub kind: String,
    pub h: String,
    pub value: u64,
    pub deps: Vec<String>,
    /// a workspace target (not a build script or a registry crate)
    pub local: bool,
    pub toolchain: u64,
    src: String,
    json: String,
    shape: Shape,
}

/// A unit cargo wrote at some point.
#[derive(Debug)]
pub struct Written {
    pub node: Node,
    /// every file cargo wrote for it
    pub files: Vec<PathBuf>,
    /// what the pruner deletes as a whole: fingerprint dir, deps files, build dir / unit dir
    pub tops: Vec<PathBuf>,
    pub fp: PathBuf,
    pub at: SystemTime,
}

pub struct Sim {
    pub root: PathBuf,
    pub prof: PathBuf,
    /// removed with the sim: `root`, or the dir holding the workspace and a shared target
    owned: PathBuf,
    pub now: SystemTime,
    pub conf: Conf,
    pub units: BTreeMap<Key, Written>,
    /// incremental dir of a workspace unit hash, and when its session started
    pub inc: BTreeMap<String, (PathBuf, SystemTime)>,
    /// units the last build needed, and those it (re)wrote
    pub used: Vec<Key>,
    pub wrote: Vec<Key>,
    /// debris of interrupted builds: must go
    pub leftovers: Vec<PathBuf>,
    /// empty `out` dirs cargo prepared: must stay
    pub prepared: Vec<PathBuf>,
    /// files that are not cargo-autoprune's to delete
    pub foreign: Vec<PathBuf>,
    pub modes: BTreeSet<Mode>,
    pub new_layout_from: Option<u64>,
    edits: BTreeMap<&'static str, u64>,
    crashes: u64,
}

pub fn hx(v: impl Hash) -> String {
    format!("{:016x}", hash(v))
}

fn hash(v: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

pub fn hex_le(v: u64) -> String {
    v.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

fn base36(mut n: u64) -> String {
    let mut s = Vec::new();
    while n > 0 {
        s.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize]);
        n /= 36;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

pub fn set_mtime(p: &Path, t: SystemTime) {
    open_for_times(p).unwrap().set_modified(t).unwrap();
}

#[cfg(windows)]
fn open_for_times(p: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_WRITE_ATTRIBUTES, and FILE_FLAG_BACKUP_SEMANTICS so a directory can be opened
    File::options()
        .access_mode(0x100)
        .custom_flags(0x0200_0000)
        .open(p)
}
#[cfg(not(windows))]
fn open_for_times(p: &Path) -> std::io::Result<File> {
    File::options().read(true).open(p)
}

/// Total size of the files under `p`.
pub fn disk(p: &Path) -> u64 {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => entries(p).iter().map(|e| disk(&e.path())).sum(),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

pub fn entries(p: &Path) -> Vec<fs::DirEntry> {
    fs::read_dir(p)
        .map(|rd| rd.flatten().collect())
        .unwrap_or_default()
}

impl Sim {
    pub fn new(tag: &str) -> Sim {
        Sim::at(super::scratch(&format!("sim-{tag}")))
    }

    /// A workspace at `root` (emptied first), its target dir in `root/target`.
    pub fn at(root: PathBuf) -> Sim {
        let _ = fs::remove_dir_all(&root);
        let target = root.join("target");
        Sim::with_target(root.clone(), target, root)
    }

    /// A workspace whose target dir is shared (`CARGO_TARGET_DIR`): outside the workspace, with
    /// no `Cargo.lock` next to it.
    pub fn shared(tag: &str) -> Sim {
        let base = super::scratch(&format!("sim-{tag}"));
        Sim::with_target(base.join("ws"), base.join("target"), base)
    }

    fn with_target(root: PathBuf, target: PathBuf, owned: PathBuf) -> Sim {
        let prof = target.join("debug");
        let real = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut s = Sim {
            now: UNIX_EPOCH + Duration::from_secs(real - 400 * DAY),
            conf: Conf {
                mode: Mode::Build,
                toolchain: 1,
                serde: 0,
                regex: false,
                util_x: false,
                tool: true,
            },
            units: BTreeMap::new(),
            inc: BTreeMap::new(),
            used: Vec::new(),
            wrote: Vec::new(),
            leftovers: Vec::new(),
            prepared: Vec::new(),
            foreign: Vec::new(),
            modes: BTreeSet::new(),
            new_layout_from: None,
            edits: BTreeMap::new(),
            crashes: 0,
            root,
            prof,
            owned,
        };
        for d in ["deps", ".fingerprint", "build", "incremental", "examples"] {
            s.mkdir(&s.prof.join(d));
        }
        s.put(
            &target.join("CACHEDIR.TAG"),
            b"Signature: 8a477f597d28d172789f06886806bc55",
        );
        s.put(&s.prof.join(".cargo-lock"), b"");
        for f in [
            target.join("keep.txt"),
            s.prof.join("tool"), // uplifted binary
            s.prof.join("deps/README"),
            s.prof.join("build/stray-file"),
            s.prof.join("build/pkg/abcdef/f"), // not a unit dir
            s.prof.join("incremental/app-9.lock"),
            s.prof.join("incremental/nodash/s-x/f"), // not a `<crate>-<id>` dir
        ] {
            s.put(&f, b"not cargo-autoprune's");
            s.foreign.push(f);
        }
        s
    }

    pub fn layout(&self) -> bool {
        self.new_layout_from
            .is_some_and(|t| self.conf.toolchain >= t)
    }

    /// Some cargo can still use the unit: its toolchain writes the layout it is in.
    pub fn usable(&self, k: &Key) -> bool {
        let t = self.units[k].node.toolchain;
        k.1 == self.new_layout_from.is_some_and(|from| t >= from)
    }

    pub fn advance(&mut self, secs: u64) {
        self.now += Duration::from_secs(secs);
    }

    pub fn edit(&mut self, pkg: &'static str) {
        *self.edits.entry(pkg).or_default() += 1;
    }

    /// From now on this toolchain and newer ones write the new build-dir layout.
    pub fn upgrade_layout(&mut self) {
        self.new_layout_from.get_or_insert(self.conf.toolchain);
    }

    pub fn run(&self, cfg: &Config) -> Report {
        match prune_at(&self.prof, cfg, self.now).unwrap() {
            Outcome::Done(r) => r,
            Outcome::Busy => panic!("busy"),
        }
    }

    pub fn exists(&self, p: &Path) -> bool {
        fs::symlink_metadata(p).is_ok()
    }

    /// The unit's fingerprint is there (cargo can find it).
    pub fn present(&self, k: &Key) -> bool {
        self.units.get(k).is_some_and(|w| w.fp.is_dir())
    }

    /// Every file cargo wrote for the unit is still there.
    pub fn intact(&self, k: &Key) -> bool {
        self.units[k].files.iter().all(|f| self.exists(f))
    }

    pub fn present_keys(&self) -> BTreeSet<Key> {
        self.units
            .keys()
            .filter(|k| self.present(k))
            .cloned()
            .collect()
    }

    /// Units of which some but not all files are left.
    pub fn half_units(&self) -> Vec<String> {
        self.units
            .iter()
            .filter(|(_, w)| {
                let n = w.files.iter().filter(|f| self.exists(f)).count();
                n != 0 && n != w.files.len()
            })
            .map(|(k, w)| format!("{} {}-{}", w.node.kind, w.node.pkg, k.0))
            .collect()
    }

    /// Make a unit's rlib `len` bytes (sparse): a big unit without writing it.
    pub fn grow(&self, k: &Key, len: u64) {
        let rlib = self.units[k]
            .files
            .iter()
            .find(|f| f.extension().is_some_and(|e| e == "rlib"))
            .unwrap();
        File::options()
            .write(true)
            .open(rlib)
            .unwrap()
            .set_len(len)
            .unwrap();
    }

    pub fn key(&self, pkg: &str, kind_prefix: &str) -> Key {
        let layout = self.layout();
        let n = self
            .graph()
            .into_iter()
            .find(|n| n.pkg == pkg && n.kind.starts_with(kind_prefix))
            .unwrap_or_else(|| panic!("no {kind_prefix} unit of {pkg} in this build"));
        (n.h, layout)
    }

    fn mkdir(&self, p: &Path) {
        if !p.exists() {
            if let Some(parent) = p.parent() {
                self.mkdir(parent);
            }
            fs::create_dir(p).unwrap();
            set_mtime(p, self.now);
            if let Some(parent) = p.parent().filter(|q| q.starts_with(&self.root)) {
                set_mtime(parent, self.now);
            }
        }
    }

    /// Write a file at the virtual time; a new entry also dates its directory.
    fn put(&self, p: &Path, content: &[u8]) {
        self.mkdir(p.parent().unwrap());
        let new = !p.exists();
        fs::write(p, content).unwrap();
        set_mtime(p, self.now);
        if new {
            set_mtime(p.parent().unwrap(), self.now);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn node(
        &self,
        pkg: &'static str,
        kind: &str,
        ver: &str,
        features: &str,
        deps: &[&Node],
        shape: Shape,
        src: String,
    ) -> Node {
        let local = matches!(pkg, "util" | "app" | "tool")
            && !matches!(shape, Shape::Script | Shape::ScriptRun);
        let class = match shape {
            Shape::Script | Shape::ScriptRun => "host",
            Shape::Rmeta => "check",
            _ => "build",
        };
        let clippy = local && self.conf.mode == Mode::Clippy;
        let dep_h: Vec<&str> = deps.iter().map(|d| d.h.as_str()).collect();
        let h = hx((
            pkg,
            kind,
            ver,
            features,
            class,
            clippy,
            self.conf.toolchain,
            &dep_h,
        ));
        let rev = self.edits.get(pkg).copied().unwrap_or(0);
        let dep_v: Vec<u64> = deps.iter().map(|d| d.value).collect();
        let value = hash((&h, rev, &dep_v));
        let dep_json: Vec<String> = deps
            .iter()
            .map(|d| {
                format!(
                    "[{},\"{}\",false,{}]",
                    hash(d.pkg),
                    d.pkg.replace('-', "_"),
                    d.value
                )
            })
            .collect();
        let json = format!(
            "{{\"rustc\":{},\"features\":\"{}\",\"declared_features\":\"[\\\"x\\\"]\",\"target\":{},\"profile\":{},\"path\":{},\"deps\":[{}],\"local\":[{{\"CheckDepInfo\":{{\"dep_info\":\"x.d\",\"checksum\":false}}}}],\"rustflags\":[],\"config\":0,\"compile_kind\":0}}",
            self.conf.toolchain,
            features.replace('"', "\\\""),
            hash((pkg, kind)),
            hash(class),
            hash(&src),
            dep_json.join(",")
        );
        Node {
            pkg,
            kind: kind.to_owned(),
            h,
            value,
            deps: dep_h.iter().map(|s| s.to_string()).collect(),
            local,
            toolchain: self.conf.toolchain,
            src,
            json,
            shape,
        }
    }

    /// The units the current configuration builds, dependencies first.
    pub fn graph(&self) -> Vec<Node> {
        let c = &self.conf;
        let check = matches!(c.mode, Mode::Check | Mode::Clippy);
        let lib = if check { Shape::Rmeta } else { Shape::Lib };
        let reg = |p: &str, v: &str, f: &str| format!("{REGISTRY}/{p}-{v}/{f}");
        let sv = format!("1.0.{}", c.serde);
        let cfg_if = self.node(
            "cfg-if",
            "lib-cfg_if",
            "1.0.0",
            "[]",
            &[],
            lib,
            reg("cfg-if", "1.0.0", "src/lib.rs"),
        );
        let script = self.node(
            "serde",
            "build-script-build-script-build",
            &sv,
            "[]",
            &[],
            Shape::Script,
            reg("serde", &sv, "build.rs"),
        );
        let run = self.node(
            "serde",
            "run-build-script-build-script-build",
            &sv,
            "[]",
            &[&script],
            Shape::ScriptRun,
            reg("serde", &sv, "build.rs"),
        );
        let serde = self.node(
            "serde",
            "lib-serde",
            &sv,
            "[]",
            &[&run],
            lib,
            reg("serde", &sv, "src/lib.rs"),
        );
        let feat = if c.util_x { "[\"x\"]" } else { "[]" };
        let util = self.node(
            "util",
            "lib-util",
            "0.1.0",
            feat,
            &[&serde, &cfg_if],
            lib,
            "util/src/lib.rs".into(),
        );
        let mut out = vec![cfg_if.clone(), script, run, serde.clone()];
        let mut app_deps = vec![&util];
        let regex;
        if c.regex {
            regex = self.node(
                "regex",
                "lib-regex",
                "1.0.0",
                "[]",
                &[&cfg_if],
                lib,
                format!("{GIT}/src/lib.rs"),
            );
            out.push(regex.clone());
            app_deps.push(&regex);
        }
        let app = self.node(
            "app",
            "lib-app",
            "0.1.0",
            "[]",
            &app_deps,
            lib,
            "app/src/lib.rs".into(),
        );
        if c.mode == Mode::Test {
            out.push(self.node(
                "util",
                "test-lib-util",
                "0.1.0",
                feat,
                &[&serde, &cfg_if],
                Shape::Exe,
                "util/src/lib.rs".into(),
            ));
            out.push(self.node(
                "app",
                "test-lib-app",
                "0.1.0",
                "[]",
                &app_deps,
                Shape::Exe,
                "app/src/lib.rs".into(),
            ));
        }
        out.push(util.clone());
        if c.tool {
            let shape = if check { Shape::Rmeta } else { Shape::Exe };
            out.push(self.node(
                "tool",
                "bin-tool",
                "0.1.0",
                "[]",
                &[&app],
                shape,
                "tool/src/main.rs".into(),
            ));
        }
        out.push(app);
        out
    }

    fn write_lock(&self) {
        let mut s = String::from("version = 4\n");
        for n in ["app", "tool", "util"] {
            s += &format!("\n[[package]]\nname = \"{n}\"\nversion = \"0.1.0\"\n");
        }
        let registry = [
            ("cfg-if", "1.0.0".to_owned()),
            ("serde", format!("1.0.{}", self.conf.serde)),
        ];
        for (n, v) in registry {
            s += &format!("\n[[package]]\nname = \"{n}\"\nversion = \"{v}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n");
        }
        if self.conf.regex {
            s += "\n[[package]]\nname = \"regex\"\nversion = \"1.0.0\"\nsource = \"git+https://example.com/regex#abcdef0\"\n";
        }
        let p = self.root.join("Cargo.lock");
        if fs::read_to_string(&p).ok().as_deref() != Some(s.as_str()) {
            self.put(&p, s.as_bytes()); // cargo rewrites it only when it changes
        }
    }

    fn write_sources(&self) {
        for pkg in ["util", "app"] {
            let p = self.root.join(pkg).join("src/lib.rs");
            if !p.exists() {
                self.put(&p, b"");
            }
        }
        let tool = self.root.join("tool/src/main.rs");
        match (self.conf.tool, tool.exists()) {
            (true, false) => self.put(&tool, b"fn main() {}"),
            (false, true) => fs::remove_file(tool).unwrap(),
            _ => {}
        }
    }

    /// `cargo build` / `check` / `clippy` / `test` with the current configuration: units whose
    /// fingerprint is up to date and whose files are all there stay untouched, the rest is
    /// (re)written now.
    pub fn build(&mut self) {
        self.advance(5);
        self.write_sources();
        self.write_lock();
        self.modes.insert(self.conf.mode);
        let layout = self.layout();
        let (mut used, mut wrote) = (Vec::new(), Vec::new());
        for n in self.graph() {
            let key = (n.h.clone(), layout);
            let fresh = self
                .units
                .get(&key)
                .is_some_and(|w| w.node.value == n.value && self.intact(&key));
            if !fresh {
                self.write_unit(n, layout);
                wrote.push(key.clone());
            }
            used.push(key);
        }
        (self.used, self.wrote) = (used, wrote);
    }

    fn write_unit(&mut self, n: Node, layout: bool) {
        let krate = n.pkg.replace('-', "_");
        let h = n.h.clone();
        let mut files = Vec::new();
        let mut tops = Vec::new();
        let file = |s: &Sim, p: PathBuf, content: &[u8], files: &mut Vec<PathBuf>| {
            s.put(&p, content);
            files.push(p);
        };
        let (fp, out) = if layout {
            let d = self.prof.join("build").join(n.pkg).join(&h);
            tops.push(d.clone());
            (d.join("fingerprint"), d.join("out"))
        } else {
            let fp = self
                .prof
                .join(".fingerprint")
                .join(format!("{}-{h}", n.pkg));
            tops.push(fp.clone());
            (fp, self.prof.join("deps"))
        };
        file(
            self,
            fp.join(&n.kind),
            hex_le(n.value).as_bytes(),
            &mut files,
        );
        file(
            self,
            fp.join(format!("{}.json", n.kind)),
            n.json.as_bytes(),
            &mut files,
        );
        file(self, fp.join("invoked.timestamp"), b"", &mut files);
        if n.shape != Shape::ScriptRun {
            file(self, fp.join(format!("dep-{}", n.kind)), b"dep", &mut files);
        }
        // rustc warnings, saved by cargo to replay: app has some until an edit fixes them (and
        // the next edit brings them back); cargo deletes the file when there are none
        let output = fp.join(format!("output-{}", n.kind));
        if n.pkg == "app" && n.local && self.edits.get("app").copied().unwrap_or(0) % 2 == 0 {
            file(self, output, b"warning: unused", &mut files);
        } else {
            let _ = fs::remove_file(output);
        }
        let dep_info = |out: &Path, name: &str| -> (PathBuf, Vec<u8>) {
            let p = out.join(name);
            let esc = |s: &str| s.replace(' ', "\\ ");
            let text = format!(
                "{}: {} src/other.rs\n",
                esc(&p.display().to_string()),
                esc(&n.src)
            );
            (p, text.into_bytes())
        };
        let big = vec![7u8; 3000];
        let small = vec![7u8; 300];
        let mut outputs: Vec<(PathBuf, Vec<u8>)> = Vec::new();
        match (n.shape, layout) {
            (Shape::Lib | Shape::Rmeta, false) => {
                if n.shape == Shape::Lib {
                    outputs.push((out.join(format!("lib{krate}-{h}.rlib")), big.clone()));
                }
                outputs.push((out.join(format!("lib{krate}-{h}.rmeta")), small.clone()));
                outputs.push(dep_info(&out, &format!("{krate}-{h}.d")));
            }
            (Shape::Lib | Shape::Rmeta, true) => {
                if n.shape == Shape::Lib {
                    outputs.push((out.join(format!("lib{krate}.rlib")), big.clone()));
                }
                outputs.push((out.join(format!("lib{krate}.rmeta")), small.clone()));
                outputs.push(dep_info(&out, &format!("{krate}-{h}.d")));
            }
            (Shape::Exe, _) => {
                outputs.push((out.join(format!("{krate}-{h}")), big.clone()));
                outputs.push(dep_info(&out, &format!("{krate}-{h}.d")));
                outputs.push((
                    out.join(format!("{krate}-{h}.dSYM/Contents/Info.plist")),
                    small.clone(),
                ));
            }
            (Shape::Script | Shape::ScriptRun, _) => {
                let dir = if layout {
                    out.clone()
                } else {
                    let d = self.prof.join("build").join(format!("{}-{h}", n.pkg));
                    tops.push(d.clone());
                    d
                };
                if n.shape == Shape::Script {
                    outputs.push((dir.join("build-script-build"), big.clone()));
                    outputs.push((dir.join(format!("build_script_build-{h}")), big.clone()));
                    outputs.push(dep_info(&dir, &format!("build_script_build-{h}.d")));
                } else {
                    let run = if layout {
                        dir.parent().unwrap().join("run")
                    } else {
                        dir.clone()
                    };
                    outputs.push((
                        run.join("output"),
                        b"cargo:rerun-if-changed=build.rs\n".to_vec(),
                    ));
                    outputs.push((run.join("stderr"), Vec::new()));
                    outputs.push((
                        dir.join(if layout { "gen.rs" } else { "out/gen.rs" }),
                        small.clone(),
                    ));
                }
            }
        }
        for (p, content) in outputs {
            if !layout && p.parent() == Some(out.as_path()) {
                tops.push(p.clone());
            }
            if !layout
                && p.parent()
                    .and_then(Path::parent)
                    .is_some_and(|q| q.ends_with(format!("{krate}-{h}.dSYM")))
            {
                tops.push(p.parent().unwrap().parent().unwrap().to_path_buf());
            }
            file(self, p, &content, &mut files);
        }
        if n.shape == Shape::ScriptRun && !layout {
            // the script ran (and wrote into its dir) after cargo stamped the unit
            let dir = self.prof.join("build").join(format!("{}-{h}", n.pkg));
            set_mtime(&dir, self.now + Duration::from_secs(2));
        }
        if n.local {
            // the wrapper records workspace units and tags clippy's
            let state = self.prof.join(STATE_DIR);
            self.mkdir(&state);
            let rec = format!("{h}\t{}\n", self.root.join(n.pkg).display());
            let units = state.join(UNITS_FILE);
            let mut all = fs::read_to_string(&units).unwrap_or_default();
            all += &rec;
            fs::write(&units, all).unwrap();
            if self.conf.mode == Mode::Clippy {
                self.put(&state.join(TAGS_DIR).join(&h), b"clippy-driver");
            }
            self.write_incremental(&n, &files);
        }
        self.units.insert(
            (h, layout),
            Written {
                node: n,
                files,
                tops,
                fp,
                at: self.now,
            },
        );
    }

    /// rustc's incremental session for a workspace unit: a new session replaces the old one;
    /// its `metadata.rmeta` is a hard link to the unit's `.rmeta`.
    fn write_incremental(&mut self, n: &Node, files: &[PathBuf]) {
        let krate = n.pkg.replace('-', "_");
        let dir = self
            .prof
            .join("incremental")
            .join(format!("{krate}-{}", &hx(("inc", &n.h))[..13]));
        for e in entries(&dir) {
            if e.path().is_dir() {
                fs::remove_dir_all(e.path()).unwrap();
            } else {
                fs::remove_file(e.path()).unwrap();
            }
        }
        let micros = self.now.duration_since(UNIX_EPOCH).unwrap().as_micros() as u64;
        let name = format!("s-{}-{}", base36(micros), &hx((&n.h, micros))[..6]);
        let session = dir.join(format!("{name}-{}", &hx(("svh", &n.h))[..8]));
        self.put(&session.join("query-cache.bin"), &[1u8; 2000]);
        self.put(&session.join("dep-graph.bin"), &[2u8; 500]);
        if let Some(rmeta) = files
            .iter()
            .find(|f| f.extension().is_some_and(|e| e == "rmeta"))
        {
            fs::hard_link(rmeta, session.join("metadata.rmeta")).unwrap();
        }
        self.put(&dir.join(format!("{name}.lock")), b"");
        set_mtime(&session, self.now);
        set_mtime(&dir, self.now);
        self.inc.insert(n.h.clone(), (dir, self.now));
    }

    /// A build killed mid-way: objects and dep-info without a fingerprint, the started incremental
    /// session, and (new layout) a unit dir with partial output next to an empty prepared one.
    pub fn crash(&mut self) {
        self.advance(1);
        self.crashes += 1;
        let h = hx(("crash", self.crashes));
        let deps = self.prof.join("deps");
        for f in [
            format!("app-{h}.d"),
            format!("app-{h}.app.1x2y3z-cgu.0.rcgu.o"),
        ] {
            self.put(&deps.join(&f), &[3u8; 400]);
            self.leftovers.push(deps.join(f));
        }
        let micros = self.now.duration_since(UNIX_EPOCH).unwrap().as_micros() as u64;
        let inc = self.prof.join(format!(
            "incremental/app-{}",
            &hx(("crashinc", self.crashes))[..13]
        ));
        self.put(
            &inc.join(format!(
                "s-{}-abcdef-working/dep-graph.part.bin",
                base36(micros)
            )),
            &[4u8; 400],
        );
        set_mtime(&inc, self.now);
        self.leftovers.push(inc);
        if self.layout() {
            let prepared = self
                .prof
                .join(format!("build/app/{}/out", hx(("prep", self.crashes))));
            self.mkdir(&prepared);
            self.prepared.push(prepared);
            let partial = self
                .prof
                .join(format!("build/app/{}", hx(("part", self.crashes))));
            self.put(&partial.join("out/libapp.rmeta"), &[5u8; 400]);
            self.leftovers.push(partial);
        }
    }

    /// Size of every unit and incremental dir: what a size cap limits.
    pub fn cargo_bytes(&self) -> u64 {
        let foreign: u64 = self
            .foreign
            .iter()
            .filter(|f| f.starts_with(&self.prof))
            .map(|f| disk(f))
            .sum();
        disk(&self.prof) - disk(&self.prof.join(STATE_DIR)) - foreign
    }

    /// `p` is or holds a file that is not ours.
    pub fn is_foreign(&self, p: &Path) -> bool {
        self.foreign.iter().any(|f| f.starts_with(p))
    }

    /// Hashes the wrapper recorded in the units file.
    pub fn recorded(&self) -> HashSet<String> {
        fs::read_to_string(self.prof.join(STATE_DIR).join(UNITS_FILE))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(h, _)| h.to_owned())
            .collect()
    }

    /// Hashes with a clippy tag.
    pub fn tags(&self) -> HashSet<String> {
        entries(&self.prof.join(STATE_DIR).join(TAGS_DIR))
            .into_iter()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }
}

impl Drop for Sim {
    fn drop(&mut self) {
        super::remove_tree(&self.owned);
    }
}
