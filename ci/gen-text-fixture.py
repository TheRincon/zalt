#!/usr/bin/env python3
"""Generate the compressible text fixture.

Repetitive tag-shaped markup with a bounded vocabulary, which is what makes
`zstd` emit a *compressed* block — four-stream Huffman literals plus an
FSE-coded sequences section — rather than falling back to raw blocks.

Seeded, so regenerating the fixture yields identical bytes. A fixture that
changes on every regeneration silently changes what the suite tests.
"""

import random
import sys

WORDS = [
    "section", "claim", "entity", "weft", "schema",
    "predicate", "object", "warrant", "substrate", "name",
]


def main() -> int:
    random.seed(42)
    lines = []
    for i in range(400):
        lines.append(
            '  <{0} ref="c_{1}">{2}</{3}>'.format(
                random.choice(WORDS), i, random.choice(WORDS), random.choice(WORDS)
            )
        )
    sys.stdout.write("\n".join(lines) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
