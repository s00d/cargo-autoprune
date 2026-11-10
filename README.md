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
- **Manual run / preview**: `cargo autoprune --dry-run [DIR]`, `cargo autoprune [--max-size 20G] [-v] [DIR]`.
- **What is going on**: `cargo autoprune status [DIR]` shows, per profile dir:
  - size on disk and what is reclaimable right now, by reason
  - the last prune
  - pinned groups and pending notices
  - whether the wrapper is configured, and settings in the environment that don't parse

```text
wrapper: enabled in /Users/me/.cargo/config.toml
./target/debug: 388.0MiB on disk, 15 units
  reclaimable now: 80.6MiB (5 units, 5 incremental dirs: stale incremental 5, superseded 5)
  last prune: 46 min ago, units=5 incremental=5 freed=80.6MiB ... [superseded 5, stale incremental 5]
```

| Variable | Default | Meaning |
|---|---|---|
| `CARGO_AUTOPRUNE` | on | `0` disables the background prune |
| `CARGO_AUTOPRUNE_INTERVAL` | `300` | Minimum seconds between background prunes of one profile dir |
| `CARGO_AUTOPRUNE_GRACE` | `600` | Copies built within this many seconds of the newest copy are kept |
| `CARGO_AUTOPRUNE_MAX_SIZE` | none | Size cap per profile dir, e.g. `20G`. The oldest units go first |
| `CARGO_AUTOPRUNE_WRAPPER` | none | Wrapper to run instead of plain rustc, e.g. `sccache` |
| `CARGO_AUTOPRUNE_QUIET` | off | `1` silences the terminal notices |
| `CARGO_AUTOPRUNE_TRACE` | off | `1` prints phase timings of manual runs (`-v` does the same) |

**Notices.** The background prune never prints during a build. It saves a notice when something is
worth knowing:

- a prune failed, or paths could not be deleted
- it freed 1 GiB or more
- it held cargo's lock for 5 s or more (the first cleanup of a huge target)

The next build prints those once on the terminal (`/dev/tty`), as
`warning: cargo-autoprune: freed 3.2GiB in target/debug (superseded 12, unreachable 40)`.
They don't go to stderr because cargo stores a crate's stderr and replays it on every later build
of that crate. Without a terminal (IDE, CI) the notices stay for `cargo autoprune status`.

## CI and servers

**GitHub Actions.** A restored `target/` cache keeps every superseded copy from earlier commits,
so it grows with each run. Trim it right before the cache is saved (the cache's post step runs
after your steps):

```yaml
- uses: actions/cache@v4
  with:
    path: |
      ~/.cargo/registry
      target
    key: cargo-${{ runner.os }}-${{ hashFiles('Cargo.lock') }}
    restore-keys: cargo-${{ runner.os }}-
- run: cargo test
- uses: s00d/cargo-autoprune@main   # = cargo autoprune --ci .
  with:
    max-size: 4G                    # optional
```

`--ci` means grace 0 (a job's own build is the only live state) and drops incremental dirs, which
CI never reuses. Don't put the wrapper into CI: a background prune could still be running when the
job ends. Run the step instead.

**Servers and self-hosted runners.** One command walks a tree of projects (or a runner's `_work`
dir), prunes every Cargo target dir it finds and deletes whole target dirs nothing was compiled
into for N days:

```bash
cargo autoprune -r --remove-idle 30 ~/projects ~/actions-runner/_work
```

Run it from cron or a systemd timer, e.g. `0 4 * * * cargo-autoprune autoprune -r --remove-idle 30 /srv/build`.
Target dirs locked by a running cargo are skipped. `--dry-run` shows what would happen.

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
   build can start meanwhile. A second pruner exits at once. A `--target` build holds the host
   profile's lock too (checked), so build scripts and proc-macros in `target/debug` are safe while a
   cross build runs. As an extra guard, registry units written within the grace period are never
   treated as unreachable.
4. **Prune.**
   - The scan takes no `stat` per file in `deps/`: names, file types and inodes come from `readdir`.
   - Fingerprints and incremental sessions are read on several threads.
   - While the lock is held, files are unlinked and whole directories are moved into
     `.autoprune/trash` with one rename each. The directories' contents are deleted after the lock
     is released.
   - Small targets take a few milliseconds (see Measurements).
5. **Bookkeeping in the wrapper.** For crates of path packages, the wrapper appends
   `<unit hash>\t<package dir>` to `.autoprune/units`. For clippy builds it writes a tag file. Both
   are single appends, and they are why shared target dirs and `build.build-dir` work (see below).

## What gets deleted

A *unit* is everything with one metadata hash `H`: `.fingerprint/<pkg>-H/`, `build/<pkg>-H/`, and
`deps/*-H*` / `examples/*-H*` files (`.rlib`, `.rmeta`, executables, `.d`, `.dSYM`, macOS `*.rcgu.o`).

| What | Rule |
|---|---|
| Units with no fingerprint (orphans, e.g. `.o`/`.d` left after an executable was deleted) | Always. Cargo can never consider them fresh |
| Units of removed or renamed targets (bin, test, example, build script) | The crate root named first in the unit's rustc dep-info file no longer exists. A build-script run goes with its compiled build script |
| Superseded workspace units | Grouped by package, target kind (lib, test, bin, example, build-script compile/run), outputs (`rlib` or `check` rmeta), the fingerprint signature (features, profile, `RUSTFLAGS`, config, host/target), rustc hash, clippy tag and workspace. In each group, copies older than the newest by more than the grace period go. So `-p x --features y` next to a plain build never thrash |
| Stale variants | A variant of a package (other features, `RUSTFLAGS`, profile or host) that hasn't been rebuilt for 7 days while the package has, e.g. the old build after an edit of `[features]` |
| Registry/git units | Kept only while a kept workspace unit depends on them, transitively, through the `deps` edges in `.fingerprint/*/*.json`. Edges from the other profile dirs of the same target count too, because with `--target` the build scripts and proc-macros live in the host profile. Removed dependencies, old versions and old feature sets go automatically. A removed workspace member is no longer in `Cargo.lock`, so it is treated the same way |
| Old toolchains | A rustc hash with no writes for 14 days before the newest unit is dropped |
| `incremental/<crate>-<id>/` | Follows its unit. The session's work products are hard links to the unit's `.rmeta` / `.rcgu.o` in `deps/`, so the inode identifies the owner. Without a shared inode (Linux deletes a binary's objects), units and dirs of the crate are paired one-to-one, closest mtime first. Dirs left without a unit are deleted |
| Over `CARGO_AUTOPRUNE_MAX_SIZE` | The oldest remaining units go first |

What it never touches: anything outside `target/<profile>/{deps,examples,.fingerprint,build,incremental}`
(or `build-dir`),
uplifted outputs in the profile root (`target/debug/app`), `target/doc`, `target/package` and `target/tmp`.
It refuses to work on a dir without Cargo's `CACHEDIR.TAG`. A wrong guess costs one rebuild of that unit,
never a broken build.

**Variants that can't be told apart** come up in two cases:

- `clippy` units have the same json as `check` units. The wrapper tags units compiled while
  `RUSTC_WORKSPACE_WRAPPER` is set, a file write that happens only under clippy.
- `cargo build -p x` vs `cargo build --workspace` resolve features differently, so a crate is built
  against different dependencies: same config, different deps. A dependency bump looks the same.

For the second case the pruner remembers deletions for 24 hours, together with a hash of the
workspace `Cargo.lock`. A deleted unit that gets rebuilt is a *comeback*:

- If `Cargo.lock` changed since the deletion, it is a dependency revert or toggle. It never counts,
  however often you switch versions back and forth.
- Under the same `Cargo.lock`, the first comeback in a group is noted and the second pins the group
  for 24 hours. That costs alternating variants one extra rebuild.

**Both build-dir layouts.** Cargo up to 1.99 uses flat `deps/` and `.fingerprint/` dirs. Cargo
1.100's layout uses `build/<pkg>/<hash>/{fingerprint,out,run}`, one dir per unit, so a unit is
deleted with a single rename. After an upgrade, legacy units are dropped once nothing has written
them for a day longer than the new layout. A toolchain still in use keeps writing them, so they
stay.

State lives in `<profile>/.autoprune/`: `stamp`, `log`, `deleted`, `suspects`, `pinned`, `tags/`,
`units`, `notice`. Each prune appends one line to `log`, with counts per reason.

**Locality** (workspace or registry) comes from `Cargo.lock` files:

- the one next to `target/`;
- the workspaces of the package dirs that the wrapper recorded.

This covers:

- a shared `CARGO_TARGET_DIR`, where several workspaces with same-named packages stay apart;
- a separate `build.build-dir`, where the profile dirs have only `.cargo-build-lock`.

With no lock file found at all (only units built before the wrapper was installed), every unit falls
back to group deduplication. That still removes superseded copies, but keeps removed dependencies
until the size cap evicts them.

## Comparison

**Head to head on a real workspace.** The test project is vld: 695 crates, a 5 GB target.
Scenario: bump `itoa` in `Cargo.lock`, build, prune, then compare with a target built from scratch
and rebuild.

| | cargo-autoprune | cargo-sweep 0.8 (`--stamp` / `--file`) |
|---|---|---|
| freed | 3.0 GiB | 2.85 GiB |
| garbage left (vs from-scratch target) | **0** | 196 files |
| live files deleted | **0** | **2,799 files** |
| next build | **0 crates** | **694 of 695 crates (full rebuild)** |
| scan (dry run, warm) | 0.18–0.21 s | 0.47–0.74 s |
| safe with a running cargo | yes (cargo's locks, 1 s batches) | no lock |

cargo-sweep's stamp mode trusts access times. On macOS (and on Linux with `noatime`) cargo doesn't
update them for files it only checks, so live artifacts look unused.

All tools:

| Tool | How it runs | What it deletes | How it chooses | Notes |
|---|---|---|---|---|
| **cargo-autoprune** | rustc-wrapper: automatic, in the background after builds; CLI; CI action; `-r` for servers | Superseded units, unreachable registry units, orphans, units of removed targets, stale incremental dirs, dead variants, old toolchains and layouts, optional size cap, idle whole targets | Cargo's fingerprint dependency graph, unit groups, comebacks under an unchanged `Cargo.lock`, hard-link inodes; takes Cargo's locks | One config line, no daemon, zero dependencies. Both build-dir layouts (cargo ≤ 1.99 and 1.100+) |
| [cargo-reclaim](https://github.com/ml-rust/cargo-reclaim) | Manual CLI (`scan`/`plan`/`apply`), or a scheduler service (launchd/systemd/Task Scheduler) on an interval or when free disk space is low | Incremental, fingerprints, dep-info, older hashed `deps` variants. Policies go up to the whole target and home caches | Filesystem scan. Keeps the newest of each hashed family and skips targets with a live cargo or recent writes | Closest analog. No build hook and no dependency graph. Its docs warn that a "superseded" hash can be a live variant |
| [cargo-sweep](https://github.com/holmgr/cargo-sweep) | Manual CLI / GitHub Action | Files in target | `--time N` days, `--stamp`/`--file` (atime), `--installed` toolchains, `--maxsize` | See the head to head above. README says unmaintained |
| [Swatinem/rust-cache](https://github.com/Swatinem/rust-cache) | CI post-step | Everything in a profile except `build`, `.fingerprint`, `deps` (so `incremental/` and binaries go). Then workspace crates by name, or entries older than 7 days | Name matching against `cargo metadata`, mtime | Caches dependencies only, never workspace crates. Name-based, so superseded dependency versions stay up to 7 days |
| [Leafwing cargo-cache](https://github.com/Leafwing-Studios/cargo-cache) | CI action (`sweep-cache: true`) | Files not used during the job | cargo-sweep-style stamp | Off by default; same atime caveat |
| [cargo-cache](https://github.com/matthiaskrgr/cargo-cache) | Manual CLI | `~/.cargo` registry/git only | By type | Never touches target. Stale (2022) |
| [cargo-clean-all](https://github.com/dnlmlr/cargo-clean-all), [clean-dev-dirs](https://docs.rs/crate/clean-dev-dirs), [kondo](https://github.com/tbillington/kondo), cargo-wipe, cargo-clean-recursive | Manual CLI | Whole `target/` dirs | Project age, size, interactive | All or nothing per project; `--remove-idle` covers this |
| [cargo-trim](https://github.com/iamsauravsharma/cargo-trim), cargo-prune | Manual CLI | `~/.cargo` registry/git, old crate versions | Version, age | Caches, not target |
| cargo-gc-target, cargo-target-gc | Manual CLI | Target artifacts | Workspace trace / undocumented | Dead (2021) / new (2026) |
| Cargo `cargo clean -p` / `--profile` | Manual | A package's units or a profile | Package spec | Takes the lock; rebuilds what it deletes |
| Cargo auto-GC (`-Zgc`, stable for `~/.cargo` since 2025) | Automatic, about daily | `~/.cargo` registry/git only | Last-use SQLite database | Target-dir GC: tracking issue [cargo#13136](https://github.com/rust-lang/cargo/issues/13136), whole idle dirs only (PR #16929, unmerged) |
| cargo-chef, sccache, rust-analyzer `cargo.targetDir` | Docker stage / compiler cache / IDE setting | Nothing in target | – | Avoid or split the problem, don't clean it |

Nothing else uses Cargo's fingerprint dependency graph or runs automatically after every build.
The closest to "after every build" is a CI post-step.

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

**Speed.**

- **Wrapper.** 2.8 ms per compiled crate, the same as a minimal C `execv` wrapper (4.7–5.5 ms per
  call vs 2.4–2.7 ms for the bare program in the same loop). The cost is the second `exec` on macOS;
  the wrapper's own work (a few `stat`s) is noise. rustc itself takes 100 ms to minutes per crate.
- **Prune.** Measured on a generated target (`scripts/bench-tree.py`) with 52,000 files:
  - 2,400 units: 150 workspace packages, each with a current and a superseded copy, plus 600
    registry crates;
  - test executables with 50 objects each;
  - incremental sessions hard-linked to `deps/`.

  Warm cache, release build:

| Phase | First version | Now |
|---|---|---|
| scan `.fingerprint` (2,400 dirs) | 200 ms | 83 ms (threads) |
| scan `deps` (32,000 files) | 210 ms | 23 ms (no `stat`, inodes from `readdir`) |
| scan incremental | 145 ms | 23 ms |
| plan | 25 ms | 11 ms (dep-info read only for kept units, on threads) |
| cargo blocked, dry run | 770 ms | 140 ms |
| cargo blocked, deleting 1,197 units / 10,000 files | 5–6.7 s (trash renames) | 1.0–1.7 s (unlink files, rename dirs) |

  The deletion is bound by APFS: about 0.1 ms per unlink in a directory of 32,000 entries,
  serialized per directory, so more threads don't help. A normal prune after a dependency bump
  deletes tens to hundreds of files and blocks cargo for a few milliseconds.

## Tests

```bash
cargo test                      # fast: 35 unit + 13 end-to-end tests (~15 s)
cargo test -- --include-ignored # all 71: the slow multi-step scenarios too (CI runs these)
python3 scripts/bench-tree.py /tmp/bench && CARGO_AUTOPRUNE_TRACE=1 cargo autoprune --dry-run /tmp/bench
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
- Two workspaces with same-named packages in one target dir, and a separate `build.build-dir`
- Dependency kinds (`tests/deps.rs`):
  - two major versions side by side
  - dev and build dependencies
  - an optional dependency behind a feature
  - a `cfg(...)` target dependency
  - a path dependency outside the workspace
  - `[patch.crates-io]`
  - `workspace = true` inheritance
  - a git dependency (a local repo, new commit)
- Notices for failed deletions, the status command, `cargo-autoprune DIR` without a subcommand
- The background prune waiting for a running build
- The wrapper being transparent: version probes, chaining, compile errors
- CLI `--dry-run`

Source layout: `wrapper.rs` (hot path), `cli.rs`, `prune.rs` (one pass), `scan.rs` (units),
`plan.rs` (rules), `fingerprint.rs` (json, dep-info, `Cargo.lock`), `state.rs`, `lock.rs`,
`layout.rs`, `fsutil.rs`.

## Limitations

- Windows builds and spawns detached, but is untested, and notices are not printed there (no `/dev/tty`).
- Uplifted outputs of a renamed or removed binary (`target/debug/oldname`) are not removed.
- The first prune of a huge, never-pruned target (tens of thousands of stale files) can hold cargo's
  lock for several seconds, which is reported as a notice. Later prunes are small.
- Units built before the wrapper was installed carry no workspace record. In a shared target dir,
  same-named packages of different workspaces can evict each other once, until they are rebuilt
  through the wrapper.
