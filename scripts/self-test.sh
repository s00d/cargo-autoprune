#!/bin/zsh
# Self-cleaning test: cargo-autoprune prunes its own target/ while version bumps rehash every unit.
set -u
# runs on a throwaway copy: the package version is changed there (it rehashes every unit, the way
# a dependency change does) and the repository's own Cargo.toml is never touched
C=${TMPDIR:-/tmp}/cargo-autoprune-selftest; rm -rf $C; mkdir -p $C
cp -R ${0:A:h}/../{Cargo.toml,Cargo.lock,src,tests} $C/ && cd $C
sz() { /usr/bin/du -sk target | cut -f1; }
copies() { /bin/ls target/debug/deps | grep -cE '^cargo_autoprune-[0-9a-f]{16}$'; }
setver() { sed -i '' -E "s/^version = \"[^\"]+\"/version = \"$1\"/" Cargo.toml; }
waitprune() { # wait until the background prune released its lock and wrote the log line
  local want=$1; for i in {1..100}; do
    n=$(wc -l < target/debug/.autoprune/log 2>/dev/null || echo 0); [ "$n" -ge "$want" ] && return; sleep 0.2; done; echo "prune did not run"; }
cargo install --offline -q --path ${0:A:h}/.. || exit 1
echo "== control: no wrapper, 4 version bumps"
cargo clean -q; unset RUSTC_WRAPPER
for v in 9.0.1 9.0.2 9.0.3 9.0.4; do setver $v; s=$(date +%s.%N); cargo test --offline -q >/dev/null 2>&1 || echo FAIL; e=$(date +%s.%N)
  echo "v$v target=$(sz)KiB test-exe-copies=$(copies) build=$(printf %.2f $((e-s)))s"; done
echo "== wrapper: same 4 bumps (CARGO_AUTOPRUNE_INTERVAL=0 GRACE=1)"
cargo clean -q; export RUSTC_WRAPPER=cargo-autoprune CARGO_AUTOPRUNE_INTERVAL=0 CARGO_AUTOPRUNE_GRACE=1
k=0
for v in 9.0.1 9.0.2 9.0.3 9.0.4; do sleep 1.1; setver $v; s=$(date +%s.%N); cargo test --offline -q >/dev/null 2>&1 || echo FAIL; e=$(date +%s.%N)
  k=$((k+1)); waitprune $k
  echo "v$v target=$(sz)KiB test-exe-copies=$(copies) build=$(printf %.2f $((e-s)))s | $(tail -1 target/debug/.autoprune/log)"; done
echo "== no-change rebuild with wrapper (cache must survive)"
cargo test --offline 2>&1 | grep -E "Compiling|Finished|test result"
cd /; rm -rf $C
echo "== wrapper overhead, 1000 calls each"
D=$PWD/target/debug/deps
s=$(date +%s.%N); for i in {1..1000}; do /usr/bin/true --out-dir $D; done; e=$(date +%s.%N); echo "direct:  $(( (e-s)*1000 ))ms"
s=$(date +%s.%N); for i in {1..1000}; do CARGO_AUTOPRUNE_INTERVAL=99999 cargo-autoprune /usr/bin/true --out-dir $D; done; e=$(date +%s.%N); echo "wrapped: $(( (e-s)*1000 ))ms"
