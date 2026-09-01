#!/usr/bin/env bash
#
# Seed the fuzz corpora.
#
# A fuzzer starting from valid frames explores vastly better than one starting
# from nothing: random bytes essentially never form a valid zstd frame past the
# magic number, so an unseeded run hammers header validation and never builds an
# entropy table. The reference implementation's own regression corpus is the
# best seed material available — every file in it is a trap somebody already
# fell into.
#
# Targets take two different input shapes:
#   raw frames                 -> decode_hostile, decode_differential
#   [level][content]           -> roundtrip_valid, backend_equivalence
#   [level][schedule][content] -> streaming_equivalence

set -euo pipefail

FIX=crates/zalt-codec/tests/fixtures
REF=zstd-fixtures

seed_frame() {  # seed_frame <target> <file>
    install -m 644 "$2" "fuzz/corpus/$1/$(basename "$2")"
}

seed_content() {  # seed_content <target> <prefix-bytes> <file>
    local target="$1" prefix="$2" file="$3"
    { printf "%b" "${prefix}"; cat "${file}"; } \
        > "fuzz/corpus/${target}/$(basename "${file}").seed"
}

for t in decode_hostile decode_differential roundtrip_valid \
         streaming_equivalence backend_equivalence; do
    mkdir -p "fuzz/corpus/$t"
done

# Raw-frame targets.
for f in "$FIX"/*.zst \
         "$REF"/golden-decompression/*.zst \
         "$REF"/golden-decompression-errors/*.zst \
         "$REF"/generated/*.zst; do
    [ -e "$f" ] || continue
    seed_frame decode_hostile "$f"
    seed_frame decode_differential "$f"
done

# Content targets. The leading byte selects a compression level.
for f in "$FIX"/*.bin "$REF"/golden-compression/* "$REF"/generated/*.expected; do
    [ -f "$f" ] || continue
    case "$(basename "$f")" in .*) continue ;; esac
    seed_content roundtrip_valid '\x03' "$f"
    seed_content backend_equivalence '\x03' "$f"
    seed_content streaming_equivalence '\x03\x11' "$f"
done

echo "corpus seeded:"
for d in fuzz/corpus/*/; do
    printf "  %-24s %s files\n" "$(basename "$d")" "$(find "$d" -type f | wc -l | tr -d ' ')"
done
