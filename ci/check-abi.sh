#!/usr/bin/env bash
#
# Keep the hand-written C header and the Rust ABI in agreement.
#
# The header is hand-written on purpose: it *is* the contract, and generating it
# would invert that — the header would follow whatever the Rust happened to look
# like, and an accidental signature change would silently become the new API.
#
# The cost of hand-writing is drift, which is what this catches. It compares two
# things a mismatch would show up in:
#
#   1. Exported symbols. A function in one and not the other is a link error for
#      somebody, discovered at integration time rather than here.
#   2. Constants. A code that disagrees is worse than a missing one: the caller
#      links fine and misreads every error.
#
# It does not compare full signatures. A change there is caught by the ABI tests,
# which call these functions the way C does.

set -uo pipefail

RUST=crates/zalt-ffi/src/lib.rs
HEADER=crates/zalt-ffi/include/zalt.h
fail=0

[ -f "$RUST" ] || { echo "no $RUST"; exit 0; }
[ -f "$HEADER" ] || { echo "no $HEADER"; exit 1; }

# Exported function names, from the Rust.
rust_fns=$(grep -oE 'pub (unsafe )?extern "C" fn [a-z0-9_]+' "$RUST" \
    | awk '{print $NF}' | sort -u)

# Declared function names, from the header. Matches a declaration's opening
# paren at the start of a line or after a return type.
header_fns=$(grep -oE '\bzalt_[a-z0-9_]+\(' "$HEADER" \
    | tr -d '(' | sort -u)

missing_in_header=$(comm -23 <(echo "$rust_fns") <(echo "$header_fns"))
missing_in_rust=$(comm -13 <(echo "$rust_fns") <(echo "$header_fns"))

if [ -n "$missing_in_header" ]; then
    echo "ABI DRIFT: exported by Rust but absent from $HEADER"
    echo "$missing_in_header" | sed 's/^/    /'
    echo
    fail=1
fi

if [ -n "$missing_in_rust" ]; then
    echo "ABI DRIFT: declared in $HEADER but not exported by Rust"
    echo "$missing_in_rust" | sed 's/^/    /'
    echo "  (a caller linking against these gets an undefined symbol)"
    echo
    fail=1
fi

# Constants must agree in value, not merely exist.
for name in ZALT_OK ZALT_ERR_NULL_POINTER ZALT_ERR_OUTPUT_TOO_SMALL \
            ZALT_ERR_OVER_BUDGET ZALT_ERR_PANIC ZALT_ERR_BAD_ARGUMENT \
            ZALT_STATUS_NEED_MORE_INPUT ZALT_STATUS_HAS_MORE_OUTPUT \
            ZALT_STATUS_FRAME_COMPLETE ZALT_ABI_VERSION; do
    r=$(grep -oE "pub const ${name}: [a-z0-9]+ = -?[0-9]+" "$RUST" \
        | grep -oE '\-?[0-9]+$' | head -1)
    h=$(grep -oE "#define ${name} \(?-?[0-9]+u?\)?" "$HEADER" \
        | grep -oE '\-?[0-9]+' | head -1)

    if [ -z "$r" ] || [ -z "$h" ]; then
        echo "ABI DRIFT: ${name} missing from $([ -z "$r" ] && echo Rust || echo header)"
        fail=1
        continue
    fi
    if [ "$r" != "$h" ]; then
        echo "ABI DRIFT: ${name} is ${r} in Rust but ${h} in the header"
        echo "  a caller would link fine and misread this value"
        fail=1
    fi
done

if [ "$fail" -eq 0 ]; then
    echo "C ABI matches the header ($(echo "$rust_fns" | wc -l | tr -d ' ') functions)."
fi
exit "$fail"
