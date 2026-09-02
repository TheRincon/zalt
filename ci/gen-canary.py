#!/usr/bin/env python3
"""Generate the canary payload — a fixed input for the machine-state control.

The canary measures the *machine*, not the code, so the only property that
matters is that it is byte-identical every time it is generated. A canary whose
input drifts cannot distinguish "the laptop got slower" from "the input got
harder", which is the entire question it exists to answer.

Seeded, like `gen-text-fixture.py`, and for the same reason.

Shaped like markup with a bounded vocabulary so `zstd -3` emits *compressed*
blocks — four-stream Huffman literals and an FSE-coded sequences section —
rather than falling back to raw blocks. A canary that decoded raw blocks would
measure `memcpy` and little else, and would not move when the thing we care
about moves.

About 8 MB, which is large enough that one decode is milliseconds rather than
microseconds, and small enough to build and keep in `target/` without thinking
about it.
"""

import random
import sys

WORDS = [
    "section", "claim", "entity", "weft", "schema",
    "predicate", "object", "warrant", "substrate", "name",
    "reference", "document", "attribute", "container", "value",
]

LINES = 160_000


def main() -> int:
    random.seed(1729)
    out = []
    for i in range(LINES):
        out.append(
            '  <{0} ref="c_{1}" kind="{2}">{3}</{4}>'.format(
                random.choice(WORDS),
                i,
                random.choice(WORDS),
                random.choice(WORDS),
                random.choice(WORDS),
            )
        )
    sys.stdout.write("\n".join(out) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
