//! Fake profile dirs for unit tests.

use crate::prune::{prune, Config, Outcome, Report};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) const A: &str = "aaaaaaaaaaaaaaaa";
pub(crate) const B: &str = "bbbbbbbbbbbbbbbb";
pub(crate) const C: &str = "cccccccccccccccc";

pub(crate) struct Fx {
    pub(crate) root: PathBuf,
    pub(crate) prof: PathBuf,
}

impl Fx {
    pub(crate) fn new(tag: &str) -> Fx {
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
    pub(crate) fn file(&self, rel: &str, age_secs: u64) -> PathBuf {
        let p = self.prof.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![0u8; 1000]).unwrap();
        set_age(&p, age_secs);
        p
    }
    pub(crate) fn dir(&self, rel: &str, age_secs: u64) -> PathBuf {
        let p = self.prof.join(rel);
        fs::create_dir_all(&p).unwrap();
        set_age(&p, age_secs);
        p
    }
    /// A lib unit: fingerprint + rlib/rmeta (or rmeta only for `check`) + .d + .o.
    pub(crate) fn lib(&self, pkg: &str, h: &str, age: u64, check: bool) {
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
    pub(crate) fn exists(&self, rel: &str) -> bool {
        self.prof.join(rel).exists()
    }
    pub(crate) fn run(&self, cfg: &Config) -> Report {
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

pub(crate) fn set_age(p: &Path, secs: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs);
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

pub(crate) fn cfg() -> Config {
    Config {
        lock_wait: Duration::ZERO,
        ..Config::default()
    }
}

impl Fx {
    /// A unit with an explicit fingerprint json: rustc hash, own value and dependency values.
    pub(crate) fn node(&self, pkg: &str, h: &str, age: u64, rustc: u64, value: u64, deps: &[u64]) {
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
    /// A unit with a hand-written fingerprint: `kind` names the fingerprint files.
    pub(crate) fn unit_json(
        &self,
        pkg: &str,
        h: &str,
        age: u64,
        kind: &str,
        json: &str,
        value: u64,
    ) {
        let fp = format!(".fingerprint/{pkg}-{h}");
        self.file(&format!("{fp}/{kind}"), age);
        fs::write(self.prof.join(format!("{fp}/{kind}")), hex_le(value)).unwrap();
        self.file(&format!("{fp}/{kind}.json"), age);
        fs::write(self.prof.join(format!("{fp}/{kind}.json")), json).unwrap();
        for f in [kind.to_owned(), format!("{kind}.json")] {
            set_age(&self.prof.join(&fp).join(f), age);
        }
        set_age(&self.prof.join(&fp), age);
    }
    /// A unit in cargo 1.100's build-dir layout: `build/<pkg>/<hash>/{fingerprint,out}`.
    pub(crate) fn new_unit(&self, pkg: &str, h: &str, age: u64, outs: &[&str]) {
        let d = format!("build/{pkg}/{h}");
        for f in [
            format!("lib-{pkg}"),
            format!("lib-{pkg}.json"),
            "invoked.timestamp".into(),
        ] {
            self.file(&format!("{d}/fingerprint/{f}"), age);
        }
        set_age(&self.prof.join(format!("{d}/fingerprint")), age);
        self.dir(&format!("{d}/out"), age);
        for o in outs {
            self.file(&format!("{d}/out/{o}"), age);
        }
        set_age(&self.prof.join(format!("{d}/out")), age);
        set_age(&self.prof.join(&d), age);
    }
    pub(crate) fn lockfile(&self, locals: &[&str], registry: &[&str]) {
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

pub(crate) fn hex_le(v: u64) -> String {
    v.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
