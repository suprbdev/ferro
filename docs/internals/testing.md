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
  `verify_typed`, `space` and `refresh`, and scans every map and index it reaches, unique indexes
  through their ranges and a composite one's prefixes too, from both ends. Read-write, it
  can commit on main and another branch, write from inside a closure, fork, merge and drop a
  branch, and run a cycle that keeps two versions. Read-only, it can follow a writer on the same
  file through 70 more commits, past a full branch table and a cycle, refreshing on the way. Either
  can bundle the file for an empty one and receive the bundle there, and a read-write handle then
  pulls a commit back from it. `admin` runs `info`, `log`, `verify`, `space`, `layout` and, when its
  mode says so, `compact`. One root type of `open`'s mode byte instead takes the rest of the input
  as a recipe: it writes a valid file of one to five commits of up to four values of up to 14,000
  bytes each, Buffered or Fsync, puts back the superblocks the file had after an earlier commit,
  and damages one frame past their anchor, by flipping a bit anywhere in it or one in each of some
  of its 4 KiB blocks and its header and table, or by filling a sector, or a run of every other
  sector, with zeroes, ones, an older frame's bytes or input bytes, and sometimes leaves a
  reservation of zeroes after the log. The high half of the recipe's first byte can make the values
  sparse instead, one or two to a commit, each zeroes with a few bits set, under 96 bytes so that
  it stays raw, up to 64 KiB so that it compresses to runs of ones, or 400 KB of noise that
  compression does not shrink around a KiB of zeroes with one bit set; or it can start the file
  with a pull of three values of such noise from another file, which arrives in pieces that spill.
  The mode then compares the damaged frame with the whole one: if each block and the header and
  table together differ in at most one bit, at most 8 blocks differ, the entry of no block differs
  along with its block, and every bit that differs sits in a sector that holds at least 8 of the
  frame's bytes and has something other than all zeroes or all ones on either side of it, both a
  read-only open and a writer's open must fail with `Corrupt` naming the frame and its first wrong
  byte and leave the file as it was. Otherwise both must open at the version the frames before it
  hold, which a pull's pieces leave where it was, the writer truncating the file at the frame,
  `verify` must pass, and a commit must survive a reopen.
- `reclaim` takes the input `admin` takes, opens the file as the type that decodes nothing and
  compacts it with `copy_threads(1)` and again, from the same image, with `copy_threads(4)`, keeping
  every version, the newest one or three, or those since time 0, and for some modes compacts the
  result again with the thread counts the other way round. Both copies must fail with the same
  error, the file's name aside, or succeed with the same report and the same bytes, and when
  `Db::verify` passed on the file as opened, the compacted file must pass `admin::verify`.
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
  frame or several. A follower opened before the forgery refreshes after each frame and once it is
  whole, so that loading the table starts from forged steps it already knows. A handle then opens
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
  commits the receiver already holds. Three of its maps keep a summary: a wrapping sum over small
  values, one over spilled blobs, some of them one value under several keys, whose `of` reads the
  key, and one over a map of maps, one of them four levels deep, whose `of` reads each inner map.
  The input picks one, then mutates its objects, links, header and count as parsed values, or
  finds the summaries in the nodes of those maps and changes one, or two of a node by amounts that
  cancel so that the node's own fold still holds, and rehashes them up to the head, or edits the
  encoded bytes, or replaces the bundle outright. It can feed the stream in small reads, interrupt
  it, break it, or commit, fork or run a cycle while the checked bundle waits to be placed, or
  commit or fork between placing a spilled bundle's pieces and landing it. A refused bundle must
  leave both branches where they were and the receiving branch readable whole. An accepted one must
  read back whole on both branches, with every map's length equal to its iteration, every key
  found, every index lookup answered, each unique index's range over the whole index as long as its
  map from either end, and each summarized map's `summary`, and the two halves of it that
  `range_summary` folds, equal to the fold of its entries,
  and pass `verify` and `verify_typed`, before and after a reopen and a commit.

The harness treats a panic whose message starts with `ferro:` as expected inside the infallible
reads, which panic on corruption by design, and as a failure anywhere else. The `seeds` example
writes a corpus from databases it builds and from any files passed to it. The databases it builds
include catalogs written under each schema in turn, branches written by concurrent lanes with and
without coalescing, devices that relay through pulls and cycles, branch tables of 40, 126, 127 and
200 branches, step chains just below and past multiples of 64, twenty branches whose commits rewrite
covered index entries across two full tables, crash images cut from the recorded writes of
concurrent commits with every kind of loss, a frame of objects nothing names, and a file caught
between placing a bundle's pieces and landing it. Frames that land out of order come from a hook
that holds one commit's frame before its write until three later frames have landed: the images
leave a hole where a held frame belongs with every later frame written, or stop just after a later
frame landed ahead of the held one. Each `refs` seed pairs one of its bases with a forgery and a
mode. Every image small enough for `admin` goes to `reclaim` too, under three modes. Files captured
from the tests make a broader corpus: the one used here kept every database file `cargo test
--workspace` deleted, copied aside by a small `LD_PRELOAD` wrapper around `unlink`, `unlinkat` and
`rename`. Images over 69,000 bytes go to `open-large`, for a run of `open` with a larger `-max_len`.
`cargo run --example fixture -- <target> <crash> <out.db>` turns a crashing input into the file it
opens, for `reclaim` the one it compacts, for `refs` the forged file before any handle opens it, and
for `sync` also writes the bundle beside it, which is how the fixtures behind
`ferro/tests/fuzzed.rs` were made.

The `Makefile` wraps the last two steps: `make fuzz-seeds` builds `corpus/seeds`, and
`make fuzz FUZZ=open FUZZ_TIME=600` runs one target for that many seconds with the flags above.
`make fuzz-lint` checks the fuzz crate with rustfmt and clippy on stable, and `make ci` runs it.

The round that added `refs`, with the seeds for covered steps, 200 branches and frames that land
out of order, ran each target on one core under the flags above and `-rss_limit_mb=2048`, starting
from the corpus the round before saved: `open` for two hours, 370,000 executions reaching 95,000
edges, and an hour more with `-use_value_profile=1`; `open-large` with `-max_len=400000` for 45
minutes, 76,000 executions; `admin`, `log_entry` and `map_node` for an hour each, 1.6 to 1.7
million executions reaching 6,200, 10,300 and 17,800 edges, with an hour more of `log_entry` under
value profiles; `sync` for two hours and 40 minutes, 99,000 executions reaching 31,800 edges;
and `refs` for three hours and 40 minutes over five runs, 280,000 executions reaching 12,800 edges.
No input crashed, hung, leaked or passed a memory limit, so the library needed no fix. The round's
one fault was in the harness: the `refs` base meant to sit a whole step table deep was a step
short, which walking the chain of a file written by `fixture` showed.

The round after it covered what had changed since: receives staged in dense columns with type
checks kept for objects linked more than once and hints resolved a parent at a time, the copy that
writes each body as stored and checks it on other threads with a bounded read-ahead, the stored
length checked on every read, unique index ranges and prefixes, and iterators that borrow nodes
under hazards. It added `reclaim` and the unique range probes, rebuilt the seeds, and ran each
target on one core of six under the flags above and `-rss_limit_mb=2048`, starting from the corpus
the round before saved. `admin`, `log_entry`, `map_node` and `refs` ran for 50 minutes each: 1.54
million, 1.27 million, 1.16 million and 50,000 executions reaching 6,700, 11,300, 20,200 and 13,600
edges. `open` replayed its 15,000 inputs in 15 minutes, 16,000 executions reaching 109,000 edges,
until libFuzzer passed the RSS limit holding that corpus and reading it back, and then ran 25 and
15 minutes from random samples of 1,000 inputs, 84,000 executions more reaching 93,000 and 96,000
edges. `sync` ran 25 minutes over its whole corpus, 14,400 executions reaching 37,500 edges, and 25
from a sample, 13,300 more. `reclaim` ran 50 minutes on each of two cores, one from the `admin`
corpus and the seeds and one from 1,000 `open` inputs, 1.27 million and 970,000 executions reaching
8,500 and 8,300 edges. Merging added 303 inputs to the saved `open` corpus, 93 to `sync`, 221 to
`log_entry`, 579 to `map_node`, 83 to `admin`, 110 to `refs`, and started `reclaim` with 2,381.

Only `reclaim` failed, twice in the library and three times in its own checks. The copy took every
link at offset 0 for a tombstone and wrote it back unread, so a file whose branch table named log
entries as plain trees, one with null parent links, compacted into a file `verify` refused as it
had refused the source. And it wrote a link whose hash was a pruned entry's as a tombstone wherever
it sat, so a tree that linked a log entry by hash came out of `Keep::Last(1)` with a null link. Both
are now tombstones only among an entry's `prev`, skip and merge links, and elsewhere fail the cycle,
as [the copy's tombstones](reclamation.md#a-cycle) describe, with fixtures in
`ferro/tests/fuzzed.rs`. The harness at first compared errors that named the two copies' files, and
asked every compacted file to verify. A chain whose skip link pointed outside the log, which the
copy places by following `prev` and cut, compacted into a file `verify` refused as it had refused
the source, so the target asks for a verifying result only from a file that passes `verify`. A table
node with keys out of order, which failed only `verify_typed`, linked a second record of an entry no
head named, which the copy wrote as an entry with its `prev` link cut and `verify` read as a tree.
That was the copy's fault, since it took any record with an entry's hash for an entry. It now copies
a record as an entry only when it reached it as one, as [Records reached as
entries](reclamation.md#records-reached-as-entries) describes, with tests in
`ferro/tests/fuzzed.rs`, so the target asks every file that passes `verify` to compact into one that
does, whatever `verify_typed` says. No other input crashed, hung or passed a memory limit.

With that change and that check, `reclaim` ran for 30 minutes as four jobs on four cores, from the
2,381 inputs the round saved, on a machine other work kept at a load average of about 100 on 32
cores: 390,000 executions, 53 a second on each core, reaching 7,980 to 7,995 edges and adding 511
inputs. No compaction of a file that passed `verify` failed it, and no input crashed, hung or passed
a memory limit.

A later `reclaim` round, run for #89, found a file that passed `verify` and compacted under
`Keep::All` into one that failed it with `commit log keeps versions from 1 but v50 links pruned
history` (#90). v50's skip link named v50's own record under a hash of zeroes. `verify` and the
copy checked only that a kept skip link's offset was not 0, the walk of roots dropped the link as
an entry it had seen by its offset, and the copy took the unknown hash for pruned history and wrote
the link as a tombstone. A background cycle that kept the first version did the same. The walk of
a chain now checks each kept skip link's hash against the entry it reaches at that version, so
`verify`, `verify_typed`, a cycle and `compact` all refuse such a file with `v50 skips to another
entry than v48` and leave it as it is. `ferro/tests/fuzzed.rs` keeps the input, and builds the two
shapes it stands for: a link with the right offset and a wrong hash, and one that names an intact
record of another v4, which reads had answered with the wrong commit.

The round after the frame table, format 4, with the judgement of [Torn and damaged
frames](storage.md#torn-and-damaged-frames), rebuilt the seeds, 656 of them for `open`, and
rewrote a copy of the 15,457 `open` inputs saved before into format 4, as the fixtures were, so
that they open in the new format. `open` then ran for 30 minutes as four jobs, one on each of four
cores, each from the seeds and a quarter of those inputs under the flags above and
`-rss_limit_mb=3072`, while a build and a test run shared the machine for part of it: 437,619
executions in all, 59 to 63 a second on each core, starting at
101,900 to 103,300 edges and ending at 103,000 to 104,600, and adding 6,973 inputs. No input
crashed, hung, leaked or passed a memory limit, and the peak RSS was 1.35 GB.

The round after it added the damage mode of `open` described above, rebuilt the seeds, 720 for
`open` with 48 of that mode, among them frames of up to 11 blocks and up to 65 for `open-large`, and
ran each job on one core of six under the flags above and `-rss_limit_mb=2048`, from a copy of the
corpus the round before saved, converted to format 4. `open`, built before the damage mode, ran 30
minutes over its 22,426 saved inputs and the seeds, which loaded in 23,000 executions to 10,148
inputs reaching 109,300 edges: 69,800 executions, 38 a second, ending at 109,600 edges and adding
178 inputs, with a peak RSS of 1.47 GB. With the damage mode it ran 30 minutes from a random sample
of 1,000 saved inputs and the seeds, 57,000 executions, 31 a second, from 100,100 edges to 102,200,
adding 1,951 inputs, and 25 minutes from the 48 seeds of that mode and 300 random inputs of it
alone, 11,500 executions, 7 a second, each building a file and searching its damaged blocks under
the address sanitizer, from 5,400 edges to 6,700, adding 664. `open-large` ran 30 minutes from a
sample of 1,000 of its 5,323 inputs and its seeds, 39,600 executions from 50,700 edges to 52,000,
adding 1,761. `admin` and `reclaim` ran 30 minutes each, 723,000 and 673,000 executions, 400 and 373
a second, from 6,800 and 8,100 edges to 7,208 and 8,483, adding 1,151 and 586, and `sync` 20
minutes, 16,100 executions from 37,700 edges to 37,800, adding 258. Merging added 269 inputs to the
saved `admin` corpus, 214 to `reclaim`, 44 to `sync`, 671 to `open-large` and 632 to `open`.

No input crashed, hung, leaked or passed a memory limit, so the library needed no fix, and no
damaged frame met a verdict other than the damage mode's. Writing that mode's oracle from the rule
showed one case the rule as stated and the judgement disagree on: a bit flipped in a block and one
in that block's entry in the table is one bit in the header and table and one in a block, but the
judgement finds a flipped entry only as one a bit from its block's hash, so it reads the frame as
torn and truncates it. Searching each failing block for a hash a bit from its entry would cost a
block search for every block of a torn frame whose table never landed, so the rule stays, [Torn
and damaged frames](storage.md#torn-and-damaged-frames) and the limitations name the case,
`crash.rs` checks that such a frame is truncated, and the damage mode expects it to be.

The round after the rule on zeroes and ones and the cap of [Torn and damaged
frames](storage.md#torn-and-damaged-frames), with the damage mode's sparse values and pulls, ran
`open` for 20 minutes as four jobs, one on each of four cores, each from a quarter of a copy of
the 23,058 saved `open` inputs, 327 of them of the damage mode, and the 64 seeds of that mode,
under the flags above and `-rss_limit_mb=3072`. Each loaded its 5,830 or so inputs in about 5,835
executions, reaching 103,900 to 106,200 edges, and ran 41,446 to 42,873 executions in all, 34 or 35
a second, 168,693 together, ending at 105,500 to 106,600 edges and adding 638 to 665 inputs, 2,593
together, 508 of those left of the damage mode, 109 of them sparse values and 76 pulls. No input
crashed, hung, leaked or passed a memory limit, the peak RSS was 1.56 GB, and no damaged frame met
a verdict other than the damage mode's.

The round after the rule that nothing at or below the newest superblock is truncated, described
under [What a superblock names](storage.md#what-a-superblock-names), ran `open` for 10 minutes as
four jobs, one on each of four cores, each from a quarter of a copy of the 25,628 saved `open`
inputs, 895 of them of the damage mode, under the flags above and `-rss_limit_mb=3072`, while the
workspace's tests ran on the other cores. Each loaded its 6,410 or so inputs in about 6,415
executions, reaching 106,400 to 107,000 edges, and ran 11,504 to 13,385 executions in all, 19 to 22
a second, 49,719 together, ending at 106,400 to 107,100 edges and adding 350 inputs together, 108 of
them of the damage mode. No input crashed, hung, leaked or passed a memory limit, the peak RSS was
1.56 GB, and no damaged frame met a verdict other than the damage mode's. The target opens files
with no other writer alive, which the change leaves as it was, so the round checks that it did.

The round for [Recomputing summaries](sync-receive.md#recomputing-summaries) first ran the new
`sync` harness, with its summarized blobs, its forged summaries and its check of every fold,
against the library from before the recompute. It failed within the first 40 executions on a
bundle that landed with a summary that was not the fold of its entries, and the same input is
refused or reads back whole with the recompute. `sync` then ran for 10 minutes as four jobs, one
on each of four cores, each from a quarter of a copy of the 3,848 saved `sync` inputs under the
flags above and `-rss_limit_mb=3072`. Each loaded its 962 inputs to 630 to 674 entries reaching
35,700 to 36,000 edges, and ran 6,476 to 7,104 executions, 10 or 11 a second, 27,296 together,
ending at 36,200 to 36,400 edges with 1,308 to 1,348 entries, about 2,680 more together. No input
crashed, hung, leaked or passed a memory limit, and the peak RSS was 954 MB.

The round after summaries that read a collection in the value were deferred to a second pass over
the receiving store and the staged objects, with the world's map of maps summarized by an `of`
that reads each inner map, so that every bundle defers its outer nodes, ran `sync` the same way
for 10 minutes from the same quarters. Each loaded its 962 inputs to 633 to 658 entries reaching
36,500 to 36,900 edges, and ran 6,625 to 7,535 executions, 11 or 12 a second, 28,271 together,
ending at 37,000 to 37,200 edges with 1,329 to 1,389 entries. No input crashed, hung, leaked or
passed a memory limit, and the peak RSS was 954 MB.

That round could not see the bug a review then found, a staged node pinned into a held one that the
receiver's own deep reads tripped over, since its inner maps were too shallow for a pinned node to
have links of its own and it read nothing back after a refused bundle. The round after the deferred
pass moved to a view of the file with no cache gave one inner map 5,000 entries, four levels, and
reads the receiver's world whole after a refused bundle as after an accepted one. It ran `sync`
the same way for 10 minutes from the same quarters: each loaded its 962 inputs to 646 to 674
entries reaching 36,800 to 37,100 edges, and ran 4,536 to 5,743 executions, 7 to 9 a second, 20,508
together, ending at 37,100 to 37,400 edges with 1,222 to 1,306 entries. No input crashed, hung,
leaked or passed a memory limit, and the peak RSS was 983 MB.

The round after the receive made one view for its hint path and its deferred pass, read each held
record once for free, kept a memo of entry summaries for bundles of several commits and lent the
staging to every thread, as described under [Recomputing
summaries](sync-receive.md#recomputing-summaries), ran `sync` the same way for 10 minutes from the
same quarters. Each loaded its 962 inputs to 654 to 669 entries reaching 37,000 to 37,300 edges, and
ran 4,550 to 4,882 executions, 7 or 8 a second, 19,072 together, ending at 37,300 to 37,700 edges
with 1,178 to 1,220 entries, 2,314 more together. No input crashed, hung, leaked or passed a memory
limit, and the peak RSS was 982 MB.

## Flipped bytes in the log

`flipped_log_bytes_never_read_back_as_wrong_data` in `ferro/tests/crash.rs` builds a file of 40
commits and flips one byte of its log 4,000 times, from a fixed seed, with a single bit half the
time and a random mask otherwise. A flip inside a record that `Store::walk` reaches must fail the
open, a read of every version or `verify`, and one outside every such record must pass all three;
none may read back wrong data. Each commit's entry holds its time, and the hashes of everything
that links an entry follow from it, so the bytes at one position differ from run to run. The build
takes its times from a counter that starts at the wall clock, or at `FERRO_TEST_CLOCK` when that is
set, and a failure names the start: `FERRO_TEST_CLOCK=<start> cargo test --release -p ferro --test
crash flipped` builds the same file and makes the same flips.

It failed once in a `make ci` run with `byte 42251 mask 0x80 live true: reachable damage went
unnoticed` (#91). With the counter in place, flipping the high bit of every byte of every reachable
record, in files built from 240 starts, found 44 flips that nothing noticed, each in the last byte
of a record followed by a zero byte: a varint read as the same number in a longer form, as
described under [Records](storage.md#records). Run from 1,500 starts, the test failed at 5, each on
byte 42,251 with mask 0x80, the failure reported. The other seven single bits of every reachable
byte, from 96 starts, were all noticed. Every mask of every reachable byte, 12.4 million flips a
file, stopped partway through files from 12 starts, had found a second hole, the same four flips of
two to four bits in each: they moved a replaced head's link to its root onto the entry it links as
its tip, which `verify` took as walked, as described under [Branch-table
steps](commit-path.md#branch-table-steps). `a_record_read_on_into_the_byte_after_it_is_reported`
and `a_link_redirected_to_a_sibling_is_reported_by_verify` build each case directly, and failed
before the changes. After them, every mask of every reachable byte, in files from 20 starts, was
noticed as far as the run went, more than half of each file's flips before it was stopped for the
420 MB/s it wrote to the disk.

The test's long values are random letters, which LZ4 does not shorten, so its file holds no
compressed body. `flipped_bytes_of_compressed_bodies_never_read_back_as_wrong_data` makes the same
flips in a file whose long values repeat four short words, where 82 of the 295 reachable records
are compressed, and lets a flip inside a compressed body pass if every read returns the right data,
since it left the body as it was, as described under [Records](storage.md#records). Every
single-bit flip of every reachable byte, in files from 12 starts, found 848 such flips in each and
none elsewhere: 328 in the low four bits of the last token of each of the 82 bodies, and 520 in
match offsets that named another copy of the same bytes. Every mask, in files from 4 starts, 8.4
million flips a file, found 17,649 such flips in each, again none outside a compressed body, and no
wrong data.

## Where the other tests are described

Tests are described next to what they test:

- The epoch protocol's stress test, the unit tests run in parallel copies by `make test-stress`,
  sanitizers and Miri, with the commands: [How the protocol is
  checked](read-scaling.md#how-the-protocol-is-checked).
- `history_heap`: [Cache weight](read-scaling.md#cache-weight).
- Modelled power cuts on Buffered checkpoints: [Buffered durability and
  checkpoints](storage.md#buffered-durability-and-checkpoints).
- Flipped bits in frames past the superblock, torn sectors and holes, frames of zeroes or ones
  torn sector by sector, and a dead writer's half frame and near copy: [Torn and damaged
  frames](storage.md#torn-and-damaged-frames).
- A frame that another branch moves, compared byte for byte: [Drafting outside
  `tip`](commit-path.md#drafting-outside-tip).
- Power cuts across branch-table steps, a reader from before steps, and a file written before them:
  [Branch-table steps](commit-path.md#branch-table-steps).
- Awaited writes, dropped futures and crash images: [Tests](async-writes.md#tests).
- A coalesced group left for a member on its way, and the copies of the booking test, pinned and
  beside busy loops, that hung on it: [Waking members](coalescing.md#waking-members).
- Writes from inside a closure, `ferro/tests/reentry.rs` and `reentry_sync.rs`: [Writes from inside
  a closure](reentrancy.md#the-cases).
- Model tests of best common ancestors: [Criss-crosses](merging.md#criss-crosses).
- Power cuts during a receive placed in pieces: [Where checked objects
  wait](sync-receive.md#where-checked-objects-wait).
- The three-device sync model with small caches: [Commits stored
  twice](sync-receive.md#commits-stored-twice).
- Mutated bundles: [Checking trees](sync-receive.md#checking-trees).
- The heap a clone of a large tree and of a long history holds on each side, `sync_memory.rs`:
  [Memory for each object](sync.md#memory-for-each-object).
- Flipped bytes in a bundle: [Applying](sync.md#applying).
- A pull whose have goes stale once, on every attempt, and between placing and landing: [A stale
  have in a pull](sync.md#a-stale-have-in-a-pull).
- A commit that adds a level and copies a held value: [What is sent](sync.md#what-is-sent).
- A header whose hints reach one held object many ways: [Memory for each
  object](sync.md#memory-for-each-object).
- A damaged raw length, links pointed into a value, and damaged records at every thread count: [The
  copy on several threads](reclamation.md#the-copy-on-several-threads).
- A damaged tree handed to the copy's threads before an error the walk meets itself: [Trees on
  several threads](reclamation.md#trees-on-several-threads).
- A tree that links a second record of a log entry, or the record a chain reads: [Records reached as
  entries](reclamation.md#records-reached-as-entries).
- Processes that write one file at once, die mid-commit and run cycles, and the frames a catch-up
  checks in memory and keeps: [Tests](processes.md#tests).
- The nodes a commit reads from the file to change, which stay out of the cache: [Nodes a commit
  changes](read-path.md#nodes-a-commit-changes).
- Followers of a read-only handle and a shared writer, across cycles, a removed file and damaged
  superblocks, and the end of their thread, `ferro/tests/follow.rs`, and a poisoned follower and a
  drop beside another handle's sync, `follow_fault.rs`: [Tests of
  followers](processes.md#tests-of-followers).
- What runs on Windows: [Tests](windows.md#tests).
- Crash safety in general: [Durability and crash safety](../guide/durability.md#what-was-tested).
