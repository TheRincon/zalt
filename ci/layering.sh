#!/usr/bin/env bash
#
# Enforce the layering contract from doc/ZALT-MAC.md §1.
#
#   Dependencies point downward only. No lower layer may name a higher one.
#
# This is a grep, not a proof. It cannot catch a leak spelled differently, and
# it is not meant to: its job is to make the *easy* violation — the one-line
# content-specific branch added under deadline — impossible to land quietly.
# A leak that evades it will be caught in review; a leak nobody was looking for
# would not have been.
#
# If this script fails, the fix is essentially never to add the word to the
# allowlist. It is to widen a generic seam in a higher layer.

set -uo pipefail

fail=0

# Only `src/` is searched, never `tests/` or fixtures. The rule is about
# behaviour that branches on a content class, and test *data* that looks like
# one proves nothing about the code: `zalt-codec/tests/fixtures/text.bin` is
# WEFT-shaped markup because that is the workload being compressed, and
# `zalt-recipe/tests/manifests.rs` copies §7's sketch verbatim so the document
# and the parser cannot drift apart. Widening this to `tests/` would fail the
# build on both, and would be enforcing the letter against the purpose.
#
# Comments are documentation, not dependencies. A doc comment in zalt-codec
# saying "this crate must not know about browsers" is the contract being
# stated, not broken — and the first version of this script failed the build
# for exactly that. Strip comment lines before judging.
#
# Filters result lines of the form `path:lineno:<whitespace>//...`, covering
# `//`, `//!`, `///`, `/*` and `*` continuation lines.
strip_comments() {
    grep -vE '^[^:]+:[0-9]+:[[:space:]]*(//|\*|/\*)'
}

check() {
    local crate="$1"
    local pattern="$2"
    local why="$3"

    if [ ! -d "crates/${crate}/src" ]; then
        return 0
    fi

    local hits
    hits=$(grep -rniE "${pattern}" "crates/${crate}/src" 2>/dev/null | strip_comments || true)

    if [ -n "${hits}" ]; then
        echo "LAYERING VIOLATION in ${crate}"
        echo "  ${why}"
        echo "${hits}" | sed 's/^/    /'
        echo
        fail=1
    fi
}

echo "checking layering contract..."
echo

# L1 is a general zstd implementation. It must not know its consumers.
check "zalt-codec" \
      '\b(ztml|zdoc|weft|zss|browser)\b' \
      "L1 must not name a content class or consumer. Widen a seam in L3 instead."

check "zalt-codec" \
      '\b(apple|darwin|macos|neon|aarch64)\b' \
      "L1 must stay architecture-neutral. Acceleration belongs behind the backend seam in zalt-neon."

# L1a is architecture-specific but not OS-specific, and still content-blind.
check "zalt-neon" \
      '\b(ztml|zdoc|weft|zss|browser)\b' \
      "L1a must not name a content class."

check "zalt-neon" \
      '\b(macos|mach_vm|dispatch_|objc)\b' \
      "aarch64 is not macOS. OS integration belongs in zalt-darwin."

# L2 knows about hostile input and memory, not about what is being decoded.
check "zalt-embed" \
      '\b(ztml|zdoc|weft|zss)\b' \
      "L2 must stay content-agnostic. Content tuning is data in L3."

# The FFI is a boundary, not a content layer.
check "zalt-ffi" \
      '\b(ztml|zdoc|weft|zss)\b' \
      "The C ABI must stay content-agnostic."

# L3 splits in two, and this is the half that matters most for the split to
# mean anything. `zalt-recipe` is the *mechanism*: manifests, routing,
# dictionary selection. A content type is a string that arrives in a manifest
# and leaves in a routing decision; this crate never spells one. Only
# `zalt-ztml`, the instance, may — and it is mostly data.
#
# If this fires, the fix is the same as everywhere else: widen a generic seam
# here so the instance can express what it needs as manifest data.
check "zalt-recipe" \
      '\b(ztml|zdoc|weft|zss|browser)\b' \
      "L3's mechanism must not name a content class. That is zalt-ztml's job, as data."

# The CLI drives L2 and knows about files and flags, not about content. When
# recipes arrive in M4 it gains `--recipe`, which is the *mechanism* and stays
# generic; the content class named inside a recipe is data it never spells.
check "zalt-cli" \
      '\b(ztml|zdoc|weft|zss)\b' \
      "The CLI selects recipes by path; it must not name a content class."

# unsafe is confined to the accelerated kernels.
if [ -d crates/zalt-codec/src ]; then
    if ! grep -q 'forbid(unsafe_code)' crates/zalt-codec/src/lib.rs; then
        echo "SAFETY VIOLATION in zalt-codec"
        echo "  lib.rs must keep #![forbid(unsafe_code)]. Every byte this crate"
        echo "  parses is attacker-controlled."
        echo
        fail=1
    fi
fi

# A binary crate's root is main.rs, a library's is lib.rs. Check whichever the
# crate actually has rather than assuming, or the check passes vacuously on a
# grep error for a file that was never going to exist.
for crate in zalt-embed zalt-cli zalt-recipe; do
    [ -d "crates/${crate}/src" ] || continue
    root=""
    for candidate in lib.rs main.rs; do
        [ -f "crates/${crate}/src/${candidate}" ] && root="crates/${crate}/src/${candidate}" && break
    done
    if [ -z "${root}" ]; then
        echo "SAFETY VIOLATION in ${crate}"
        echo "  no crate root (src/lib.rs or src/main.rs) to check."
        echo
        fail=1
    elif ! grep -q 'forbid(unsafe_code)' "${root}"; then
        echo "SAFETY VIOLATION in ${crate}"
        echo "  ${root} must keep #![forbid(unsafe_code)]."
        echo
        fail=1
    fi
done

# Only two crates may contain unsafe, and both have a reason that cannot be
# designed away:
#
#   zalt-neon  SIMD intrinsics take raw pointers, and a vectorised match copy is
#              inherently an aliasing read-and-write of one buffer.
#   zalt-ffi   a C ABI cannot exist without it.
#
# `forbid(unsafe_code)` already makes this a compile error where it is set; this
# check exists to catch the crate that quietly drops the attribute, and to cover
# crates added later before anyone remembers to add it.
#
# Adding a third name to this list should be an argument, not an edit.
for dir in crates/*/src; do
    crate=$(basename "$(dirname "${dir}")")
    case "${crate}" in zalt-neon | zalt-ffi) continue ;; esac
    unsafe_hits=$(grep -rnw 'unsafe' "${dir}" 2>/dev/null | strip_comments || true)
    if [ -n "${unsafe_hits}" ]; then
        echo "SAFETY VIOLATION in ${crate}"
        echo "  Only zalt-neon may contain unsafe. Found:"
        echo "${unsafe_hits}" | sed 's/^/    /'
        echo
        fail=1
    fi
done

if [ "${fail}" -eq 0 ]; then
    echo "layering contract holds."
fi

exit "${fail}"
