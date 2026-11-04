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

- `open` and `admin` take one mode byte and a file image. For `open` the mode picks the root type,
  read-only or read-write, whether to repair the image first, and what runs after the reads, and
  for `admin` whether to repair and whether to compact. The root types are six maps and a shop,
  three versions of a catalog whose table has covered, unique, each and composite indexes under
  changed cover lists, kinds, stored names and defaults, so that a file written under one opens
  under another and rebuilds its indexes, a type with sets, lists, tables and summaries that the
  `sync` target uses too, and a type that decodes nothing so that any file opens. Repairing
  recomputes every object hash bottom-up, then the frame checks and the superblock checksums, so
  that a mutation gets past the hash checks to the code behind them. `open` then reads the value,
  the log, `at`, `as_of`, `history_of`, `bisect`, branches, `common_ancestor`, `verify`,
  `verify_typed`, `space` and `refresh`, and scans every map and index it reaches. Read-write, it
  can commit on main and another branch, write from inside a closure, fork, merge and drop a
  branch, and run a cycle that keeps two versions. Read-only, it can follow a writer on the same
  file through 70 more commits, past a full branch table and a cycle, refreshing on the way. Either
  can bundle the file for an empty one and receive the bundle there, and a read-write handle then
  pulls a commit back from it. `admin` runs `info`, `log`, `verify`, `space`, `layout` and, when its
  mode says so, `compact`.
- `log_entry` writes a log of crafted entries with valid hashes, mostly coherent and with fields
  perturbed by the input, plus a branch table that points into it and up to 75 branch-table steps
  over it, whose links, names, tags and heads the input perturbs. It then runs the `open` reads
  over it and, for half the inputs, the `admin` functions. Half the time a follower opens it
  read-only before more steps are written, and refreshes through them.
- `refs` forges branch tables. Four honest files are built once per binary: three branches whose
  140 commits cross the 64-step table twice, 130 branches whose table spans several map nodes, a
  chain exactly 64 steps deep so that the next commit writes a full table, and two branches that
  merged each other, dropped a third and ran a cycle. The input picks one and appends steps whose
  tag, name, link, head and trailing bytes it perturbs, runs of 62 to 1,025 honest steps, tables of
  0, 126, 127, 128 or 300 branches, raw objects, and steps over an older link of the chain, in one
  frame or several. A follower opened before the forgery refreshes across it. A handle then opens
  the file, reads it as `open` does and can commit on its branches, past a full table when the
  input asks for 70 commits, fork, merge, drop a branch and run a cycle, before the follower
  refreshes again and, for some inputs, the `admin` functions run.
- `map_node` writes crafted map nodes with valid hashes for six key, value and summary types,
  including a value with a `Table`, loads the map and reads, scans and edits it.
- `sync` receives a bundle into a copy of a database through `receive_from`. Nine honest pairs of a
  database and a bundle are built once per binary and cached beside the scratch files: hints into
  held trees, a clone into an empty file, merges on both sides, edited covered indexes, a 1.2 MB
  bundle that spills to the scratch file and is placed in pieces, and four taken from devices that
  pull from each other's branches with pruned history and cycles, so that a bundle can carry
  commits the receiver already holds. The input picks one, then mutates its objects, links, header
  and count as parsed values and rehashes them up to the head, or edits the encoded bytes, or
  replaces the bundle outright. It can feed the stream in small reads, interrupt it, break it, or
  commit, fork or run a cycle while the checked bundle waits to be placed, or commit or fork
  between placing a spilled bundle's pieces and landing it. A refused bundle must leave both
  branches where they were. An accepted one must read back whole on both branches, with every map's
  length equal to its iteration, every key found and every index lookup answered, and pass
  `verify` and `verify_typed`, before and after a reopen and a commit.

The harness treats a panic whose message starts with `ferro:` as expected inside the infallible
reads, which panic on corruption by design, and as a failure anywhere else. The `seeds` example
writes a corpus from databases it builds and from any files passed to it. The databases it builds
include catalogs written under each schema in turn, branches written by concurrent lanes with and
without coalescing, devices that relay through pulls and cycles, branch tables of 40, 126, 127 and
200 branches, step chains just below and past multiples of 64, twenty branches whose commits
rewrite covered index entries across two full tables, crash images cut from the recorded writes of
concurrent commits with every kind of loss, a frame of objects nothing names, and a file caught
between placing a bundle's pieces and landing it. Frames that land out of order come from a hook
that holds one commit's frame before its write until three later frames have landed: the images
leave a hole where a held frame belongs with every later frame written, or stop just after a later
frame landed ahead of the held one. Each `refs` seed pairs one of its bases with a forgery and a
mode. Files captured from the tests make a
broader corpus: the one used here kept every database file `cargo test --workspace` deleted, copied
aside by a small `LD_PRELOAD` wrapper around `unlink`, `unlinkat` and `rename`. Images over 69,000
bytes go to `open-large`, for a run of `open` with a larger `-max_len`.
`cargo run --example fixture -- <target> <crash> <out.db>` turns a crashing input into the file it
opens, for `refs` the forged file before any handle opens it, and for `sync` also writes the bundle
beside it, which is how the fixtures behind
`ferro/tests/fuzzed.rs` were made.

The `Makefile` wraps the last two steps: `make fuzz-seeds` builds `corpus/seeds`, and
`make fuzz FUZZ=open FUZZ_TIME=600` runs one target for that many seconds with the flags above.

## Where the other tests are described

Tests are described next to what they test:

- The epoch protocol's stress test, sanitizers and Miri, with the commands: [How the protocol is
  checked](read-scaling.md#how-the-protocol-is-checked).
- `history_heap`: [Cache weight](read-scaling.md#cache-weight).
- Modelled power cuts on Buffered checkpoints: [Buffered durability and
  checkpoints](storage.md#buffered-durability-and-checkpoints).
- A frame that another branch moves, compared byte for byte: [Drafting outside
  `tip`](commit-path.md#drafting-outside-tip).
- Power cuts across branch-table steps, a reader from before steps, and a file written before them:
  [Branch-table steps](commit-path.md#branch-table-steps).
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
