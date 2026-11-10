#!/usr/bin/env python3
"""Generate a large fake Cargo target for benchmarking the pruner.

usage: bench-tree.py DIR [LOCAL_PKGS] [REGISTRY_PKGS]

DIR/Cargo.lock + DIR/target/debug with: every workspace package as lib/check/test units in a
current and a superseded copy (test exes with 50 objects each), registry crates as rlib + rmeta
units, some with build scripts, fingerprint json edges, and incremental sessions whose work
products are hard links to the deps objects, like rustc makes them.
"""
import os, random, shutil, struct, sys, time

root = sys.argv[1]
n_local = int(sys.argv[2]) if len(sys.argv) > 2 else 150
n_reg = int(sys.argv[3]) if len(sys.argv) > 3 else 600
random.seed(1)
shutil.rmtree(root, ignore_errors=True)
prof = f"{root}/target/debug"
for d in ["deps", ".fingerprint", "build", "incremental"]:
    os.makedirs(f"{prof}/{d}")
open(f"{root}/target/CACHEDIR.TAG", "w").write("Signature: 8a477f597d28d172789f06886806bc55")
open(f"{prof}/.cargo-lock", "w").close()
now = time.time()
OLD, NEW = now - 7200, now - 60


def h16():
    return "%016x" % random.getrandbits(64)


def touch(p, t):
    os.utime(p, (t, t))


def unit(pkg, krate, kind, outs, deps, t, objs=0, build=None, inc=False):
    h, val = h16(), random.getrandbits(64)
    fp = f"{prof}/.fingerprint/{pkg}-{h}"
    os.makedirs(fp)
    edges = ",".join(f'[1,"d",false,{v}]' for v in deps)
    json = ('{"rustc":7,"features":"[]","declared_features":"[]","target":%d,"profile":%d,"path":3,'
            '"deps":[%s],"local":[{"CheckDepInfo":{"dep_info":"x","checksum":false}}],'
            '"rustflags":[],"config":1,"compile_kind":0}') % (hash(kind) & 0xffff, len(outs), edges)
    for name, body in [(f"{kind}-{krate}.json", json), (kind + "-" + krate, struct.pack("<Q", val).hex()),
                       (f"dep-{kind}-{krate}", "x"), ("invoked.timestamp", "")]:
        open(f"{fp}/{name}", "w").write(body)
        touch(f"{fp}/{name}", t)
    touch(fp, t)
    files = [f"{krate}-{h}.d"] + [f"lib{krate}-{h}.{o}" for o in outs if o != "bin"]
    if "bin" in outs:
        files.append(f"{krate}-{h}")
    for f in files:
        p = f"{prof}/deps/{f}"
        open(p, "w").write("x" * 64)
        if f == f"{krate}-{h}":
            os.chmod(p, 0o755)
        touch(p, t)
    open(f"{prof}/deps/{krate}-{h}.d", "w").write(f"{prof}/deps/{krate}-{h}: {pkg}/src/lib.rs\n")
    touch(f"{prof}/deps/{krate}-{h}.d", t)
    objects = []
    for k in range(objs):
        p = f"{prof}/deps/{krate}-{h}.{h16()}.cgu{k}.rcgu.o"
        open(p, "w").close()
        objects.append(p)
    if inc:
        d = f"{prof}/incremental/{krate}-{h16()[:13]}/s-x"
        os.makedirs(d)
        for p in objects:
            os.link(p, f"{d}/{os.path.basename(p).split('.')[1]}.o")
        if "rmeta" in outs:
            os.link(f"{prof}/deps/lib{krate}-{h}.rmeta", f"{d}/metadata.rmeta")
        touch(os.path.dirname(d), t)
    if build:
        b = f"{prof}/build/{pkg}-{h}"
        os.makedirs(b)
        open(f"{b}/{build}", "w").close()
        touch(b, t)
    return val


lock = ["version = 4"]
reg_vals = []
for i in range(n_reg):
    pkg = f"reg{i}"
    lock += ["", "[[package]]", f'name = "{pkg}"', 'version = "1.0.0"',
             'source = "registry+https://github.com/rust-lang/crates.io-index"']
    deps = random.sample(reg_vals, min(len(reg_vals), 3))
    if i % 6 == 0:
        cv = unit(pkg, "build_script_build", "build-script-build-script-build", ["bin"], [], OLD,
                  build="build-script-build")
        deps.append(unit(pkg, "build_script_build", "run-build-script-build-script-build", [], [cv], OLD,
                         build="output"))
    v = unit(pkg, pkg, "lib", ["rlib", "rmeta"], deps, OLD, objs=1)
    unit(pkg, pkg, "lib", ["rmeta"], deps, OLD)  # check variant (unreachable here: garbage)
    reg_vals.append(v)
    if i % 5 == 0:  # an old version nobody uses any more
        unit(pkg, pkg, "lib", ["rlib", "rmeta"], [], OLD - 3600, objs=1)
for i in range(n_local):
    pkg = f"ws{i}"
    lock += ["", "[[package]]", f'name = "{pkg}"', 'version = "0.1.0"']
    for t in (OLD, NEW):  # superseded and current copy
        deps = random.sample(reg_vals, 8)
        unit(pkg, pkg, "lib", ["rlib", "rmeta"], deps, t, objs=8, inc=True)
        unit(pkg, pkg, "lib", ["rmeta"], deps, t, inc=True)
        unit(pkg, pkg, "test-lib", ["bin"], deps, t, objs=50, inc=True)
open(f"{root}/Cargo.lock", "w").write("\n".join(lock) + "\n")
os.makedirs(f"{root}/src", exist_ok=True)
for i in range(n_local):
    os.makedirs(f"{root}/ws{i}/src", exist_ok=True)
    open(f"{root}/ws{i}/src/lib.rs", "w").close()
count = sum(len(fs) for _, _, fs in os.walk(prof))
print(f"{root}: {count} files")
