#!/usr/bin/env python3
"""Report the source that no test ever runs, file by file.

`cargo llvm-cov`'s summary is per file, and a file at 85% can still have an
entire mode nobody exercises — which is exactly the failure §10 records: the
whole fixture set used `FSE_Compressed` mode, so the *predefined* FSE tables
were untouched by any test, the `ML_DEFAULT` bug lived in that gap, and every
internal consistency check passed. This prints the gap.

# Why this reads `files`, not `functions`

The obvious implementation walks the report's per-function records. It is wrong
here, and wrong in a way that looks plausible: llvm-cov emits one record per
*instantiation*, `zalt-codec` is generic over the backend, and the
instantiations a given test binary never calls are recorded with zero hits. That
version put `sequences::decode` — the hottest function in the decoder, 93%
covered per the file summary — at the top of the list of uncovered code. The
records cannot be merged by name either, because each instantiation carries a
different mangled name.

`files[].segments` are already merged across instantiations by llvm-cov itself,
which is the same view its own summary reports. Use the tool's merge rather than
reimplementing it badly.
"""

import json
import sys
from pathlib import Path


def uncovered_ranges(segments):
    """Line ranges whose regions were entered zero times."""
    dead = set()
    for line, _col, count, has_count, is_region_entry, is_gap in segments:
        if is_gap or not has_count or not is_region_entry:
            continue
        if count == 0:
            dead.add(line)
    if not dead:
        return []
    ordered = sorted(dead)
    ranges = [[ordered[0], ordered[0]]]
    for line in ordered[1:]:
        # Fold near-adjacent lines together; a one-line hole between two dead
        # regions is noise, not a separate finding.
        if line - ranges[-1][1] <= 2:
            ranges[-1][1] = line
        else:
            ranges.append([line, line])
    return ranges


def main() -> int:
    if len(sys.argv) < 2:
        sys.stderr.write("usage: coverage-gaps.py <cov.json> [max-per-file]\n")
        return 2
    per_file = int(sys.argv[2]) if len(sys.argv) > 2 else 8

    data = json.load(open(sys.argv[1]))

    findings = []
    for export in data.get("data", []):
        for entry in export.get("files", []):
            path = entry.get("filename", "")
            if "/crates/zalt-" not in path or "/tests/" in path:
                continue
            # The benchmark harness is a development tool, not shipped code, and
            # at ~5% covered it otherwise dominates this report with findings
            # nobody intends to act on. Everything else, including the CLI, is
            # something a user can run.
            if "/crates/zalt-bench/" in path:
                continue
            ranges = uncovered_ranges(entry.get("segments", []))
            if not ranges:
                continue
            lines = entry.get("summary", {}).get("lines", {})
            findings.append((lines.get("percent", 100.0), path, ranges))

    if not findings:
        print("no uncovered regions in zalt sources")
        return 0

    findings.sort(key=lambda f: f[0])

    print(f"{'cover':>6}  file")
    print("-" * 74)
    for pct, path, ranges in findings:
        short = path.split("/crates/")[-1]
        total_dead = sum(b - a + 1 for a, b in ranges)
        print(f"{pct:5.1f}%  {short}   ({total_dead} lines in {len(ranges)} regions)")
        src = []
        p = Path(path)
        if p.is_file():
            src = p.read_text(errors="ignore").splitlines()
        for a, b in ranges[:per_file]:
            text = src[a - 1].strip()[:58] if 0 < a <= len(src) else ""
            span = f"{a}" if a == b else f"{a}-{b}"
            print(f"           {span:>10}  {text}")
        if len(ranges) > per_file:
            print(f"           ... {len(ranges) - per_file} more regions")
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
