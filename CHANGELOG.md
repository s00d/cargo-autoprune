# Changelog

## 0.1.2 — 2026-11-13

Support for cargo 1.100 (stable since 2026-11-09), which writes the `build/<pkg>/<hash>/` layout by
default.

### Fixed
- cargo 1.100's layout: a prune left `build/<pkg>/` dirs behind once all their units were gone; they
  are removed now when empty, as a fresh build has none.
- `status` (Windows): the project's `.cargo\config.toml` is printed with one kind of path separator.

### Tests and CI
- Toolchain pinned to 1.100.0 (`rust-toolchain.toml`); CI also installs cargo 1.99 to test the
  upgrade from the legacy layout.
- The end-to-end tests read unit outputs in either layout, and the simulator names new-layout files
  as cargo 1.100 does.
- CI runs every test binary (`--no-fail-fast`) and is green on Linux, macOS and Windows.

## 0.1.1 — 2026-11-13

Bug fixes found by the new scenario tests.

### Fixed
- `--remove-idle` never removed anything: it judged idleness by the prune log and `.fingerprint`
  mtimes, which every prune touches, so a nightly cron kept every target alive. It now looks at
  when cargo last built a unit.
- A git dependency whose checkout dir is named `<name>-<all digits>` was taken for a registry
  version not in `Cargo.lock`, so it and everything built on it were deleted as outdated (and
  rebuilt next time).
- An old toolchain's last use was read from the units left on disk: once its newest units went, it
  looked older than it was and could be retired early, and a second prune could delete more than
  the first. The last use is now remembered.
- Incremental dirs left by a crashed build could be adopted by live units instead of being removed.
- With `--max-size`, units of the same age were evicted in arbitrary order, so a dry run could name
  other units than the real run deleted.
- Windows: dep-info paths with spaces (e.g. a `CARGO_HOME` with a space) were misread; rustc
  escapes spaces as `\ ` on every platform.
- A dry run (and `status`) left an empty `trash` dir in `.autoprune/`.
- Units without a package name were not treated as workspace units.
- `status`: the "last prune … ago" text could be off at a unit boundary.

### Tests
- The unit tests are replaced by scenario tests through the public entry points: a proptest model
  over a simulated cargo, checking the pruner's contract after every prune, plus one
  Given/When/Then story per rule.

Known issue: on cargo 1.100, empty `build/<pkg>/` dirs are left after a prune (fixed in 0.1.2).

## 0.1.0 — 2026-11-10

First release: a `rustc-wrapper` that keeps Cargo `target/` dirs from growing.

- After a build, a detached pruner removes what Cargo can no longer use, taking Cargo's own locks:
  superseded copies, outdated dependency versions, removed dependencies and targets, orphans and
  stale incremental dirs, old toolchains and the legacy build-dir layout. The pruned target equals
  a from-scratch build and nothing recompiles.
- About 3 ms per crate in the wrapper; the prune runs in the background and holds cargo's lock in
  batches of at most 1 s.
- `cargo autoprune -r --remove-idle 30 ~/projects` for servers, `--ci`, and a GitHub composite
  action that trims a cached `target/`.
- `cargo autoprune status`: reclaimable space, the last prune and notices.
- Both build-dir layouts (cargo ≤ 1.99 and 1.100+); Linux, macOS and Windows.
