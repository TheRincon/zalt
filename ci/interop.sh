#!/usr/bin/env bash
#
# Interoperability, in both directions, against two independent decoders.
#
# The tests in `crates/zalt-codec/tests/encode.rs` assert all of this in-process.
# This script exists because the two are not the same evidence: a test links our
# encoder and our decoder into one binary, while this runs the *actual* `zstd`
# executable over files on disk — the thing an ecosystem would do with our
# output. If the two ever disagree, the disagreement is the finding.
#
# Four directions:
#
#   1. zalt  -> zstd     the reference reads what we write. Its refusal is our
#                        bug; there is no second opinion to appeal to.
#   2. zalt  -> ruzstd   an implementation sharing no code with libzstd or with
#                        zalt reads it too, so a misreading common to our own
#                        encoder and decoder cannot hide behind itself.
#   3. zstd  -> zalt     we read what the reference writes, at several levels.
#   4. zalt  -> zalt     the round trip, which proves the least and is listed
#                        last for that reason.
#
# Usage:  ci/interop.sh [files...]      (defaults to a spread of repo files)

set -uo pipefail

ZSTD="${ZSTD:-zstd}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

command -v "$ZSTD" >/dev/null || { echo "need zstd on PATH (or \$ZSTD)" >&2; exit 1; }

echo "building..."
cargo build -q --release -p zalt-codec --example compress --example ruzstd_decompress
cargo build -q --release -p zalt-cli

COMPRESS="target/release/examples/compress"
RUZSTD="target/release/examples/ruzstd_decompress"
ZALT="target/release/zalt"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if [ "$#" -gt 0 ]; then
    FILES=("$@")
else
    FILES=()
    for f in doc/showcase.zdoc doc/handsome.zdoc doc/rfc9842.txt \
             crates/zalt-codec/tests/fixtures/text.bin \
             crates/zalt-codec/tests/fixtures/random4k.bin; do
        [ -f "$f" ] && FILES+=("$f")
    done
    # An empty file and a single byte: the degenerate frames every encoder
    # gets wrong at least once.
    : > "$WORK/empty.bin"; FILES+=("$WORK/empty.bin")
    printf 'x' > "$WORK/one.bin"; FILES+=("$WORK/one.bin")
fi

pass=0
fail=0

check() {  # check <label> <expected-file> <actual-file>
    if cmp -s "$2" "$3"; then
        pass=$((pass + 1))
        printf '    %-46s ok\n' "$1"
    else
        fail=$((fail + 1))
        printf '    %-46s MISMATCH\n' "$1"
    fi
}

refuse() {  # refuse <label>
    fail=$((fail + 1))
    printf '    %-46s REFUSED\n' "$1"
}

for src in "${FILES[@]}"; do
    name="$(basename "$src")"
    size="$(wc -c < "$src" | tr -d ' ')"
    echo
    echo "=== $name ($size bytes)"

    for mode in stored compressed; do
        for sum in nochecksum checksum; do
            frame="$WORK/$name.$mode.$sum.zst"
            "$COMPRESS" "$src" "$frame" "$mode" "$sum" || { refuse "encode $mode/$sum"; continue; }
            fsize="$(wc -c < "$frame" | tr -d ' ')"
            printf '  %s/%s -> %s bytes\n' "$mode" "$sum" "$fsize"

            # 1. the reference
            if "$ZSTD" -q -d -c "$frame" > "$WORK/via-zstd" 2>"$WORK/err"; then
                check "zalt -> zstd" "$src" "$WORK/via-zstd"
            else
                refuse "zalt -> zstd: $(head -1 "$WORK/err")"
            fi

            # 2. the independent implementation
            if "$RUZSTD" "$frame" "$WORK/via-ruzstd" 2>"$WORK/err"; then
                check "zalt -> ruzstd" "$src" "$WORK/via-ruzstd"
            else
                refuse "zalt -> ruzstd: $(head -1 "$WORK/err")"
            fi

            # 4. ourselves
            if "$ZALT" d -q --trusted -o "$WORK/via-zalt" -f "$frame" 2>"$WORK/err"; then
                check "zalt -> zalt" "$src" "$WORK/via-zalt"
            else
                refuse "zalt -> zalt: $(head -1 "$WORK/err")"
            fi
        done
    done

    # 3. the inverse: the reference writes, we read.
    for level in 1 3 9 19; do
        ref="$WORK/$name.ref.$level.zst"
        "$ZSTD" -q -"$level" -c "$src" > "$ref"
        if "$ZALT" d -q --trusted -o "$WORK/ref-via-zalt" -f "$ref" 2>"$WORK/err"; then
            check "zstd -$level -> zalt" "$src" "$WORK/ref-via-zalt"
        else
            refuse "zstd -$level -> zalt: $(head -1 "$WORK/err")"
        fi
    done
done

echo
echo "interop: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
