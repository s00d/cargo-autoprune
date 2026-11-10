#!/bin/zsh
# Churn test: a workspace whose deps are added/removed/re-versioned step by step.
# After each step, compare the auto-pruned target/ with an oracle target built from scratch with
# the same commands: "left" = garbage autoprune kept, "lost" = live files it removed (must be 0).
set -u
W=${CHURN_DIR:-${TMPDIR:-/tmp}/cargo-autoprune-churn}; L=$W/logs; mkdir -p $L; cd $W || exit 1
# needs the pinned crate versions below in ~/.cargo/registry (runs --offline)
rm -rf target target-oracle app util Cargo.toml Cargo.lock $L/*
mkdir -p app/src app/tests app/examples util/src
cat > Cargo.toml <<'T'
[workspace]
members = ["app", "util"]
resolver = "2"
T
cat > util/Cargo.toml <<'T'
[package]
name = "util"
version = "0.1.0"
edition = "2021"
[features]
extra = []
[dependencies]
T
echo 'pub fn u() -> u32 { if cfg!(feature = "extra") { 2 } else { 1 } }' > util/src/lib.rs
echo 'fn main() { println!("cargo:rerun-if-changed=build.rs"); }' > app/build.rs
echo 'pub fn a() -> u32 { util::u() }' > app/src/lib.rs
echo 'fn main() { println!("{}", app::a()); }' > app/src/main.rs
echo '#[test] fn it() { assert!(app::a() > 0); }' > app/tests/it.rs
echo 'fn main() { app::a(); }' > app/examples/ex.rs

deps() { # $1 = dependency lines for app
  { print '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\nutil = { path = "../util" }'; print -r -- "$1"; } > app/Cargo.toml
}
cmds() { # same command set for the pruned and the oracle target
  cargo build --offline --workspace --all-targets &&
  cargo test --offline --workspace --no-run &&
  cargo check --offline --workspace &&
  cargo build --offline -p util --features extra &&
  cargo clippy --offline --workspace &&
  cargo build --offline --release -p app
}
waitbg() { for i in {1..300}; do pgrep -qf 'cargo-autoprune --background' || return 0; sleep 0.2; done; echo "  background prune still running"; }
inventory() { # every entry that cargo/autoprune manage, one per line, relative to target
  (cd $1 && for p in debug release; do [ -d $p ] || continue
     for a in deps .fingerprint build incremental examples; do /bin/ls $p/$a 2>/dev/null | sed "s|^|$p/$a/|"; done
     /bin/ls -A $p | grep -vE '^(deps|\.fingerprint|build|incremental|examples|\.autoprune|\.cargo.*lock)$' | sed "s|^|$p/|"
   done; /bin/ls -A | grep -vE "^(debug|release)$" | sed "s|^|/|") | sed -E "s/^(.*-[0-9a-f]{16})\..*\.rcgu\.o$/\1.*.rcgu.o/" | sort -u
}
kib() { /usr/bin/du -sk $1 | cut -f1; }

export CARGO_AUTOPRUNE_INTERVAL=0 CARGO_AUTOPRUNE_GRACE=1
step=0
run() { # $1 = label, $2 = deps
  step=$((step+1)); deps "$2"
  s=$(date +%s.%N)
  RUSTC_WRAPPER=cargo-autoprune cmds > $L/step$step.log 2>&1 || { echo "step $step BUILD FAILED"; cat $L/step$step.log; exit 1; }
  e=$(date +%s.%N); waitbg
  # oracle: same commands, empty target, no wrapper
  rm -rf target-oracle; CARGO_TARGET_DIR=target-oracle cmds > $L/oracle$step.log 2>&1 || echo "oracle failed"
  inventory target > $L/inv-pruned$step.txt; inventory target-oracle > $L/inv-oracle$step.txt
  left=$(comm -23 $L/inv-pruned$step.txt $L/inv-oracle$step.txt | grep -v '^/target-oracle'); lost=$(comm -13 $L/inv-pruned$step.txt $L/inv-oracle$step.txt)
  # rebuild check: rerun the command set, count units cargo had to compile again
  re=$(RUSTC_WRAPPER=cargo-autoprune cmds 2>&1 | grep -c Compiling); waitbg
  rebuilt=$( (cd $W && RUSTC_WRAPPER=cargo-autoprune cargo build --offline --workspace --all-targets -v 2>&1) | grep -c '^ *Compiling')
  printf "%-28s target=%6sKiB oracle=%6sKiB left=%3d lost=%3d noop-rebuilt=%d rerun-compiled=%d build=%.1fs\n" "$1" $(kib target) $(kib target-oracle) \
    $(print -r -- "$left" | grep -c .) $(print -r -- "$lost" | grep -c .) $rebuilt $re $((e-s))
  [ -n "$left" ] && print -r -- "$left" | sed 's/^/    left: /' | head -40
  [ -n "$lost" ] && print -r -- "$lost" | sed 's/^/    LOST: /' | head -40
  tail -1 target/debug/.autoprune/log | sed 's/^/    log: /'
}
D1='serde = { version = "=1.0.228", features = ["derive"] }
libc = "=0.2.190"'
run "1 serde+libc"               "$D1"
run "2 libc 0.2.190->0.2.191"    'serde = { version = "=1.0.228", features = ["derive"] }
libc = "=0.2.191"'
run "3 add regex+itoa"           'serde = { version = "=1.0.228", features = ["derive"] }
libc = "=0.2.191"
regex = "=1.12.4"
itoa = "=1.0.15"'
run "4 remove regex"             'serde = { version = "=1.0.228", features = ["derive"] }
libc = "=0.2.191"
itoa = "=1.0.15"'
run "5 serde +rc feature"        'serde = { version = "=1.0.228", features = ["derive", "rc"] }
libc = "=0.2.191"
itoa = "=1.0.15"'
run "6 add tokio 1.53.3"         'serde = { version = "=1.0.228", features = ["derive", "rc"] }
libc = "=0.2.191"
itoa = "=1.0.15"
tokio = { version = "=1.53.3", features = ["rt"] }'
run "7 tokio->1.54.1, -serde"    'libc = "=0.2.191"
itoa = "=1.0.15"
tokio = { version = "=1.54.1", features = ["rt"] }'
echo 'pub fn a() -> u32 { util::u() + 0 }' > app/src/lib.rs
run "8 edit app source"          'libc = "=0.2.191"
itoa = "=1.0.15"
tokio = { version = "=1.54.1", features = ["rt"] }'
# a revert done later than the 30 min pin window: age the deletion log by an hour
for f in target/*/.autoprune/deleted; do awk -F"\t" -v OFS="\t" "{print \$1, \$2-3600}" $f > $f.x && mv $f.x $f; done
run "9 revert to step 1 (+1h)"   "$D1"
run "10 quick re-revert (<30m)"  'libc = "=0.2.191"
itoa = "=1.0.15"
tokio = { version = "=1.54.1", features = ["rt"] }'

echo "== final"; /bin/ls -A target target/debug; cat target/debug/.autoprune/pinned 2>/dev/null | sed 's/^/pinned: /'
