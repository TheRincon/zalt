# zalt — task runner
#
#   just            list recipes
#   just t          fast inner-loop tests (nextest, parallel)
#   just gate       everything CI runs, in CI order
#
# See doc/ZALT-MAC.md for what any of this is for.

set shell := ["bash", "-uc"]

# Reference zstd for baselines and differential testing.
# Found on PATH so this works on the Linux CI runners too; `ZSTD=/path/to/zstd`
# overrides. It was an absolute Homebrew path, which does not exist on Linux and
# made every fixture-generating recipe macOS-only.
zstd := env_var_or_default("ZSTD", "zstd")
# no_std target used to prove zalt-codec stays portable and allocation-light.
nostd_target := "thumbv7em-none-eabi"
# Host triple, passed explicitly to `cargo fuzz`.
#
# cargo-fuzz picks its own default target, and on the Apple Silicon CI runner it
# chose `x86_64-apple-darwin` — for which no standard library is installed, so
# every target failed with "can't find crate for `std`". It builds fine locally
# without this, which is exactly why it went unnoticed: the default is only
# wrong on some hosts. Asking rustc what it is removes the guess.
fuzz_target_triple := `rustc +nightly -vV 2>/dev/null | awk '/^host:/{print $2}'`
fixtures := "crates/zalt-codec/tests/fixtures"

# List available recipes.
default:
    @just --list --unsorted

# ---------------------------------------------------------------- building

# Type-check everything, fast. The inner loop when editing signatures.
check:
    cargo check --workspace --all-targets

# Debug build.
build:
    cargo build --workspace

# Release build (LTO, one codegen unit — slow, and what benchmarks need).
build-release:
    cargo build --workspace --release

# ----------------------------------------------------------------- testing

# Fast inner-loop tests: nextest, one process per test, all cores.
#
# Does NOT run doctests — nextest cannot. `just test` covers those.
[doc("Fast inner-loop tests: nextest, parallel, one process per test.")]
fast-test: _fixtures
    cargo nextest run --workspace

alias t := fast-test

# Everything `fast-test` runs, plus doctests.
test: fast-test
    cargo test --workspace --doc

# Release-mode tests.
#
# Not redundant with `test`: `debug_assert!` is compiled out here, so this is
# the configuration where the NEON kernels' safety contracts are unchecked and
# the raw pointers are on their own. A backend bug can pass in debug and fail
# here.
[doc("Release-mode tests — where debug_assert is compiled out.")]
test-release: _fixtures
    cargo nextest run --workspace --release

# Run one test by substring, with output shown.
#
#   just test-one streaming_equivalence
test-one PATTERN:
    cargo nextest run --workspace --no-capture -E 'test(/{{PATTERN}}/)'

# Backend equivalence only — scalar vs NEON, debug and release.
equivalence:
    cargo nextest run -p zalt-neon
    cargo nextest run -p zalt-neon --release

# ------------------------------------------------------------------ checks

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings
    @# The workspace build turns `native-fs` on (zalt-cli asks for it); the
    @# format layer must also stand without it — the fuzz targets use it so.
    cargo clippy -p zalt-archive --all-targets -- -D warnings

clippy-fix:
    cargo clippy --fix --workspace --all-targets --allow-dirty -- -D warnings

# ZALT-archives at size, against `gtar | zstd`, interleaved; ratios only mean
# anything on a quiet machine (it refuses above 75% swap). CORPUS defaults to
# ~/.cargo/registry/src/<index>.
#   just archive-bench ~/some/dir 5
archive-bench CORPUS="" ROUNDS="3":
    python3 ci/archive-bench.py {{CORPUS}} {{ROUNDS}}

# ZLTA members past 4 GiB and 8 GiB, round-tripped in memory (~14 GiB of
# zeros encoded and decoded; ignored by `just test`).
archive-huge:
    cargo test --release -p zalt-archive --test roundtrip members_past_4_and_8_gib -- --ignored --nocapture

# Enforce the layering contract from doc/ZALT-MAC.md §1.
layering:
    @bash ci/layering.sh

# What may enter the dependency tree (deny.toml): permissive licenses only,
# no known advisories, crates.io only. Both workspaces, since fuzz/ has its own
# lockfile. Not in `gate`: the advisory database is fetched over the network.
[doc("Check dependency licenses, advisories and sources (cargo-deny).")]
deny:
    cargo deny check
    cargo deny --manifest-path fuzz/Cargo.toml check

# Check the hand-written C header still matches the Rust ABI.
abi-check:
    @bash ci/check-abi.sh

# Compile and run a real C program against the staticlib.
#
# The Rust ABI tests prove the logic; this proves a C compiler can consume the
# header and that the symbols actually link — which is the part that breaks at
# integration time if nobody checks it here.
[doc("Build and run the C smoke test against the staticlib.")]
ffi-smoke:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p zalt-ffi
    cc -std=c11 -Wall -Wextra -Werror \
       -Icrates/zalt-ffi/include \
       crates/zalt-ffi/examples/smoke.c \
       target/release/libzalt.a \
       -framework CoreFoundation -framework Security \
       -o target/zalt-smoke
    ./target/zalt-smoke

# ---------------------------------------------------------------- the CLI

# Build and run zalt(1).
#
#   just zalt info file.zst
#   just zalt d --in-buf 1 file.zst
zalt *ARGS:
    @cargo run --release -q -p zalt-cli -- {{ARGS}}

# Decode every reference frame through the CLI, both directions.
#
# The golden frames must all decode and the golden *error* frames must all be
# refused. Round-tripping only ever proves a decoder is permissive enough;
# the second half is what catches one that is too permissive.
[doc("Run the CLI over the reference corpus, both directions.")]
cli-corpus:
    #!/usr/bin/env bash
    set -uo pipefail
    root=zstd-fixtures
    [ -d "$root" ] || { echo "$root not present -- nothing to check"; exit 0; }
    cargo build --release -q -p zalt-cli
    bin=target/release/zalt

    echo "=== golden-decompression: all must decode ==="
    "$bin" check -q --trusted "$root"/golden-decompression/*.zst
    good=$?

    echo
    echo "=== golden-decompression-errors: all must be refused ==="
    if "$bin" check -q "$root"/golden-decompression-errors/*.zst >/dev/null 2>&1; then
        echo "FAIL: an error frame was accepted"
        "$bin" check "$root"/golden-decompression-errors/*.zst
        bad=1
    else
        echo "all refused."
        bad=0
    fi
    [ "$good" -eq 0 ] && [ "$bad" -eq 0 ]

# Prove zalt-codec is still a general no_std crate rather than an Apple-only one.
#
# zalt-recipe is built twice, because it has two shapes and only one of them is
# the interesting one. With `parse` it is a manifest reader; without it, it is
# the bare `Recipe` struct a host bakes in as a Rust value with no parser in the
# binary at all. That second build is what keeps the split honest — a `String`
# or a `format!` that drifts into the data types would still compile with
# `parse` on and would silently make the embedded path impossible.
no-std:
    rustup target add {{nostd_target}} 2>/dev/null || true
    cargo build -p zalt-codec --target {{nostd_target}}
    cargo build -p zalt-recipe --target {{nostd_target}} --no-default-features
    cargo build -p zalt-recipe --target {{nostd_target}} --no-default-features --features parse
    cargo build -p zalt-ztml --target {{nostd_target}}

# Miri over the scalar path.
#
# Miri cannot execute NEON intrinsics — which is exactly why the scalar backend
# is never deleted. It is the only way to get UB detection over this code.
#
# Requires nightly explicitly: rust-toolchain.toml pins stable, so a bare
# `cargo miri` resolves to a toolchain that has no miri component.
# Isolation is disabled because the conformance tests read the reference corpus
# from disk. Isolation governs determinism (filesystem, clock, randomness), not
# undefined-behaviour detection, so nothing Miri is here for is weakened.
[doc("Miri UB detection over the scalar path (nightly).")]
miri: _fixtures
    rustup +nightly component add miri 2>/dev/null || true
    MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri nextest run -p zalt-codec

# Everything CI runs, in CI order. Run before pushing.
gate: fmt-check clippy layering ztml-dict-check abi-check test test-release
    @echo
    @echo "gate passed."

# ------------------------------------------------------------------- hooks

# What the pre-push hook runs.
#
# `gate` is not by itself what CI checks. The workflow has five jobs, and gate
# is one of them; `no-std` is a separate job and gate does not cover it. That
# gap is the one worth closing here, because it is the cheapest to break without
# noticing — a `std` import in `zalt-codec` compiles fine on this laptop and
# fails only on the cross-compile that proves the crate is still general.
#
# Left out: `fuzz-build` (needs nightly and cargo-fuzz) and `deny` (needs the
# network), which still run in CI, and `miri`, which no longer does — it had grown past six hours a run — and is
# local only. Both are excluded on the reasoning that **a hook people bypass is
# worse than a shorter hook they run**: past a few minutes, `--no-verify`
# becomes the habit and then nothing is checked at all. `just pre-push-full`
# runs them when the change warrants it.
[doc("What the pre-push hook runs: gate + no-std.")]
pre-push: gate no-std
    @echo
    @echo "pre-push passed (gate + no-std)."
    @echo "for the slow CI jobs too:  just pre-push-full"

# Everything CI runs, plus Miri, which only runs here. Worth it before a PR, and
# before anything touching `unsafe` — though note Miri cannot execute NEON intrinsics,
# so it validates the scalar oracle rather than the accelerated path. What
# guards the kernels is `test-release` (already in gate, where `debug_assert` is
# compiled out) and the `backend_equivalence` tests and fuzz target.
[doc("Everything CI runs, plus Miri (adds fuzz-build + deny + miri).")]
pre-push-full: gate no-std fuzz-build deny miri
    @echo
    @echo "pre-push-full passed (gate + no-std + fuzz-build + deny + miri)."

# Activate the repo's git hooks.
#
# Hooks live in `ci/hooks/` so they are version-controlled and reviewed like
# everything else in `ci/`, rather than in `.git/hooks/` where they are
# per-clone, invisible and unshareable. `core.hooksPath` is itself per-clone, so
# this has to be run once after cloning.
[doc("Activate the repo git hooks (sets core.hooksPath).")]
hooks-install:
    @git config core.hooksPath ci/hooks
    @chmod +x ci/hooks/* 2>/dev/null || true
    @echo "core.hooksPath = $(git config core.hooksPath)"
    @echo "hooks active:   $(ls ci/hooks | tr '\n' ' ')"
    @echo
    @echo "bypass once with:  git push --no-verify"

[doc("Deactivate the repo git hooks.")]
hooks-uninstall:
    @git config --unset core.hooksPath 2>/dev/null || true
    @echo "hooks deactivated."

# ------------------------------------------------------------- benchmarking

# Time the backend kernels, scalar vs NEON.
bench: build-release
    cargo run --release -p zalt-bench -- kernels

# Build the decode-benchmark corpus into `corpus/` so it survives a reboot.
#
# `corpus/` is gitignored and local by design (§5: the harness has no built-in
# opinion about content). This recipe fills it with *varied* text — many
# distinct Rust sources rather than one file repeated — because a repeated file
# compresses to almost nothing at long range and then measures match copy
# rather than entropy decoding. That mistake cost a whole measurement round.
[doc("Build the decode-benchmark corpus into corpus/ (survives a reboot).")]
bench-corpus:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p corpus
    if [ -s corpus/varied.txt ]; then
        echo "corpus/varied.txt already present ($(du -h corpus/varied.txt | cut -f1))"
    else
        just _collect-varied
    fi
    ls -la corpus/varied.txt
    # The documents this decoder actually targets, kept separate because they
    # measure a different thing: 3-12 KB each, where per-frame setup dominates
    # and steady-state throughput does not. `just measure corpus/ztml` is the
    # number that matters for the browser; `just measure corpus` is the one
    # that matters for a general-purpose zstd crate.
    mkdir -p corpus/ztml
    cp doc/*.zdoc corpus/ztml/ 2>/dev/null || true
    echo "ztml corpus: $(ls corpus/ztml 2>/dev/null | wc -l | tr -d ' ') documents"

# The measurement protocol. Run this, not `decode`, when a number matters.
#
#   just measure                  # 5 rounds over corpus/
#   just measure corpus 9         # more rounds
#
# Interference on a shared machine is **one-sided** — another process, a core
# migration, a thermal step, page reclaim under memory pressure — so every one
# of them can only make a sample slower. The best round is therefore the
# closest estimate of what the code costs, and any absolute number taken on a
# busy machine is a *lower bound*.
#
# For a number worth recording:
#   1. Close other build tools and editors; a background `cargo` is the usual
#      culprit and it moved figures here by up to 58%.
#   2. Restart if swap is in use. Darwin's memory compressor competes for both
#      CPU and bandwidth, and a decode that allocates its output then measures
#      page reclaim as much as entropy coding.
#   3. Check the header says the machine is quiet before believing a delta.
#
# To compare two *versions*, this is still not enough: build both, keep the
# binaries, and alternate them. Two runs at different times on this machine are
# not comparable, which is how a 4% "regression" was once recorded.
[doc("Measured decode throughput, with machine state stamped on it.")]
measure DIR="corpus" ROUNDS="5":
    #!/usr/bin/env bash
    set -uo pipefail
    [ -d "{{DIR}}" ] && [ -n "$(ls -A {{DIR}} 2>/dev/null | grep -v README)" ] || {
        echo "{{DIR}} is empty — run 'just bench-corpus' first"; exit 1; }
    cargo build --release -q -p zalt-bench
    before=$(just _canary-run)
    out=$(./target/release/zalt-bench decode "{{DIR}}" 3 2>/dev/null)
    printf '%s\n' "$out" | sed -n '1,/^-----/p'
    [ -n "$before" ] && echo "canary before: $before MB/s"
    echo "running {{ROUNDS}} rounds; the best of them is the estimate..."
    rounds=$(for i in $(seq 1 {{ROUNDS}}); do
        ./target/release/zalt-bench decode "{{DIR}}" 3 2>/dev/null | grep '^SUMMARY'
    done | sed 's/[a-z]*=//g')
    printf '%s\n' "$rounds" | awk '{if($2>s)s=$2; if($3>n)n=$3; if($4>k)k=$4}
        END{printf "\nBEST OF {{ROUNDS}}:  scalar %.1f   neon %.1f   sink %.1f  MB/s\n",s,n,k}'
    sink=$(printf '%s\n' "$rounds" | awk '{if($4>k)k=$4} END{printf "%.1f",k}')
    after=$(just _canary-run)
    if [ -n "$before" ] && [ -n "$after" ]; then
        echo
        awk -v b="$before" -v a="$after" 'BEGIN{
            d = (a>b ? a-b : b-a) / (b>a ? b : a) * 100
            printf "canary: %.0f -> %.0f MB/s (%.1f%% drift)\n", b, a, d
            if (d > 10) {
                print ""
                print "  WARNING: the machine changed *during* the measurement. The reference"
                print "  decoder does not get faster when zalt changes, so this drift is"
                print "  thermal or contention, and the rounds above straddle two different"
                print "  machines. Re-run; do not compare these numbers with any others."
            }
        }'
    fi
    just _gap "{{DIR}}" "$sink"

# The number to actually quote: the gap against zstd on the *same* corpus,
# measured in the same window.
#
#   just gap corpus/ztml
#
# An absolute MB/s figure from this machine means very little. The same binary
# has measured 457 and 750 on the same input, and the reference has read 1905
# and 3058, from thermal state alone. A *ratio* cancels all of it, because
# whatever slows zalt slows zstd in the same breath: measured this way the gap
# repeats to about 1% while the figures behind it spread by ten.
#
# So never quote a raw number as evidence of an improvement. Take three
# readings per round — before, after, and zstd on the same files — and compare
# the two ratios.
[doc("Gap against zstd on the same corpus, measured in the same window.")]
gap DIR="corpus":
    #!/usr/bin/env bash
    set -uo pipefail
    cargo build --release -q -p zalt-bench
    # Best of three, matching the estimator `measure` uses. Both sides of a
    # ratio must be estimated the same way or the ratio is not one.
    sink=$(for i in 1 2 3; do
        ./target/release/zalt-bench decode "{{DIR}}" 3 2>/dev/null | grep '^SUMMARY' | sed 's/.*sink=//'
    done | sort -g | tail -1)
    just _gap "{{DIR}}" "$sink"

# Print the zstd reference for a corpus and the resulting gap.
[private]
_gap DIR SINK:
    #!/usr/bin/env bash
    set -uo pipefail
    [ -n "{{SINK}}" ] || exit 0
    files=$(find "{{DIR}}" -type f ! -name 'README*' 2>/dev/null)
    [ -n "$files" ] || exit 0
    # `-b3` decompression is the *second* MB/s figure on a line, and its frames
    # carry no checksum — which is why the corpus is built with `--no-check`.
    #
    # Match only lines carrying both figures. zstd rewrites its progress line in
    # place, so `tr '\r' '\n'` turns each repaint into a line and some of them
    # are *partial* — compression measured, decompression not yet:
    #
    #   /- 7 files : 263798 -> 35999 (x7.328),  763.3 MB/s
    #   =- 7 files : 263798 -> 35999 (x7.328),  763.3 MB/s, 1768.7 MB/s
    #
    # Taking the last `MB/s` in the stream therefore picks a *compression* rate
    # whenever the output happens to end on a partial repaint, which reported a
    # gap of 1.05x where the truth was 2.7x. A measurement tool that is quietly
    # wrong is worse than none, so this insists on a complete line.
    z=$(printf '%s\n' $files | xargs {{zstd}} -b3 -i3 2>&1 | tr '\r' '\n' \
        | grep -E '[0-9.]+ MB/s, *[0-9.]+ MB/s' | tail -1 \
        | sed -E 's|.*, *([0-9.]+) MB/s.*|\1|')
    [ -n "${z:-}" ] || exit 0
    echo
    awk -v a="{{SINK}}" -v z="$z" 'BEGIN{
        printf "zalt %.1f   zstd %.1f   GAP %.2fx   (same corpus, same window)\n", a, z, z/a
    }'

# The machine-state control: reference throughput on a fixed frame.
#
# `just measure` already prints load and swap, and both were clean through a
# session in which the *same binary* measured 457 and then 750 MB/s. Load
# average cannot see thermal and frequency state on a fanless laptop, so it
# reported a quiet machine twice — quiet and slow, then quiet and fast.
#
# The reference decoder is the control, because its speed does not change when
# zalt changes. If this number moved, the machine moved, and nothing measured
# either side of it is comparable. It caught a 55% swing that would otherwise
# have been read as an optimisation.
#
# Needs the zstd headers (`brew install zstd`); degrades to silence without them
# rather than failing a measurement over a missing control.
[doc("Reference throughput on a fixed frame — a control for machine state.")]
canary:
    #!/usr/bin/env bash
    set -uo pipefail
    v=$(just _canary-run)
    if [ -z "$v" ]; then
        echo "canary unavailable (needs zstd headers: brew install zstd)"
        exit 0
    fi
    echo "canary: $v MB/s"

# Print the canary throughput, or nothing if it cannot be built.
#
# Best of three, for the reason the decode estimator is also a best-of:
# interference is one-sided, so the fastest sample is the closest estimate and a
# mean is an average of the machine's mood. It matters more here than elsewhere,
# because a *drifting control is worse than no control* — it would report drift
# that is its own. Consecutive single readings spread 35% while the CPU ramped;
# best-of-three repeats to within about 1%, and the cold first run simply never
# wins.
[private]
_canary-run:
    #!/usr/bin/env bash
    set -uo pipefail
    just _canary-frame >/dev/null 2>&1 || exit 0
    just _build-zstd-profile >/dev/null 2>&1 || exit 0
    best=0
    for _ in 1 2 3; do
        v=$(./target/zstd-profile target/canary.zst 1 2>&1 | grep -o '~[0-9]* MB/s' | tr -d '~' | cut -d' ' -f1)
        [ -n "${v:-}" ] && [ "$v" -gt "$best" ] && best=$v
    done
    [ "$best" -gt 0 ] && echo "$best"

# Build the canary frame. Deterministic, so its value is comparable across
# sessions and machines, and cached because regenerating it is the slow part.
[private]
_canary-frame:
    #!/usr/bin/env bash
    set -euo pipefail
    [ -s target/canary.zst ] && exit 0
    mkdir -p target
    python3 ci/gen-canary.py > target/canary.txt
    {{zstd}} -3 -q -f --no-check target/canary.txt -o target/canary.zst
    rm -f target/canary.txt

# Establish the zstd baseline over a corpus directory.
#
#   just baseline corpus/
baseline DIR="corpus":
    cargo run --release -p zalt-bench -- baseline {{DIR}}

# Every compression level against libzstd's *same* level.
#
#   just encode-levels corpus/ztml
#   just encode-levels corpus/ztml 1,9,19 --time
#
# Size is deterministic and exact; add `--time` for speed, interleaved with
# libzstd per level, which needs the same quiet machine every timing does. Each
# row is zalt at level N divided by zstd at level N on both axes, so below 1.00
# is better — a level has not matched libzstd's level until both columns say
# so. Every frame at every level is decoded and compared before it is counted.
[doc("Each compression level against libzstd's same level: size, and with --time, speed.")]
encode-levels DIR="corpus" *ARGS="":
    cargo run --release -p zalt-bench -- encode-levels {{DIR}} {{ARGS}}

# Encode throughput against libzstd, over a corpus directory.
#
#   just encode-bench corpus/ztml
#
# The counterpart to `encode-ratio`, which answers what the encoder produces and
# never what it cost. Reports the *gap* to `zstd -3` and `-9` rather than MB/s,
# because a rate off this laptop is a number about the laptop: §10 records the
# same encode measured at 0.37 s and 0.17 s in consecutive runs. libzstd is
# linked in and its batches are interleaved with ours, so a thermal step or a
# core migration moves both columns and leaves the gap where it was.
#
# Both levels, because zalt's output sits between them — quoting one picks the
# flattering half.
[doc("Encode throughput vs libzstd over a corpus, reported as a gap ratio.")]
encode-bench DIR="corpus":
    cargo run --release -p zalt-bench -- encode {{DIR}}

# How decode scales across threads, and what sharing a pool costs.
#
#   just concurrent corpus/ztml
#   just concurrent corpus/ztml 4
#
# §6 gives L2 a global budget and a decoder pool; §8 says the workload is many
# small resources decoded concurrently; §10's zalt-darwin re-check admits none
# of it was ever measured under concurrency. This is that measurement.
#
# Two shapes run the identical workload — through the shared pool, and with one
# decoder per thread sharing nothing — so the gap between them at a given thread
# count is the cost of sharing, with everything else held constant.
#
# **This mode needs a quiet machine in a way the others do not.** Interleaving
# rescues a comparison between two implementations, because a disturbance lands
# on both and cancels in the ratio. It cannot rescue a comparison between thread
# counts: with no idle cores an added thread has nowhere to run, and the row
# reports the scheduler. The harness checks load against core count, and voids
# itself outright if any row comes back superlinear.
[doc("Decode scaling across threads, and what the pool costs. Needs an idle machine.")]
concurrent DIR="corpus/ztml" THREADS="":
    cargo run --release -p zalt-bench -- concurrent {{DIR}} {{THREADS}}

# Leaf-weighted profile of encoding one file. Needs nothing installed.
#
#   just profile-encode doc/rfc8878.txt
#   just profile-encode corpus/ztml/showcase.zdoc 10
#   just profile-encode corpus/varied.txt 14 1      (at level 1)
#
# The encode counterpart of `profile-decode`, and it earned its place the same
# way: the gap ratio says how far behind libzstd we are and never which part is
# behind. One run of this attributed 70% of encode time to `Matcher::find`, and
# every M5 throughput change came out of it — including the two that were
# reverted because the profile's explanation turned out not to predict a
# measurement. See doc/ZALT-MAC.md §10.
[doc("Leaf-weighted profile of encoding one file.")]
profile-encode FILE SECS="14" LEVEL="":
    #!/usr/bin/env bash
    set -uo pipefail
    cargo build --release -q -p zalt-bench || exit 1
    out=$(mktemp -t zalt-encode-sample)
    ./target/release/zalt-bench profile-encode "{{FILE}}" "{{SECS}}" {{LEVEL}} &
    pid=$!
    sleep 2
    sample "$pid" "$(( {{SECS}} - 3 ))" -mayDie -f "$out" >/dev/null 2>&1
    wait "$pid"
    echo
    if grep -q 'Sort by top of stack' "$out"; then
        sed -n "$(grep -n 'Sort by top of stack' "$out" | cut -d: -f1),/^$/p" "$out" \
            | sed -E 's/\(in [^)]*\)//'
    else
        echo "sample produced no call graph — is it permitted to attach?"
        echo "full output: $out"
    fi
    rm -f "$out"

# Profile a release binary with Instruments (needs cargo-instruments).
#
# For cycle-level work — cache misses, branch misses, the M1 PMU counters.
# When the question is only "which function", `profile-decode` needs no
# install and answers it in ten seconds.
profile BENCH="kernels":
    @command -v cargo-instruments >/dev/null || \
        (echo "cargo-instruments not installed: cargo install cargo-instruments" && exit 1)
    cargo instruments -t "CPU Profiler" --release -p zalt-bench -- {{BENCH}}

# Leaf-weighted profile of decoding one frame. Needs nothing installed.
#
#   just profile-decode corpus/big.zst
#   just profile-decode corpus/big.zst 10 scalar
#
# `sample` ships with macOS. This is the workflow that found every M3 win, and
# the reason it is a recipe rather than a note: the throughput table cannot
# tell "the kernels do not help" from "the kernels never ran", and that
# distinction was worth 50% on its own. Run the same file through both
# backends and compare — a hot function that costs the same in each is a
# function the acceleration is not reaching.
# Profile the *reference* decoder, in the same shape as `profile-decode`.
#
#   just profile-zstd /tmp/varied.zst
#   just profile-zstd corpus/ztml/showcase.zst 14
#
# Answers "where is zstd faster than us", which a throughput ratio cannot: a
# ratio says how much slower and never which part. Inference answered it wrongly
# three times — bounds checks, double-symbol Huffman, prefetching — before this
# harness answered it in one run. See doc/ZALT-MAC.md §10, tenth pass.
#
# Compare against `just profile-decode` on the *same frame*. libzstd inlines the
# FSE state machine, bit reader and match copy into `ZSTD_decompressSequences`,
# so compare that one symbol against the sum of zalt's `sequences::decode`,
# `BackwardBits`, `execute` and the copy kernels.
#
# Needs the zstd headers and static library (`brew install zstd`).
[doc("Leaf-weighted profile of libzstd decoding one frame, for comparison.")]
profile-zstd FILE SECS="14":
    #!/usr/bin/env bash
    set -uo pipefail
    just _build-zstd-profile || exit 1
    out=$(mktemp -t zstd-sample)
    ./target/zstd-profile "{{FILE}}" "{{SECS}}" &
    pid=$!
    sleep 2
    sample "$pid" "$(( {{SECS}} - 3 ))" -mayDie -f "$out" >/dev/null 2>&1
    wait "$pid"
    echo
    if grep -q 'Sort by top of stack' "$out"; then
        sed -n "$(grep -n 'Sort by top of stack' "$out" | cut -d: -f1),/^$/p" "$out" \
            | sed -E 's/\(in [^)]*\)//'
    else
        echo "sample produced no call graph — is it permitted to attach?"
        echo "full output: $out"
    fi
    rm -f "$out"

# Throughput only, no profiler. The number that must track `zstd -b3` to within
# a few percent; if it stops doing so, the harness is wrong rather than zstd.
[doc("Reference decode throughput through the C harness.")]
bench-zstd FILE SECS="6":
    #!/usr/bin/env bash
    set -uo pipefail
    just _build-zstd-profile || exit 1
    ./target/zstd-profile "{{FILE}}" "{{SECS}}"

# Build the reference harness against libzstd. Static, so the profiler can
# attribute time to libzstd's internal functions rather than to one dylib stub.
[private]
_build-zstd-profile:
    #!/usr/bin/env bash
    set -uo pipefail
    prefix=$(brew --prefix zstd 2>/dev/null || echo /opt/homebrew)
    if [ ! -f "$prefix/include/zstd.h" ] || [ ! -f "$prefix/lib/libzstd.a" ]; then
        echo "need the zstd headers and static lib: brew install zstd" >&2
        echo "  looked in $prefix" >&2
        exit 1
    fi
    src=crates/zalt-bench/reference/zstd-profile.c
    mkdir -p target
    [ -f target/zstd-profile ] && [ target/zstd-profile -nt "$src" ] && exit 0
    cc -O2 -Wall -Wextra -I"$prefix/include" \
       "$src" "$prefix/lib/libzstd.a" -o target/zstd-profile

[doc("Leaf-weighted profile of one decode, via macOS `sample`.")]
profile-decode FILE SECS="12" BACKEND="":
    #!/usr/bin/env bash
    set -uo pipefail
    cargo build --release -q -p zalt-bench
    out=$(mktemp -t zalt-sample)
    ./target/release/zalt-bench profile "{{FILE}}" "{{SECS}}" {{BACKEND}} &
    pid=$!
    sleep 2
    sample "$pid" "$(( {{SECS}} - 3 ))" -mayDie -f "$out" >/dev/null 2>&1
    wait "$pid"
    echo
    if grep -q 'Sort by top of stack' "$out"; then
        # `rustfilt` renders v0 symbol names properly. Without it the raw
        # mangled names are still legible enough to rank functions, which is
        # all this recipe is for, so it is a hint and not a dependency.
        if command -v rustfilt >/dev/null; then
            demangle() { rustfilt; }
        else
            demangle() { sed -E 's/_RN[A-Za-z0-9_]*zalt_(codec|neon|embed)[0-9]*/zalt::/'; }
            echo "(cargo install rustfilt for readable symbol names)"
            echo
        fi
        sed -n "$(grep -n 'Sort by top of stack' "$out" | cut -d: -f1),/^$/p" "$out" \
            | sed -E 's/\(in [^)]*\)//' | demangle
    else
        echo "sample produced no call graph — is it permitted to attach?"
        echo "full output: $out"
    fi
    rm -f "$out"

# ---------------------------------------------------------------- fuzzing

# List fuzz targets.
fuzz-list:
    @cd fuzz && cargo +nightly fuzz list

# Build every fuzz target without running one.
#
# Cheap enough for CI, and the thing that actually rots: a target referencing a
# renamed API compiles nowhere and nobody notices until someone tries to fuzz.
[doc("Build all fuzz targets (compile check).")]
fuzz-build:
    #!/usr/bin/env bash
    set -uo pipefail
    cd fuzz
    fail=0
    for t in $(cargo +nightly fuzz list); do
        printf '%-26s ' "$t"
        # Capture rather than discard. The first version sent both streams to
        # /dev/null, so a CI failure read `backend_equivalence FAILED` and
        # nothing else — a red build with the diagnosis thrown away, which cost
        # a round trip to even start guessing at.
        if out=$(cargo +nightly fuzz build --target {{fuzz_target_triple}} "$t" 2>&1); then
            echo ok
        else
            echo FAILED
            printf '%s\n' "$out" | sed 's/^/    /'
            fail=1
        fi
    done
    # Every target is built before reporting: one rotted target should not hide
    # the state of the other four.
    exit $fail

# Run a fuzz target. Requires nightly.
#
#   just fuzz decode_roundtrip 60
fuzz TARGET TIME="60":
    cd fuzz && cargo +nightly fuzz run --target {{fuzz_target_triple}} {{TARGET}} -- \
        -max_total_time={{TIME}} -rss_limit_mb=2048 -timeout=15

# Run every fuzz target briefly. A smoke test, not a campaign.
[doc("Run every fuzz target for a short time.")]
fuzz-all TIME="60":
    #!/usr/bin/env bash
    set -euo pipefail
    cd fuzz
    for t in $(cargo +nightly fuzz list); do
        echo "=== $t ==="
        cargo +nightly fuzz run --target {{fuzz_target_triple}} "$t" -- \
            -max_total_time={{TIME}} -rss_limit_mb=2048 -timeout=15 2>&1 \
            | grep -E 'Done [0-9]+ runs|panicked at|ERROR: libFuzzer' || true
    done

# --------------------------------------------------------------- fixtures

# Regenerate the zstd interoperability fixtures.
#
# Deliberately DETERMINISTIC: the "random" payloads come from AES-CTR over
# zeros with a fixed key, not /dev/urandom. Fixtures generated from real
# randomness change on every regeneration, which silently changes what the
# suite tests and makes a failure impossible to reproduce.
[doc("Regenerate the zstd interoperability fixtures (deterministic).")]
fixtures:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v {{zstd}} >/dev/null || { echo "zstd not found at {{zstd}}"; exit 1; }
    mkdir -p {{fixtures}}
    cd {{fixtures}}
    det() {  # det <bytes> <counter> -> deterministic incompressible bytes
        head -c "$1" /dev/zero | openssl enc -aes-256-ctr -nosalt \
            -K 5a414c5400000000000000000000000000000000000000000000000000000000 \
            -iv "$(printf '%032d' "$2")" 2>/dev/null
    }
    det 4096 1 > random4k.bin
    det 307200 2 > random300k.bin
    printf 'hello world' > hello.bin
    : > empty.bin
    # Compressible, repetitive markup: forces zstd to emit a real compressed
    # block with four-stream Huffman literals and FSE-coded sequences. The
    # generator is a separate script because a multi-line heredoc inside a
    # recipe breaks just's indentation-based body parsing.
    python3 "${OLDPWD}/ci/gen-text-fixture.py" > text.bin
    {{zstd}} -1 -c random4k.bin           > random4k.zst
    {{zstd}} -1 --no-check -c random4k.bin > random4k_nocheck.zst
    {{zstd}} -1 -c random300k.bin         > random300k.zst
    {{zstd}} -1 -c hello.bin              > hello.zst
    {{zstd}} -1 -c empty.bin              > empty.zst
    {{zstd}} -3 -c text.bin               > text.zst
    {{zstd}} -3 --no-check -c text.bin    > text_nocheck.zst
    echo "regenerated:"
    ls -la *.zst *.bin

# Interoperability against the reference and an independent decoder.
#
# In-process tests link our encoder and our decoder into one binary; this runs
# the real `zstd` executable over files on disk, which is what an ecosystem
# would actually do with our output. Both directions, both strategies, with and
# without a checksum.
[doc("zalt <-> zstd <-> ruzstd interop over real files.")]
interop *FILES:
    @bash ci/interop.sh {{FILES}}

# Compressed size against the reference, per file.
#
# Reports the gap, not a bare ratio: our number alone says nothing about
# whether the encoder is any good. Ratios are deterministic, so this needs no
# quiet-machine caveat — unlike anything measuring time.
[doc("Compression ratio against zstd -1, for the files given.")]
encode-ratio +FILES:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v {{zstd}} >/dev/null || { echo "need zstd on PATH"; exit 1; }
    printf "%-24s %9s %9s %8s %9s %8s %8s\n" file raw zalt ratio "zstd -1" ratio gap
    for f in {{FILES}}; do
        line=$(cargo run -q --release -p zalt-codec --example ratio -- "$f")
        raw=$(echo "$line" | cut -f2); ours=$(echo "$line" | cut -f3)
        ref=$({{zstd}} -1 -c "$f" | wc -c | tr -d ' ')
        printf "%-24s %9s %9s %7.2fx %9s %7.2fx %7.2fx\n" "$(basename "$f")" \
            "$raw" "$ours" "$(echo "scale=4;$raw/$ours" | bc)" \
            "$ref" "$(echo "scale=4;$raw/$ref" | bc)" \
            "$(echo "scale=4;$ours/$ref" | bc)"
    done

# ------------------------------------------------------- the ZTML recipe

ztml_data := "crates/zalt-ztml/data"
ztml_samples := "doc/antikythera.zdoc doc/christmas.zdoc doc/handsome.zdoc doc/painted.zdoc doc/scripts.zdoc doc/showcase.zdoc"

# Rebuild the ZTML content-prefix dictionary and commit the result.
#
# The blob is checked in rather than built by build.rs: `cargo build` has to
# work on a fresh clone, a browser needs the bytes via include_bytes!, and a
# dictionary that changes with whatever is in someone's corpus directory makes
# every ratio measured against it unreproducible.
#
# `doc/synthetic.zdoc` is deliberately excluded. It is 219 KB against 3-12 KB
# for the rest, and measured it makes the held-out ratio slightly *worse* while
# growing the dictionary sevenfold. Dictionaries pay on small resources.
[doc("Rebuild the ZTML content-prefix dictionary from doc/*.zdoc.")]
ztml-dict:
    cargo run -q -p zalt-recipe-train -- build \
        --vocabulary {{ztml_data}}/vocabulary.txt \
        --out {{ztml_data}} --name ztml-0.3 \
        {{ztml_samples}}

# Prove the checked-in dictionary is exactly what its inputs produce.
#
# Part of the gate. The generator is `cat` in a fixed order, so this needs no
# tools and is exact — and it is worth having because the order is part of the
# artifact: the same bytes concatenated differently measured up to 3.7% apart.
[doc("Verify the committed ZTML dictionary matches its recorded inputs.")]
ztml-dict-check:
    #!/usr/bin/env bash
    set -euo pipefail
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    cargo run -q -p zalt-recipe-train -- build \
        --vocabulary {{ztml_data}}/vocabulary.txt \
        --out "$tmp" --name ztml-0.3 {{ztml_samples}} >/dev/null
    if ! cmp -s "$tmp/ztml-0.3.dict" {{ztml_data}}/ztml-0.3.dict; then
        echo "ZTML dictionary does not match its inputs. Run \`just ztml-dict\`." >&2
        exit 1
    fi
    if ! cmp -s "$tmp/ztml-0.3.provenance.txt" {{ztml_data}}/ztml-0.3.provenance.txt; then
        echo "ZTML provenance is stale. Run \`just ztml-dict\`." >&2
        exit 1
    fi
    echo "ZTML dictionary matches its inputs."

# Measure what the dictionary is worth, held out.
#
# Leave-one-out: for each document the dictionary is rebuilt WITHOUT it. A
# dictionary containing the file it is measured on reports memorisation — the
# shipped dictionary compresses showcase.zdoc from 7 782 bytes to 23, because
# it contains it verbatim.
[doc("Leave-one-out ratio for the ZTML dictionary (needs zstd).")]
ztml-ratio level="9":
    @cargo run -q -p zalt-recipe-train -- eval \
        --vocabulary {{ztml_data}}/vocabulary.txt --level {{level}} {{ztml_samples}}

# Search dictionary size against compression level, held out.
#
# Where `level = 9` in the ZTML manifest came from. Slow: every cell is a full
# leave-one-out pass, so it is a deliberate act rather than part of the gate.
[doc("Sweep dictionary size and level for the ZTML recipe (needs zstd).")]
ztml-sweep:
    @cargo run -q -p zalt-recipe-train -- sweep \
        --vocabulary {{ztml_data}}/vocabulary.txt {{ztml_samples}}

# Materialise derived artifacts for the reference conformance corpus.
#
# `zstd-fixtures/` holds facebook/zstd's own regression files. The golden
# decompression frames need expected outputs, and the golden compression inputs
# need compressed forms; both are derived, so they are generated rather than
# committed.
[doc("Regenerate derived files for the zstd reference corpus.")]
zstd-fixtures:
    #!/usr/bin/env bash
    set -euo pipefail
    root=zstd-fixtures
    [ -d "$root" ] || { echo "$root not present"; exit 1; }
    command -v {{zstd}} >/dev/null 2>&1 || {
        echo "need the zstd binary on PATH (or ZSTD=/path/to/zstd) to derive fixtures" >&2
        exit 1; }
    mkdir -p "$root/generated"
    for f in "$root"/golden-decompression/*.zst; do
        [ -e "$f" ] || continue
        b=$(basename "${f%.zst}")
        {{zstd}} -d -c "$f" > "$root/generated/$b.expected"
    done
    for f in "$root"/golden-compression/*; do
        [ -f "$f" ] || continue
        b=$(basename "$f")
        for L in 1 3 9 19; do
            {{zstd}} -$L -c "$f" > "$root/generated/$b.L$L.zst"
        done
    done
    # Dictionary-compressed frames, so the dictionary path has something to
    # decode. Without these the loader is tested and the *wiring* is not — and
    # the wiring is where a dictionary can silently seed the wrong history.
    for dict in "$root"/golden-dictionaries/*; do
        [ -f "$dict" ] || continue
        dname=$(basename "$dict")
        for f in "$root"/golden-compression/*; do
            [ -f "$f" ] || continue
            b=$(basename "$f")
            for L in 1 3 9; do
                {{zstd}} -$L -D "$dict" -c "$f" \
                    > "$root/generated/$b.D-$dname.L$L.zst" 2>/dev/null || true
            done
        done
    done
    echo "generated $(ls "$root/generated" | wc -l | tr -d ' ') files"

# Ensure the derived fixtures exist, cheaply.
#
# The conformance tests read `zstd-fixtures/generated`, which is derived and so
# deliberately not committed. They *skip* when `zstd-fixtures/` is absent
# entirely and **fail** when it is present without the derived files — which is
# exactly the state of a fresh clone, and is what broke CI: the golden corpora
# are committed, the artifacts derived from them are not, and nothing generated
# them before the tests ran.
#
# Depending on this from the test recipes rather than adding a step to the
# workflow is deliberate. CI runs the same recipes as the laptop precisely so
# the two cannot drift, and a CI-only step is drift by construction. Costs one
# `ls` when the files are already there, and 0.6s when they are not.
[private]
_fixtures:
    #!/usr/bin/env bash
    set -uo pipefail
    root=zstd-fixtures
    [ -d "$root" ] || exit 0
    [ -n "$(ls -A "$root/generated" 2>/dev/null)" ] && exit 0
    just zstd-fixtures

# Build facebook/zstd's own valid-frame generator and decode what it emits.
#
#   just decodecorpus           # 200 frames
#   just decodecorpus 1000 42   # count, seed
#
# # Why this reaches frames nothing else does
#
# `roundtrip_valid` fuzzes by handing bytes to libzstd's *encoder*, so it can
# only ever produce encodings libzstd chooses to emit. Large parts of the legal
# format are unreachable that way — §9 notices the symptom without naming it,
# calling `rle-first-block` "a block type encoders almost never emit" and
# keeping exactly one example of it by hand.
#
# `decodecorpus` generates random *valid* frames across the legal space on
# purpose: every literals mode, every block type, unusual window sizes, the
# offset-code paths an optimising encoder never picks. Each frame is checkable
# against libzstd, so a disagreement is unambiguous.
#
# # Why it has to be fetched
#
# `zstd-fixtures/decodecorpus.c` is vendored, but it `#include`s zstd's internal
# headers and `compress/zstd_compress.c` outright, so it only builds inside the
# zstd source tree. The vendored copy has therefore never compiled here — it is
# the source to read, not to build. This fetches the tree at the version the
# reference binary reports, so the generator matches the oracle.
[doc("Generate valid frames with zstd's decodecorpus and decode them all.")]
decodecorpus COUNT="200" SEED="0":
    #!/usr/bin/env bash
    set -uo pipefail
    ver=$({{zstd}} --version 2>/dev/null | sed -E 's/.*v([0-9]+\.[0-9]+\.[0-9]+).*/\1/')
    [ -n "$ver" ] || { echo "no reference zstd on PATH" >&2; exit 1; }
    src="target/zstd-$ver"
    gen="$src/tests/decodecorpus"
    if [ ! -x "$gen" ]; then
        if [ ! -d "$src" ]; then
            echo "fetching zstd $ver source (needed to build decodecorpus)..."
            mkdir -p target
            url="https://github.com/facebook/zstd/archive/refs/tags/v$ver.tar.gz"
            curl -sSfL "$url" | tar -xz -C target || {
                echo "fetch failed — needs network. Source would go in $src" >&2
                exit 1; }
        fi
        echo "building decodecorpus..."
        make -C "$src/tests" decodecorpus >/dev/null 2>&1 || {
            echo "build failed; try: make -C $src/tests decodecorpus" >&2; exit 1; }
    fi
    out=target/decodecorpus
    rm -rf "$out"; mkdir -p "$out/frames" "$out/orig"
    # `-o` is not optional. Without it the generator writes frames and no
    # expected output, and a comparison guarded on the expected file existing
    # then silently checks nothing — which is how the first version of this
    # recipe reported "2500 frames match the reference" having compared none of
    # them. A missing expected file is a failure here, not a skip.
    "$gen" -p"$out/frames/" -o"$out/orig/" -n{{COUNT}} -s{{SEED}} >/dev/null 2>&1 || {
        echo "generator failed" >&2; exit 1; }
    n=$(ls "$out"/frames/*.zst 2>/dev/null | wc -l | tr -d ' ')
    echo "generated $n frames in $out/frames"
    [ "$n" -gt 0 ] || exit 1
    cargo build --release -q -p zalt-cli
    fail=0
    checked=0
    for f in "$out"/frames/*.zst; do
        base=$(basename "${f%.zst}")
        exp="$out/orig/$base"
        if [ ! -f "$exp" ]; then
            echo "NO EXPECTED OUTPUT for $base — comparison would be vacuous"
            fail=1; continue
        fi
        if ! ./target/release/zalt d -q -c --trusted "$f" > "$f.out" 2>/dev/null; then
            echo "REFUSED: $base"; fail=1; continue
        fi
        if ! cmp -s "$f.out" "$exp"; then
            echo "MISMATCH: $base"; fail=1; continue
        fi
        checked=$((checked+1))
    done
    echo "$checked/$n frames decoded byte-exactly against the reference"
    [ "$checked" -eq "$n" ] || exit 1
    exit $fail

# Mutation testing: does the suite actually *notice* a bug?
#
#   just mutants                       # the whole codec (slow)
#   just mutants crates/zalt-codec/src/fse.rs
#
# Coverage answers "was this line run". Mutation testing answers the question
# that matters — "if this line were wrong, would anything fail?" — by changing
# the code and checking a test goes red.
#
# It targets this project's actual failure history exactly. The `ML_DEFAULT`
# distribution was a *wrong constant that still summed to 64*, so the table
# built, every internal consistency check passed, and only a checksum
# eventually objected. A mutant that changes a constant and sees the suite stay
# green is that bug, reported before it is written.
#
# Slow by nature: each mutant is a rebuild and a test run. Scope it to one file
# while iterating; run the codec overnight.
[doc("Mutation testing over zalt-codec — do the tests catch injected bugs?")]
mutants FILE="":
    #!/usr/bin/env bash
    set -uo pipefail
    command -v cargo-mutants >/dev/null 2>&1 || {
        echo "cargo-mutants is not installed:  cargo install cargo-mutants --locked" >&2
        exit 1; }
    just _fixtures
    args=(--package zalt-codec --no-shuffle --timeout 120)
    # `just t` is the inner loop; running the release suite per mutant would
    # multiply an already slow process by the cost of LTO.
    if [ -n "{{FILE}}" ]; then
        args+=(--file "{{FILE}}")
    fi
    cargo mutants "${args[@]}"

# Line and region coverage over the whole workspace.
#
#   just coverage            # summary table
#   just coverage html       # browsable report
#
# This exists because of a specific failure §10 records. The entire fixture set
# used `FSE_Compressed` mode, so the *predefined* FSE tables were never
# exercised by a single test — a whole code path unverified, and nothing said
# so. The `ML_DEFAULT` bug lived there, it summed to 64 like the correct table,
# every internal consistency check passed, and only a checksum objected.
#
# Coverage would have printed that path as dark on day one. It answers "which
# tests should we write" with evidence rather than intuition, which is the
# lesson the rest of this file keeps learning the expensive way.
#
# Needs `cargo-llvm-cov`; degrades to instructions rather than failing.
[doc("Line and region coverage over the workspace.")]
coverage MODE="summary":
    #!/usr/bin/env bash
    set -uo pipefail
    if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
        echo "cargo-llvm-cov is not installed." >&2
        echo "  cargo install cargo-llvm-cov --locked" >&2
        echo "  rustup component add llvm-tools-preview" >&2
        exit 1
    fi
    just _fixtures
    case "{{MODE}}" in
        html)
            cargo llvm-cov --workspace --html
            echo "report: target/llvm-cov/html/index.html"
            ;;
        lcov)
            cargo llvm-cov --workspace --lcov --output-path target/lcov.info
            echo "wrote target/lcov.info"
            ;;
        *)
            cargo llvm-cov --workspace
            ;;
    esac

# The parts of the codec that no test reaches, worst first.
#
# The summary is per file; this is the question actually worth asking, because
# an 85%-covered file can still have an entire mode unexercised.
[doc("Least-covered regions in zalt-codec, worst first.")]
coverage-gaps:
    #!/usr/bin/env bash
    set -uo pipefail
    command -v cargo-llvm-cov >/dev/null 2>&1 || {
        echo "cargo-llvm-cov is not installed; see 'just coverage'" >&2; exit 1; }
    just _fixtures
    cargo llvm-cov --workspace --json --output-path target/cov.json >/dev/null
    python3 ci/coverage-gaps.py target/cov.json

# Seed the fuzz corpora from the fixtures and the reference corpus.
[doc("Seed fuzz corpora from fixtures and the reference corpus.")]
fuzz-seed:
    @bash ci/seed-fuzz-corpus.sh

# Show how the reference zstd frames a file, for reading alongside RFC 8878.
inspect FILE:
    @{{zstd}} -1 -c {{FILE}} | xxd | head -8

# ------------------------------------------------------------------- misc

# Build and open the API docs.
doc:
    cargo doc --workspace --no-deps --open

# Report the selected backend and toolchain.
info:
    @echo "toolchain: $(rustc --version)"
    @echo "target:    $(rustc -vV | grep host | cut -d' ' -f2)"
    @echo "zstd:      $({{zstd}} --version 2>/dev/null || echo 'not found')"
    @cargo run --release -q -p zalt-bench -- kernels 2>/dev/null | head -1 || true

# Install the optional tools these recipes can use.
install-tools:
    cargo install cargo-nextest --locked
    cargo install cargo-instruments --locked
    cargo install cargo-fuzz --locked
    cargo install cargo-deny --locked

# Report what is taking disk space, and whether it is worth keeping.
[doc("Show disk usage of build and fuzz caches.")]
sizes:
    #!/usr/bin/env bash
    set -uo pipefail
    row() { [ -e "$1" ] && printf "  %-26s %8s  %s\n" "$1" "$(du -sh "$1" 2>/dev/null | cut -f1)" "$2"; }
    echo "repo-local:"
    row target                  "build artifacts — regenerable"
    row fuzz/target             "fuzz build artifacts — regenerable"
    row fuzz/corpus             "accumulated fuzz coverage — EXPENSIVE to rebuild"
    row fuzz/artifacts          "crash reproducers — keep until triaged"
    row zstd-fixtures/generated "derived fixtures — just zstd-fixtures"
    row corpus                  "benchmark corpus — local, not regenerable"
    printf "  %-26s %8s\n" "TOTAL" "$(du -sh . 2>/dev/null | cut -f1)"
    echo
    echo "shared (not repo-local):"
    printf "  %-26s %8s  %s\n" "~/.cargo/registry" \
        "$(du -sh ~/.cargo/registry 2>/dev/null | cut -f1)" \
        "downloaded crates — cargo cache clean"

# Remove build artifacts from both workspaces.
#
# `fuzz/` is a separate workspace, so a bare `cargo clean` misses it and leaves
# the larger half behind.
[doc("Remove build artifacts from both workspaces.")]
clean:
    cargo clean
    cd fuzz && cargo clean

# Remove derived fixtures. Regenerate with `just zstd-fixtures`.
clean-generated:
    rm -rf zstd-fixtures/generated

# Remove fuzz build artifacts and crash reproducers, keeping the corpus.
#
# Crash reproducers are deleted, so triage anything interesting first.
[doc("Remove fuzz build artifacts and crash reproducers (keeps corpus).")]
clean-fuzz:
    rm -rf fuzz/artifacts
    cd fuzz && cargo clean

# Remove the accumulated fuzz corpus.
#
# Deliberately separate from `clean-fuzz`: the corpus is not cache. It is
# coverage the fuzzer paid CPU time to discover, and throwing it away means the
# next campaign starts cold. `just fuzz-seed` restores the seeds but not the
# inputs the fuzzer found for itself.
[doc("Delete the accumulated fuzz corpus (loses coverage).")]
clean-corpus:
    rm -rf fuzz/corpus
    @echo "corpus deleted — run 'just fuzz-seed' to reseed"

# Everything regenerable, corpus included.
clean-all: clean clean-generated clean-fuzz clean-corpus
    @echo
    @just sizes

# Collect the large-file half of the benchmark corpus.
[private]
_collect-varied:
    #!/usr/bin/env bash
    set -uo pipefail
    echo "collecting varied text..."
    # `find | head` makes `find` die of SIGPIPE, which `pipefail` then reports
    # as a failure. Collect the list first.
    files=$(find ~/.cargo/registry -name '*.rs' 2>/dev/null | sort | head -3000 || true)
    [ -n "$files" ] || { echo "no cargo registry sources found"; exit 1; }
    printf '%s\n' "$files" | xargs cat 2>/dev/null > corpus/varied.txt || true
    [ -s corpus/varied.txt ] || { echo "collection produced nothing"; exit 1; }
