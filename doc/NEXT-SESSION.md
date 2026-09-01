# Resume here

Written 2026-09-01, at a clean stopping point. `just gate` passes: 290 tests,
clippy, fmt, layering, ABI, release-mode.

---

## 1. After the restart, before anything else

The point of restarting is to get a machine quiet enough to measure on. Check
that it worked:

```sh
sysctl vm.swapusage      # want: used ≈ 0
uptime                   # want: load < 2
```

Keep other editors and build tools closed while measuring. A second `cargo`
running elsewhere moved figures in this project by up to 58%, and swap pressure
moved them by about the same again — both larger than any single optimisation
recorded in `ZALT-MAC.md` §10.

---

## 2. Run the measurements

The corpora live in `corpus/`, which is gitignored but survives a reboot. They
should already be there; `just bench-corpus` rebuilds them if not.

```sh
just bench-corpus                 # idempotent; ~60 MB of text + 7 .zdoc
just measure corpus       9       # large-file steady state
just measure corpus/ztml  9       # the workload this decoder is actually for
```

`just measure` prints the load average and swap usage above the table and warns
if they are not clean. **If it warns, the numbers are lower bounds** — read them
as "at least this fast" and do not act on a difference under ~10%.

### The reference to compare against

```sh
zstd -3 -q -f corpus/varied.txt -o /tmp/varied.zst
time (for i in $(seq 1 10); do zstd -t -q /tmp/varied.zst; done)
```

Use `zstd -t`, **not** `zstd -d -c > /dev/null`. Test mode decodes and verifies
without writing, which is what the `sink` column does; piping to `/dev/null`
costs real `write` syscalls and understated the reference by 30% (948 vs
1400 MB/s).

### What the columns mean

| column | measures |
|---|---|
| `scalar` / `neon` | one-shot `decompress` — allocates a fresh output `Vec` per frame |
| `sink` | steady-state decode, reused decoder, zero-copy `Sink` |

`sink` is the codec. The other two include the allocator, and under memory
pressure they include the kernel.

---

## 3. Numbers to compare against

All taken on a **busy** machine (load 4–7, ~10 GB swap in use), so all are
floors. The interesting question is how much of the remaining gap is real.

| | MB/s |
|---|---:|
| `corpus` (61 MB code), sink | ~640 |
| `corpus/ztml` (3–12 KB documents), sink | ~406–434 |
| `zstd -t` on `corpus` | ~1400 |

Starting point at the beginning of M3 was 99 MB/s on a 35 MB corpus, so the
series is not a single controlled comparison — the per-change A/B results in
§10 are the trustworthy part.

**To compare two versions**, build both and alternate them. Two runs at
different times on this machine are not comparable; that method produced a
false 4% "regression" and a false 9% "improvement" before it was understood.

```sh
cp target/release/zalt-bench /tmp/bench-before
# ... change code ...
cargo build --release -p zalt-bench
for r in 1 2 3; do
  printf "  before=%s  after=%s\n" \
    "$(/tmp/bench-before        decode corpus/ztml 3 | grep ^SUMMARY | sed 's/.*sink=//')" \
    "$(./target/release/zalt-bench decode corpus/ztml 3 | grep ^SUMMARY | sed 's/.*sink=//')"
done
```

---

## 4. Where the project is

**M0, M1, M2 complete.** M1's last open item — a sustained fuzz campaign — is
done, and found two real validation bugs, both fixed and pinned as regression
tests. See §9 and §10.

**M3 in progress.** §5's ranked list of expected wins is exhausted: two of its
seven items paid, three were struck after measuring, and the changes that
actually moved the number appear nowhere on it. The safe-code work is close to
done; remaining items are 1–2% each.

Everything is recorded in `doc/ZALT-MAC.md` §10 under the M3 heading, including
the six changes that were **reverted for measuring worse** — read those before
re-attempting any of them:

- fusing sequence decode with execution (~4% slower)
- flattening the LL/ML code `match` into a lookup table (9% slower)
- match-source prefetch (3.5% slower)
- double-symbol Huffman (8.4% slower on real `.zdoc`)
- `extend_from_within` instead of the window's zero-fill (5% slower)
- hoisting the xxh64 accumulators (no change)

---

## 5. What we were about to discuss

**The unsafe seam.** This is the decision that determines whether parity with
libzstd is reachable, and it is deliberately unstarted.

- `zalt-codec` is `#![forbid(unsafe_code)]` today. Every slice access in the
  hot loop is a compare-and-branch, several per sequence.
- §6 puts that requirement on **"parsing and table-construction code"** and
  explicitly permits audited unsafe in kernels. So moving the FSE and Huffman
  *inner loops* behind the backend seam with unchecked indexing is arguably
  within the stated design rather than a breach of it.
- `zalt-neon` and `zalt-ffi` are already the two crates permitted unsafe, and
  `ci/layering.sh` enforces that list.

Nothing has been done toward this. The question to settle first is whether the
project's central security claim is worth the remaining performance, and that
is yours to answer, not mine.

**Second, and possibly ahead of it: M4.** The strongest finding of the last
session is that real `.zdoc` documents are 3–12 KB and spend ~18% of decode
time building entropy tables before touching a byte of content. That is the
per-frame cost §7 says dictionaries eliminate, because a dictionary carries the
tables pre-built. On this evidence M4 may buy more on the target workload than
any remaining M3 work — subject to §7's adoption caveat, which is real.

**Also worth revisiting:** §8's corpus table predicts `.zdoc` is
literal-dominated by hashes, signatures and UUIDs. In the seven real documents
those are **0.8% of the bytes**. That section should be re-derived from real
traffic before it drives a recipe.
