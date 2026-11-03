# Fuzzing and testing

## Fuzzing

`fuzz/` holds `cargo-fuzz` targets. It is its own workspace, excluded from the main one, and needs a
nightly toolchain:

```
cargo install cargo-fuzz
cd fuzz
cargo run --release --example seeds -- corpus/seeds [files or directories of captured databases]
cargo +nightly fuzz run open corpus/seeds/open -- -max_len=70000 -timeout=10 -malloc_limit_mb=1024
```

- `open` and `admin` take one mode byte and a file image. For `open` the mode picks the root type
  (one of six, or a type that decodes nothing so that any file opens), read-only or read-write, and
  whether to repair the image first, and for `admin` whether to repair and whether to compact.
  Repairing recomputes every object hash bottom-up, then the frame checks and the superblock
  checksums, so that a mutation gets past the hash checks to the code behind them. `open` then reads
  the value, the log, `at`, `as_of`, `history_of`, `bisect`, branches, `common_ancestor`, `verify`,
  `space` and `refresh`, and scans every map it reaches. `admin` runs `info`, `log`, `verify`,
  `space`, `layout` and, when its mode says so, `compact`.
- `log_entry` writes a log of crafted entries with valid hashes, mostly coherent and with fields
  perturbed by the input, plus branch heads that point into it, then runs the `open` reads over it
  and, for half the inputs, the `admin` functions.
- `map_node` writes crafted map nodes with valid hashes for six key, value and summary types,
  including a value with a `Table`, loads the map and reads, scans and edits it.

The harness treats a panic whose message starts with `ferro:` as expected inside the infallible
reads, which panic on corruption by design, and as a failure anywhere else. The `seeds` example
writes a corpus from databases it builds and from any files passed to it; files captured while
`cargo test --workspace` and the examples ran make a broader one.
`cargo run --example fixture -- <target> <crash> <out.db>` turns a crashing input into the file it
opens, which is how the fixtures behind `ferro/tests/fuzzed.rs` were made.

The `Makefile` wraps the last two steps: `make fuzz-seeds` builds `corpus/seeds`, and
`make fuzz FUZZ=open FUZZ_TIME=600` runs one target for that many seconds with the flags above.

## Where the other tests are described

Tests are described next to what they test:

- The epoch protocol's stress test, sanitizers and Miri: [Guarded gets and the epoch
  scheme](read-scaling.md#guarded-gets-and-the-epoch-scheme).
- `history_heap`: [Cache weight](read-scaling.md#cache-weight).
- Modelled power cuts on Buffered checkpoints: [Buffered durability and
  checkpoints](storage.md#buffered-durability-and-checkpoints).
- A frame that another branch moves, compared byte for byte: [Drafting outside
  `tip`](commit-path.md#drafting-outside-tip).
- Awaited writes, dropped futures and crash images: [Tests](async-writes.md#tests).
- Writes from inside a closure, `ferro/tests/reentry.rs` and `reentry_sync.rs`: [Writes from inside
  a closure](reentrancy.md#the-cases).
- Model tests of best common ancestors: [Criss-crosses](merging.md#criss-crosses).
- Power cuts during a receive placed in pieces: [Where checked objects
  wait](sync-receive.md#where-checked-objects-wait).
- The three-device sync model with small caches: [Commits stored
  twice](sync-receive.md#commits-stored-twice).
- Mutated bundles: [Checking trees](sync-receive.md#checking-trees).
- Flipped bytes in a bundle: [Applying](sync.md#applying).
- What runs on Windows: [Tests](windows.md#tests).
- Crash safety in general: [Durability and crash safety](../guide/durability.md#what-was-tested).
