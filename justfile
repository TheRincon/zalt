# zalt — task runner
#
#   just            list recipes
#   just t          fast inner-loop tests (nextest, parallel)
#   just gate       everything CI runs, in CI order
#
# See doc/ZALT-MAC.md for what any of this is for.

set shell := ["bash", "-uc"]

# Reference zstd for baselines and differential testing.
zstd := env_var_or_default("ZSTD", "/opt/homebrew/bin/zstd")
# no_std target used to prove zalt-codec stays portable and allocation-light.
nostd_target := "thumbv7em-none-eabi"
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
fast-test:
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
test-release:
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

clippy-fix:
    cargo clippy --fix --workspace --all-targets --allow-dirty -- -D warnings

# Enforce the layering contract from doc/ZALT-MAC.md §1.
layering:
    @bash ci/layering.sh

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
no-std:
    rustup target add {{nostd_target}} 2>/dev/null || true
    cargo build -p zalt-codec --target {{nostd_target}}

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
miri:
    rustup +nightly component add miri 2>/dev/null || true
    MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri nextest run -p zalt-codec

# Everything CI runs, in CI order. Run before pushing.
gate: fmt-check clippy layering abi-check test test-release
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
# The two CI jobs deliberately left out are `miri` (~4 minutes) and `fuzz-build`
# (needs nightly and cargo-fuzz). Both still run in CI. They are excluded on the
# reasoning that **a hook people bypass is worse than a shorter hook they run**:
# past a few minutes, `--no-verify` becomes the habit and then nothing is
# checked at all. `just pre-push-full` runs them when the change warrants it.
[doc("What the pre-push hook runs: gate + no-std.")]
pre-push: gate no-std
    @echo
    @echo "pre-push passed (gate + no-std)."
    @echo "for the slow CI jobs too:  just pre-push-full"

# Everything CI runs, including the slow jobs. Worth it before a PR, and before
# anything touching `unsafe` — though note Miri cannot execute NEON intrinsics,
# so it validates the scalar oracle rather than the accelerated path. What
# guards the kernels is `test-release` (already in gate, where `debug_assert` is
# compiled out) and the `backend_equivalence` tests and fuzz target.
[doc("Everything CI runs, slow jobs included (adds fuzz-build + miri).")]
pre-push-full: gate no-std fuzz-build miri
    @echo
    @echo "pre-push-full passed (gate + no-std + fuzz-build + miri)."

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
    out=$(./target/release/zalt-bench decode "{{DIR}}" 3 2>/dev/null)
    printf '%s\n' "$out" | sed -n '1,/^-----/p'
    echo "running {{ROUNDS}} rounds; the best of them is the estimate..."
    for i in $(seq 1 {{ROUNDS}}); do
        ./target/release/zalt-bench decode "{{DIR}}" 3 2>/dev/null | grep '^SUMMARY'
    done | sed 's/[a-z]*=//g' | awk '{if($2>s)s=$2; if($3>n)n=$3; if($4>k)k=$4}
        END{printf "\nBEST OF {{ROUNDS}}:  scalar %.1f   neon %.1f   sink %.1f  MB/s\n",s,n,k}'

# Establish the zstd baseline over a corpus directory.
#
#   just baseline corpus/
baseline DIR="corpus":
    cargo run --release -p zalt-bench -- baseline {{DIR}}

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
    set -euo pipefail
    cd fuzz
    for t in $(cargo +nightly fuzz list); do
        printf '%-26s ' "$t"
        cargo +nightly fuzz build "$t" >/dev/null 2>&1 && echo ok || { echo FAILED; exit 1; }
    done

# Run a fuzz target. Requires nightly.
#
#   just fuzz decode_roundtrip 60
fuzz TARGET TIME="60":
    cd fuzz && cargo +nightly fuzz run {{TARGET}} -- \
        -max_total_time={{TIME}} -rss_limit_mb=2048 -timeout=15

# Run every fuzz target briefly. A smoke test, not a campaign.
[doc("Run every fuzz target for a short time.")]
fuzz-all TIME="60":
    #!/usr/bin/env bash
    set -euo pipefail
    cd fuzz
    for t in $(cargo +nightly fuzz list); do
        echo "=== $t ==="
        cargo +nightly fuzz run "$t" -- \
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
    echo "generated $(ls "$root/generated" | wc -l | tr -d ' ') files"

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
