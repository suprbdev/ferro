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
  with a pull of three values of such noise from another file, which arrives in staging frames
  that the mode walks over, damaging only frames with a table, those after them among them.
  The mode then compares the damaged frame with the whole one: if each block and the header and
  table together differ in at most one bit, at most 8 blocks differ, the entry of no block differs
  along with its block, and every bit that differs sits in a sector that holds at least 8 of the
  frame's bytes and has something other than all zeroes or all ones on either side of it, both a
  read-only open and a writer's open must fail with `Corrupt` naming the frame and its first wrong
  byte and leave the file as it was. Otherwise both must open at the version the frames before it
  hold, which a pull's staging frames leave where it was, the writer truncating the file at the
  end of the frame before it, with any staging frames between them, `verify` must pass, and a
  commit must survive a reopen. The seeds of the `open` corpus include small files with staging
  frames written beside a shared writer, ending the log, followed by commits, with zeroed
  records, cut in the header and with a bit flipped in it.
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
  bundle that spills into staging frames, and four taken from devices that
  pull from each other's branches with pruned history and cycles, so that a bundle can carry
  commits the receiver already holds. Three of its maps keep a summary: a wrapping sum over small
  values, one over spilled blobs, some of them one value under several keys, whose `of` reads the
  key, and one over a map of maps, one of them four levels deep, whose `of` reads each inner map.
  The input picks one, then mutates its objects, links, header and count as parsed values, or
  finds the summaries in the nodes of those maps and changes one, or two of a node by amounts that
  cancel so that the node's own fold still holds, and rehashes them up to the head, or edits the
  encoded bytes, or replaces the bundle outright. It can feed the stream in small reads, interrupt
  it, break it, or commit, fork or run a cycle while the checked bundle waits to be placed, or
  commit or fork between syncing a spilled bundle's staging frames and landing it. A refused bundle
  must leave both branches where they were and the receiving branch readable whole. An accepted one
  must read back whole on both branches, with every map's length equal to its iteration, every key
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
between staging a bundle and landing it. Frames that land out of order come from a hook
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

A 20-minute `reclaim` round on 16 cores after that fix found another file that passed `verify` and
compacted under `Keep::All`, on one thread, into one that failed it with the same message, for v14
(#94). v13's `prev` link named v1, so the `prev` links ran from v13 straight down to v1, while
v14's skip link named an intact v12 whose own `prev` links ran down to that v1 one version at a
time. `verify` and the copy checked only that versions fell along `prev`, so nothing walked v2 to
v12 and the skip check never met v12. The copy finds a skip target by counting one version down at
each `prev` step, so it landed on v1's copy for v12, took the v12 that nothing walked for pruned
history and wrote v14's skip link as a tombstone. A background cycle that kept v12 did the same, on
any number of threads. No writer makes such a link, since a commit links the tip below it and a
received chain must descend one version at a time. The walk of a chain, and of the history merges
brought in, now refuses a `prev` link that reaches another version than the one below, so
`verify`, `verify_typed`, a cycle and `compact` all refuse the file with `v13 links v1 as the entry
before it, not v12` and leave it as it is. `ferro/tests/fuzzed.rs` keeps the input, and builds the
shape on a chain of six versions, where the copy cut the skip link as it did here, and in history
a merge brought in, where `verify` passed the same damage and the copy kept it. The round also
stopped once in the harness: copying the image failed with `Interrupted` on the loaded machine,
since `fs::copy` returns the `EINTR` of `copy_file_range` rather than trying again, so the target
now tries again. With both changes `reclaim` ran 10 minutes as 16 forked jobs on 16 cores, from a
copy of its 7,756 saved inputs, while another campaign held the machine's other cores. Loading
them took 260 seconds and reached 10,166 edges, and the run made 571,653 executions in all,
ending at 10,177 edges and adding 104 inputs. No compaction of a file that passed `verify` failed
it, and no input crashed, hung or passed a memory limit.

#90, #94 and a tip at another version than its head named were each a rule that reads or the copy
relied on and `verify` did not check, found one at a time. #95 listed every such rule of the heads
and the chains, as [What readers rely on](history.md#what-readers-rely-on) has them, and
`ferro/tests/fuzzed.rs` now builds a file that breaks each one `verify` did not check. Each of those
files passed `verify`:

- A head whose root was not its tip's: `read` answered with one value and `at` of the same version
  with another, and a cycle kept both.
- A branch with no tip that named v5 and a root: `read` gave that root's value at version 0, and the
  next commit, v1, was built on it.
- A commit older than the one below it: with v4 at 900 and v5 at 500, `as_of(550)` jumped from v6
  through its skip link to v4 and answered v3.
- A merge older than the commit it merged, generation, reach, last merge or bounded bit other than
  its parents give, or a merge floor past the merged commit's version: a pull from the file refused
  each with `does not agree with the commits it links to`. A last merge or a reach of 0 above a
  merge made `common_ancestor` answer version 0 where the base was v3, so a merge would have
  applied every change again, and the floor made it fail with `merge link below its floor`.
- A skip link in merged history that named another entry: the walk of merged history did not check
  skip links, a pull from the file refused it, and a cycle kept it.
- A skip link in merged history past where the `prev` links stop, or past where the history kept by
  time stops to a commit that time keeps: `lowest_kept` followed it below what a cycle keeps, and a
  cycle cut it, so the base search could answer differently after one.
- A fork whose skip link named another entry in the history it shares with its parent branch:
  `verify` refused it, but a cycle walked the shared history once, for the other branch, wrote the
  link as a tombstone, and left a file that failed `verify` with `links pruned history`.

The heads' rules are checked where a head is decoded and at the tip, the times and the merge
numbers with the function a receive checks a sent commit with, so the two cannot disagree, and the
merged history's skip links as the chain walk checks them. The copy also refuses a skip link whose
target its own copy of the chain holds under another hash. Every such file now fails `verify`,
`verify_typed`, a cycle and `compact` on one thread and on four, and is left as it was. The fixture
of the fix that made the first failure in walk order win turned out to hold one: a v18 with no
`prev` link under a v20 that skipped to v16, in history a merge brought in. It fails at the walk
now, on any number of threads, and the test of that ordering builds its own file, where a tree
handed to the copy's threads fails before a record the walk reads later.

The checks read little more. A merge entry's merged commit is read once more, and a walk of merged
history that stops where another took the chain must still meet the targets of the skip links it
passed. Reading on down the chain for them made `verify` of a 210 MB file of 202,000 commits, in
which `main` took 2,000 branches of 50 commits each by merging and dropping them, take 1.38 s where
it took 1.06 s, and `verify_typed` 3.71 s where it took 3.11 s, medians of five and three runs.
The chain walks now record the hashes they pass, and a stop on a recorded chain checks the skip
links against those, which brought the two to 1.07 s and 3.13 s.

With the checks, `reclaim` ran 10 minutes as 12 forked jobs on 24 cores, from a copy of its 7,859
saved inputs, under `-rss_limit_mb=1280` and the flags above. Loading them took 230 seconds and
reached 10,353 edges, and the run made 871,757 executions in all, ending at 10,390 edges and adding
117 inputs. No compaction of a file that passed `verify` failed it, no thread count failed
differently from another, and no input crashed, hung or passed a memory limit.

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

A later `refs` round crashed on a 20-byte input over the base of three branches (#169). It forged a
head for `main` that named v112 and kept versions from 65, over the tip at v48. Decoding the head
passed it, since a head may keep versions 65 to 112, and only the chain walk compared the tip with
the version its head names. Opening the branch took its commit from the tip, so the commits after
it ran on from v49 with a chain that still kept versions from 65, and `compact` under
`Keep::Last(2)` panicked clamping the first version between 65 and 51. Every read of a tip on its
own now checks it too, so opening, refreshing or attaching such a branch, `info`, `Log::last`, a
commit and planning a bundle fail with `names v112 as its last but its tip is v48`. Pruning and a
commit's new chain refuse a chain whose first version is past its last rather than panic, and its
length is 0. `ferro/tests/fuzzed.rs` keeps the input's file and builds the shape on a second
branch, which a commit cannot land on and a cycle refuses, and the `refs` seeds keep the input.
With the fix `refs` ran 10 minutes on one core from a copy of the 4,125 inputs of its seed
corpus, beside a workspace build and test run: it loaded them to 2,052 inputs reaching 18,702
edges in 4,148 executions, and made 6,267 in all, 9 a second, ending at 18,707 edges with 2,081
inputs. No input crashed, hung, leaked or passed a memory limit, and the peak RSS was 714 MB.

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

## A store a test left open

AddressSanitizer's leak check on `ferro/tests/fuzzed.rs` reported 2,457,048 bytes in 627
allocations at exit, every one an indirect leak below the `Shared` that `Store::open_as` makes for
the store's state, in stores opened by the test helper `crafted` (#93). The report named
`a_node_whose_child_counts_overflow_is_rejected`, whose read fails decoding a node whose children's
counts overflow, which suggested a count the read path took and did not give back on the error. Run
one at a time, each of the five tests built through `crafted` leaked, 49 KB to 2.1 MB, among them
reads that succeed, and the other 35 tests leaked nothing. Indirect leaks alone are a cycle:
`crafted` loads its map with `Store::load`, which keeps the decoded map in the store's cache, and
the map holds a clone of the store, whose state holds the cache. Nothing on the read path kept a
count. `Store::close` drops the cache and so breaks the cycle, and every other test and fuzz target
that opens a store outside a database closes it, but `crafted` handed its store to tests that
dropped it unclosed. It now hands the store out in a guard that closes it when dropped, and
`Store`'s docs say that a store that decoded such a value keeps its file open until closed.
`a_crafted_map_lets_its_file_close_after_a_read_fails` checks that the file closes once the guard
and the map drop, after a read that fails decoding a node and after one that fails a cached node's
count, without a sanitizer; it failed before the guard. Built with AddressSanitizer, the 41 tests
of `fuzzed.rs` then ran clean together and one at a time, and so did the library's 51 unit tests,
the 84 of `sync.rs`, the 32 of `crash.rs` and the 35 of `reclaim.rs`.

## The gate model

Five designs of the gate that lets a cycle, a landing or a prune take every branch lock under
`shared`, described under [The gate](processes.md#the-gate), each fixed what a review of the last
found and brought new hangs or stalls (#128, #136, #142, #145, #146, #147, #149, #155). The last
four were chosen in a model first: `ferro/tests/gate_model.rs` runs each design as tasks of a
single-threaded executor on a simulated clock, which moves only when every task waits, so a run is
fixed by its seed.

The model has what the designs differ in, and nothing of the file itself:

- Open file description locks, exclusive, held per open, which wait as Linux's `fs/locks.c` has
  them. A blocked `F_OFD_SETLKW` is linked below the lock in its way, and below the first waiter
  linked there that it conflicts with, and so on down. An unlock wakes only the waiters linked
  directly below the lock, and each, when it runs, takes the lock if nothing is in its way and
  wakes the waiters linked below it, or wakes them and is linked again under what is in its way. So
  a thread that runs first, by a try or a new blocking take, gets a lock just let go before the
  waiter woken for it, and one that blocks while the waiters are being relinked goes ahead of those
  not relinked yet. Other opens test a range as `F_OFD_GETLK` does, finding the lock that starts
  first.
- Each open's lanes, which behave as `parking_lot` mutexes do: a free lane goes to whoever asks
  first, an unlock wakes the longest parked thread, which parks again at the back if another took
  the lane meanwhile, and about once a millisecond an unlock hands the lane over instead. A lane can
  also be waited for with a timeout, as 51dbfc6 did.
- Each open's count of the branch bytes its lanes hold, its `whole` mark, its table of the bytes
  its threads hold, through which the gate, the cycle lock and the placement lock are taken, and
  the queue of awaited writes at each lane, and the coalesced groups that keep a branch's byte
  between closures. The table's condition variable behaves as `parking_lot`'s: a release moves its
  waiters, in order, to the table's mutex, which wakes them one at a time as each lets it go, one
  that finds its byte taken waits again at the back, and a thread that runs takes a free byte at
  once.
- The cycle lock, which a cycle holds from its prune to the end of its pause, and the placement
  lock, which a commit, a group's seal and a holder that has every branch take.
- Writers that pass the gate, take their lane and byte, run a closure and commit; awaited writes
  that take the lane through the queue, test the gate unless they were held up before and their
  byte without waiting, and hand the wait to a thread that each byte's awaited writes share, as
  db.rs does; closures that sleep, write from inside themselves or join a thread that writes
  through any open, the cycling one included.
- The whole-range takers of each design: 6fb68bc's epochs, read from the holder's stamp, with its
  holder taking its own lanes in order and giving them up after three epochs; 51dbfc6's rounds with
  their 1 ms openings and lanes taken with a 1 ms timeout; and the phase gates. These differ in
  their rule for a busy branch: none, as many phases as the branch stays busy, or one, which both
  phase gates chose. They also differ in how the phases are told apart. b2741a3's pair of bytes
  started every hold on the first and moved to the other before letting the old one go; its writer
  found the phase with a test over both and then one of the first, and its process numbered phases
  afresh at each hold. A pair spaced 1.5 ms between holds, a counter the holder bumps that writers
  read, and the chosen pool of 1,024 bytes start each hold on one drawn at random and number phases
  without repeating. The pool waits 1 ms after a gate it had to wait for, and b919d64's gate is
  the pool with that wait. They differ last in how takers queue for the gate byte: in the table and
  the kernel, as b919d64 did; the same with no try before the blocking take; in a line of each
  open's takers by ticket; with a turn drawn among the opens as well by the first in each line,
  waiting in the kernel on the turn before it; the same keeping a pull's place in line and turn
  from its landing for its integration, as e11aab6 did; a turn drawn by each thread when it asks,
  counting the turns its own open's threads hold, which a test does not show, waiting on its open's
  record for a turn one of them holds and in the kernel for another's; the chosen one, which tests a
  turn another open holds between sleeps, passes it once it has stayed held for 10 ms with the turn
  before it and the gate free, and gives the door up after 10 ms; and that one with a mark byte for
  each turn that the gate's holder holds, on which a waiter sleeps in the kernel between holds. The
  last four keep a pull's turn from its landing for its integration. The writers of every phase gate
  give up after 2 s, in their own process as in others, and test the time before each sleep, as the
  code does. A scenario can stop a taker for a while where it stands, holding what it holds there,
  as `SIGSTOP` does: while it waits for its turn, or, in the designs without turns, out of the
  kernel's chain of waiters for the gate byte, which a stop takes it out of; while it holds the
  door; or between a pull's landing and its integration.

Every wake runs after a delay drawn from one of three profiles, which alternate by seed: calm, a few
microseconds with 0.2% of wakes preempted for up to 4 ms; busy, up to 300 µs with 4% preempted for
up to 12 ms, as beside spinning threads; and hostile, 1% preempted for 5 to 50 ms. A sleep
overshoots by 50 to 80 µs, a commit takes 20 to 60 µs and a pause up to a millisecond. Each scenario also adds
one of 400 offsets 25 µs apart, chosen by the seed, to when a closure joins its write or a thread
begins, so that 1,200 seeds cover 10 ms of phase against the rounds in every profile.

A run ends stuck when no task can run and none sleeps, late when its scenario's time runs out, and
otherwise is checked against the scenario's bounds on how long a taker and a joined write took. The
scenarios are those of each issue: five cycles beside 4 or 16 other handles on branches of their
own with closures of 0 to 20 ms, and 50 commits through a handle that cycles over and over (#128);
a closure joining a commit, blocking or awaited, beside another handle's cycle (#136); a cycle
beside four threads of one handle with 600 ms closures begun 150 ms apart, eight with 300 ms ones,
two handles of four, and four with 900 ms ones, and a closure joining a commit through the cycling
handle (#142); three cycles beside four threads of the cycling handle on one branch and four
handles with 20 ms closures, a closure on `b` joining a commit on `a` through the cycling handle
while a blocking or awaited write on `b` there waits for its byte, a closure joining a commit on an
earlier branch of the cycling handle, and a cycle and pulls in one handle (#145); a cycle beside two
or three threads of its handle taking one lane in turn, closures joining commits back to back
beside a handle cycling as fast as it can, through another handle and through the cycling one, a
coalesced cycle that seals a group holding `a` while a closure on `b` joins a commit on `a`, and
cycles and pulls in two handles (#146). Four more are adversarial: nested and awaited writes from
closures beside twenty cycles; two threads taking turns at closures on one branch that each join a
commit, each queued before the last ends; closures joining commits on a branch three other handles
keep busy; and four threads each looping closures that join commits on branches of their own.

Those of #147 take every branch over and over beside writers whose closures last up to 100 µs: one
taker and two, with no gap between holds and 100 µs apart, with the writers in other handles or in
the taker's, and two and four handles pulling, each pull two holds back to back and a merge
committed through the puller. Each checks that no writer waits at the gate longer than the round
of the hold it found and the next, three times the hold's age and 4 ms, with 200 ms to spare for
preempted threads. The rest are four threads of the cycling handle each looping closures that join
commits through it, which checks only the joined commits, as for the threads of another handle;
one cycle beside closures just under and just over a round, 3.9, 4.1, 16.1, 128.1, 511.9 and
512.1 ms, which checks the three times C and 4 ms that the gate promises, with 250 ms to spare;
and a cycle whose rounds grow to a second beside writers that come at any time, which checks that
no writer tests the phase it found after it ended and still finds it held. Where two threads take
turns at joining closures on one branch, the chain can outlast a few rounds before the rule for a
busy branch breaks it, and with the cycle and placement locks in the model a cycle there took just
over 2 s about once in 5,000 runs, under b2741a3's gate and the chosen one alike, so that scenario
allows 2.5 s.

Those of #149 run sixteen takers that pull over and over for 3 s, each pull a landing and an
integration and a merge committed through the puller: in opens of their own beside four and, for
6 s, sixteen opens committing in 5 ms closures, and as threads of one open beside four; #155 adds
eight threads of one open beside eight opens of their own. Taken in turn, each taker pulls once in
about the time all of them take to pull once each, the run's length times the takers over the
pulls made in all, so each checks that no pull, nor the wait for one that had not ended when the
run stopped, took more than twice that and 100 ms, that every taker pulled, and that the taker
that pulled fewest times, counted with one more, pulled at least half as often as the one that
pulled most. Beside sixteen writers each hold waits out a few rounds and the takers pulled once or
twice each in 3 s, so a taker that never pulled passed the other checks there; the run is twice as
long now, and the check that every taker pulled fails b919d64's order. The rest of #155 stop a
fourth taker for 2 s, while it waits, while it holds the door and between its landing and its
integration, beside three handles pulling over and over and four committing in closures of up to
1 ms, and check that none of the three's pulls, nor a wait for one, took 750 ms. With the
kernel's chains of waiters in the model, a cycle beside sixteen handles with 5 ms closures took up
to 380 ms once in 6,000 runs, under b919d64's gate and the chosen one alike, past the 350 ms that
scenario allowed, so the five of #128 allow 250 ms rather than 200 ms over thirty closures.

`the_phase_gate_passes_every_scenario` runs the chosen design over 1,200 seeds of each scenario, in
about two minutes, or as many as `FERRO_GATE_SEEDS` says.
`the_model_finds_what_the_reviews_of_earlier_designs_found` checks that the model finds each failure
the reviews found, in the design they were found in, within 120 seeds: 6fb68bc's 4 to 6 s cycles
beside its own threads on one branch, its hangs beside a write waiting for the joining closure's
byte, beside a closure joining a commit on an earlier branch of its handle and beside the coalesced
group, and its joined commits through the cycling handle of up to 3 s; 51dbfc6's stalls beside
threads of its handle taking one lane in turn, its back-to-back livelock, its joined commits of up
to 16.4 s through the cycling handle, as the review measured, and its hang beside the group; and
b2741a3's writers waiting out holds after the one they found, beside one taker and two and four
pullers, within 600 seeds for four, and finding their phase after it ended; and b919d64's takers
served out of turn in all three runs of #149, and a line in each open out of turn across opens;
e11aab6's turns for each open, which serve the threads of an open that others share the gate with
out of turn and hold up every taker behind one that stops while it waits or between a pull's
holds; and turns for each thread that wait in the kernel, which hold every taker up behind one
stopped while it waits or holds the door. It
also finds that a phase gate without the rule for a busy branch lets a chain of joining closures on
one branch keep a cycle waiting for good, and that a rule that waits as long as the branch stays
busy holds a joined commit for 2 s on a branch other handles keep busy. `compare_designs`, ignored, prints each design's failures and the spread
of its takers' and joined writes' times, and `sweep_one_design` one design's, named by
`FERRO_GATE_DESIGN`, over the scenarios `FERRO_GATE_ONLY` names.

With 300 seeds of each scenario, the chosen phase gate passed all 50, and the others failed these,
out of 300, where the phase gates of the first table, which queue takers as the chosen one does,
are named by their rule for a busy branch:

| Scenario | 6fb68bc | 51dbfc6 | No rule | Every phase |
| --- | --- | --- | --- | --- |
| #136, a closure joining a commit, blocking and awaited | 0, 0 | 21, 12 | 0, 0 | 0, 0 |
| #145, cycles beside the cycling handle's threads on one branch | 300 | 11 | 0 | 0 |
| #145, a write waiting for the joining closure's byte, blocking and awaited | 297, 297 | 0, 0 | 0, 0 | 0, 0 |
| #145, a closure joining a commit on an earlier branch of the cycling handle | 300 | 0 | 0 | 0 |
| #146, three threads with 5 ms closures and two with 2 ms taking one lane in turn | 0, 0 | 196, 64 | 0, 0 | 0, 0 |
| #146, joined commits back to back, and 5 ms apart | 0, 0 | 171, 52 | 0, 0 | 0, 0 |
| #146, joined commits back to back through the cycling handle | 9 | 102 | 0 | 0 |
| #146, a coalesced group's byte | 300 | 300 | 0 | 0 |
| Two threads' joining closures on one branch, each queued before the last ends | 155 | 151 | 187 | 0 |
| Closures joining commits on a branch three other handles keep busy | 0 | 9 | 0 | 220 |
| Four threads' joining closures on branches of their own, joined commits only | 0 | 212 | 0 | 0 |
| #147, four handles pulling | 0 | 19 | 0 | 0 |
| #147, closures of 128.1, 511.9 and 512.1 ms | 4, 203, 206 | 0, 0, 0 | 0, 0, 0 | 0, 0, 0 |
| #149, sixteen opens pulling beside 4 and 16 writers, and sixteen threads of one open | 299, 299, 300 | 300, 297, 269 | 0, 0, 0 | 0, 0, 0 |
| #155, eight threads of one open beside eight opens | 300 | 300 | 0 | 0 |
| #155, a taker stopped while it waits, holding the door and between a pull's holds | 0, 0, 0 | 91, 91, 94 | 0, 0, 0 | 0, 0, 0 |

and the phase gates that tell phases apart in other ways, each with the rule for a busy branch and
takers queued in the kernel:

| Scenario | b2741a3's pair | Pair, 1.5 ms apart | Counter | Pool | Pool and 1 ms |
| --- | --- | --- | --- | --- | --- |
| #147, one taker back to back, writers in other handles | 18 | 0 | 0 | 0 | 0 |
| #147, two takers back to back | 50 | 1 | 0 | 0 | 0 |
| #147, two and four handles pulling | 1, 2 | 0, 0 | 0, 0 | 0, 0 | 0, 0 |
| #147, writers that come at any time beside a cycle | 71 | 71 | 0 | 0 | 0 |
| #147, closures of 511.9 ms | 0 | 0 | 1 | 0 | 0 |
| #149, the three runs | 198, 178, 300 | 197, 161, 298 | 198, 167, 299 | 197, 156, 300 | 196, 189, 296 |
| #155, eight threads of one open beside eight opens | 298 | 208 | 297 | 298 | 299 |

The pair spaced 1.5 ms apart keeps b2741a3's two tests, which a holder moving between them defeats,
and costs every hold its space; the counter would have to live in the file. The pool alone passes
every scenario before #149 in the model, which has no cost for a writer's catch-up, but in
the real code holds queued for the gate still followed one another with no gap, and the writers
let on at each end took their bytes as the next hold tried for every one, as
[The gate](processes.md#the-gate) describes; the 1 ms after a gate the holder waited for is what
the real code needed.

The ways of queueing takers, each with the pool and 1 ms, failed these, and the last rows give, at
the median of the seeds, how many pulls the taker that pulled fewest and the one that pulled most
made, and the longest pull of any seed:

| Scenario | b919d64, the kernel | No try first | A line in each open | Turns for each open | And a pull's holds in one, e11aab6 | Turns for each thread | And passing, chosen | And marks |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| #149, sixteen opens pulling beside 4 and 16 writers, and sixteen threads of one open | 196, 189, 296 | 200, 197, 296 | 196, 189, 0 | 0, 0, 0 | 0, 0, 0 | 0, 0, 0 | 0, 0, 0 | 0, 0, 0 |
| #155, eight threads of one open beside eight opens | 299 | 296 | 293 | 300 | 300 | 0 | 0 | 0 |
| #155, a taker stopped while it waits, holding the door and between a pull's holds | 0, 0, 0 | 0, 0, 0 | 0, 0, 0 | 300, 300, 0 | 300, 300, 300 | 300, 300, 300 | 0, 0, 0 | 0, 0, 0 |
| Pulls each, #147, two handles pulling in 1.5 s | 45-48 | 45-48 | 45-48 | 33-34 | 41-43 | 41-43 | 37-39 | 36-38 |
| Pulls each, #147, four handles pulling in 1.5 s | 21-25 | 21-25 | 21-25 | 18-19 | 21-22 | 21-22 | 19-20 | 17-18 |
| Pulls each, #149, sixteen opens beside 4 writers in 3 s | 1-7 | 1-7 | 1-7 | 3-4 | 4-5 | 4-5 | 3-4 | 3-4 |
| Pulls each, #155, eight threads of one open beside eight opens in 3 s | 1-7 | 1-7 | 1-8 | 0-6 | 1-7 | 4-5 | 3-5 | 3-4 |
| Longest pull, #149, sixteen opens beside 4 writers | 3.00 s | 3.01 s | 3.00 s | 1.52 s | 1.34 s | 1.34 s | 1.37 s | 1.65 s |
| Longest pull, #155, eight threads of one open beside eight opens | 3.01 s | 3.00 s | 2.78 s | 3.01 s | 3.01 s | 1.31 s | 1.48 s | 1.39 s |

A blocking take with no try first changes nothing, since the kernel grants a lock just let go to
whichever taker asks first, blocking or not. A line orders the threads of one open and leaves the
opens to the kernel. Turns for each open order both, but a pull that queues again for its
integration waits behind every taker that came during its landing and 1 ms more after it, and two
handles pulling made a quarter fewer pulls; keeping the landing's place and turn for the
integration makes nearly as many as b919d64 with two and as many with four. A thread queued behind
another of its open still draws its turn only once those ahead of it in the line have held the gate,
after every open that asked meanwhile, so eight threads of one open beside eight opens pulled 1 to
7 times each, the threads of the one open least. A turn for each thread, drawn when it asks, orders
them all and pulls as often as e11aab6 elsewhere. All three wait in the kernel for the turn before
their own, holding their own, so a taker stopped while it waits, or while it holds the door that a
blocking draw waits for, holds up every taker after it for the whole 2 s, and so does one stopped
between a pull's holds wherever the landing keeps its turn. Passing turns test the turn before
their own between sleeps, so their waiters wake more often, and in the model, where any wake may be
preempted, beside a fast writer two and four handles made a tenth fewer pulls; in the real code they
came within the noise of e11aab6's, as [Takers in turn](processes.md#takers-in-turn) records. A
mark that the holder holds, for waiters to sleep on in the kernel between holds, pulled no more
often and made the longest pulls longer.

The last scenario of the first table, four threads looping closures that join commits on branches
of their own, is beyond what the gate promises a taker, as [The gate](processes.md#the-gate) says,
and so is the same through the cycling handle. Under every phase gate the five cycles often had not
finished after 10 s, so they check only that nothing is stuck and that each joined commit waits at
most 1.2 s, and 2.1 s through the cycling handle, which 51dbfc6's 2 s waits fail. Through the
cycling handle 6fb68bc did better, joined commits taking up to 130 ms and the cycles finishing,
since its own threads did not wait at its gate, which is what let them keep its cycles waiting in
#145; the phase gates' joined commits there take about a second, the longest round.

The model does not have the kernel's merging of one open's locks on neighbouring bytes, which the
first build of the two-byte gate tripped on and `ferro/tests/gate.rs` caught, nor a writer's
catch-up with what other processes wrote, which in the real code made holds queued for the gate
keep writers out far longer than the model shows. Nor does it have the holder's catch-up, which
makes a hold longer by what others wrote since its last but moves no taker in the queue, the flag
that tells a writer its process holds the gate, or a holder taking the gate again on the new file
after another process's cycle replaced it.

## Timed checks

The tests of the gate in `ferro/tests/gate.rs` and `ferro/tests/shared.rs` time how long writes wait
beside cycles, pulls and pauses, and those times stretch with the load on the machine. Their bounds
were set on a quiet machine, and on one shared with other builds, at a load average of 20 to 70 on
32 cores, they failed a few times a day (#165): the 90th percentile of 40 joined commits took 105 ms
and 101.8 ms against 100 ms, and a writer held at a pause that panicked took 674 ms against 600 ms.
Beside 48 processes spinning on the cores, at a load average of 80 to 145, 14 of 18 runs of
`gate.rs` failed. The 90th percentile of joined commits went past 100 ms in 52 of 168 runs of a
check, up to 150 ms, against about 40 ms beside only the rest of `gate.rs`; the writers at the
panicking pause took up to 1.1 s; and a writer beside four handles pulling back to back made 4,123
to 14,155 commits in 3 s against the 8,000 asked, which 24 of 36 runs missed. What the checks are
for costs far more: a writer that misses the end of its phase, or a gate whose phases never end,
waits the full 2 s after which a writer goes on, and the designs before the phase gate kept joined
commits and cycles waiting from 2 s to 18 s.

So a timed check has two bounds. Its ceiling is a time the failure it is for always reaches, and
does not move with the load: every joined commit under 2 s, which [The gate](processes.md#the-gate)
promises; nine in ten under 500 ms, half the longest round, so that joined commits that wait out
long rounds fail however busy the machine; the writers at the panicking pause, and a writer at a
phase byte another open holds, under 1.5 s, short of the 2 s a writer waits for an end that never
comes; and a commit joined at a hold about 50 ms old under 600 ms, short of the 655 ms it waited
with the second version's record. Below the ceiling the check holds the bound it had on a quiet
machine, stretched by the load the run met. `Lateness::watch`, in `ferro/tests/common/mod.rs`, runs
a thread beside the timed work that sleeps 1 ms over and over and times each sleep, and
`Lateness::bound` multiplies the quiet bound by one more than the 99th percentile of how late those
sleeps woke, counted in milliseconds. The gate's writers and holders wait between sleeps of up to a
millisecond, so a machine on which such a sleep takes several times as long stretches their waits
about as much, and a slow tenth of the timed commits falls in stretches of load that more than a
hundredth of the sleeps also fall in. Run one at a time, the tests saw sleeps 55 to 70 µs late at
the 99th percentile, which stretches a bound by about 1.06. The whole of `gate.rs`, three of whose
tests spin two threads a core, beside other builds at a load average of about 30, saw them 1.7 to 2
ms late, about 2.9 times, and beside the 48 spinning processes 2.6 to 10 ms, 3.6 to 11 times. A
count of work rather than a time is compared with other work counted in the same run.

Adding the lateness once for each of eight hand-offs between threads, rather than multiplying,
widened the 100 ms to 121 to 181 ms beside the spinning processes, and the 90th percentile reached
0.9 to 1.12 times that, since a hold that lasts longer begins longer rounds, and a joined commit
waits out the round it meets. Multiplied, the bounds came to 377 ms and more, and the 90th
percentile reached at most a third of its bound.

The writers beside four handles pulling back to back now commit at least four times for each pull
made in the same run, instead of 8,000 times in 3 s. A writer that waits only for the hold it finds
goes on when that hold ends and commits many times between holds: 112 and 141 times a pull on an
idle machine, and 9.6 to 28.7 times beside the spinning processes. One that waits out later holds
commits about once a wait: with every hold starting on the first phase byte, its process
numbering phases afresh and no 1 ms after a gate the holder waited for, as b2741a3 had it, the
writers committed 0.6 and 1.2 times a pull. They also committed 14,485 and 23,429 times, past the
8,000, since without the 1 ms the pulls ran ten times as often.

Each check still fails on what it is for, with the code changed to fail that way for the run. With
phases that never end, the six checks of joined commits and
`a_joined_commit_waits_at_the_gate_no_longer_than_the_hold_after_an_earlier_wait_ran_out` waited
2.0 s. With every hold starting on a round of a second, the six waited about 1 s at the 90th
percentile and the commit joined at a hold 50 ms old 950 ms. With a gate that keeps its phase while
it unwinds, the writers at the panicking pause took 2.0 s in each of three runs, and with writers
that wait for every phase byte rather than the one they found,
`a_writer_waits_for_the_phase_byte_it_finds_until_the_holder_moves_to_the_next` took 2.0 s. With a
first round of 400 ms rather than 4 ms, which breaks no promise of the gate, joined commits took
about 380 ms, and the four checks run on their own failed against bounds of 105 to 116 ms, but
beside the rest of `gate.rs` the bounds stretched to 360 to 540 ms and five of the six passed: as
the machine gets busier, the checks narrow to what costs seconds.

Checks whose bound is several times what they take on a loaded machine keep it fixed: the 1 s of
`cycles_beside_threads_of_the_cycling_handle_contending_for_one_branch_take_under_a_second`, which
took up to 140 ms beside the spinning processes, the 1 s of a commit through the cycling handle in
`a_closure_joining_a_commit_through_the_handle_that_cycles_finishes_beside_its_cycle` and its
awaited form, up to 92 ms, and in `shared_process.rs` the 1 s within which a writer held at the
gate of a killed process goes on, up to 14 ms.

With the checks as they are, 210 runs of `gate.rs` passed: 50 beside the 48 spinning processes, at
a load average of 75 to 180 with other work on the machine, 80 beside a release build of the
workspace, 60 beside the workspace's tests and 20 as other work eased, from a load average of
about 8. The writers beside pulls committed at least 9.2 times a pull, and in 52 of the 100 checks
beside the spinning processes fewer than the 8,000 times the old check asked. So did 50 runs of
`shared.rs`, 30 beside the spinning processes and 20 beside the workspace's tests.

A check that a thread did some work beside others, such as a reader that read while a writer
committed, or a thread of the copy that wrote before the copy was shut down, asks for the outcome of
a race, which a loaded machine can lose: at a load average of about 185,
`shutting_down_while_threads_verify_the_copy_joins_them_and_leaves_no_temp_file` found that no
thread of the copy had written by the time the copy reached 2 MiB, since the walk can write every
job itself. So such a check waits for the work it asks for instead of for a time or an amount of
other work. That test waits until a thread of the copy writes, and holds that write until the
handle is about to be dropped, so the copy is still running when it is. The others keep the side
with a fixed amount of work going until the other side has done its part, or start it once the
other side is under way, so that the work they count has happened before they count it.

## Test hooks

Tests, fuzz targets and some `ferro-bench` tools reach inside the library through hooks that only
a build with its `testing` cargo feature has, as
[Development](../guide/development.md#the-testing-feature) describes:

- `Options::hook` takes a `db::Hook`, a callback that commits, cycles, catch-ups and sync call
  with the name of each step they reach, such as `drafted`, `landing` and `published` in a commit,
  `created`, `copied` and `swapped` in a cycle, and `received` and `placed` in a receive; the calls
  to `step` in `db.rs` and `sync.rs` name them all. A test injects a fault at a step by panicking
  there, blocking until another thread has acted, or acting on the database itself.
- `Options::clock` takes a `db::Clock`, which replaces the system clock for commit times.
- `Options::trace` takes a `store::Trace`, which records each creation, write, change of length,
  sync and rename of the store's file, and each sync of its directory, as a `store::Op`: a write or
  a change of length before it is made, since it may land in part, and a sync once it succeeds.
  Crash tests replay a prefix of the trace to build the image that a power cut at that point could
  leave. `Db::trace_op` adds a test's own marks, such as `Op::Acked` for a commit acknowledged at a
  version.
- `Options::follow_polling` makes a follower look at an interval even where inotify could tell it
  of changes.
- The `store` module, public only with the feature, opens a `Store` and writes `Txn`s outside a
  database, and `admin::layout` lists where the records a compaction would copy lie in a file.
- `Decoder::in_store` decodes an object with the store it was read from behind it, so that the
  collections in it can read their nodes, as the `refs` fuzz target does with the heads it reads.
  `Encoder::sink` hands out an encoder's sink and `Encoder::mark_unsettled` marks what it wrote as
  not yet in the file, for a hand-written `Data` impl in a test that writes another value's
  encoding, as `index_agreement` splices one table's indexes onto another's rows. Without the
  feature those two are private to the crate, whose collections call them as they encode.

Without the feature none of these exist. `Options` and a database's shared state hold an empty
`Hooks`, whose step is an empty function and whose clock is the system's, and the store's file
wrapper holds no trace and records nothing, so the commit and write paths call no hook and test no
`Option` for one. A Buffered single-key commit measured on 2026-11-13 with `ferro-bench`'s
`batch --batch 1 --n 1000 --commits 20000 --durability buffered`, built with `--no-default-features`
and with the feature, in seven alternating runs of each, took a median 102,887 user-space
instructions outside its closure for a new key without the feature against 103,055 with it, 168
fewer, and 58,971 against 58,979 for an overwrite, within the 100 that the runs of either build
spread over. Wall time was the same, 15 to 20 µs for a new key and 8 to 10 µs for an overwrite.

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
- A coalesced group sealed by another branch before a failed write or a failed group, a leader
  that panics in its sync, and the stress of injected failures, `coalesce_failure.rs`, with the
  coalesced runs of `landing_fault.rs` and `poison.rs`: [A group nobody
  led](coalescing.md#a-group-nobody-led).
- Writes from inside a closure, `ferro/tests/reentry.rs`, `reentry_sync.rs`, `reentry_shared.rs`
  and `reentry_locks.rs`: [Writes from inside a closure](reentrancy.md#the-cases).
- Model tests of best common ancestors: [Criss-crosses](merging.md#criss-crosses).
- Pulls between branches that hold the same value, a revert relayed through such a branch and a
  ring of pulls that must settle: [The same value on both
  sides](sync.md#the-same-value-on-both-sides).
- Files syncing in random orders against a model of the merge graph, with shared handles, threads
  and processes, `sync_model.rs`: [The sync model](sync.md#the-sync-model).
- Power cuts, killed processes, cycles, refusals and failed writes during a receive staged in the
  log, `sync_stream.rs`, `sync_unlocked.rs`, `sync_staging.rs` and `landing_fault.rs`: [Where
  checked objects wait](sync-receive.md#where-checked-objects-wait).
- The three-device sync model with small caches: [Commits stored
  twice](sync-receive.md#commits-stored-twice).
- Mutated bundles: [Checking trees](sync-receive.md#checking-trees).
- Tables whose indexes a peer set apart from their rows, sent whole, drifting in a pull, behind the
  head, nested in a map, and beside a value the receiver cannot read, whose walk's error must be the
  one returned, a table storing no index with rows under other keys, one holding an index the
  receiver's type lacks, which is refused, and one lacking an index the receiver's has, which lands,
  a table whose type declares no index with rows under other keys, cloned, pulled and nested, and
  holding an index, which is refused, and a table that drifts in a pull at each place a table can
  sit, a field, an `Option`, a `Vec`, a `BTreeMap`, a `Map`, a `List`, an enum, a `Box` and a tuple,
  checked against its own previous version, and a clone of tables in a map whose index changed kind,
  refused until the map is rewritten and the versions before pruned, and of a table in the root
  after one commit, and of tables in the rows of a table in the root, refused until that table is
  rewritten and the versions before pruned, in `sync.rs`, written to a file for `verify_typed`, the
  same with no index declared, and versions holding an index the type dropped, which pass, read
  without its indexes, with every kind of index, and pulled with an `each` index over a `Set`, in
  `index_agreement.rs`, and the diff the check makes, against `Map::diff`, on maps small enough to
  diff on one thread and large enough to split among threads, with runs that only one side holds, in
  `map.rs`. What the check reads of a set an `each` index names, how many jobs 2,000 small tables
  take, and what it reads of a table of 20,000 rows that shares none with its base, which it builds,
  and what a pairing pairs: the fields of one object sharing its zero-width items, two roots of
  200,000 fields of unknown tags, the tables of a `BTreeMap` by key and those of a `Vec` that gained
  or lost elements, in `table.rs`; a check beside the walk that must panic on no thread, counted by
  a panic hook, `index_check_panics.rs`; the bytes a receive allocates for a forged map of rows that
  links one node many times, `index_check_cost.rs`; the bytes a pull and `verify_typed` allocate for
  one-row edits to tables in a map against the same at the root, for an optional table placed before
  another against one placed after, for tables in maps cloned from one template against maps apart,
  and for 64 maps cloned from one set to clones of another, `index_check_pairing.rs`; the heap a
  pull of 50 versions of 20,000 tables in a `Vec` holds, `sync_memory.rs`; the threads a receive and
  `verify_typed` take, `index_check_threads.rs`; and the threads of a pool that run work, in
  `pool.rs`: [Indexes that agree with their
  rows](sync-receive.md#indexes-that-agree-with-their-rows).
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
- Cycles, landings and prunes beside writers in other handles and processes, and closures that
  join writes beside them, `ferro/tests/gate.rs` and `ferro/tests/shared.rs`: [The
  gate](processes.md#the-gate).
- The nodes a commit reads from the file to change, which stay out of the cache: [Nodes a commit
  changes](read-path.md#nodes-a-commit-changes).
- Superblocks held half written beside a read-only open and a `refresh` held up between the two
  slots, and beside a closing writer, `superblock_fault.rs`: [Reading the
  slots](storage.md#reading-the-slots).
- A read-only open beside a recovery that rolls back, held in its first superblock write, in the
  sync after it, and between the open's read of the slots and its look at the writer, one held
  between finding a writer alive and testing the recovery lock while that writer dies, one beside a
  writer it can never vouch for, and a power cut after each step of that recovery, with the file cut
  short and with it ending at the frame the recovery keeps, `superblock_fault.rs`: [Buffered
  durability and checkpoints](storage.md#buffered-durability-and-checkpoints).
- Followers of a read-only handle and a shared writer, across cycles, a removed file and damaged
  superblocks, and the end of their thread, `ferro/tests/follow.rs`, and a poisoned follower and a
  drop beside another handle's sync, `follow_fault.rs`: [Tests of
  followers](processes.md#tests-of-followers).
- Ranges and prefixes combined with smaller lookups, and lookups of two tables or two versions of
  one, against a filter over the rows, `combine_range.rs` and `combine.rs`: [Ranges and prefixes in
  a combination](schema-and-unique-indexes.md#ranges-and-prefixes-in-a-combination) and [Lookups of
  several tables](schema-and-unique-indexes.md#lookups-of-several-tables).
- What runs on Windows: [Tests](windows.md#tests).
- Crash safety in general: [Durability and crash safety](../guide/durability.md#what-was-tested).
