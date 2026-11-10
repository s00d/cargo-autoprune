//! E2E harness: a throwaway workspace with an offline "registry" (a Cargo `directory` source), real
//! cargo builds through the cargo-autoprune wrapper, and an oracle target built from scratch with
//! the same commands to tell garbage that was left from live files that were lost.

#![allow(dead_code)]

use cargo_autoprune::layout::profile_dirs;
use cargo_autoprune::prune::{prune, Config, Outcome, Report};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const BIN: &str = env!("CARGO_BIN_EXE_cargo-autoprune");

pub struct Ws {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub target: PathBuf,
    logs: PathBuf,
    n: AtomicUsize,
}

impl Ws {
    /// Workspace `<root>/ws` with members `app` (bin + lib + test + example + build.rs) and `util`
    /// (`extra` feature), plus vendored crates:
    /// dep_a 1.0.0/1.1.0, dep_b (build script, depends on dep_a 1), dep_c (`fast` feature),
    /// dep_m (proc-macro).
    pub fn new(name: &str) -> Ws {
        let root =
            std::env::temp_dir().join(format!("cargo-autoprune-e2e/{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("ws");
        let ws = Ws {
            target: dir.join("target"),
            logs: root.join("logs"),
            dir,
            root,
            n: AtomicUsize::new(0),
        };
        fs::create_dir_all(&ws.logs).unwrap();
        let v = ws.root.join("vendor");
        vendor(&v, "dep_a", "1.0.0", "", "pub fn a() -> u32 { 100 }");
        vendor(&v, "dep_a", "1.1.0", "", "pub fn a() -> u32 { 110 }");
        vendor(
            &v,
            "dep_b",
            "1.0.0",
            "build = \"build.rs\"\n[dependencies]\ndep_a = \"1\"",
            "pub fn b() -> u32 { dep_a::a() + env!(\"DEP_B_BUILT\").len() as u32 }",
        );
        fs::write(
            v.join("dep_b-1.0.0/build.rs"),
            "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); println!(\"cargo:rustc-env=DEP_B_BUILT=yes\"); }",
        )
        .unwrap();
        vendor(
            &v,
            "dep_c",
            "1.0.0",
            "[features]\nfast = []",
            "pub fn c() -> u32 { if cfg!(feature = \"fast\") { 2 } else { 1 } }",
        );
        vendor(
            &v,
            "dep_m",
            "1.0.0",
            "[lib]\nproc-macro = true",
            "use proc_macro::TokenStream;\n#[proc_macro]\npub fn m(_: TokenStream) -> TokenStream { \"7u32\".parse().unwrap() }",
        );
        ws.write(
            ".cargo/config.toml",
            &format!(
                "[source.crates-io]\nreplace-with = \"v\"\n[source.v]\ndirectory = \"{}\"\n",
                v.display()
            ),
        );
        ws.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\", \"util\"]\nresolver = \"2\"\n",
        );
        ws.write("util/Cargo.toml", "[package]\nname = \"util\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nextra = []\n");
        ws.write(
            "util/src/lib.rs",
            "pub fn u() -> u32 { if cfg!(feature = \"extra\") { 2 } else { 1 } }\n",
        );
        ws.write(
            "app/build.rs",
            "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
        );
        ws.write("app/src/lib.rs", "pub fn app() -> u32 { util::u() }\n");
        ws.write(
            "app/src/main.rs",
            "fn main() { println!(\"{}\", app::app()); }\n",
        );
        ws.write(
            "app/tests/it.rs",
            "#[test] fn it() { assert!(app::app() > 0); }\n",
        );
        ws.write("app/examples/ex.rs", "fn main() { app::app(); }\n");
        ws.deps("");
        ws
    }

    pub fn write(&self, rel: &str, s: &str) {
        let p = self.dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    /// Replace the extra `[dependencies]` lines of `app` (util is always there).
    pub fn deps(&self, lines: &str) {
        self.write(
            "app/Cargo.toml",
            &format!(
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nutil = {{ path = \"../util\" }}\n{lines}\n"
            ),
        );
    }

    fn command(&self, args: &str, target: &Path, wrapper: bool) -> Command {
        let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        c.current_dir(&self.dir)
            .args(args.split_whitespace())
            .args(["--offline", "--color", "never"])
            .env("CARGO_TARGET_DIR", target)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            // the harness prunes explicitly; tests that want the background prune override this
            .env("CARGO_AUTOPRUNE", "0");
        c.env("RUSTC_WRAPPER", if wrapper { BIN } else { "" });
        c
    }

    /// Run one cargo command (`args` split on spaces); log goes to `<root>/logs/NN-*.log`.
    /// Returns how many units cargo compiled.
    pub fn cargo_in(
        &self,
        args: &str,
        target: &Path,
        wrapper: bool,
        envs: &[(&str, &str)],
    ) -> usize {
        let mut c = self.command(args, target, wrapper);
        c.envs(envs.iter().copied());
        let out = c.output().expect("run cargo");
        let n = self.n.fetch_add(1, Ordering::SeqCst);
        let log = self
            .logs
            .join(format!("{n:03}-{}.log", args.replace([' ', '/'], "_")));
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        fs::write(&log, &text).unwrap();
        assert!(
            out.status.success(),
            "cargo {args} failed, log: {}\n{text}",
            log.display()
        );
        text.lines()
            .filter(|l| l.trim_start().starts_with("Compiling "))
            .count()
    }

    pub fn cargo(&self, args: &str) -> usize {
        self.cargo_in(args, &self.target.clone(), true, &[])
    }

    /// Run a command set; returns total compiled units.
    pub fn run_all(&self, cmds: &[&str]) -> usize {
        cmds.iter().map(|c| self.cargo(c)).sum()
    }

    /// Prune every profile dir synchronously (as the background process would).
    pub fn prune(&self) -> Vec<Report> {
        self.prune_with(Config {
            grace: Duration::ZERO,
            lock_wait: Duration::from_secs(60),
            ..Config::default()
        })
    }

    pub fn prune_with(&self, cfg: Config) -> Vec<Report> {
        profile_dirs(&self.target)
            .iter()
            .map(|p| match prune(p, &cfg).unwrap() {
                Outcome::Done(r) => r,
                Outcome::Busy => panic!("busy: {}", p.display()),
            })
            .collect()
    }

    /// Build the same commands into an empty target without the wrapper and compare.
    /// Returns (left: only in the pruned target, lost: only in the oracle).
    pub fn oracle_diff(&self, cmds: &[&str]) -> (Vec<String>, Vec<String>) {
        let oracle = self.root.join("oracle");
        let _ = fs::remove_dir_all(&oracle);
        for c in cmds {
            self.cargo_in(c, &oracle, false, &[]);
        }
        let a = inventory(&self.target);
        let b = inventory(&oracle);
        (
            a.difference(&b).cloned().collect(),
            b.difference(&a).cloned().collect(),
        )
    }

    /// Assert the pruned target equals a from-scratch build and nothing recompiles.
    #[track_caller]
    pub fn assert_clean(&self, cmds: &[&str]) {
        let (left, lost) = self.oracle_diff(cmds);
        assert!(lost.is_empty(), "live files were deleted: {lost:#?}");
        assert!(left.is_empty(), "garbage left: {left:#?}");
        assert_eq!(self.run_all(cmds), 0, "rerun recompiled units");
        let again: usize = self
            .prune()
            .iter()
            .map(|r| r.removed_units + r.removed_incremental)
            .sum();
        assert_eq!(again, 0, "a second prune still found garbage");
    }

    /// Age the recent-deletion log so the next rebuild looks like a revert done later.
    pub fn age_deleted_log(&self, secs: u64) {
        for p in profile_dirs(&self.target) {
            let f = p.join(".autoprune/deleted");
            let Ok(s) = fs::read_to_string(&f) else {
                continue;
            };
            let aged: String = s
                .lines()
                .filter_map(|l| l.rsplit_once('\t'))
                .map(|(id, ts)| format!("{id}\t{}\n", ts.parse::<u64>().unwrap() - secs))
                .collect();
            fs::write(f, aged).unwrap();
        }
    }

    pub fn pins(&self) -> usize {
        profile_dirs(&self.target)
            .iter()
            .map(|p| {
                fs::read_to_string(p.join(".autoprune/pinned"))
                    .unwrap_or_default()
                    .lines()
                    .count()
            })
            .sum()
    }

    pub fn deps_files(&self, profile: &str, prefix: &str) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.target.join(profile).join("deps"))
            .map(|rd| {
                rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        v.retain(|n| n.starts_with(prefix));
        v.sort();
        v
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn vendor(v: &Path, name: &str, version: &str, extra: &str, lib: &str) {
    let d = v.join(format!("{name}-{version}"));
    fs::create_dir_all(d.join("src")).unwrap();
    fs::write(
        d.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n{extra}\n"
        ),
    )
    .unwrap();
    fs::write(d.join("src/lib.rs"), lib).unwrap();
    fs::write(
        d.join(".cargo-checksum.json"),
        "{\"files\":{},\"package\":null}",
    )
    .unwrap();
}

/// Everything cargo/autoprune manage, relative to the target dir. Object files are collapsed to
/// their unit (codegen-unit names are not deterministic), autoprune's own state is ignored.
pub fn inventory(target: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let ls = |p: &Path| -> Vec<String> {
        fs::read_dir(p)
            .map(|rd| {
                rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    for prof in profile_dirs(target) {
        let rel = prof.strip_prefix(target).unwrap().display().to_string();
        for area in ["deps", ".fingerprint", "build", "incremental", "examples"] {
            for n in ls(&prof.join(area)) {
                let n = match n.find('.') {
                    Some(i) if n.ends_with(".rcgu.o") => format!("{}.*.rcgu.o", &n[..i]),
                    _ => n,
                };
                out.insert(format!("{rel}/{area}/{n}"));
            }
        }
        for n in ls(&prof) {
            let skip = [
                "deps",
                ".fingerprint",
                "build",
                "incremental",
                "examples",
                ".autoprune",
            ];
            if !skip.contains(&n.as_str()) && !n.starts_with(".cargo") {
                out.insert(format!("{rel}/{n}"));
            }
        }
    }
    out
}

/// Move every mtime under `p` back by `secs` (simulates time passing).
pub fn age_tree(p: &Path, secs: u64) {
    let Ok(m) = fs::symlink_metadata(p) else {
        return;
    };
    if m.is_dir() {
        for e in fs::read_dir(p).unwrap() {
            age_tree(&e.unwrap().path(), secs);
        }
    }
    if let (Ok(t), Ok(f)) = (m.modified(), fs::File::open(p)) {
        let _ = f.set_modified(t - Duration::from_secs(secs));
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn has_clippy() -> bool {
    Command::new("cargo")
        .args(["clippy", "--version"])
        .output()
        .is_ok_and(|o| o.status.success())
}
