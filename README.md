# cargo-autoprune

Cargo never deletes superseded build artifacts. Every dependency bump, feature flip, `check` vs
`build`, or `-p x --features y` run gives units a new hash in `target/<profile>/{deps,.fingerprint,build,incremental}`,
and the old copies stay there forever. A busy workspace easily grows `target/` to 50+ GB.

`cargo-autoprune` is a `rustc-wrapper`. After builds it removes what Cargo can no longer use, in the
background, and keeps everything the current build needs.

## Install (one line of config)

```bash
cargo install --path .        # not on crates.io
```

`~/.cargo/config.toml`:

```toml
[build]
rustc-wrapper = "cargo-autoprune"
```

That is all. Every project you build cleans itself.

- **Chain another wrapper** (sccache etc.): set `CARGO_AUTOPRUNE_WRAPPER=sccache` (in the shell, or in
  `[env]` of the config) instead of putting it in `rustc-wrapper`.
- **Disable**: remove the line, or set `CARGO_AUTOPRUNE=0` (the wrapper stays a transparent pass-through).
- **Manual run / preview**: `cargo autoprune --dry-run [DIR]`, `cargo autoprune [--max-size 20G] [DIR]`.

| Variable | Default | Meaning |
|---|---|---|
| `CARGO_AUTOPRUNE` | on | `0` disables the background prune |
| `CARGO_AUTOPRUNE_INTERVAL` | `300` | Minimum seconds between background prunes of one profile dir |
| `CARGO_AUTOPRUNE_GRACE` | `600` | Copies built within this many seconds of the newest copy are kept |
| `CARGO_AUTOPRUNE_MAX_SIZE` | none | Size cap per profile dir, e.g. `20G`. The oldest units go first |
| `CARGO_AUTOPRUNE_WRAPPER` | none | Wrapper to run instead of plain rustc, e.g. `sccache` |

## How it works

1. **Wrapper (hot path).** Cargo runs `cargo-autoprune rustc <args>` for every crate it compiles.
   The wrapper finds the profile dir from `--out-dir`, does one `stat` of
   `<profile>/.autoprune/stamp` and then `exec`s rustc (or the chained wrapper). Cargo skips the wrapper
   entirely for fresh crates, so a no-op build never touches it.
2. **Trigger.** If the stamp is older than the interval, the wrapper touches it and spawns a
   detached copy of itself (`--background`, own process group, stdio closed). The build never waits for it.
3. **Lock.** The background process takes every Cargo lock of the profile dir (`.cargo-lock`,
   `.cargo-build-lock`, `.cargo-artifact-lock`). It takes all of them or none, so it can't deadlock with
   Cargo. It waits until the running build finishes and holds the locks while it deletes, so no Cargo
   build can start meanwhile. A second pruner exits at once.
4. **Prune.** It runs in tens of milliseconds on small targets.

## What gets deleted

A *unit* is everything with one metadata hash `H`: `.fingerprint/<pkg>-H/`, `build/<pkg>-H/`, and
`deps/*-H*` / `examples/*-H*` files (`.rlib`, `.rmeta`, executables, `.d`, `.dSYM`, macOS `*.rcgu.o`).

| What | Rule |
|---|---|
| Units with no fingerprint (orphans, e.g. `.o`/`.d` left after an executable was deleted) | Always. Cargo can never consider them fresh |
| Units of removed or renamed targets (bin, test, example, build script) | The crate root named first in the unit's rustc dep-info file no longer exists. A build-script run goes with its compiled build script |
| Superseded workspace units | Group by package + target kind (lib, test, bin, example, build-script compile/run) + outputs (`rlib` or `check` rmeta) + fingerprint json signature (features, profile, rustflags, config, target) + rustc hash + clippy tag. In each group, delete copies older than the newest by more than the grace period |
| Registry/git units | Kept only while a kept workspace unit depends on them, transitively, through the `deps` edges in `.fingerprint/*/*.json`. Edges from the other profile dirs of the same target count too, because with `--target` the build scripts and proc-macros live in the host profile. Removed dependencies, old versions and old feature sets go automatically. A removed workspace member is no longer in `Cargo.lock`, so it is treated the same way |
| Old toolchains | A rustc hash with no writes for 14 days before the newest unit is dropped |
| `incremental/<crate>-<id>/` | Follows its unit. The session's work products are hard links to the unit's `.rmeta` / `.rcgu.o` in `deps/`, so the inode identifies the owner. Without a shared inode (Linux deletes a binary's objects), units and dirs of the crate are paired one-to-one, closest mtime first. Dirs left without a unit are deleted |
| Over `CARGO_AUTOPRUNE_MAX_SIZE` | The oldest remaining units go first |

What it never touches: anything outside `target/<profile>/{deps,examples,.fingerprint,build,incremental}`,
uplifted outputs in the profile root (`target/debug/app`), `target/doc`, `target/package` and `target/tmp`.
It refuses to work on a dir without Cargo's `CACHEDIR.TAG`. A wrong guess costs one rebuild of that unit,
never a broken build.

**Variants that can't be told apart** come up in two cases:

- `clippy` units have the same json as `check` units. The wrapper tags units compiled while
  `RUSTC_WORKSPACE_WRAPPER` is set, a file write that happens only under clippy.
- `cargo build -p x` and `cargo build --workspace` resolve features differently, so the same crate is
  built against different dependencies.

For the second case only timing helps. The pruner remembers deletions for 24 hours. A deleted unit
that gets rebuilt is a *comeback*:

- The first comeback in a group is treated as a revert (you tried a dependency and went back), so
  the group stays deduplicated.
- A second comeback in the same group within 24 hours means two variants alternate. The group is
  pinned and not deduplicated for 24 hours.

Alternating variants therefore cost one extra rebuild before the pin. Toggling a dependency back and
forth twice looks the same as alternation and also pins.

State lives in `<profile>/.autoprune/` (`stamp`, `log`, `deleted`, `suspects`, `pinned`, `tags/`).
Each prune appends one line to `log`.

**Locality** (workspace or registry) comes from the `Cargo.lock` next to `target/`. With a shared
`CARGO_TARGET_DIR` there is no such file. Then every unit falls back to group deduplication, which
still removes superseded copies but keeps removed dependencies until the size cap evicts them.

## Comparison

| Tool | How it runs | What it deletes | How it chooses | Notes |
|---|---|---|---|---|
| **cargo-autoprune** | rustc-wrapper: automatic, in the background after builds | Superseded units, unreachable registry units, orphans, stale incremental dirs, old toolchains, optional size cap | Cargo's fingerprint graph, unit groups, mtime; waits for Cargo's locks | One config line, no daemon, zero dependencies |
| [cargo-reclaim](https://github.com/ml-rust/cargo-reclaim) | Manual CLI (`scan`/`plan`/`apply`), or a scheduler service (launchd/systemd/Task Scheduler) on an interval or when free disk space is low | Incremental, fingerprints, dep-info, older hashed `deps` variants. Policies go up to the whole target and home caches | Filesystem scan. Keeps the newest of each hashed family and skips targets with a live cargo or recent writes | The closest analog. Large policy surface. No build hook, and doesn't understand the dependency graph |
| [cargo-sweep](https://github.com/holmgr/cargo-sweep) | Manual CLI / GitHub Action | Files in target | `--time N` days, `--stamp`/`--file`, `--installed` toolchains, `--maxsize` | Simple. Age-based, so a day of churn survives `--time` entirely. The README says unmaintained |
| [cargo-cache](https://github.com/matthiaskrgr/cargo-cache) | Manual CLI | `~/.cargo` registry/git only | By type | Never touches target. Stale (2022) |
| [cargo-clean-all](https://github.com/dnlmlr/cargo-clean-all) | Manual CLI, interactive | Whole `target/` of projects under a dir | Project age or size | All or nothing per project |
| [kondo](https://github.com/tbillington/kondo) | Manual CLI / GUI | Whole artifact dirs, many ecosystems | Project age | Not Rust-specific |
| [cargo-trim](https://github.com/iamsauravsharma/cargo-trim) | Manual CLI | `~/.cargo` registry/git, old crate versions | Version, age, orphans | Mostly about caches |
| cargo-prune | Manual CLI | `~/.cargo` registry cache | Old versions | Dead (2019) |
| cargo-gc-target | Manual `cargo gc` | `deps`, `build`, `.fingerprint` | Traces what the current workspace needs | Dead (2021). Breaks shared targets |
| Cargo `-Zgc` / `cargo clean gc` | Automatic about daily (registry cache), nightly CLI | `~/.cargo` registry/git only | Last-use database | Target GC not implemented yet ([cargo#13136](https://github.com/rust-lang/cargo/issues/13136)) |

None of the others prunes per build through a wrapper or uses the fingerprint dependency graph.

**Why a rustc-wrapper.** Cargo has no post-build hook. The alternatives are worse:

- A `cargo` alias can't override built-in commands.
- `[env]` can't run anything.
- A `cargo` shim on `PATH` breaks with rustup proxies and IDEs.
- A scheduler daemon runs blind to builds.

`build.rustc-wrapper` is official, works with every cargo command, IDE and rust-analyzer, and its cost
is one extra `exec` per compiled crate.

## Measurements (macOS, Apple Silicon, cargo 1.99)

**Self-test** (`scripts/self-test.sh`): this crate's own `cargo test` (three test executables) is run
four times with a version bump each time, which rehashes every unit.

| Run | No wrapper | With cargo-autoprune |
|---|---|---|
| v0.1.1 | 64.6 MiB, 3 test exes | 64.6 MiB, 3 |
| v0.1.2 | 126.8 MiB, 6 | 64.6 MiB, 3 (pruned 80.6 MiB in 170 ms) |
| v0.1.3 | 189.1 MiB, 9 | 64.6 MiB, 3 |
| v0.1.4 | 251.4 MiB, 12 | 64.6 MiB, 3 |

A rebuild right after the last run: `Finished ... in 0.00s`. Nothing is recompiled, so the cache survives.

**Churn test** (`scripts/churn-test.sh`) uses real crates.io crates: serde, libc, regex, tokio. The
workspace has two crates, a build script, an integration test, an example, a `--features` variant,
`check`, `clippy` and `--release`. Each step changes the dependencies, then runs the whole command set
with the wrapper. The result is compared with an *oracle*: the same commands in an empty target dir.

- `left`: files autoprune kept that the oracle doesn't have.
- `lost`: files the oracle has that autoprune deleted.
- `rerun`: units recompiled when the command set runs again.

| Step | target | oracle | left | lost | rerun |
|---|---|---|---|---|---|
| 1 serde + libc | 90.1 MiB | 90.1 MiB | 0 | 0 | 0 |
| 2 libc 0.2.190 → 0.2.191 | 90.2 | 90.2 | 0 | 0 | 0 |
| 3 add regex + itoa | 193.2 | 193.1 | 0 | 0 | 0 |
| 4 remove regex | 90.7 | 90.7 | 0 | 0 | 0 |
| 5 serde `+rc` feature | 90.8 | 90.7 | 0 | 0 | 0 |
| 6 add tokio 1.53.3 | 108.8 | 108.7 | 0 | 0 | 0 |
| 7 tokio → 1.54.1, drop serde | 47.0 | 46.9 | 0 | 0 | 0 |
| 8 edit app source | 47.9 | 47.0 | 0 | 0 | 0 |
| 9 revert to step 1 (an hour later) | 90.2 | 90.1 | 0 | 0 | 0 |
| 10 back to step 8 again | 131.4 | 47.0 | 168 | 0 | 0 |

Step 10 is the second comeback, so those groups are pinned for 24 hours (see above). Without the
pruner, every step keeps all of its copies.

**Speed.** Wrapper overhead is 2.8 ms per compiled crate: 1000 calls take 5.3 s instead of 2.5 s.
That is one extra `exec`, while rustc takes 100 ms to minutes per crate. A background prune scans a
profile dir with 126,000 files (2,000 units) in 0.7 s warm and 1.8 s cold, and a small dir in
5–50 ms. The prune runs after the build, while Cargo's lock is held for that short time.

## Tests

```bash
cargo test                      # 26 unit tests + 20 end-to-end tests with real cargo
scripts/self-test.sh            # self-cleaning on this crate
scripts/churn-test.sh           # real crates.io crates vs oracle (needs them in ~/.cargo/registry)
```

The end-to-end suite (`tests/e2e.rs`, harness in `tests/support/`) needs no network. It builds a
throwaway workspace against a vendored `directory` registry: two versions of a crate, a crate with a
build script, a feature crate and a proc-macro. After every change it checks three things:

- The pruned target equals a from-scratch oracle target.
- A rerun compiles nothing.
- A second prune finds nothing.

Scenarios covered:

- Version bump, quick revert, revert an hour later
- Added and removed dependencies, a transitive bump through a build-script crate, a registry feature change
- Workspace source edits
- Feature variants of a member (`-p util --features extra`), and feature-unification alternation
  (pinned after the second comeback, then nothing rebuilds)
- `check` / `clippy` / `doc` / `bench` / `--release`
- A custom profile, and an explicit `--target <host>` (cross-profile edges)
- A `RUSTFLAGS` change
- A removed workspace member and a renamed binary
- A toolchain switch (another installed toolchain), and its retirement after 3 simulated weeks
- Orphaned objects, the size cap, a shared target without `Cargo.lock`
- The background prune waiting for a running build
- The wrapper being transparent: version probes, chaining, compile errors
- CLI `--dry-run`

Source layout: `wrapper.rs` (hot path), `cli.rs`, `prune.rs` (one pass), `scan.rs` (units),
`plan.rs` (rules), `fingerprint.rs` (json, dep-info, `Cargo.lock`), `state.rs`, `lock.rs`,
`layout.rs`, `fsutil.rs`.

## Limitations

- Windows: builds and spawns detached, but untested.
- Units of a second workspace that shares one target dir through `CARGO_TARGET_DIR` count as
  registry units. Without a `Cargo.lock` next to the target, everything falls back to group
  deduplication.
- Uplifted outputs of a renamed or removed binary (`target/debug/oldname`) are not removed.
