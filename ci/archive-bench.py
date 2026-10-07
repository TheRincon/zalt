#!/usr/bin/env python3
"""ZSTA archives at size: zalt against `gtar | zstd`, interleaved.

    just archive-bench [CORPUS] [ROUNDS]

Times each operation zalt's archive commands offer next to the stock
pipeline that does the same job — create, verify, list, extract everything,
extract one file — alternating which goes first each round, and reports the
best of each and the ratio (reference / zalt; above 1 is zalt faster). Raw
times are printed for context only: this machine swings far more between
runs than within one, so only the ratios mean anything (doc/ZALT-MAC.md,
"measure against zstd every time").

`append` and `recover` have no stock counterpart; they are timed alone and
set against zalt's own `verify` (one full decode) from the same round.

Before measuring it checks swap and refuses above 75%, and suspends the
language servers (rust-analyzer, cpptools) with `kill -STOP`, restoring them
however the script ends. `--no-suspend` skips that, for a smoke run.
"""

import argparse
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ZALT = ROOT / "target" / "release" / "zalt"
# Precise on purpose: a bare "rust-analyzer" also matches a devcontainer's
# `docker exec` whose command line merely mentions it.
SUSPEND = (
    r"rustup/toolchains/[^ ]*/(bin/rust-analyzer|libexec/rust-analyzer-proc-macro-srv)"
    r"|cpptools"
)


def sh(cmd, **kw):
    return subprocess.run(cmd, shell=True, check=True, **kw)


def swap_fraction():
    out = subprocess.run(
        ["sysctl", "-n", "vm.swapusage"], capture_output=True, text=True
    ).stdout
    total = float(re.search(r"total = ([\d.]+)M", out).group(1))
    used = float(re.search(r"used = ([\d.]+)M", out).group(1))
    return used / total if total else 0.0, out.strip()


def timed(cmd, cwd):
    start = time.perf_counter()
    subprocess.run(cmd, shell=True, check=True, cwd=cwd,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return time.perf_counter() - start


def fresh(path):
    shutil.rmtree(path, ignore_errors=True)
    path.mkdir(parents=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("corpus", nargs="?", help="directory to archive")
    ap.add_argument("rounds", nargs="?", type=int, default=3)
    ap.add_argument("--level", type=int, default=3)
    ap.add_argument("--no-suspend", action="store_true",
                    help="leave the language servers running (smoke runs only)")
    ap.add_argument("--force", action="store_true", help="measure even with swap above 75%%")
    args = ap.parse_args()

    if args.corpus:
        corpus = Path(args.corpus).expanduser().resolve()
    else:
        registry = Path.home() / ".cargo" / "registry" / "src"
        found = sorted(registry.glob("*/"))
        if not found:
            sys.exit("no corpus given and no ~/.cargo/registry/src to default to")
        corpus = found[0]
    if not corpus.is_dir():
        sys.exit(f"{corpus}: not a directory")

    tar = shutil.which("gtar") or shutil.which("tar")
    zstd = shutil.which("zstd")
    if not zstd:
        sys.exit("zstd(1) is not on PATH")

    frac, swap = swap_fraction()
    print(f"swap: {swap}")
    print(f"load: {os.getloadavg()[0]:.1f} {os.getloadavg()[1]:.1f} {os.getloadavg()[2]:.1f}")
    if frac > 0.75 and not args.force:
        sys.exit(f"swap is {frac:.0%} used: numbers taken now are not comparable. "
                 "Free memory (quit browsers, restart) and run again, or pass --force.")

    print("building zalt (release)…")
    sh(f"cargo build --release -q -p zalt-cli", cwd=ROOT)

    size = int(subprocess.run(["du", "-sk", str(corpus)], capture_output=True, text=True)
               .stdout.split()[0]) * 1024
    free = shutil.disk_usage(tempfile.gettempdir()).free
    if free < 4 * size:
        sys.exit(f"{free >> 20} MiB free in {tempfile.gettempdir()}; need ~{4 * size >> 20} MiB")
    print(f"corpus: {corpus} ({size / 2**20:.0f} MiB), {args.rounds} rounds, level {args.level}")
    print(f"reference: {tar} | {zstd} -T0")

    suspended = []
    if not args.no_suspend:
        pids = subprocess.run(["pgrep", "-f", SUSPEND], capture_output=True, text=True).stdout.split()
        suspended = [p for p in pids if p != str(os.getpid())]

    def restore(*_):
        for p in suspended:
            subprocess.run(["kill", "-CONT", p], stderr=subprocess.DEVNULL)
        if _:
            sys.exit(1)

    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, restore)
    for p in suspended:
        subprocess.run(["kill", "-STOP", p], stderr=subprocess.DEVNULL)
    if suspended:
        print(f"suspended {len(suspended)} language-server processes (restored on exit)")

    work = Path(tempfile.mkdtemp(prefix="zalt-archive-bench-"))
    try:
        run(args, corpus, tar, zstd, work)
    finally:
        restore()
        shutil.rmtree(work, ignore_errors=True)


def run(args, corpus, tar, zstd, work):
    parent, name = corpus.parent, corpus.name
    ours, theirs = work / "ours.tar.zst", work / "theirs.tar.zst"
    x1, x2 = work / "x-ours", work / "x-theirs"
    lvl = args.level

    # One file late in the tree, for single-file extraction: the case the
    # index exists for, and the one a stream must decode everything to reach.
    files = sorted(p for p in corpus.rglob("*") if p.is_file() and not p.is_symlink())
    one = str(Path(name) / files[int(len(files) * 0.9)].relative_to(corpus))

    pairs = {
        "create": (
            f"{ZALT} archive create -q -f -{lvl} -o {ours} {name}",
            f"{tar} -cf - {name} | {zstd} -q -f -T0 -{lvl} -o {theirs}",
        ),
        "verify": (f"{ZALT} archive verify -q {ours}", f"{zstd} -q -t {theirs}"),
        "list": (f"{ZALT} archive list {ours}", f"{zstd} -q -dc {theirs} | {tar} -tf -"),
        "extract all": (
            f"{ZALT} archive extract -q {ours} -C {x1}",
            f"{zstd} -q -dc {theirs} | {tar} -xf - -C {x2}",
        ),
        "extract one": (
            f"{ZALT} archive extract -q {ours} -C {x1} '{one}'",
            f"{zstd} -q -dc {theirs} | {tar} -xf - -C {x2} '{one}'",
        ),
    }
    best = {op: [float("inf"), float("inf")] for op in pairs}
    alone = {"append": float("inf"), "recover": float("inf"), "verify (same round)": float("inf")}
    extra = work / "extra"
    extra.mkdir()
    (extra / "added.txt").write_text("appended\n")

    for r in range(args.rounds):
        for op, (a, b) in pairs.items():
            if op.startswith("extract"):
                fresh(x1)
                fresh(x2)
            order = [(0, a), (1, b)] if r % 2 == 0 else [(1, b), (0, a)]
            for k, cmd in order:
                best[op][k] = min(best[op][k], timed(cmd, parent))

        # append: on a copy, so every round appends to the same archive.
        grown = work / "grown.tar.zst"
        shutil.copyfile(ours, grown)
        alone["append"] = min(alone["append"],
                              timed(f"{ZALT} archive append -q {grown} extra", work))
        # recover: the trailer cut off, as a crash leaves it.
        cut = work / "cut.tar.zst"
        data = ours.read_bytes()
        cut.write_bytes(data[: len(data) - 4096])
        fixed = work / "fixed.tar.zst"
        fixed.unlink(missing_ok=True)
        alone["recover"] = min(alone["recover"],
                               timed(f"{ZALT} archive recover -q {cut} -o {fixed}", work))
        alone["verify (same round)"] = min(alone["verify (same round)"],
                                           timed(f"{ZALT} archive verify -q {ours}", work))
        print(f"round {r + 1}/{args.rounds} done")

    print()
    print(f"{'operation':<14} {'zalt s':>9} {'tar|zstd s':>11} {'ratio':>8}   (ratio > 1: zalt faster)")
    for op, (a, b) in best.items():
        print(f"{op:<14} {a:>9.3f} {b:>11.3f} {b / a:>7.2f}x")
    v = alone["verify (same round)"]
    for op in ("append", "recover"):
        print(f"{op:<14} {alone[op]:>9.3f} {'—':>11} {alone[op] / v:>7.2f}x of zalt's verify")
    print()
    print(f"size: zalt {ours.stat().st_size:,} B, tar|zstd {theirs.stat().st_size:,} B "
          f"({ours.stat().st_size / theirs.stat().st_size:.3f}x)")
    print(f"extract one: {one}")


if __name__ == "__main__":
    main()
