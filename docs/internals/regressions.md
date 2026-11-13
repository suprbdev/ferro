# Regressions between measurements

When [Performance](../guide/performance.md) was measured again, a figure that came out slower than
the one before beyond the spread of its runs was checked against builds of older trees run
alternately with the new one. This page records each such regression, newest first, with what
caused it and how it was fixed. The fixes themselves are described where their subsystems are.

## A range read on 2026-11-05

Measured again on 2026-11-05, one figure came out slower than on 2026-11-04 beyond the spread of the
runs, and builds of older trees run alternately with the tree of that day confirmed it: a range
read, 29.8 ns a key against 28.6 for the build of 2026-11-04 in `ferro-bench`, and 28.4 µs for 1,000
keys against 26.8 in the compare harness (#79). The harness puts the step at db07c34, which borrows
nodes under hazards in iterators, as [Iterators under
hazards](read-scaling.md#iterators-under-hazards) describes: its parent took 27.0 µs and it took
28.1. A range runs no more instructions than before, 160.1 a key in the trees of 2026-11-04 and
2026-11-05 built the same way. The build the guide's figures of that day came from ran 162.1, and
the same tree built from a copy in another directory ran 160.1 and 28.7 ns, so what the compiler
makes of the range loop differs between builds of one tree by about that much.

Index lookups beside a writer with the 16 MiB cache vary between builds too. Beside one thread
committing single-key updates, the guide's first pass that day gave 61M gets a second at 32 threads
with the 1 GiB cache and 2.6M with the 16 MiB one, and 2.9M and 89,000 index lookups, and a later
pass 66M, 2.5M, 2.9M and 83,000. In four passes of those lookups alone, alternating the builds, the
build the guide's figures came from ran 87,900 a second at 777,000 instructions a lookup, the same
tree built in another directory 93,000 at 751,000, and the tree of the first pass 90,800 at 762,000.

The range read is fixed. A borrow under a hazard read its node's flags behind the value, in a line
of their own, and the exchange that writes the slot waited for that line before the node's headers
were asked for, so each of the 130 or so nodes a range borrows cost two misses in a row where the
count had cost one. The weak count and the flags now sit at the front of each allocation, beside
the headers, as [The flags' line](read-scaling.md#the-flags-line) describes. In 15 alternating runs
of the harness's ferro child a range took 27.4 µs against 28.2 before the fix and 27.3 for db07c34's
parent, and in nine of `ferro-bench` 28.8 ns a key against 29.7. A get, a scan, a bulk load and its
commit and drop moved by less than the spread of their runs, a table's row get took 157 ns against
155, and in `scaling` at 1, 8 and 32 threads no cell ran more than 0.4% slower while gets, index
lookups and `for_each` scans with the 1 GiB cache ran 3 to 6% faster. The figures under [Reads from
many threads](../guide/performance.md#reads-from-many-threads) were measured before the fix.

## Four figures on 2026-11-04

On 2026-11-04 four figures came out slower than before, beyond the spread of the runs, and builds of
older trees run alternately with the tree of that day confirmed each: a filtered scan of table rows,
31.6 ns a row against 17.8 (#56), `wiki` commits, 564 a second against 691 (#57), a covered page of
200 rows on one thread, 798,000 a second against 896,000 (#58), and bulk inserts, 3,092
instructions a key against 2,978 (#59).

All four have been fixed since. For #57 and #59, as [Where the flags
live](read-scaling.md#where-the-flags-live) describes, the bulk inserts ran more instructions
because `Shared::make_mut` was not inlined, and now run 2,941 a key. The store did no more work for
a `wiki` commit than before: the example's tokenizer had compiled to slower code, and with it
rewritten `wiki` commits 1,004 times a second against 548 before the fix and 638 before `Shared`,
medians of three alternating runs on a machine that other work was sharing.

The filtered scan is fixed. A fold called its closure from a loop for each kind of run, so the
compiler stopped inlining a closure over table rows, and the scan now takes 18.7 ns a row against
32.6 in alternating runs, as [Scans](read-path.md#scans) describes.

The covered page is fixed too. Its commit changed no read code, but its new functions moved the
index lookup's constructors into another codegen unit than the benchmark's read loop, which then
called the index cursor through its vtable for every row. The constructors are now always inlined
into the code that reads them, and in alternating runs a covered page ran 810,000 a second on one
thread against 742,000 and 3.27M at eight threads against 2.98M, as [Covering
indexes](schema-and-unique-indexes.md#covering-indexes) describes. The table under [Reads from many
threads](../guide/performance.md#reads-from-many-threads) was measured again after both fixes.
