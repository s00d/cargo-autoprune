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
| Superseded workspace units | Group by package + target kind (lib, test, bin, example, build-script compile/run) + outputs (`rlib` or `check` rmeta) + fingerprint json signature (features, profile, rustflags, config, target) + rustc hash + clippy tag. In each group, delete copies older than the newest by more than the grace period |
| Registry/git units | Kept only while a kept workspace unit depends on them, transitively, through the `deps` edges in `.fingerprint/*/*.json`. Removed dependencies, old versions and old feature sets go automatically |
| Old toolchains | A rustc hash with no writes for 14 days before the newest unit is dropped |
| `incremental/<crate>-<id>/` | Follows the workspace unit of the same crate with the closest mtime, and goes with it. Dirs of crates that no longer exist are deleted |
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

For the second case, a deleted unit that gets rebuilt within 30 minutes is treated as a live variant.
Its group is pinned, which stops deduplication for 24 hours. A revert done more than 30 minutes later
is deduplicated as usual.

State lives in `<profile>/.autoprune/` (`stamp`, `log`, `pinned`, `deleted`, `tags/`). Each prune
appends one line to `log`.

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

**Self-test** (`scripts/self-test.sh`): this crate's own `cargo test`, run four times with a version
bump each time, which rehashes every unit:

| Run | No wrapper | With cargo-autoprune |
|---|---|---|
| v0.1.1 | 18.2 MiB, 1 test exe | 18.2 MiB, 1 |
| v0.1.2 | 36.5 MiB, 2 | 18.2 MiB, 1 (pruned 24.7 MiB in 25 ms) |
| v0.1.3 | 54.7 MiB, 3 | 18.2 MiB, 1 |
| v0.1.4 | 72.9 MiB, 4 | 18.2 MiB, 1 |

A rebuild right after the last run: `Finished ... in 0.00s`. Nothing is recompiled, so the cache survives.

**Churn test** (`scripts/churn-test.sh`): a 2-crate workspace with a build script, integration test,
example, `--features` variant, `check`, `clippy` and `--release`. Each step changes the dependencies,
then runs the whole command set with the wrapper. The result is compared with an *oracle*: the same
commands in an empty target dir.

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
| 10 re-revert within 30 min | 131.4 | 47.0 | 168 | 0 | 0 |

Step 10 is the pin rule at work: a unit that comes back within 30 minutes looks like a live variant,
so that group keeps its copies for 24 hours. Without pruning, the same steps accumulate every copy.
In the first version (no dependency graph), step 4 alone left 215 files.

**Wrapper overhead**: 1000 calls of `cargo-autoprune <prog> --out-dir …` take 3.4 ms more per call
than calling `<prog>` directly. That is one extra `exec`, while rustc itself takes 100 ms to minutes per
crate. A prune of a small profile dir takes 4–50 ms.

## Tests

```bash
cargo test                      # unit tests on temp dirs: grouping, graph, pins, incremental, cap, locks
scripts/self-test.sh            # self-cleaning on this crate
scripts/churn-test.sh           # dependency churn vs oracle (needs the pinned crates in ~/.cargo/registry)
```

## Limitations

- Windows: builds and spawns detached, but untested.
- Units of a second workspace that shares one target dir through `CARGO_TARGET_DIR` count as
  registry units. Without a `Cargo.lock` next to the target, everything falls back to group
  deduplication.
- Uplifted outputs of a renamed or removed binary (`target/debug/oldname`) are not removed.
