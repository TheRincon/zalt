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

# zalt-ztml is the one crate allowed to name a content class, so there is no
# vocabulary check for it. What is checked instead is that it stays DATA.
#
# §3: "zalt-ztml is mostly a dictionary and a manifest. If it accumulates
# meaningful amounts of Rust, that is a signal the seam in zalt-recipe is too
# narrow." That is a design property stated in prose, and prose does not fail a
# build. Counting is crude, but the thing being guarded against is gradual — a
# helper here, a special case there — and the crate is 63 code lines today, so
# a limit of 200 is several years of honest growth and still catches a content
# class quietly acquiring behaviour.
#
# If this fires, widening a seam in zalt-recipe is the fix. Raising the number
# should be an argument, not an edit.
if [ -d crates/zalt-ztml/src ]; then
    ztml_code=$(cat crates/zalt-ztml/src/*.rs 2>/dev/null | grep -vcE '^[[:space:]]*(//|$)' || echo 0)
    if [ "${ztml_code}" -gt 200 ]; then
        echo "LAYERING VIOLATION in zalt-ztml"
        echo "  ${ztml_code} lines of Rust (limit 200). L3's instance should be"
        echo "  a manifest and a dictionary. Logic here means the seam in"
        echo "  zalt-recipe is too narrow — widen it generically instead."
        echo
        fail=1
    fi
fi

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
for crate in zalt-embed zalt-cli zalt-recipe zalt-ztml zalt-recipe-train zalt-archive; do
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
# And one test-only crate, with its argument:
#
#   zlta-seekable-probe
#              the oracle for ZALT-archives. Its whole job is to run libzstd's
#              *reference* seekable reader and frame inspector over zalt's
#              archives, and those exist only as C (`contrib/seekable_format`),
#              reached through `zstd-sys`. A safe re-implementation would not be
#              the reference, which is the point of an oracle. It ships in
#              nothing: the check below fails the build if any crate takes it as
#              anything but a dev-dependency.
#
# `forbid(unsafe_code)` already makes this a compile error where it is set; this
# check exists to catch the crate that quietly drops the attribute, and to cover
# crates added later before anyone remembers to add it.
#
# Adding a third name to this list should be an argument, not an edit.
for dir in crates/*/src; do
    crate=$(basename "$(dirname "${dir}")")
    case "${crate}" in zalt-neon | zalt-ffi | zlta-seekable-probe) continue ;; esac
    unsafe_hits=$(grep -rnw 'unsafe' "${dir}" 2>/dev/null | strip_comments || true)
    if [ -n "${unsafe_hits}" ]; then
        echo "SAFETY VIOLATION in ${crate}"
        echo "  Only zalt-neon may contain unsafe. Found:"
        echo "${unsafe_hits}" | sed 's/^/    /'
        echo
        fail=1
    fi
done

# The probe's exemption above holds only while it stays test-only: it may be a
# dev-dependency, never a dependency of anything that builds into a product.
for manifest in crates/*/Cargo.toml; do
    hits=$(awk '
        /^\[/ { section = $0 }
        /zlta-seekable-probe/ && section !~ /dev-dependencies/ && section !~ /^\[package\]/ {
            print FILENAME ": " section ": " $0
        }' "${manifest}")
    if [ -n "${hits}" ]; then
        echo "SAFETY VIOLATION: zlta-seekable-probe outside dev-dependencies"
        echo "  It contains unsafe FFI and is exempt only as a test oracle."
        echo "${hits}" | sed 's/^/    /'
        echo
        fail=1
    fi
done

if [ "${fail}" -eq 0 ]; then
    echo "layering contract holds."
fi

exit "${fail}"
