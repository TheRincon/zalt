# Benchmark corpus

Not tracked in git. Populate locally, then:

```sh
cargo run --release -p zalt-bench -- baseline corpus/
```

The harness takes a directory and has **no built-in opinion about content**.
Which files belong here is a recipe concern (L3), not a benchmark concern.

## Suggested layout

```
corpus/
├── zdoc-small/       1–16 KB documents. The dominant case; per-frame overhead rules.
├── zdoc-large/       Long documents; steady-state throughput.
├── zdoc-weft/        Many entity/claim blocks — template matching, dictionary material.
├── zdoc-refs/        Many hashes/UUIDs — worst-case literal entropy.
├── wasm/             ZAPP modules. Binary; LEB128 varints, opcode streams.
└── media/            WebP / PNG / AVIF / WOFF2.
```

## Two notes that change how results should be read

**Media is a throughput measurement, not a ratio one.** WebP, PNG, AVIF and
WOFF2 payloads are already compressed. zstd over them yields roughly 1.0x while
costing full decode time, and origins should serve them `identity`. They belong
here because incompressible input travels the raw-literal and raw-block paths,
where the target is near-`memcpy` — but a ratio column for these files means
nothing.

**Do not add HTML, CSS, or JavaScript.** None of them reflect what this browser
fetches, and optimising against them would tune the wrong paths. This is a
deliberate exclusion, not an oversight.

**Silesia and enwik are for comparability only — keep them out of `corpus/`.**
Silesia (12 files, ~211 MB, assembled 2003: tarred Mozilla binaries, a medical
MRI, a star catalogue, the 1913 Webster dictionary) is the de facto yardstick
for compression benchmarks — zstd's own published numbers are Silesia numbers —
so it is the data anyone will ask for if zalt ever claims to beat zstd. But it
is a 2003 desktop/server workload in 5–50 MB files, and tuning against it would
optimise large-file steady-state throughput when the real bottleneck here is
per-frame table construction on kilobyte resources.

Keep it in a separate directory and run it deliberately, as a sanity check
(pathological behaviour on general data usually means a bug) and as the number
for external comparison. Never as a tuning target.

## Recording results

A ratio without its corpus is not a measurement. When recording baseline
numbers, record the corpus revision alongside them.
