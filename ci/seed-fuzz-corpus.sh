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
#   [variant][body]            -> dcz_prelude
#   [level][split:2][material] -> dcz_roundtrip

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
         streaming_equivalence backend_equivalence \
         dcz_prelude dcz_roundtrip; do
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

# The dcz targets (RFC 9842).
#
# Both build their input from a leading tag, so a seed is that tag plus
# material. `dcz_prelude` variant 0 is "a well-formed header": the 32 bytes
# after the tag become the hash and the rest becomes the frame, so a real
# compressed frame here seeds a realistic stream. `dcz_roundtrip` splits its
# material into dictionary and payload, so prose seeds it best — two halves of
# one document are exactly the shape dictionary transport is for.
ZTML=crates/zalt-ztml/data/ztml-0.3.dict
for f in "$FIX"/*.zst "$REF"/golden-decompression/*.zst; do
    [ -e "$f" ] || continue
    # \x00 selects the well-formed-header variant; 32 bytes of hash follow.
    { printf '\x00'; head -c 32 /dev/zero | tr '\0' 'A'; cat "$f"; } \
        > "fuzz/corpus/dcz_prelude/$(basename "$f").seed"
done
[ -e "$ZTML" ] && seed_content dcz_prelude '\x05' "$ZTML"   # the dcb variant

for f in doc/*.zdoc "$REF"/golden-compression/*; do
    [ -f "$f" ] || continue
    case "$(basename "$f")" in .*) continue ;; esac
    # level 3, split 2048 bytes in: enough dictionary to find matches with.
    seed_content dcz_roundtrip '\x03\x00\x08' "$f"
done

echo "corpus seeded:"
for d in fuzz/corpus/*/; do
    printf "  %-24s %s files\n" "$(basename "$d")" "$(find "$d" -type f | wc -l | tr -d ' ')"
done
