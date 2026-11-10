//! Kinds of dependencies: every change must leave the pruned target equal to a from-scratch
//! target of the same commands, with nothing recompiled on a rerun.

mod support;

use std::fs;
use std::process::Command;
use support::*;

const CMDS: &[&str] = &[
    "build --workspace --all-targets",
    "test --workspace --no-run",
    "check --workspace",
];

fn step(ws: &Ws, deps: &str) {
    ws.deps(deps);
    ws.run_all(CMDS);
    ws.prune();
    ws.assert_clean(CMDS);
}

#[test]
fn two_major_versions_side_by_side() {
    let ws = Ws::new("major");
    step(
        &ws,
        "dep_a = \"=1.0.0\"\ndep_a2 = { package = \"dep_a\", version = \"=2.0.0\" }",
    );
    step(
        &ws,
        "dep_a = \"=1.1.0\"\ndep_a2 = { package = \"dep_a\", version = \"=2.0.0\" }",
    );
    step(
        &ws,
        "dep_a2 = { package = \"dep_a\", version = \"=2.0.0\" }",
    ); // 1.x dropped
}

#[test]
fn dev_and_build_dependencies() {
    let ws = Ws::new("devbuild");
    step(
        &ws,
        "[dev-dependencies]\ndep_c = \"1\"\n[build-dependencies]\ndep_a = \"=1.0.0\"",
    );
    step(&ws, "[dev-dependencies]\ndep_c = { version = \"1\", features = [\"fast\"] }\n[build-dependencies]\ndep_a = \"=1.1.0\"");
    step(&ws, "");
}

#[test]
fn optional_dependency_behind_a_feature() {
    let ws = Ws::new("optional");
    let with = "dep_c = { version = \"1\", optional = true }\n[features]\ndefault = [\"fast\"]\nfast = [\"dep_c\"]";
    let without = "dep_c = { version = \"1\", optional = true }\n[features]\nfast = [\"dep_c\"]";
    step(&ws, with);
    // the default feature set changes: the old variant stays (it could be `--features fast`)...
    ws.deps(without);
    ws.run_all(CMDS);
    ws.prune();
    let (_, lost) = ws.oracle_diff(CMDS);
    assert!(lost.is_empty(), "{lost:#?}");
    assert_eq!(ws.run_all(CMDS), 0);
    // ...until a week passes in which only the new one is rebuilt
    age_tree(&ws.target, 8 * 24 * 3600);
    ws.write("app/src/lib.rs", "pub fn app() -> u32 { util::u() + 1 }\n");
    ws.run_all(CMDS);
    ws.prune();
    ws.assert_clean(CMDS);
}

#[test]
fn target_specific_dependency() {
    let ws = Ws::new("cfgdep");
    step(&ws, "[target.'cfg(unix)'.dependencies]\ndep_c = \"1\"\n[target.'cfg(windows)'.dependencies]\ndep_a = \"1\"");
    step(&ws, "[target.'cfg(not(unix))'.dependencies]\ndep_c = \"1\"");
}

#[test]
fn path_dependency_outside_the_workspace() {
    let ws = Ws::new("outside");
    let ext = ws.root.join("ext");
    fs::create_dir_all(ext.join("src")).unwrap();
    let manifest = |a: &str| {
        fs::write(
            ext.join("Cargo.toml"),
            format!("[package]\nname = \"ext\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\ndep_a = \"={a}\"\n"),
        )
        .unwrap();
    };
    fs::write(ext.join("src/lib.rs"), "pub fn e() -> u32 { dep_a::a() }\n").unwrap();
    manifest("1.0.0");
    let dep = format!("ext = {{ path = \"{}\" }}", ext.display());
    step(&ws, &dep);
    manifest("1.1.0"); // the outside crate's own dependency changes
    step(&ws, &dep);
    step(&ws, ""); // and it is dropped
}

#[test]
fn patched_registry_crate() {
    let ws = Ws::new("patch");
    let patched = ws.root.join("patched_a");
    fs::create_dir_all(patched.join("src")).unwrap();
    fs::write(
        patched.join("Cargo.toml"),
        "[package]\nname = \"dep_a\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(patched.join("src/lib.rs"), "pub fn a() -> u32 { 999 }\n").unwrap();
    let workspace = |patch: bool| {
        let p = if patch {
            format!(
                "[patch.crates-io]\ndep_a = {{ path = \"{}\" }}\n",
                patched.display()
            )
        } else {
            String::new()
        };
        ws.write(
            "Cargo.toml",
            &format!("[workspace]\nmembers = [\"app\", \"util\"]\nresolver = \"2\"\n{p}"),
        );
    };
    workspace(true);
    step(&ws, "dep_a = \"=1.0.0\"\ndep_b = \"1\""); // dep_b depends on the patched dep_a too
    workspace(false); // back to the registry version: the patched (path) units become garbage
    step(&ws, "dep_a = \"=1.0.0\"\ndep_b = \"1\"");
}

#[test]
fn inherited_workspace_dependencies() {
    let ws = Ws::new("inherit");
    let workspace = |v: &str| {
        ws.write(
            "Cargo.toml",
            &format!("[workspace]\nmembers = [\"app\", \"util\"]\nresolver = \"2\"\n[workspace.dependencies]\ndep_a = \"={v}\"\n"),
        );
    };
    workspace("1.0.0");
    step(&ws, "dep_a.workspace = true");
    workspace("1.1.0"); // only the workspace table changes
    step(&ws, "dep_a.workspace = true");
}

#[test]
fn git_dependency() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not installed, skipped");
        return;
    }
    let ws = Ws::new("git");
    let repo = ws.root.join("dep_g");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"dep_g\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let commit = |body: &str| -> String {
        fs::write(repo.join("src/lib.rs"), body).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(&repo)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        if !repo.join(".git").exists() {
            git(&["init", "-q"]);
        }
        git(&["add", "-A"]);
        git(&["commit", "-qm", "c"]);
        git(&["rev-parse", "HEAD"])
    };
    // the fetch writes to CARGO_HOME: give the test its own
    let home = ws.root.join("cargo-home");
    let home_s = home.display().to_string();
    let env = [("CARGO_HOME", home_s.as_str())];
    let run = |rev: &str| {
        ws.deps(&format!(
            "dep_g = {{ git = \"file://{}\", rev = \"{rev}\" }}",
            repo.display()
        ));
        let fetch = Command::new("cargo")
            .current_dir(&ws.dir)
            .arg("fetch")
            .env("CARGO_HOME", &home)
            .output()
            .unwrap();
        assert!(
            fetch.status.success(),
            "{}",
            String::from_utf8_lossy(&fetch.stderr)
        );
        for c in CMDS {
            ws.cargo_in(c, &ws.target.clone(), true, &env);
        }
        ws.prune();
        let oracle = ws.root.join("oracle-git");
        let _ = fs::remove_dir_all(&oracle);
        for c in CMDS {
            ws.cargo_in(c, &oracle, false, &env);
        }
        let (a, b) = (inventory(&ws.target), inventory(&oracle));
        let left: Vec<_> = a.difference(&b).collect();
        let lost: Vec<_> = b.difference(&a).collect();
        assert!(lost.is_empty(), "{lost:#?}");
        assert!(left.is_empty(), "{left:#?}");
        for c in CMDS {
            assert_eq!(ws.cargo_in(c, &ws.target.clone(), true, &env), 0);
        }
    };
    let r1 = commit("pub fn g() -> u32 { 1 }\n");
    run(&r1);
    let r2 = commit("pub fn g() -> u32 { 2 }\n");
    run(&r2); // new commit of the git dependency
}
