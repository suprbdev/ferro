# Writers in several processes

How `Options::shared` lets several processes commit to one file: the locks, how a process catches
up with the others, how frames and superblocks are written, how reclamation cycles are shared, and
what was measured and left out (#60). [Followers](#followers) describes how `Options::follow` keeps
a read-only handle or a shared writer up to date with the others on a thread of its own (#84). The
user-facing rules are in [Writers and concurrency](../guide/concurrency.md#several-processes).

## The locks

Without the option one process holds the writer lock for as long as its `Db` is open. A shared
writer takes that lock shared rather than exclusive, so it and a writer without the option, from
this build or an older one, refuse each other with `Error::Locked`, and shared writers do not refuse
each other. Everything else is a byte-range lock past any real file, at 2^62 and up:

| Lock | Held | For |
| --- | --- | --- |
| The writer lock, a `flock` | Shared, for the writer's life | Keeping out writers without the option |
| 2^62 + 1 | Shared, from the end of the writer's open for its life | Telling a read-only open, and a writer that opens, that a writer has opened and is alive |
| 2^62 + 2, placement | Exclusive, per frame, or shared by a read-only open's scan | Catching up, placing a frame and writing it |
| 2^62 + 3, superblock | Exclusive, per sync | Syncing and writing a superblock |
| 2^62 + 4, cycle | Exclusive, per cycle | A reclamation cycle, and the landing of a large `receive` |
| 2^62 + 5, gate | Exclusive, while a thread wants or holds every branch lock | Letting one thread of the machine at a time want them |
| 2^62 + 7, door | Exclusive, while a thread that wants the gate draws a turn | Handing out turns one at a time |
| 2^62 + 9, recovery | Exclusive, by every writer, with the option or without, from before it takes the writer lock until it has recovered, or for a shared writer until it finds another alive | Telling a read-only open that finds a writer alive whether the newest superblock may still name frames a rollback drops |
| 2^62 + 2^19 and every other byte of the 2,048 from there, turn | Exclusive, one per thread queued for the gate, through its open, from its draw until its hold ends | Letting takers take the gate in the order they drew |
| 2^62 + 3 · 2^18 and every other byte of the 2,048 from there, phase | Exclusive, one at a time, by the gate's holder, from one drawn at random to the next at the end of each round; only ever tested by writers | Holding off new branch locks until the end of a phase |
| 2^62 + 2^20 + a 60-bit xxh3 of the branch name | Exclusive, per closure | A branch's closure, across processes |

A cycle's prune and pause, the landing of a `receive` and anything else that replaces every
branch's head lock the whole range of branch bytes at once, as they take every lane within a
process, and hold the gate and a phase byte while they try for them and hold them, as [The
gate](#the-gate) describes.
Locks are taken in the order lane, branch lock, `tip`, placement lock, and the leader of a sync
takes the superblock lock holding none of them. A writer passes the gate before its lane, holding
no lock of the file, and the whole range is taken after every lane and the gate, and the gate
after a turn. The cycle lock comes before all of them, and the recovery lock before the writer
lock and every other.

The byte-range locks are open file description locks, `fcntl` with `F_OFD_SETLK` and
`F_OFD_SETLKW`, which only Linux has, on 64-bit targets where `off_t` holds the offsets. Classic
POSIX record locks would not do: they belong to the process, so threads of one process never
conflict on them, and closing any descriptor of the file drops all of them, which a read-only handle
or a retired generation does. An open file description lock belongs to the open file, so two shared
`Db`s of one file in one process exclude each other as two processes do, and the tests use that.
The kernel releases them when a process dies, which is what lets the others go on. On every other
target, Windows included, an open with the option fails with an `Unsupported` I/O error, as
described under [Limits](#limits).

### One owner per lock in a process

Every thread of a process locks through the same open file description, and a lock it already holds
is granted again at once, as a no-op, and released by whichever thread unlocks it first. So each
lock has an owner within the process as well. The placement, superblock and cycle locks and the
gate are taken through a table of the ranges a thread of this process holds: a thread waits on a
condition variable until no other thread holds the range, takes it from the kernel, and on release
unlocks it in the kernel before it frees the entry. The first version had none.
`Tip`, the guard that holds
`tip` and the placement lock, released `tip` first, so a second thread took `tip` and was granted
the placement lock as a no-op, and the first thread's release then unlocked it while the second was
placing. Another process placed a frame at the same offset, which ended in `StalePointer`, hash
mismatches and a file that no longer opened.
`threads_of_two_handles_on_branches_of_their_own_place_in_turn` runs four threads on branches of
their own in each of two handles of one file and failed on its first round. The guard now drops the
placement lock first as well, and the receive's hold on the cycle lock likewise, but neither order
matters any more.

Branch bytes are shared within a process on purpose: two branch names can hash to one byte, and a
process counts how many of its lanes hold each byte and unlocks it when the last one lets go. That
costs only parallelism, except that with nested writes across branches two such names could
deadlock two processes. With 60 bits, a thousand branch names share a byte with odds of about 4 in
10^13. Taking the whole range is exclusive with them: under `shared` a thread takes it only at a
moment when no thread of its process holds a byte, under the lock of that count, and marks it held,
so no thread of the process takes a byte until it lets the range go. Without `shared` there is no
range, and a cycle takes every lane in name order. An earlier version marked the range wanted and
waited for the process's threads to let their bytes go while it held every lane, and gave up when
the set of attached branches changed, since a thread holding a byte for a branch attached after the
cycle listed its lanes could be waiting for one of them; a thread that wants the range now holds
nothing while it waits and lists the lanes again at each try. In the first version an awaited
write's waiting thread was granted a branch byte as a no-op inside its own process's cycle, then
unlocked it, and another process's closure ran during the pause and failed with `the file was
replaced while a closure held its branch`.
`an_awaited_writes_wait_never_opens_a_branch_inside_a_cycles_pause` reproduces it.

So the cycle's lanes must cover every branch byte the process can hold for long, and attached
branches are not all of them: a coalesced group keeps its branch's byte between closures, and a
branch can stop being attached under it. A closure that took the byte on a branch another process
had dropped, and caught up, retired the branch and then joined a group on it anyway, and the group
stayed open while a sync ran. The cycle took every attached lane, which no longer included that
branch, and waited for its byte; the group's member was parked until the sync thread sealed it,
and the sync thread was waiting for a lane the cycle held. A stress run hung about once in 25 to 50
runs of 20 rounds on two CPUs. Now a closure checks its branch is live again after taking the byte
and catching up, and lets the byte go and fails with `Error::NoBranch` if it is not, before it
runs (`a_closure_on_a_branch_another_handle_dropped_never_runs`), and a cycle also takes the lane
of every branch with a group open, attached or not, which seals the group.

### The gate

The kernel grants a lock on the whole range only at a moment when every byte of it is free, and it
holds back no byte that another process asks for while the range is wanted: a waiting
`F_OFD_SETLKW` is woken when a lock in its way goes, tries again, and sleeps again if another byte
is held by then. Within a process the `whole` mark keeps new takers out, but across processes
nothing did, and a writer holds its branch's byte for nearly all of each commit, 93% of the time
in the measurements under [Reading back what other processes
wrote](#reading-back-what-other-processes-wrote). So writers in other processes, each on a branch
of its own, kept the range from ever being free at once. With four handles of one file committing
on branches of their own, a cycle reached its pause 0.28 s after it began and then waited 10.7 s,
until the writers were stopped, and the same with four processes; a pull's landing and a prune
waited the same way. The cycling process held every one of its own lanes while it waited, so its
own writers waited as long (#128).

So a thread that wants every branch lock, for a cycle's prune or pause, a landing or a `Keep::Last`
prune, holds a gate that keeps writers from taking new branch locks while it waits. Any wait it
adds has to end, because a closure can wait for another thread's write in a way ferro cannot see:
by joining a thread that commits, by polling an awaited commit on another thread, or through a
channel, and that write may go through any handle of the file, the waiting thread's own included.
The gate promises six things:

1. No cycle of waits that ferro can see lasts.
2. A write that a closure waits for waits at the gate for at most two rounds of the hold, as below,
   and never more than 2 s, through any handle.
3. The thread that wants every branch lock gets them beside writers in other processes and
   threads, its own handle's included, within about three times C and 4 ms, where C is how long
   the writers that the end of a phase lets on keep their branches, as long as C is under a
   second.
4. Without `shared` none of it runs, and lanes behave and cost what they did.
5. A writer that finds the gate free pays one relaxed load and one `fcntl`.
6. Threads that want every branch lock get the gate in the order they asked for it, whatever
   handle they ask through, so none waits for more than the holds of those ahead of it, and a
   pull's landing and integration hold it one after the other. A taker stopped while it waits
   holds up the takers after it for 10 ms at most.

The thread that wants every branch lock, the holder, first takes the gate byte, 2^62 + 5, exclusive,
through the table of [One owner per lock in a process](#one-owner-per-lock-in-a-process), so one
thread of the machine holds the gate of a file at a time. It queues for it in the order it came,
as [Takers in turn](#takers-in-turn) describes, by a turn among every thread of every handle of
the file that wants it. If it had to wait, for a turn or for the gate byte, it waits 1 ms more,
the longest sleep of a writer between tests
of its phase byte, so that the writers the last hold let on take their branch bytes before this
hold tries for them. It then holds one of 1,024
phase bytes, every other byte from 2^62 + 3 · 2^18, starting at one drawn at random, and gives
the phase a number in its process that no phase there had before, which its process's writers
wait on, since a process never sees its own locks. It tries for everything at once between sleeps that
double from 20 µs to 1 ms, and to at most an eighth of the round: it seals any coalesced group whose
lane is free, since a group keeps its branch's byte, tests without taking anything whether a thread
of its process or another process holds a branch byte, and only if none does takes every lane of
its handle with `try_lock`, all or none, and then the range with `F_OFD_SETLK` under the lock of
its process's count of bytes, so no thread of the process takes one meanwhile. Between tries it
holds nothing but the gate and the phase byte, and a cycle's lock: no lane, no branch byte and no
`whole` mark. Its rounds last 4 ms, then twice as long as the one before, up to a second. At the end
of each it ends the phase: it locks the next phase byte, lets the one it held go and wakes the
writers of its process with the next number. Once it has every lock it catches up, runs its prune,
pause or landing, and lets the range, its lanes, the phase byte and the gate go.

A writer whose thread holds no lock of the file tests the gate before it takes its lane: a relaxed
load of its process's flag for a holder there, and one `F_OFD_GETLK` over every phase byte. If
none is held it goes on. If one is, it waits until the phase it found ends, on a condition
variable for its own process's phase number, or testing that phase byte after sleeps that double
from 50 µs to 1 ms until it is let go. If its branch's byte is then held, in its process or another, it
waits for the next end too, once in each pass, since the closure holding the byte may be waiting
for a write that the end just let on, and a writer queued behind it would hold the byte again as
soon as the closure let it go. Whatever the holder does, it goes on after 2 s. It holds no lock of
the file while it waits, and goes on to its lane and byte as before.

The test reports the start of the lock it finds, and that is the phase byte held. The kernel merges
the locks one open holds on bytes next to each other into one, so the phase bytes lie every other
byte, far from the gate byte and from every other lock: the first build of the two-byte design
read a test over both phase bytes as the start of the holder's gate byte and phase byte merged,
waited for the gate byte, which the holder keeps for its whole hold, and every commit a closure
joined beside a cycle that the closure held up waited the full 2 s. While the holder moves to the
next byte it holds both for a moment, and of the locks one open holds the kernel reports the one
that starts first, the old one, but at the wrap from the last byte to the first, where a writer
that tests in that moment waits for one phase more, once in 1,024 ends.

Phases have to be told apart across holds as well as within one. A writer waits for the byte it
found to be let go, testing it at most a millisecond apart, and a hold that gets every branch in
its first round lasts well under that; with every hold starting on the same byte, as the two-byte
design did, the next hold, by the same taker or by another that was waiting in the kernel for the
gate, took that byte again within microseconds, and the writer slept through the end it waited for
and on through later holds until it happened to test in a gap, or for 2 s (#147). Starting each
hold on a byte drawn at random makes that one hold in 1,024, which costs the writer that hold,
and the numbers in the holder's process never repeat. The 1 ms a holder waits after a gate it had
to wait for keeps holds that queue for the gate from following one another with no gap at all, in
which the writers each end lets on would take their bytes just as the next hold tried for every
one, and that hold would wait out its first round for them: with four handles pulling over and
over, a writer in a fifth committed fewer than 4,000 times in 3 s without it, at the same rate
as before the change to the starting byte.

An awaited write takes its lane first, as it does, and tests the gate and its byte without waiting.
If either is held it lets the lane go and returns `Pending`, and its waiting thread waits at the
gate as above and then for the byte. Once held up there it goes to its byte on later polls without
the gate, as a blocking write that passed the gate does. A write whose thread holds a lock of the
file, through any handle, goes straight to its byte, as before: a nested write to a branch that
sorts after the closure's own, or anything under the whole-file record. The holder may be waiting
for that thread's byte, so a wait there would only hold both up for a round. The reentrancy record
of each thread, described in [Writes from inside a closure](reentrancy.md), says whether it holds
such a lock: each entry names its file by an xxh3 of the canonical path as well as its handle.
Locks on other files do not count, since the holder waits only for this file's bytes. An earlier
version let a write from a closure on another file go straight on too, and four writers that
committed in a loop inside closures on files of their own kept five cycles from finishing for 20 s,
as before the gate (#136). The path is the one the file was opened by, canonical, so a cycle's
rename of the new file over it keeps the id. Two paths to one file, through a bind mount, give two
ids, and a thread holding a lock through one then waits at the gate through the other, for at most
two rounds; two files whose paths hash alike only let a thread skip the gate. The sync thread and
the seal of a group take no new branch byte and never pass the gate.

Why each promise holds:

1. The holder waits for nothing while it holds anything a writer needs. It waits for a cycle's lock,
   for the door, for the turn before its own and for the gate byte, each held only by another
   cycle or taker, and then sleeps a millisecond, and holds nothing of the branches while it does;
   everything after that it tries without waiting. A pull that keeps its turn between its landing
   and its integration runs only ferro's own code between them. A writer waiting at
   the gate holds no lock of the file, and the end of its phase comes on the holder's clock
   whatever the range does, or after 2 s. A lane or a byte is held only by a closure, a nested write within one, a coalesced
   group, which the holder or the sync thread seals, or a holder in its pause, which waits for
   nothing but `tip` and the placement lock, which nobody holds while waiting for a lane. So every
   cycle of waits runs through a wait ferro cannot see, and those listed under [What stays
   undetected](reentrancy.md#what-stays-undetected) aside, it comes undone at the next end of a
   phase, when the write the closure waits for goes on.
2. A write that a closure waits for waits at the gate until the end of its phase, a round at most
   4 ms longer than the hold had lasted when the write came, and at most a second, and once
   more if its branch is busy then: at most two rounds and 2 s. It then waits for its lane and byte
   as any write does, which only the closures ahead of it on its branch and a holder in its pause
   hold, since the holder takes no lane before it has everything. That holds through the holder's
   own handle too, where the write waits on the count of phases.
3. While the gate is held, writers take new branch bytes only at the ends of phases. The exceptions
   end on their own: a nested write ends with its closure, a writer that passed the gate before the
   holder took it takes its byte once, and a group is sealed. The writers that one end lets on
   begin together, and each branch they take is held for at most C after that end: the longest
   closure times the writers that queue for one branch, which cannot grow, since a writer whose
   branch is held at the end of its phase waits for the next. Once a round outlasts C and a sleep,
   every branch is free before the round ends, the holder's next try finds them so, and since no
   new writer goes on until that end it gets them. The rounds double, so the holder gets them
   within the first round after the closures already running end when C is under 4 ms, and within
   about 3C and 4 ms when C is longer, up to a second. Writers of the holder's own handle wait on
   its count of phases and are let on at the same ends, so they are held to the same bound.
4. Without `shared`, `Shared::exclusively` takes the lanes in name order, waiting for each, and
   nothing tests a gate, as before. The lanes themselves are the same `parking_lot` mutexes,
   waited for without a timeout.
5. A writer reads its process's flag, and finds no holder there, and makes one `F_OFD_GETLK`.
6. Every thread draws its own turn when it asks, after every turn held, its own handle's included,
   and waits for the turn drawn just before its own, so no taker that asks later gets the gate
   first, as [Takers in turn](#takers-in-turn) describes. With n takers queued each waits for at
   most the n holds ahead of it, each bounded as in 3. A pull's landing keeps its turn, and its
   integration goes straight to the gate byte. A taker that holds a turn and does not run is passed
   10 ms after the turn before its own and the gate came free, and the door is waited for 10 ms at
   most.

What stays unbounded:

- Closures that keep their branches longer than a second, C over the longest round, in several
  threads or processes. Each round lets on writers whose closures outlast it.
- Closures that each wait for another thread's write that the gate holds up, begun by several
  threads at different ends of phases. Each such closure that one end lets begin holds its byte
  until the write it waits for goes on at the next end, by when another has begun, so once two
  threads are out of step the range need never be free at once. The writes still wait at most two
  rounds each, so nothing hangs, but the holder waits until such closures stop or fall into step.
  None of the designs below gets the range there: the writes that end those closures and the
  writers that begin new ones look alike at the gate, and only telling them apart would let the
  first go and hold the others. A closure that writes to another branch itself, rather than through
  another thread, is a nested write and is not held at all.
- A holder stopped in its process, by a debugger or `SIGSTOP`, keeps writers at the gate for 2 s at
  a time, and the takers queued behind it for as long as it stays stopped, since it holds the gate
  byte. A taker stopped while it waits for its turn, or between a pull's landing and its
  integration, holds its turn but not the gate: the taker that drew the turn after it passes it
  10 ms after the gate and the turn before it came free, and writers go on, since it holds no
  phase. While it stays stopped, each taker that draws the turn right after its own, as the first
  to ask once the others have gone, waits those 10 ms again. One stopped while it holds the door,
  for the few tests a draw makes, keeps the takers that ask meanwhile from drawing a turn for
  10 ms, and they then go to the gate byte without one, in the kernel's order. A stopped taker that
  goes on takes the gate in the kernel's order with any others that went without a turn.
- Writers of earlier builds. One that tests the gate byte only, as 6fb68bc's did, finds a holder of
  this build there for its whole hold, reads no stamp and so goes on at the end of each 2 ms epoch.
  A holder of an earlier build holds the gate byte, and writers of this build, which test the phase
  bytes, pay it no heed, while a holder of this build waits in the kernel for that gate byte until
  the earlier holder is done. Writers of b2741a3 test only its two phase bytes, which a holder of
  this build never takes, and writers of this build pay no heed to them. A shared writer from
  before the gate pays no heed to either. Beside one, the range can wait as it did before the gate.

The gate and the phase bytes keep nothing in the file, a process that dies holding them lets them go
with its other locks, and a test of the gate that fails lets the writer on, since it only keeps the
range from waiting for ever. Within a handle a second thread that wants every branch lock, a pull
beside a cycle, waits in the handle's line for the first to finish.

#### Takers in turn

The gate byte alone left the order of takers to the kernel and to the table of [One owner per lock
in a process](#one-owner-per-lock-in-a-process), and neither keeps one. The kernel links a blocked
`F_OFD_SETLKW` below the lock in its way, and below the first waiter already linked there that it
conflicts with, so the waiters for the gate byte form a chain. An unlock wakes only the first, and
each waiter woken tries again when it runs, wakes the one linked below it and is linked again under
whatever is then in its way. A thread that runs meanwhile, such as the taker that just let the gate
go and comes back for it, takes the byte before the waiter it was handed to, and a newcomer that
blocks is linked ahead of the waiters not yet relinked. A pull takes the gate for its landing and
again for its integration, and came back each time. The table's condition variable wakes its
waiters the same way, one at a time through its mutex, and the thread that let the byte go took it
again first. Beside four processes committing in 5 ms closures, sixteen processes pulling over and
over made from 10 to 156 pulls each in 5 s, and some waited about a second for one; sixteen threads
of one handle waited up to 911 ms (#149).

So a taker now draws a turn among every thread of every handle that wants the gate, and waits
for the turn before its own. Holding the door, 2^62 + 7, and a lock its handle's threads share
while they draw, it locks the turn byte after the last one held, of 1,024 turn bytes every other
byte from 2^62 + 2^19, lets the door go, and waits for the turn before its own to be let go. A
handle never sees its own locks in a test, so each handle also keeps the turns its threads hold,
which a draw counts with those other handles hold, and a thread whose turn before its own a thread
of its handle holds waits on a condition variable for that thread. The last turn held is found by
halving: a test over every turn byte reports one held, and tests over the turn bytes past it,
counting the handle's own, find the furthest held within half of them, about ten tests while
others are queued and none more when no turn is held. The thread lets its turn go after the gate
byte, at the end of its hold.

The first build of turns, e11aab6, drew one for each handle, by the thread at the front of a line of
the handle's threads in the order they asked. A thread queued in its handle's line drew only once
the threads ahead of it in that line had held the gate, after every handle that asked meanwhile, so
a handle with k takers in it got one turn for each of the others' and each of its threads waited
about k rounds of every other handle (#155): in the model's calm runs, 8 threads of one handle
beside 8 handles pulling made 3 pulls each in 3 s to the others' 26, and the longest pull took about
a second, where b919d64's kernel order gave 8 to 15.

It also held a turn while it waited in `F_OFD_SETLKW` for the one before it, and a process stopped
by `SIGSTOP` or a debugger lets no lock go. The kernel takes a stopped process's waiting
`F_OFD_SETLKW` out of its chain of waiters, so before turns a taker stopped while it waited for
the gate byte held up no other; with them, a taker stopped while it held a turn held up every
taker that drew after it for as long as it stayed stopped, and so did one stopped between a pull's
landing and its integration, or while it held the door. A cycle in another handle waited 5 s for a
process stopped while it queued for a pull, and went on within a few milliseconds once the process
did.

So a taker now waits for the turn before its own, when another handle holds it, by testing it
between sleeps that double from 50 µs to 2 ms, or are 50 µs once the turn before that one is free
and it is next. A taker that holds a turn takes the gate once the turn before its own and the gate
are free, within a sleep, so a turn that stays held for 10 ms while both are free belongs to a taker
that is not running, and the taker after it passes it, keeping its own turn so that the takers after
it still wait for it. A live taker preempted for longer than that just as it should take the gate is
passed too, and takes the gate in the kernel's order once the taker that passed it lets it go, which
costs it one hold; the unit tests, which check the order of takers that run while `make test-stress`
runs copies of them, pass after a second instead. The door is tried between sleeps for 10 ms at
most, since it is held only for the few tests of a draw, and a thread that cannot get it goes on to
the gate byte without a turn. The only wait left in the kernel is for the gate byte, which a taker
stopped while it waits does not hold and a stopped holder holds anyway. The model tried holding a
mark byte for each turn while it held the gate, so that a waiter could sleep in the kernel on the
holder's mark and test only between holds; it pulled no more often and its longest pulls were
longer.

A pull would then queue for its integration behind every taker that came during its landing, and
wait 1 ms more after it: beside one writer, two processes pulling made fewer than half as many
pulls as before in one run. So the landing keeps its turn for the integration, which goes straight
to the gate byte, through a guard that the pulling thread holds from the landing until the
integration's decision is made, before the merge closure runs. A `receive` keeps it the same way.
A large one streams its staging frames, and syncs them or places them anew, before its landing
queues for the gate, so it holds no turn while it does; between the landing and the decision it
only gives back the disk of staging frames it placed anew, well within the 10 ms after which the
taker after it would pass it.

Turns only order the takers; the gate byte still lets one hold at a time. A draw that fails, from
an error, a door held for 10 ms, or because the byte after the last turn held is held too, which
takes more than 512 takers queued at once, lets the thread on to the gate byte without a turn. A
process that dies holding a turn lets it go with its other locks, and the taker behind it goes on
to the gate byte. A taker of an earlier build draws no turn and may take the gate byte ahead of
those queued, as before.

The model in [The gate model](testing.md#the-gate-model), with the kernel's chains of waiters in
it, finds b919d64's order unfair in all three of #149's runs and in #155's mix of handles, a line
in each handle fair only among the threads of one handle, a turn for each handle unfair to threads
that share a handle beside other handles, and a turn for each thread fair in all four. It stops a
taker for 2 s while it waits, while it holds the door and between a pull's holds, and finds every
pull beside it waiting those 2 s under turns that wait in the kernel, and none waiting longer than
a pull takes without it under turns that pass.

#### Earlier versions

Five designs before this one each fixed what a review of the last found, and each review found new
hangs or stalls:

- The first waited at the gate until it came free, by taking it shared, and hung: a closure on `a`
  that joins a thread committing on `b`, or a thread that polls an awaited commit on `b`, holds the
  byte of `a`, and the other thread, which holds no lock, waited at the gate of a process that was
  waiting for the byte of `a` (#136).
- The second bounded each writer's wait by how long the writers of its handle had found the gate
  held, from 2 ms to 1 s, counted from when each writer came. Writers that came first waited least,
  so their closures began further apart with each round until the bound reached a second, and from
  then each waited a second after its own closure and their closures never all ended together: a
  cycle beside four threads of one handle whose 600 ms closures began 150 ms apart had not finished
  after 30 s. Its record of when the gate was found held also outlived the hold, so a commit that a
  closure joined waited 1.0 s at a gate held for 100 ms. A holder that took its own lanes and then
  waited in the kernel for the range hung a commit through its handle that a closure in another
  handle had joined (#142).
- The third, 6fb68bc, let writers on together at the ends of epochs of `CLOCK_MONOTONIC` that grew
  with the hold, read from a stamp byte the holder locked, and had the holder give its lanes to its
  own waiting threads after three epochs of trying. Its patience was counted from the hold as it
  grew while it waited, so it gave way only once the epoch reached a second: beside four threads of
  the cycling handle on one branch and four handles with 20 ms closures, cycles took 7.1 s. It gave
  way only while it tested the range, and hung holding lane `a` while it waited for lane `b`, when a
  write on `b` through its handle waited in the kernel for a closure on `b` in another handle that
  had joined a commit on `a` through it. A writer that read its clock just before a new holder
  locked its stamp read the hold as 17 minutes old and waited a second (#145). Closures a little
  longer than an epoch kept the range busy across several.
- The fourth, 51dbfc6, held the gate in growing rounds and let it go for 1 ms between them, and
  took the holder's own lanes with a 1 ms timeout in every mode. Without `shared`, a cycle or pull
  could no longer get a lane that two or more threads of its handle took in turn, since each timeout
  queued it behind them again: 3 threads with 5 ms closures kept a cycle waiting over 20 s. Each
  opening let the next closure of a chain through by the lane's hand-over before the holder tried
  again, so closures that joined commits back to back kept a cycle waiting 18 s, and its rounds grew
  without a cap, so commits joined through the cycling handle waited up to 16.4 s. Nor did it seal a
  coalesced group before it waited for the next lane, so the group kept its byte of `a` while a
  closure on `b` waited for a commit on `a` through another handle, a hang 6fb68bc had too (#146).

- The fifth, b2741a3, held one of two phase bytes, starting every hold on the first, and numbered
  the phases in its process from that byte at each hold. A hold that got every branch in its first
  round ended within a millisecond, and the next, whether the same taker's or one waiting in the
  kernel for the gate, took the same byte again within microseconds, so a writer that tested its
  byte a millisecond apart missed the end it waited for and waited out later holds. Beside four
  handles pulling over and over, each pull two holds back to back, a writer in a fifth committed
  900 to 2,300 times in 3 s, against 52,000 to 72,000 with 6fb68bc. A writer also found its phase
  with a test over both bytes and then a test of the first alone, and one that the holder moved
  between the two waited for the next phase as well (#147).

This design keeps what worked: writers held up go on together, so the range comes free between the
closures one end lets on and the next end, as epochs did, but at ends the holder makes, so no
writer reads a clock or a stamp, and no end lets on a writer that came after it, as an opening of
the gate did. The holder taking nothing of its own while it waits is what ended the hangs that ran
through its lanes in #142, #145 and #146, and the wait for one more phase when a branch is busy is
what keeps a chain of closures on one branch from passing the range between them. Phase bytes drawn
at random, and phase numbers that never repeat, tell a writer that the hold it found ended however
soon the next begins. The model in [The gate model](testing.md#the-gate-model) runs every design
above but the first two against each scenario of those issues and finds each failure its review
found, and none in this one.

#### Tests of the gate

`ferro/tests/shared.rs`, `ferro/tests/gate.rs` and `ferro/tests/shared_process.rs` test the gate,
and [The gate model](testing.md#the-gate-model) tests the designs:

- Takers beside writers.
  `a_cycle_finishes_while_other_handles_commit_on_branches_of_their_own` runs five cycles beside
  four handles committing on branches of their own,
  `a_cycle_finishes_while_writers_in_closures_on_other_files_commit_on_branches_of_their_own`
  does it with each writer's commits inside a closure on a file of its own, and
  `a_handles_own_commits_wait_for_its_cycle_only_while_other_handles_finish_theirs` makes 50
  commits on a branch of a handle that cycles over and over; the first and third waited past 20 s
  without the gate, and the second with its first version.
  `a_cycle_finishes_beside_threads_of_one_handle_whose_600_ms_closures_end_at_staggered_times`, and
  `a_cycle_finishes_while_other_processes_commit_on_branches_of_their_own` and
  `a_cycle_finishes_beside_processes_whose_threads_hold_600_ms_closures_ending_at_staggered_times`
  in `shared_process.rs`, do it beside threads and processes; the staggered ones had not finished
  after 20 s with the second version.
  `cycles_beside_threads_of_the_cycling_handle_contending_for_one_branch_take_under_a_second` took
  7.1 s with 6fb68bc, and
  `a_shared_cycle_gets_a_lane_that_threads_of_its_handle_take_in_turn_with_5_ms_closures` over
  20 s with 51dbfc6.
  `a_cycle_and_a_pull_in_two_handles_both_wanting_every_branch_finish_beside_writers` and
  `cycles_and_pulls_in_one_handle_all_finish_beside_other_handles_committing` run two takers at
  once.
- Writes that closures join.
  `a_closure_joining_a_thread_that_commits_on_a_later_branch_finishes_beside_another_handles_cycle`
  and `a_closure_joining_a_thread_that_awaits_a_commit_finishes_beside_another_handles_cycle` hung
  with the first version,
  `a_closure_joining_a_commit_through_the_handle_that_cycles_finishes_beside_its_cycle` and its
  awaited form until #142, and
  `a_closure_joining_a_commit_on_an_earlier_branch_finishes_beside_a_blocked_write_on_its_own`,
  its awaited form, and
  `a_closure_joining_a_commit_on_an_earlier_branch_of_its_handle_finishes_beside_its_cycle`, with
  and without `coalesce`, with 6fb68bc.
  `a_joined_commit_waits_at_the_gate_no_longer_than_the_hold_after_an_earlier_wait_ran_out` waited
  655 ms with the second version's record. The five `a_joined_commit_beside_cycles_stays_short_*`
  tests join 40 commits from 20 ms closures back to back beside a handle cycling as fast as it can,
  on an idle machine, 5 ms apart, beside two spinning threads a core and awaited, every one under
  2 s and nine in ten under 100 ms on a quiet machine, as [Timed
  checks](testing.md#timed-checks) describes, where 51dbfc6 kept a cycle waiting 18 s, and
  `closures_joining_commits_through_the_cycling_handle_back_to_back_wait_a_bounded_time` does it
  through the cycling handle, where 51dbfc6's commits waited up to 16.4 s.
  `forks_and_drops_joined_from_closures_finish_beside_cycles` joins forks and drops, and
  `nested_and_awaited_writes_go_on_through_another_handles_cycles` runs twenty cycles beside writes
  nested in closures, which had not finished after 60 s without the gate.
- `a_coalesced_cycle_holding_an_open_groups_byte_lets_a_joined_commit_through_another_handle_land`
  hung with 6fb68bc and 51dbfc6.
- Holds back to back. `a_writer_beside_four_handles_pulling_back_to_back_waits_only_for_the_hold_it_finds`
  and `a_writer_in_a_handle_pulling_back_to_back_in_four_threads_waits_only_for_the_hold_it_finds`
  commit beside four threads pulling over and over, in handles of their own or the writer's; with
  b2741a3 the first made 900 to 2,300 commits in 3 s with a p99 of 18 to 33 ms (#147).
- The bytes. `a_writer_waits_at_the_gate_until_the_phase_another_open_holds_ends`,
  `a_writer_waits_for_the_phase_byte_it_finds_until_the_holder_moves_to_the_next` and
  `a_writer_goes_on_past_a_phase_that_never_ends_within_a_bounded_time` hold phase bytes from a
  bare open of the file, and
  `a_cycle_that_panics_in_its_pause_leaves_neither_the_gate_nor_the_branch_range_locked` and
  `a_coalesced_cycle_that_panics_in_its_pause_leaves_its_lanes_the_bytes_and_the_gate_free` check
  that a panic lets everything go, and `writers_held_at_a_pause_that_panics_go_on_as_it_unwinds`
  that the writers it held up go on. In `shared_process.rs`,
  `a_writer_held_at_the_gate_of_another_process_goes_on_when_that_process_dies` kills a process
  whose cycle holds the gate.
- Takers in turn. The unit tests
  `opens_that_want_the_gate_get_it_in_the_order_they_asked_before_one_that_just_let_it_go` and
  `threads_of_one_open_that_want_the_gate_get_it_in_the_order_they_asked` in `store.rs` queue three
  takers, through opens of their own or through one, behind one that holds the gate, which lets it
  go and asks for it again at once and must come last; with b919d64's order the open took it back
  first. `holds_bound_together_take_the_gate_again_before_those_queued_and_then_let_them_on` checks
  that a landing's place and turn carry over to the integration and go with the guard. The ignored
  `puller_processes_beside_writer_processes_report_how_often_each_pulls`, in `shared_process.rs`,
  and `threads_pulling_beside_writers_report_how_often_each_pulls` print how often each of many
  pullers pulls and its longest pull, and `cycles_beside_writer_processes_report_their_times` the
  cycles beside writer processes, as [What the gate costs](#what-the-gate-costs) gives them.
- Without `shared`.
  `a_plain_cycle_gets_a_lane_that_threads_of_its_handle_take_in_turn_with_5_ms_closures`,
  `a_plain_cycle_gets_a_lane_that_two_threads_of_its_handle_take_in_turn_with_2_ms_closures`,
  `a_coalesced_plain_cycle_gets_a_lane_that_threads_of_its_handle_take_in_turn` and
  `a_plain_pull_lands_beside_threads_of_its_handle_taking_one_lane_in_turn` took from 1.7 s to over
  20 s with 51dbfc6's timeouts, and
  `plain_cycles_beside_writers_on_branches_of_their_own_report_their_times` prints cycle times
  beside 8 and 16 writers.

#### What the gate costs

The test costs a writer one `fcntl` a commit. On 2026-11-09, when the gate was first added, pinned
to eight cores of the 32-thread machine while other work ran on the rest, alternating with the
build before, under Buffered: `group` with one shared writer made 44,445 single-key commits a
second against 45,277, median of five rounds, 1.8% fewer, and with eight threads 38,160 against
37,993. In `processes`, 2, 4 and 8 processes on one branch made 35,241, 29,757 and 24,646 commits a
second against 35,776, 29,829 and 24,644, and on branches of their own 66,422, 101,017 and 59,471
against 67,596, 101,061 and 59,540.

The phase gate adds a relaxed load of the process's flag to that path. It was measured on
2026-11-09 against the build before it, which ran 6fb68bc's epochs, pinned to eight cores while
other work ran on the rest, the two builds alternating, under Buffered:

- Without `shared`, which runs none of it, `group` with 1, 8 and 16 writers on one branch made
  49,710, 41,960 and 40,013 single-key commits a second against 49,784, 42,123 and 40,099, and on
  branches of their own 49,375, 164,240 and 142,921 against 49,767, 164,011 and 143,305, medians of
  five rounds; the one cell 0.8% apart, one writer on a branch of its own, came out at 49,896
  against 50,152 over six more rounds, from 49,739 to 50,446 against 49,040 to 50,532. Ten cycles
  beside 8 and 16 writers on branches of their own whose closures slept 2 and 5 ms took 22.7, 67.3,
  42.0 and 103.5 ms at the median against 23.3, 65.9, 41.3 and 109.6. `ferro-bench`'s Buffered
  single-key commit took 23.67 µs against 23.81 µs over three rounds, at 178,591 instructions against
  178,610, and its Fsync commit 407 µs against 399 µs, from 394 to 439 against 399 to 445.
- With `shared`, `group` with 1 and 8 writers made 44,751 and 38,630 commits a second against
  45,008 and 38,627, medians of five rounds, and `processes` at 2, 4 and 8 processes, on one branch
  and on branches of their own, and the same writers as threads, came within 0.7% of the build
  before at every count, medians of three.
- Five cycles, each a prune and a pause, beside handles committing on branches of their own in
  closures that spun 0, 1, 2 and 5 ms took 5.8, 4.5, 6.4 and 12.8 ms at the median of three rounds
  beside 4 handles, against 4.7, 6.2, 7.8 and 13.4, and 10.5, 14.2, 17.5 and 32.4 ms beside 16,
  against 10.8, 19.0, 29.3 and 57.6. Beside processes they took 4.2, 5.4, 6.4 and 12.7 ms beside 4,
  against 4.3, 7.0, 8.0 and 13.5, and 12.2, 16.3, 20.4 and 58.5 ms beside 16, against 11.4, 19.8,
  32.8 and 61.3. The spread between rounds was up to 30% either way at 0 ms, where the cycle waits
  only for closures already running.
- A commit held up waits for the end of a round rather than of a 2 ms epoch, so beside 4 handles
  with 1 and 2 ms closures the longest commit took 7.5 and 8.4 ms against 4.2 and 5.0. A first round
  of 2 ms instead of 4 made neither the longest commit nor the cycles shorter.

The phase bytes drawn at random and the 1 ms after a gate the holder waited for (#147) were measured
on 2026-11-09 against b2741a3 and against the build that ran 6fb68bc's epochs, pinned to eight
cores, the three builds alternating, medians of three rounds, under Buffered:

- Beside four handles pulling over and over, a writer in a fifth committed 156,208 times in 3 s,
  with a p99 of 19 µs, against 561 with 22.6 ms and 32,638 with 2.0 ms; with the four pulls and the
  writer in one handle, 190,194 times against 2,004 and 2,410. The pulls themselves ran 884 times
  in all against 1,123 and 2,728, and in one handle 759 against 1,045 and 761. With no wait after a
  contested gate they ran about as often, from 946 to 1,213 in four runs, and the writer committed
  1,044 to 2,552 times; with 250 µs it committed 23,324 to 28,120 times and with 500 µs 80,289 to
  86,377.
- Beside two handles pulling, the writer committed 191,034 times against 1,933 and 133,822, b2741a3
  making 108,604 in one round of three; the pulls ran 1,251 times against 1,433 and 3,596. A pull
  that waits for the other's hold now waits 1 ms more each time, so two takers that keep taking in
  turn run about as often as with b2741a3 and a third as often as with 6fb68bc, which let writers
  on every 2 ms whatever its takers did.
- Five cycles beside 4 handles with closures that spun 0, 1, 2 and 5 ms took 0.2, 1.7, 2.3 and
  5.5 ms against 0.1, 1.3, 2.3 and 5.7 and 0.1, 1.7, 0.2 and 0.2, and beside 16 handles 1.0, 4.0,
  10.3 and 13.8 against 0.9, 3.7, 9.2 and 25.5 and 1.1, 7.7, 14.4 and 30.8. Beside processes they
  took 4.5, 4.5, 6.6 and 10.4 ms beside 4, against 5.2, 4.4, 6.6 and 11.0 and 4.7, 5.0, 8.2 and
  13.4, and 11.4, 12.6, 12.7 and 46.7 beside 16, against 9.0, 11.4, 14.4 and 56.2 and 10.2, 20.4,
  29.9 and 62.6.
- `group` with 1 and 8 shared writers made 47,165 and 39,987 commits a second against 45,595 and
  40,539 and 46,510 and 40,640, and `processes` at 2, 4 and 8 came within 1.8% of both at every
  count, on one branch and on branches of their own. Without `shared`, `group` with 1, 8 and 16
  writers made 51,825, 44,088 and 42,890 on one branch, against 50,807, 43,405 and 42,511 and
  51,747, 44,513 and 43,273, and 52,247, 164,678 and 142,592 on branches of their own, against
  51,548, 153,962 and 139,728 and 51,574, 162,451 and 140,895; ten plain cycles beside 8 and 16
  writers whose closures slept 2 and 5 ms took 20.2, 50.6, 39.0 and 95.9 ms against 20.6, 55.7,
  37.1 and 91.2 and 20.4, 55.4, 38.8 and 95.8.

The line and the turns (#149) were measured on 2026-11-11 against b919d64, the two builds
alternating, beside a stress run of the fault tests that kept the load average between about 4 and
27 on the 32-thread machine, under Buffered, three rounds unless said, with the ignored tests
`puller_processes_beside_writer_processes_report_how_often_each_pulls`,
`threads_pulling_beside_writers_report_how_often_each_pulls` and
`cycles_beside_writer_processes_report_their_times`:

- Sixteen processes pulling over and over beside four writer processes whose closures slept 5 ms
  made from 39 to 41 pulls each in 5 s, 632 to 651 in all, the longest taking 156 to 159 ms,
  against from 22 to 45, 15 to 132 and 22 to 47 each, 490 to 715 in all, the longest 449 ms to
  1.29 s. Beside sixteen they made 34 to 36 each, 546 to 564 in all, the longest 172 to 193 ms,
  against 18 to 38, 16 to 57 and 21 to 35, 388 to 455 in all, the longest 624 to 862 ms. The
  writers committed 3% to 7% less, 2,818 to 2,830 times against 2,924 to 3,032, and 9,548 to
  9,637 against 9,739 to 10,489.
- Sixteen threads of one handle pulling beside four handles whose closures slept 5 ms made 80 to 86
  pulls each in 10 s, 1,283 to 1,352 in all, the longest 146 to 169 ms, against 29 to 52 each, 595
  to 692 in all, the longest 1.13 to 1.27 s; sixteen handles made 81 to 85 each, 1,299 to 1,340 in
  all, the longest 162 to 170 ms, against 40 to 146 each, 818 to 1,144, the longest 670 to 926 ms.
- Beside one process committing with no closure, four processes pulling made 1,926 to 2,246 pulls
  in 3 s, from 471 to 648 each, against 967 to 1,203, from 207 to 455, and the writer committed
  223,000 to 308,000 times against 184,000 to 213,000. Two processes pulling made 2,228 to 2,289
  pulls, 1,114 to 1,166 each, against 2,487 to 2,911, from 568 to 2,343 each, a sixth fewer, and
  the writer committed 238,000 to 334,000 times against 214,000 to 325,000. Two pullers take the
  gate in turn, each waiting for the other's pull and 1 ms more, where b919d64's often took it back
  ahead of the other; the writer gets the time.
- Five cycles beside 4 and 16 processes committing in closures that spun 0, 1, 2 and 5 ms took 4.4,
  4.5, 6.9 and 9.3 ms and 13.0, 7.1, 7.2 and 15.2 ms at the median, against 3.5, 4.7, 6.5 and 9.3
  and 5.2, 6.8, 7.5 and 19.7. The cell 13.0 against 5.2, beside 16 at 0 ms, came out at 11.7
  against 11.3 over five more rounds, and beside 4 at 4.2 against 4.7, with the load near 17 then.
- Writers run nothing new, nor does anything without `shared`. Alternating each round, `group` with
  1 and 8 shared writers made 40,109 and 34,730 commits a second against 37,981 and 35,561, and
  without `shared` 1, 8 and 16 writers on one branch made 46,403, 37,256 and 37,633 against 44,909,
  38,486 and 37,341, medians of five rounds. `processes` at 2, 4 and 8 on one branch made 30,490,
  25,948 and 21,829 against 32,754, 25,470 and 22,466, and on branches of their own 62,626, 92,961
  and 55,724 against 60,692, 89,163 and 54,392, medians of three.
- Measured again on top of the staging frames of 344e36d, against c44151a, two rounds with the
  load average near 7: sixteen processes pulling beside four writers made 38 to 42 pulls each,
  the longest 157 ms, against 14 to 117, the longest 1.08 s; sixteen threads of one handle 77 to
  81 each, the longest 166 ms, against 30 to 51, the longest 1.49 s; four processes pulling beside
  a writer made 1,825 and 2,042 pulls against 835 and 966, and two 2,236 and 2,326 against 1,149,
  one of them pulling 300 times and the other 849, and 3,296.

The turns for each thread and the passing of a taker that does not run (#155) were measured on
2026-11-12 against 539c711, which runs e11aab6's turns for each handle, the two builds alternating,
with the load average between about 2 and 17 on the 32-thread machine, under Buffered, with the same
ignored tests, the one of threads pulling now able to put the first `FERRO_IN_ONE_HANDLE` of them
in one handle:

- Sixteen processes pulling beside four writer processes whose closures slept 5 ms made 577, 696 and
643 pulls in 5 s, the longest taking 176, 162 and 153 ms, against 658, 610 and 683, the longest 156,
162 and 141 ms. - Sixteen threads of one handle beside four handles made 1,266 pulls in 10 s, 79 or
80 each, the longest 146 ms, against 1,278, 79 to 81 each, the longest 152 ms. - Eight threads of
one handle beside eight handles of their own, all pulling beside four handles, made 77 to 79 pulls
each in 10 s, the longest 164 ms, against 19 or 20 each for the threads of the one handle and 150 or
151 for the others, the longest 590 ms. - Beside one process committing with no closure, four
processes pulling made 2,131 pulls in 3 s against 2,129, and two made 2,271, 1,961 and 2,259 against
2,307, 2,249 and 2,195. A waiter that tests the turn before its own at most 250 µs apart once it was
next made 1,968 and 2,022, and it now tests 50 µs apart; one that waits on that turn in the kernel
would wake at once but could not pass a taker that stopped. -
`a_taker_stopped_while_it_waits_for_the_gate_holds_up_no_taker_that_comes_after_it` stops a process
queued for a pull and runs a cycle in another handle: under e11aab6 the cycle waited the whole 5 s
the process stayed stopped, and now it finishes while the process is still stopped.

## A commit

1. Under its lane the writer takes its branch's lock, waiting while another process runs a closure
   on that branch, and catches up, as described below. The closure then starts from the newest
   commit on its branch from any process, and nothing can move that head until the lock is
   released.
2. It runs the closure and drafts the commit outside `tip`, as before.
3. It takes `tip` and then the placement lock and catches up again, this time with frames other
   processes placed on other branches while it drafted. If any did, the draft moves, as it does when
   another branch's frame lands first within a process. It checks that its branch's head is the one
   it started from, then places the frame and writes it before releasing the placement lock.
4. With the frame written and its branch's state moved to it, the writer lets the branch lock go,
   and only then registers the objects it wrote for deduplication and puts the nodes in its cache,
   still holding its lane. Another process takes the branch meanwhile, and a thread of the same
   process still waits for the lane, so it finds those nodes cached as before. Another process can
   then also take every branch lock and run a cycle that replaces the file, and a thread of this
   one can follow it and close the old store, so the writer caches nothing in a store that is
   closed, and clears the cache again if the close came while it cached, as every read that
   caches does: a node that holds a nested map holds its store, and would keep the old file open
   for as long as the process runs.
5. The leader of the sync takes the superblock lock, syncs and writes a superblock, as described
   under [Superblocks and group commit](#superblocks-and-group-commit).

A closure never runs against a head another process has since moved, so it never runs again. The
other way round, optimistic closures checked at placement and run again on a conflict, would need
them to be `FnMut`, and `update` and every other write take an `FnOnce`, which is what lets a
closure move its captures into the root. `drop_branch` takes the dropped branch's lock too, so a
drop waits for a closure another process is running on that branch, and a closure that starts after
it gets `Error::NoBranch`, as it does when the drop is in its own process. Without that lock a drop
and a fork of the same name in another process, while a closure ran, left the closure's commit
failing with `Corrupt` (`a_drop_and_fork_in_another_handle_wait_for_a_closure_on_that_branch`).

## Catching up

A process knows where the log ended when it last looked, `placed`. To catch up it reads the frame
header there. If it is a frame with the next sequence number, it reads the whole frame and checks
it, then the next, until it finds one that is not. It reads a header's 68 bytes and the 16 after
them, where the first record of a frame that spilled would start, so a process that is alone reads
84 bytes, a reservation of zeroes or the end of the file, each time it looks. A process that finds
frames reads more at once: its first read takes as many bytes as the last catch-up of the same kind,
under the placement lock or not, followed, and a quarter more, up to 256 KiB, a guess that halves
after each catch-up that finds nothing. A frame that fits in 4 MiB together with the 84 bytes after
it, which hold the next header, is checked in memory within a read that holds both, and a frame
that runs past the read is read again with it, in a read twice as long as the last, so following
_n_ frames that the guess covers takes one read where it took _n_ + 1; a larger frame, or one that
does not check in memory, is read again from the file and judged there, as below. In this mode a
frame is written before the placement lock is released, so a frame that a live process placed is
whole whenever another can take that lock, and one that a process left half-written when it died
fails its check and ends the log there. A process killed in a write stops at a page boundary, so
the rest of its frame keeps its older bytes and reads as torn. A catch-up under the placement lock
that finds anything but zeroes in those 84 bytes past the last whole frame, part of a dead
process's frame or the records of one that spilled before its header was written, truncates the
file there and syncs before the next frame is written in its place, as recovery at open does, so
that a power cut cannot leave a sector of the next frame holding the dead one's bytes, which can be
a bit from it. It does the same when a stale cycle marker, one the path still names, has anything
but zeroes in the 16 bytes past its 68. A frame that reached the disk whole and was damaged since,
one bit from checking as [Torn and damaged frames](storage.md#torn-and-damaged-frames) describes,
fails a catch-up under the placement lock with `Error::Corrupt`, so no commit is written over it. A
catch-up without that lock, on taking a branch lock, at `refresh` or before a cycle's pass, may meet
a frame another process is still writing, so it takes any frame that does not check for the end of
the log, and the catch-up under the placement lock before the next frame is placed judges it.

Nothing at or below the newest superblock is ever truncated or written over. A catch-up under the
placement lock starts from this process's `placed`, which can lie many frames behind the newest
superblock when the process has been idle, so it also reads both superblock slots and takes the
newest. Every frame that superblock names was whole in the page cache when it was named, and no
crash can have taken one back while this process is alive, so if the frames it follows stop before
the end of the log that superblock names, the frame where they stop was damaged since, however the
judgement reads it, and the catch-up fails with `Error::Corrupt` and leaves the file as it is, under
either durability. Only past that log does it judge a frame torn or truncate anything, as
[What a superblock names](storage.md#what-a-superblock-names) describes. Its own frames are placed
where the catch-up ended, so none is ever written below the newest superblock either. The read costs
about 0.9 µs of CPU on each placement, measured there. The first version judged from `placed` alone:
an idle handle's commit after another had committed four frames, with two bits flipped in the
second, read it as torn, truncated it and the two after it, synced, and acknowledged its own frame
in their place, and the other handle's next commit failed reading past the end of the file, and no
open succeeded after (#82).

The frames' head pointers name [steps](commit-path.md#branch-table-steps) on the table each process
already holds, so loading the new refs reads only the steps since the head this process last knew,
as a read-only `refresh` does. For every branch attached in this process whose head moved, it loads
the new state into the branch's tip, and a branch that is gone is retired. So is one whose name now
has a head with another `born`, the number taken when the branch was created: another process
dropped it and forked the name again before this one caught up, and the table this process loads
has only the newest of those frames' heads, as [A branch of the same
name](history.md#a-branch-of-the-same-name) describes. The first version took that head for the
old branch moved, so a write through an old handle landed on the new branch, where a process that
caught up between the drop and the fork failed it with `Error::NoBranch`. The frames go into the
queue as one entry, so the next sync
publishes those states with the process's own commits, in frame order, and fires their watchers.
Nothing another process wrote is published here before it is durable, by that process's sync or by
this one's.

Loading a state decodes its root as `T`, which runs the user's `Data::decode` under `tip`, so a
catch-up that adopts a frame or follows a cycle's marker first records the whole file, as [Writes
from inside a closure](reentrancy.md) describes, and a write, `refresh` or `Db::branch` from that
decode fails with `Error::Reentrant`. The first version recorded nothing there, and a write took
`tip` with only its lane recorded, so a `refresh`, a `Db::branch` or a write to a branch that sorts
after the lane's, made from the decode, passed the check and waited for `tip` for good (#105). The
states a catch-up replaces, and those of a branch it retires, go into notices its caller drops once
it has let `tip` and its lanes go, so a `Drop` among them runs outside the locks. A catch-up that
finds nothing records nothing, so a process alone pays nothing for either.

The bytes of the frames a catch-up followed and checked in memory stay in memory, in the buffers of
the reads that took them, until a later catch-up keeps the frames it follows, a catch-up truncates
the file or the store closes. Each buffer counts whole towards the 4 MiB they may take, read past
the frames or not, and the oldest frames go first when they would take more. While they stay, a
read of an object in them takes its record from there rather than from the file: the steps, the new root and log entry of each branch that moved, and the nodes of those frames that
the next closure reads back. A read loads the frames through an `ArcSwapOption`, so reads on many
threads take no lock and change no shared count, tests its offset against where the first frame
starts and the last ends, and finds its frame by a binary search. Every such read still hashes the
object against the pointer it came from, as a read from the file does. The bytes are those the
catch-up checked, and a frame that checks stays as it is in the file for as long as the file is in
use, since nothing is written below the end of the log a live process has placed and only bytes past
the last whole frame are ever truncated. A damaged copy on the disk is found by the next read from
the file, once a later catch-up has replaced the bytes held. The frames of one read share its
buffer, and a catch-up that replaces them keeps the buffer once no reader holds it, for the next
catch-up to read into, up to 1 MiB, so a writer that catches up on every turn neither allocates nor
zeroes memory for it.

The first form kept them behind a mutex that every read of a shared writer took, found a read's
frame by a scan, and let the oldest frame go by removing it from the front of a `Vec`, which made a
long catch-up quadratic. In probes kept out of the tree, median of three alternating runs, a
follower catching up with 100,000 frames of one object each took 791 ms where it takes 17 ms with
8-byte objects, 228 and 21 ms with 300-byte ones and 89 and 51 ms with 2.3 KB ones, and each read of
an 8-byte object after it took 15.3 µs where it takes 0.25 µs. Sixteen threads reading 100,000
records of 200 bytes each from outside the kept frames took 1.8 s, in runs from 0.25 to 1.9 s, where
they take 115 ms, and four threads 68 and 55 ms, while one thread took 53 and 52 ms. Reading 50,000
each from inside some 14,000 kept frames, one thread took 166 ms where it takes 20 ms and sixteen
threads 726 where they take 29.

Catching up happens when a process takes a branch lock, before it places a frame, on each
`Db::branch`, `Db::have`, `Db::bundle` and `Db::receive`, before each pass of a cycle and on
`refresh`, and on a look of a [follower](#followers) that finds a superblock naming a frame past its
own. A process that does none of these sees nothing new, as a read-only handle that never refreshes
sees nothing new.

Catching up only rebuilds branches that are attached. A branch attached later takes its state from
the published refs, so attaching first catches up and syncs what is queued, and then attaches only
if the newest head for that name is the published one, and otherwise goes round again. The first
version attached from the published refs alone, so a branch whose newer head another process had
written, and this one had caught up with but not yet synced, attached with the older state and
never moved: every commit on it failed with `another process moved branch`.
`a_branch_attached_while_a_followed_frame_waits_for_its_sync_reads_the_newest_head` holds a commit
on `main` between catching up and syncing while it attaches the branch. `Db::branch` catches up
even for a branch this process has attached, so that one another process dropped, or dropped and
forked again, is retired first: the first version handed back the attached branch as it was, so
`branch("x")` after a drop gave the dead handle, and after a drop and fork the old branch rather
than the new one. `branch_after_another_handle_dropped_and_forked_again_gives_the_new_branch`
covers both.

The marker a cycle leaves, described under [Reclamation](#reclamation), is checked against the path,
which a shared writer's store keeps canonical; other stores keep the path they were given, as
before. The first version kept the path it was opened by, so a handle
opened by a relative path, after the working directory changed, could not find the file, read the
marker as a cycle that failed, and went on committing to the retired file: an acknowledged commit
was lost (`a_handle_opened_by_a_relative_path_follows_a_cycle_after_the_directory_changes`). It also
treated any error checking the path as no change, and now fails the commit instead.

## Superblocks and group commit

Within a process one leader syncs every frame queued behind it. Across processes each leader used
to sync for itself, so 8 processes made 8 fdatasyncs where 8 threads made one. A superblock under
`Durability::Fsync` anchors itself, and every frame up to its anchor was written before the
fdatasync that preceded it, so a leader that finds a superblock anchored at or past its own frame
has nothing to sync. So the leader takes the superblock lock, waiting out any sync another process
is running, and reads both slots. If the newest one covers its frame, it publishes at once.
Otherwise it reads past its own frame to the newest whole frame of any process, syncs, and writes a
superblock naming that frame to the slot that does not hold the newest. Every process that queued a
frame meanwhile then finds it covered. A superblock is written only when it is newer than the
newest, so slots alternate and only one superblock write is ever in flight, as with one process.

Under `Durability::Buffered` nothing is synced and the lock only orders superblock writes. Each
process checkpoints for itself, and a superblock carries the anchor of the process that wrote it,
which is never past what an fdatasync covered.

A sync that fails in one process is reported to the others, since Linux reports a writeback error
once to every open file of the inode at its next fsync, so a process whose frame followed the failed
one fails its own sync rather than acknowledging over a hole.

## Opening and closing

A shared writer opens under the placement and superblock locks and recovers as a writer always has:
it scans from the anchor, syncs what it accepted and truncates the rest. Other writers may be alive,
but none can be placing a frame, and every frame they placed is whole, so the scan finds exactly the
log. A Buffered file with live writers may have up to `checkpoint_bytes` past its anchor to scan. It
removes a stale `.compacting` file only if it can take the cycle lock, since another process's cycle
may be writing it.

If another writer that has opened is alive, which the lock at 2^62 + 1 tells, no crash can have come
since that writer opened, so the scan from the anchor must reach the end of the log the newest
superblock names, under either durability, or the open fails with `Error::Corrupt` and writes
nothing. Under Fsync the newest superblock anchors itself and the scan starts there, but under
Buffered the anchor can lie far behind, and a frame between them that does not check was damaged
since, not torn. A writer takes that lock only once it has recovered, still holding the placement
lock, so two writers that open after a power cut never take each other for live ones: the first to
recover may roll a Buffered log back past what its superblock names, and the other then finds the
log that one left. Before, an open beside a live Buffered writer that met a frame with two bits
flipped in one block rolled the log back to the frame before it, as after a crash, truncating the
live writer's acknowledged commits from under it, and the live writer's next commit failed reading
past the end of the file (#82). Checking the lock while it was still taken with the writer lock,
before recovery, would have failed one of two writers that open together after a power cut on a
Buffered log the crash cut short. A shared writer of an earlier build still takes it that way, so
beside one of those that is recovering after a power cut, a writer of this build takes it for a live
one, and on a Buffered log the crash cut short its open fails with `Error::Corrupt` and writes
nothing; opening again once that writer has closed succeeds. The format does not tell the builds
apart, so processes that share a file should run the same build.

Once it holds the placement lock it checks again that the path names the file it opened, and opens
the path again if not. A cycle renames the new file over the path under the old file's placement
lock, so a file still named by the path once that lock is held is one no cycle has retired. The
first version checked the path only before taking the lock. A writer that opened the path during
another process's cycle pause got the old file's placement lock once the cycle had renamed and let
go, recovered the retired file, and truncated the marker the cycle had left at its end as anything
past the last frame, so it never learned of the new file and acknowledged commits into one that no
longer had a name. A process that ran two cycles before another followed did the same to the file
in the middle. `a_writer_that_opens_during_another_handles_cycle_pause_writes_the_new_file` holds a
cycle at its pause while another handle opens, and lost the commit that handle acknowledged. Once
two processes were on different files, both could run a cycle, each into the same `.compacting`
and each renaming over the other's file, which is the likeliest way a stress run ended in `varint
overflow in record`.

Recovery truncates everything past the last frame, a marker included, as it always has. A marker is
only ever on a retired file or, left by a cycle that died or failed before its rename, at the end of
a live one, and the check under the placement lock keeps recovery off retired files, so the marker
it truncates is always a stale one. For a while recovery by a shared writer kept a marker as a
second guard, and that broke the promise under [File format and
durability](storage.md#file-format-and-durability) that a frame stranded past a torn one can never
line up with the log later. Frames written over a stale marker, then a power cut that kept the
second frame but not the page with the first one's header, left the marker in front of a whole
frame, and recovery stopped at the marker and kept the frame. The next frame of the same length
then lined it up again, linking into what had been rewritten, and the file failed to open with a
hash mismatch. `a_marker_left_by_a_dead_cycle_never_keeps_a_stranded_frame_after_a_power_cut` writes
a marker, writes two frames over it, puts the marker and the superblocks back as such a power cut
would, and failed that way while the marker was kept.

A shared writer does not truncate the Fsync reservation when it closes, since other writers may be
placing frames into it. The next writer to open truncates it, so the file can stay up to 4 MiB
longer than `size_on_disk` while no shared writer has it open.

A read-only open that can take the writer lock shared takes the placement lock shared too, and
trusts the superblock if another holds it, then tests the lock that shared writers hold once they
have opened, and treats a held one as a live writer, trusting the superblock instead of scanning and
syncing. If either test fails with an error rather than finding the lock held, it scans, as it
would with no writer alive, without the placement lock when that was the test that failed. It holds
the placement lock for the scan when it has it, so a shared writer that opens meanwhile waits for it
before it recovers or writes, and the scan, which judges a frame that does not check as damaged or
torn, never reads one being written. Without it, a shared writer that opens meanwhile can recover
and place frames during the scan, so the scan may meet one being written. The first version of the
placement lock trusted the superblock when its test failed with any error, so a reader whose lock
failed with `ENOLCK` missed a commit whose superblock write a crash had lost (#82). Wherever it
finds a writer alive and a superblock that does not anchor itself, it tests the recovery lock, and
scans without the locks while a writer holds it, or reads the slots again once none does, as
described under [Buffered durability and
checkpoints](storage.md#buffered-durability-and-checkpoints). Read-only handles otherwise behave as
before.

## Reclamation

A cycle runs in one process at a time, under the cycle lock. A background cycle that finds it taken
skips its turn, and one that gets it after another process's cycle follows the new file first and
runs only if the file has grown enough since. `Db::compact` waits for the lock. Each pass catches up
first, so it copies the other processes' commits too, and the pause takes every branch lock and the
placement lock, catches up once more and copies what is left. Before it renames the new file over
the old one, the cycle writes a marker at the end of the old log, a frame header with the magic
`FERROGEN`. It holds the new file's placement lock until it has rebound its own branches.

A process that catches up and reads the marker checks the path. If the path names another file, it
syncs and publishes what it had queued in the old one, opens the new one, loads every attached
branch's state from it and rebinds its watchers' feeds, as a read-only `refresh` of a replaced file
does. It retires a branch whose name is gone from the new file or has a head with another `born`
there, since the frames of the old file it had not read and those of any generation between are not
read at all. If the path still names this file, the cycle failed before its rename and the marker is
written over, or, with anything but zeroes past it, truncated with what follows, as described under
[Catching up](#catching-up). Every frame placed in the old file before the marker was copied into
the new one, since the pause caught up under the placement lock and the marker went in under it, and
no process can place in the old file after the marker without first finding it. Recovery reads the
marker as the end of the log, so a crash between the marker and the rename leaves the old file as it
was.

## Watchers, coalescing, awaited writes and receive

- Watchers fire for another process's commits when this process catches up and syncs them: at its
  next commit on any branch, at `refresh`, when it attaches a branch, or when its follower finds a
  superblock naming them, as described under [Followers](#followers).
- With `coalesce`, a group holds its branch's lock from its first closure until it is sealed, so
  another process's closures on that branch wait until the group is written. Groups are per process:
  closures in two processes never share a version.
- An awaited write takes its branch's lock without waiting. If another process holds it, the
  future returns `Pending` and a thread waits for the lock, lets it go and wakes every future
  waiting on that branch, so no executor thread waits on another process's closure. There is one
  such thread per branch at a time. The first version started one for every future: 32 futures
  queued behind another process's closure took the process from 3 threads to 36, and now take it
  to 5, the waiting thread and the sync thread
  (`awaited_writes_waiting_on_another_process_share_one_waiting_thread`).
- `receive` places a large bundle's staging frames as it arrives, each under the placement lock
  only while it writes the frame's header, so other processes commit between them, and a catch-up
  checks a staging frame's header and goes on at its end without reading its records, keeping none
  of them for later reads. It lands the bundle under the cycle lock and every branch lock, so its
  checks run against the newest heads of every process. Only a writer that opens with no writer
  alive drops the staging frames that end the log, since a live process may still be filling them,
  and a shared writer leaves its own in place as it closes, since another process may have
  followed them.

## Followers

`Options::follow` gives a read-only handle or a shared writer a thread, `ferro-follow`, that keeps
it up to date with what other processes commit, so `read` stays current and watchers fire without
`refresh` (#84). Every handle of one open shares the thread, which the `Keeper` owns beside the
reclamation thread. A writer without `shared` is the only writer and starts none, and a handle
opened without the option has no thread, and nothing in its commits, reads or `refresh` changes.

### A look

The thread looks at the file whenever it may have changed. On a read-only handle a look is a
`refresh`, which reads both superblocks and stats the path under `tip` and loads only what moved.
On a shared writer a look first stats the path and reads both superblock slots in one read, as a
sync does, without `tip`: only if the path names another file, or the newest superblock names a
frame past the last one this process placed or followed, does it catch up. A look that finds
nothing changes nothing and costs a few microseconds on a core that is awake. The first version
made a read-only look the same check, against the store's last sequence number. A refresh that
failed on a damaged object of the new commit, after `Store::refresh` had moved that number, then
left the next look finding nothing, so the follower cleared the error and stayed at the old
version, where `refresh` keeps failing until the damage goes
(`a_follower_whose_refresh_fails_keeps_trying_and_reporting_until_it_succeeds`). A shared writer
moves the number only once it has loaded what it followed.

A shared writer's catch-up reads on from `placed`, and a frame another process placed may not be
synced yet. A catch-up that followed one would sync it before publishing, as `refresh` on a shared
writer does: an fdatasync under the superblock lock on behalf of the other process, and watchers
that fire before that process has acknowledged its commit. So a look on a shared writer catches up
only to the frame the superblock it read names. The frames up to it were whole when they were
named, and a frame among them that does not check fails the catch-up with `Error::Corrupt`, as
under the placement lock, rather than end the log there, since no crash can have taken it back
while this process is alive.

The sync that publishes them has nothing to write either: the look notes in the store how far the
superblock it read covers, by anchor under Fsync and by name under Buffered, and a shared sync of a
frame up to there returns as the locked path would, without the superblock lock, since superblocks
only grow while this process is alive. Under Buffered the locked path could still have written a
superblock carrying this process's newer anchor; one carrying an older anchor only makes a
recovery scan from further back. The first version took the superblock lock for that sync, so a
look, and a drop waiting for it, waited as long as another process held the lock through a slow
fdatasync: `dropping_a_shared_follower_waits_for_no_other_handles_superblock_lock` holds one
handle's fdatasync under that lock, and the follower did not publish the commit a superblock had
already named, nor did its drop return, until the fdatasync did. So a look that finds a new
superblock takes no lock another process can hold. A look still leads a real sync, under the
superblock lock, when this process's own commits have queued frames past what the superblock
covers, as any leader would, and a look that meets a cycle's marker opens the new file as a shared
writer's open does, under that file's placement and superblock locks. Under Fsync a commit that a
Buffered process named is past the anchor the superblock covers, so a Fsync look that follows it
syncs under the superblock lock and waits for whoever holds it. A Buffered follower runs the
checkpoint after each look that led, as any Buffered writer does, so it syncs the file once every
`checkpoint_bytes` of other processes' commits. A look never takes a branch lock.

### Waking

On 64-bit Linux the thread waits in `ppoll` on an inotify instance and an eventfd. The instance
watches the file for `IN_MODIFY` and its directory for a file created, removed or renamed at the
file's name. A commit wakes it once for its frame and once for its superblock, and the first look
usually finds nothing yet. Events for other names in the directory, such as the `.compacting` copy
a cycle writes, are read and dropped without a look. When the name moves to another file, as at a
cycle's rename, the file's watch moves to the new file and the old one is removed, so no watch
outlives the generation it was on. A queue overflow counts as a change. The thread also looks once
a second whatever happens, for a file system that does not report another process's writes.

After a look that refreshed, the thread waits until a millisecond after that look began before it
looks again, and events meanwhile stay queued for that next look. Without the wait, a writer
committing as fast as it can kept the thread refreshing for every commit it could reach: in
`ferro-bench --bin follow`, with Buffered single-key commits from another process for five
seconds, the follower refreshed 408,649 times for the writer's 415,355 commits and used 3.26 s of
CPU, 65% of a core, and the writer made 9% fewer commits than with no follower. With it, the
follower refreshes about 960 times a second. A look that found nothing does not wait, so the look
for a commit's superblock is not held up by the one for its frame. The first version waited after
every look, so the look for a commit's frame held up the one for its superblock by up to a
millisecond: a commit reached a watcher at a median of 673 µs after the writer's closure began,
470 µs more than the commit took, where without that wait it took about 90 µs more.

Where inotify cannot be had, on every other target or when `inotify_init1` or a watch fails, as
once the user's `max_user_instances`, 128 by default, are in use, the thread looks every 10 ms,
waiting on a condvar. `Following::notified` says which it does. A watch that fails later on falls
back to looking every 10 ms, and the failure is reported in `last_error`.

### When it ends

The thread holds the handle's `Shared` by a `Weak` and upgrades it only for the length of a look, so
while it waits it keeps neither the file nor a lock, and a watcher's promise not to keep the file
open still holds. The last `Db` handle to drop drops the `Keeper`, which sets the stop flag, rings
the eventfd or wakes the condvar, and joins the thread before it stops the reclamation and sync
threads. A look in progress finishes first, and if it held the last count of `Shared`, the file
closes on the follower thread before the join returns, so either way the file is closed and the
inotify instance gone when the drop returns. So a drop waits for a look in progress. That is a few
microseconds for one that finds nothing and the length of a refresh for one that finds something; a
look that opens the file a cycle renamed waits there for that file's placement and superblock locks,
as an open would, and the drop waits with it. The drop of a Buffered shared writer already takes the
superblock lock to write its last anchor. A look that finds `Shared` gone ends the thread too. The
thread runs no closure and no watcher test, only what `refresh` runs: decoding the new states and
dropping the ones they replace. A panic there is caught and recorded, and the thread goes on.

### Errors

A look that fails changes nothing: `read` shows what it showed, watchers go on waiting, and the
thread looks again at the next change or interval. `Following::last_error` keeps the error as text
until a look succeeds, as `Space::last_error` keeps a cycle's. Ending the watchers instead would
end every subscriber over a fault that may pass, such as a file moved away and back, and a watcher
cannot carry the error.

- A path that names nothing fails with `Error::Io`. Reads go on from the file the handle has open,
  and when a file appears at the name again, created or renamed there, the directory's watch wakes
  the thread and the refresh opens it, as it opens a file a cycle renamed there.
- Superblocks that are both invalid fail with `Error::Corrupt`, and so do a damaged branch table or
  root on a read-only handle and, on a shared writer, a frame a superblock names that does not
  check.
- `Error::Replaced` never reaches it: `refresh` opens the new file when it meets that, and a shared
  writer's catch-up follows the cycle's marker to it.
- A shared writer whose write or sync failed is poisoned until it is reopened. Its catch-up then
  fails with `Error::Poisoned` before it reads, decodes or queues anything, at a look and at
  `refresh` alike, and the follower records the error and stops, so it stays. In the first version
  a poisoned writer's catch-up still adopted each frame another process named, decoded its states
  and queued it behind the failed sync, which never drained, and the follower's next look found
  nothing new and cleared the error. With 300 commits of a 256 KiB root from another handle, a
  poisoned follower grew by 75 MiB and a poisoned handle that called `refresh` after each by
  76 MiB, where a healthy follower grew by 6 MiB; with the check neither grows.
  `a_poisoned_following_shared_writer_follows_nothing_more_and_keeps_its_error` counts the frames
  it adopts after the failure, which must be none.

### Reentrancy

The thread holds no lane and runs no user closure, so nothing it could wait for waits for it. A look
takes `tip`, as `refresh` does, and on a shared writer the superblock lock only when it leads a sync
of this process's own frames or opens a file a cycle renamed, holding nothing else. A watcher test
runs on the `ferro-watch` thread holding nothing and may refresh while the follower does: they take
`tip` one after the other. A closure may refresh too, as before, since `refresh` needs only `tip`,
which a look holds briefly and never while it waits for a lane. [Writes from inside a
closure](reentrancy.md) has the rules.

### What following costs

`ferro-bench --bin follow` runs a follower in one process against a writer in another, on the
32-thread machine of the other figures here, pinned to the 16 hardware threads whose siblings
another job was using. The writer makes Buffered single-key commits. Latency runs from the moment
the writer's commit returned to a watcher's receipt, over 300 commits a random 0 to 40 ms apart;
idle is a minute with no writer; busy is five seconds of the writer committing as fast as it can,
with no follower as the baseline. Medians of three rounds, with a read-only follower beside a plain
writer:

| Follower | p50 after the ack | p99 | Skipped | Idle CPU a minute | Idle wakes a minute | Writer's busy commits | Follower's busy CPU | Busy refreshes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| None | | | | | | 478,260 | | |
| inotify | 169 µs | 926 µs | 1 | 3.0 ms | 59 | 478,565 | 153 ms | 4,808 |
| Every 1 ms | 579 µs | 1.38 ms | 3 | 140 ms | 59,706 | 487,703 | 114 ms | 4,702 |
| Every 10 ms | 4.96 ms | 10.5 ms | 35 | 254 ms | 5,920 | 476,084 | 25 ms | 487 |
| Every 100 ms | 11.0 ms | 35.6 ms | 238 | 42 ms | 598 | 469,164 | 9 ms | 51 |

And with both the follower and the writer shared writers:

| Follower | p50 after the ack | p99 | Skipped | Idle CPU a minute | Idle wakes a minute | Writer's busy commits | Follower's busy CPU | Busy refreshes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| None | | | | | | 394,418 | | |
| inotify | 116 µs | 762 µs | 1 | 4.1 ms | 59 | 370,824 | 306 ms | 4,869 |
| Every 10 ms | 5.04 ms | 10.7 ms | 35 | 274 ms | 5,917 | 346,098 | 251 ms | 471 |

A watcher skips a commit when a later one landed before it looked, and only the commits it handed
out are timed, the newest in each wait, so the figures for 100 ms understate the 50 ms a commit
waits on average there.

- Woken by inotify, a follower delivers in a fraction of a millisecond and an idle one costs
  nothing measurable: one look a second. Its median swung from 32 to 460 µs between rounds with the
  load on the sibling threads, since two wakes of cores that may be asleep, the follower's and the
  watcher's, make up most of it.
- An interval trades latency against wakes, and the wakes cost more than the looks: every 10 ms an
  idle follower used 80 to 311 ms a minute across the rounds, 13 to 53 µs a wake, where every 1 ms
  the core never slept deeply and a wake cost 2.3 µs, though it woke a thousand times a second.
- Under the busy writer the read-only follower refreshed once a millisecond, 32 µs a refresh, and
  looking every 10 ms took 25 ms. The writer's median was the same with an inotify follower as with
  none. Over these rounds and three of an earlier run, five of the six with inotify fell within the
  462,214 to 496,358 commits of the rounds with no follower and one, at 427,054, below them, so the
  `IN_MODIFY` event that each of the writer's writes queues costs it little if anything. Shared
  writers' rounds spread from 336,980 to 412,934 whatever followed.
- A shared writer's catch-up reads every frame placed since its last look, where a read-only
  refresh reads the newest head and the branch-table steps since the one it knew. So a shared
  follower took 63 µs a refresh at once a millisecond, and 530 µs when it looked every 10 ms and
  read ten milliseconds of frames each time: a longer interval saves it little under load. These
  rounds ran while a look still took the superblock lock to publish; three more once it no longer
  did gave 169 µs at the median after the ack, and 268 ms of CPU for 4,841 busy refreshes, 55 µs
  each.

`examples/shop` ran its replicas on a thread that called `refresh` every 5 ms and now opens them
with `follow`. Its `shop-load replica-lag`, three runs of 300 commits against a Buffered writer,
measured a replica reaching each commit 0.06 to 0.14 ms after the writer acknowledged it, at the
median, where the timer took 0.88 to 0.97 ms, and that only because the tool's own 3 ms pause
between commits fell in step with it.

So a follower waits on inotify where it can, looks every 10 ms elsewhere, looks once a second
besides, and waits a millisecond after a look that refreshed.

### Left out of followers

- Looking at an interval only, measured above. The cost is in the wakes, whatever the look does: a
  thread that only slept 10 ms and then stat a file used 55.7 µs of CPU a wake, 0.54% of a core,
  while its core was otherwise idle, and 1.1 µs a wake when it slept 1 ms. It is kept for targets
  without inotify.
- Watching only the directory, which reports writes to the files in it by name and needs no watch
  moved at a cycle's rename. Every write to any file beside the database, such as a log, would then
  wake the follower, where the second watch costs one more call at each rename.
- No wait after a look that refreshed: 65% of a core under a busy writer, as measured above.
- A `Db` method that starts following and returns a guard that stops it when dropped. It would let
  a library follow a handle it was given, but the thread would then have two owners, a guard
  dropped at once would follow nothing, and two guards on one file would need a rule. Following is
  a property of how a handle meets the file, like `read_only` and `shared`, so it is an option.
- Watchers that refresh while they wait. `read` would stay stale while no watcher waited, each
  waiting watcher, or the watch thread of each branch, would look on its own, and an async waiter
  would need a timer.
- One inotify instance and thread for every follower in a process. It would save instances past
  128 followed opens, at the price of one thread's lifetime shared by unrelated files. Not built.
- kqueue on macOS and the BSDs and `ReadDirectoryChangesW` on Windows: none of them can run here, so
  those targets poll.

### Tests of followers

`ferro/tests/follow.rs` opens a writer and a read-only follower of the same file, and runs each test
once woken by inotify and once looking every 2 ms:

- Watchers, filtered or not, fire for another handle's commits without `refresh`, and a handle on
  another branch shares the follower.
- The follower follows three cycles that rename a new file over the path, and reads the new file.
- A follower that finds nothing new refreshes nothing.
- With the file removed, the follower reports `Error::Io`, `refresh` fails the same way, and reads
  keep the last version; a file renamed back with one more commit is followed, and the error clears.
- Both superblocks damaged are reported as `Error::Corrupt`, and nothing moves.
- A refresh that fails on a damaged object of a new commit goes on failing and being reported, look
  after look, until the byte is put back.
- A filtered watcher whose test refreshes and attaches a branch runs while the follower publishes
  50 commits.
- A child process counts its descriptors on the file and its inotify and eventfd descriptors as soon
  as it has dropped every handle of a follower, inotify and polling, with a watcher left waiting,
  which must have ended, and then its `ferro-follow` threads; it does the same for a shared writer
  that follows, after which a writer without `shared` must open at once. It also drops a shared
  follower while a hook holds its look for 300 ms, and the drop must take that long and leave
  nothing open; a drop that did not join the thread returned at once with the watch still open.

`ferro/tests/shared.rs` has a shared writer follow another handle's commits and three cycles
without committing. It holds one handle between placing a frame and syncing it while a second
handle places the next, so the newest superblock names only the first: the follower must publish
the first once its superblock is written and the second only once the second handle syncs, where a
catch-up past the superblock would sync and publish both. It also damages the middle of three
frames that superblocks name while the follower is held in a sync, and the follower must report
frame 3 with `Error::Corrupt` and follow all of them once the byte is put back. In
`ferro/tests/shared_process.rs` a child process follows as a shared writer, without committing,
while the parent makes 60 commits and three cycles, and must see every one.

`ferro/tests/follow_fault.rs` intercepts `fdatasync`. It fails one sync of a shared writer that
follows, then has another handle make 100 commits of a 64 KiB root: the poisoned writer's
`refresh` fails with `Error::Poisoned` each time, its follower keeps that error and stops looking,
and a hook counts no frame adopted after the failure. It also holds one handle's fdatasync under
the superblock lock: a shared follower must still publish the commit a superblock already names,
and its drop must return while the lock is held.

## Crashes and killed processes

A process killed anywhere in a commit holds up the others only until the kernel releases its locks.
Killed while it holds a branch lock, its closure is lost. Killed between placing and writing its
frame, it wrote nothing, and the next frame goes where it would have. Killed while writing it, the
frame stops at a page boundary, fails its check and is written over. Killed after writing it and
before syncing, the frame is whole and stays in the log, so its commit may appear though it was
never acknowledged, as after a crash in one process. Killed while a large `receive` streams, its
staging frames stay in the log, unlinked, until the next cycle, or until a writer opens with no
writer alive, if they end the log. Power cuts are unchanged: frames that follow
each other from the anchor survive, and every process's acknowledged commits were synced before
they were acknowledged.

## Tests

`ferro/tests/shared_process.rs` spawns children of the test binary that open one file with the
option. Each commits `n += 1` with a counter of its own and prints a line for every commit that
returned, and the parent checks that versions run from 1 to `n` with no gap, that each child's
counters rise in order, that every acknowledged commit sits at the version it was acknowledged at,
and that the file verifies:

- Four processes under Fsync and six under Buffered write at once.
- A process aborts after drafting, between placing and writing its frame, after writing it, and on
  its first commit, while another writes throughout and a third takes its place.
- A process is killed at random eight times beside a steady writer.
- One process runs a cycle every 25 commits beside three writers and a read-only follower that must
  see every commit, and in another test aborts at each stage of its second cycle while two writers
  go on. In a third, a process cycling every three commits is killed at a random moment eight times
  beside two writers, under Buffered.
- A process runs five cycles while four others commit on branches of their own, and in another test
  one cycle beside two processes of four threads each, whose 600 ms closures begin 75 ms apart, as
  [The gate](#the-gate) describes.
- A child opens the file by a relative path beside a second handle, changes directory, and commits
  after the other handle's cycle, and that commit must survive a reopen.
- A child counts its threads while 32 awaited writes wait for another handle's closure.
- A child runs a stress mix, adapted from the review's: two handles of one file, one coalesced, each
  with three branches of its own written by a thread that awaits every third commit and checks that
  each commit acknowledged the version after the last, writers on `main` in both, background cycles
  every 128 KiB with `Keep::Last(3)`, one handle forking and dropping four names in a loop and the
  other attaching and committing on them. After each round the file is reopened and every branch
  and every acknowledged `main` commit must be there, and the file must verify. It runs 40 rounds of
  400 commits a branch in about two seconds, `FERRO_TEST_SCALE` multiplies the rounds and
  `FERRO_SHARED_EACH` sets the commits, and the parent kills it and fails if it does not finish.
  Run on two CPUs, `taskset -c 0,1`, 20 rounds a run, the build before the fixes for opening
  during a pause and for groups on dropped branches went bad in 12 of 200 runs: 6 hung and 6
  failed, three of those with acknowledged commits missing after the reopen. With the fixes, 600
  runs, 12,000 rounds, ran clean, and so did 150 runs of 20 rounds of the review's own mix.

`ferro/tests/shared.rs` opens one file twice in one process, which locks as two processes do, for
what is easier to arrange there: forks and drops seen by the other handle, watchers fired by a
commit and by `refresh`, coalesced and awaited writes, an awaited write held up by the other
handle's closure, a pull landing beside the other handle's commits, background cycles in three
handles at once, four threads on branches of their own in each of two handles, an awaited write
waiting through the other handle's cycle pause, a branch attached between catching up and syncing, a
drop and fork that must wait for a closure, a branch the other handle dropped and forked again, with
and without a cycle between, whose old handles must stay dead, a writer opening during the other
handle's cycle pause, a closure on a branch the other handle dropped, a stale marker that a power
cut leaves in front of a whole frame, and a frame a dead writer left cut at a page boundary, which
the next writer writes over, or whole with a bit flipped, which it reports. It also has an idle
handle commit after the other committed four frames, with two bits flipped in the second's first
block, two in its magic, or its first 84 bytes zeroed, under both durabilities, and opens a handle
beside a live Buffered one over the same frames; both must fail with `Error::Corrupt`, change
nothing in the file, and leave the live handle's next commit and a reopen with every acknowledged
commit. And it leaves a dead writer's near copy behind a stale marker, which the next writer must
truncate and sync before it writes. A read-only and a shared follower each hold a snapshot and a
half-read iterator across three cycles of another handle and read them to the end.
Three handles with different `Keep`s fork, drop, merge, restore, pull and make stamped commits on
a few shared names while one of them cycles, and must meet only the errors those calls promise and
leave a file that passes `verify_typed`.
Each test added for a finding of the review failed before its fix, as described above.
A unit test in `ferro/src/store.rs` holds the placement lock while two
shared writers open a Buffered file whose superblock names a frame past its anchor that reads as
torn, then lets it go, and both must open at the frame before it, which one of them did not with the
lock at 2^62 + 1 taken before recovery. Three more check the frames a catch-up reads in one read: a
frame checked in memory must be judged as the file judges it, whole, with a bit flipped in each byte
of its header past the magic, in every 997th byte of its body and in its table, with its first block
changed and that block's entry in the table rewritten to match, which only the frame's check finds,
and cut a byte short; the frames kept must answer for offsets inside them only, up to the end of
each frame or the readable tail, across a gap between them; and a follower must read the objects of
the frames it last kept from memory after their copy on the disk is damaged, and those of a frame
larger than 4 MiB, or of one it no longer keeps, from the file. `ferro/tests/placement_fault.rs`
intercepts `fcntl` to fail a read-only open's test of the placement lock with `ENOLCK` over a
superblock one commit behind, and checks that the open scans and finds the commit, where it trusted
the superblock before. `ferro/tests/reentry_shared.rs` opens one file twice and makes reentrant
calls from a decode and from a `Drop` that run while one handle catches up with the other's commit,
cycle or `drop_branch`, as [Writes from inside a closure](reentrancy.md) describes. These files
build on 64-bit Linux only, and `ferro/tests/smoke.rs` checks elsewhere that the option is refused.

## Measurements

Without the option a writer takes no new lock and reads nothing past its own end: a commit only
tests a flag on its way. `tables`, one Buffered writer, median of six interleaved runs against the
tree before shared writers, on a machine other work was sharing:

| Commit | Instructions outside the closure, before | After | Shared, one process | CPU time, before | After | Shared |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Hold | 563,684 | 564,306 | 569,393 | 99.2 µs | 97.3 µs | 99.8 µs |
| Confirm | 729,967 | 730,246 | 734,852 | 127.1 µs | 124.2 µs | 127.4 µs |
| Cancel | 898,249 | 897,894 | 902,830 | 164.5 µs | 160.2 µs | 164.7 µs |

Bytes per write stayed at 13,362 in all three. The counter counts user-space instructions only, so
the shared column's 0.5 to 0.9% more leaves out the locks and reads themselves, which the CPU time
holds: 2.5 to 4.5 µs a commit over the build without the option. `group`, one writer making
single-key Buffered commits into 100,000 keys, median of seven alternating three-second runs: 47,940
commits a second before, 47,488 after and 42,718 with the option, p50 19, 19 and 21 µs. With fsync,
median of five: 2,604, 2,554 and 2,453, p50 377, 377 and 402 µs, where single runs of each ranged
over 20%. These ran before a later change read both superblock slots in one read, which takes one
read off each sync with the option. The fixes from review left a writer without the option where it
was: against the commit before the second set of them, median of five interleaved runs, a hold ran
564,131 instructions outside the closure before and 564,739 after, a confirm 730,014 and 729,972,
and a cancel 897,147 and 896,960, with CPU time within its runs' spread.

`ferro-bench`, median of three alternating runs against the tree before: no figure that does not
wait on fsync was more than 1.4% slower, and several were faster, but `map_diff_per_change`, a read
this change does not touch, at 8.3 and 8.9 µs, whose runs spread from 6.8 to 9.2 µs. A map commit
took 30.3 and 30.1 µs Buffered and 404 and 410 µs with fsync, and a table update with fsync 417 and
442 µs, within what their runs spread over. Seven more alternating runs of the concurrent workload,
eight fsync writers beside readers, gave 9,456 and 9,566 commits a second and 64.0M and 64.3M reads.

The `processes` tool in `ferro-bench` spawns one process per writer, each committing single-key
updates over 100,000 keys from one thread, on `main` or with `--branches own` on a branch of its
own, and then runs the same number of writers as threads of one process. Median of three rounds on
the NVMe drive of the 32-core machine, shared with other work, commits a second:

| Durability | Branches | Closure | Writers | Processes | Threads of one process |
| --- | --- | ---: | ---: | ---: | ---: |
| Fsync | one | none | 1 | 2,382 | 2,536 |
| Fsync | one | none | 2 | 2,690 | 2,575 |
| Fsync | one | none | 4 | 5,347 | 5,288 |
| Fsync | one | none | 8 | 9,727 | 10,406 |
| Fsync | own | none | 2 | 2,770 | 2,705 |
| Fsync | own | none | 4 | 4,921 | 5,278 |
| Fsync | own | none | 8 | 9,827 | 10,284 |
| Buffered | one | none | 1 | 38,280 | 43,684 |
| Buffered | one | none | 2 | 29,134 | 40,614 |
| Buffered | one | none | 4 | 24,895 | 42,027 |
| Buffered | one | none | 8 | 17,888 | 41,249 |
| Buffered | own | none | 2 | 57,459 | 62,842 |
| Buffered | own | none | 4 | 82,583 | 92,915 |
| Buffered | own | none | 8 | 54,202 | 107,160 |
| Buffered | own | 50 µs | 2 | 23,996 | 25,806 |
| Buffered | own | 50 µs | 4 | 45,038 | 48,941 |
| Buffered | own | 50 µs | 8 | 55,232 | 69,399 |

These ran with the fixes described under [One owner per lock in a
process](#one-owner-per-lock-in-a-process) and after it, and came within the spread of three rounds
run before them. p99 under Fsync was 1.5 ms at 8 processes on one branch against 1.0 ms for 8
threads, and p50 813 against 765 µs.

Under Fsync the disk is the limit, one fdatasync a round, and processes keep up with threads. Under
Buffered, writers on one branch take turns, and each turn in a process other than the last one's
starts cold: it reads the frame and steps the last writer placed, loads the branch's new root and
entry, and reads back the nodes of its path that the other process rewrote, where a thread finds
all of them in the cache the last writer filled. So two processes commit 0.72 times as fast as two
threads on one branch, and eight 0.43 times. On branches of their own the closures overlap, but
every placement reads, under the placement lock, every frame placed since this process last looked,
and the steps they wrote, and rebuilds the state of every branch it has attached that moved, here
`main` of the first process. That work grows with the processes, and at eight it is the limit,
about half of what eight threads make. With 50 µs of work in each closure four processes come within
8% of four threads.

### Reading back what other processes wrote

Before the two changes below, two processes on one branch under Buffered made 31,800 commits a
second and eight 21,900, where two and eight threads of one process made 43,000 and 38,500 (#88).
These figures and the profile below come from `ferro-bench --bin processes` on 2026-11-06,
pinned to the eight cores of one CCD of the 32-thread machine, `taskset -c 0-7`, with nothing else
running. `perf` is not installed there, so CPU time came from `getrusage` in each child, the time
each process held its branch lock from timers around the kernel lock in a build kept for the
measurement, and where the time went from a profile of the children sampled on `SIGPROF`, one
sample a millisecond of CPU at the kernel's tick, over ten-second runs:

| Before | 2 processes | 2 threads | 8 processes | 8 threads |
| --- | ---: | ---: | ---: | ---: |
| Branch lock held per commit | 29.5 µs | 21.9 µs | 43.0 µs | 23.8 µs |
| Share of the time it was held | 94% | 93% | 93% | 91% |
| Nodes read from the file per commit | 3.5 | 0.2 | 5.3 | 0.2 |
| Other objects read from the file per commit | 3.5 | 1.0 | 9.5 | 1.0 |
| Frames followed per turn | 1.0, 2.4 KB | | 7.0, 17 KB | |
| CPU per commit, all processes | 33.4 µs | | 47.3 µs | |

The lock passed from one process to the next about as fast as between two threads, the time it
was free coming to 2.0 µs a commit at two processes and 1.6 µs at two threads, and 2.9 and 2.4 µs
at eight, so the difference was nearly all work done holding it: 7.6 µs more a commit at two
processes and 19 µs at eight. Against two threads, the profile put 6.6 µs more of each
commit at two processes in reading nodes back inside the closure: 2.4 µs reading the record, 1.8
µs decoding the node and 2.8 µs putting it in the cache, which evicted another entry each time;
1.5 µs more in BLAKE3, most of it hashing what was read back; and 3.7 µs in the catch-up on taking
the lock, of which the frame took 0.9 µs, the branch's new root and log entry 1.5 µs and the step
0.4 µs. At eight processes against eight threads the reads back took 8.1 µs more, their hashes
2.8 µs and the catch-up 6.0 µs. The other objects are the root, the log entries and the steps:
threads read one log entry a commit too, as appending to the log looks one up.

A turn reads back so many nodes because a node of its path is stale whenever another process
rewrote it since this one last read or wrote it, which at every level below the root is about
(n - 1)/n of the time. A key among 100,000 lies about six levels down, so a turn reads about
1 + 5(n - 1)/n nodes, 3.5 at two processes and 5.4 at eight, as counted, and most of them were
rewritten by commits older than the frames the turn had just followed, which is why the
alternatives under [Measured and left out](#measured-and-left-out) that warm a process's cache
from those frames gain little. Two changes came of it:

- A node a closure reads from the file to change is not cached, and the closure changes it in
  place, as described under [Nodes a commit changes](read-path.md#nodes-a-commit-changes). Each
  node read back had cost a cache insert, an eviction and a copy as the closure changed it.
- A catch-up reads each frame in one read with the next header and checks it in memory, and keeps
  the bytes of the frames it followed for reads of their objects, as described under [Catching
  up](#catching-up). The steps, roots and entries of those frames, and the nodes the next closure
  reads back from them, come from memory, still hashed as they are read.

The same tool on 2026-11-07, pinned the same way with nothing else running, alternating `main`
before the changes, the first change alone and both, median of five rounds under Buffered and three
under Fsync, commits a second:

| Durability | Branches | Writers | Processes, before | First change | Both | Threads, before | Both |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Buffered | one | 1 | 45,555 | 46,201 | 46,331 | 46,173 | 46,128 |
| Buffered | one | 2 | 32,921 | 35,322 | 35,951 | 43,649 | 43,566 |
| Buffered | one | 4 | 26,557 | 28,499 | 29,407 | 40,969 | 41,046 |
| Buffered | one | 8 | 22,162 | 23,787 | 24,738 | 39,096 | 39,161 |
| Buffered | own | 2 | 65,497 | 66,331 | 66,537 | 66,260 | 66,153 |
| Buffered | own | 4 | 88,244 | 88,868 | 88,899 | 90,777 | 91,593 |
| Buffered | own | 8 | 58,393 | 59,008 | 58,669 | 105,972 | 104,797 |
| Fsync | one | 2 | 2,697 | 2,661 | 2,710 | 2,613 | 2,621 |
| Fsync | one | 4 | 5,447 | 5,090 | 5,535 | 5,060 | 5,233 |
| Fsync | one | 8 | 10,038 | 10,046 | 10,126 | 10,614 | 10,014 |
| Fsync | own | 2 | 2,782 | 2,601 | 2,752 | 2,613 | 2,649 |
| Fsync | own | 4 | 5,134 | 5,017 | 5,431 | 5,162 | 5,380 |
| Fsync | own | 8 | 10,190 | 10,279 | 9,941 | 9,900 | 9,792 |

On one branch under Buffered the first change gave 7.3% at two, four and eight processes, and the
second 1.8, 3.2 and 4.0% more, every round of each build above every round of the build before it:
9.2, 10.7 and 11.6% together. Two processes went from 0.75 of what two threads make to 0.83, four
from 0.65 to 0.72 and eight from 0.57 to 0.63, and at eight p50 went from 346 to 312 µs and p99
from 704 to 618 µs. Threads, and processes on branches of their own, which read back little, moved
by at most 1.6% either way. Under Fsync one fdatasync a round is the limit, and the figures moved
by up to 6% either way, as rounds of one build spread: four processes on their own branches made
2,315 to 5,429 commits a second in the rounds before the changes.

On one process neither change moved a figure beyond its rounds' spread. `group`, one writer making
single-key Buffered commits into 100,000 keys, median of three alternating rounds, made 51,597
commits a second before and 51,310 after, and 46,089 and 46,084 with the option. `ferro-bench`'s
single-key Buffered map commit took 23.5 µs and 179,743 instructions before and 23.4 µs and
178,156 after, and `tables`, median of seven rounds, took 86.5, 107.0 and 145.7 µs for a hold, a
confirm and a cancel before and 87.2, 107.6 and 147.8 after, with the instructions outside the
closure within 0.15% and each one's runs spreading over up to 5.3 µs.

The kept frames' first form, described under [Catching up](#catching-up), went in with those
figures. Measured again with the deque, the binary search and the `ArcSwapOption`, in a second run
the same day of five Buffered rounds alternating `main`, the first change and both, one branch made
32,452, 34,990 and 35,759 commits a second at two processes, 26,474, 28,344 and 29,291 at four and
22,099, 23,658 and 24,742 at eight: 10.2, 10.6 and 12.0% over `main`, with every round of each
build above every round of the one before. Branches of their own made 66,599, 87,512 and 57,850 at
two, four and eight processes against 65,017, 87,973 and 57,766 for `main`, and threads moved by
less than 1% but for eight threads on their own branches, 103,622 against 105,368, whose rounds
overlap. Six more Fsync rounds at eight processes spread from about 500 to 10,500 commits a second
in every build as the disk stalled, and the medians, 9,774, 9,450 and 9,422 on one branch and
9,381, 9,659 and 9,680 on branches of their own for `main`, the first change and both, moved in no
one direction.

Two more changes came of a profile of the same tool on 2026-11-12, taken by a sampler built into
probes kept out of the tree. At eight processes on one branch a turn spent about 6 µs following the
seven frames before it: eight reads, one for the first header and one for each frame with the
header after it, each into a buffer allocated and zeroed for it, which alone took about 1.3 µs a
turn, much of it page faults as the heap grew and shrank. A catch-up now reads the frames at once,
into the buffer of the frames it let go before, as [Catching up](#catching-up) describes. And a
writer held its branch lock while it put the nodes it wrote in its cache, 3 to 4 µs a commit that
no other process waits for, which it now does once it has let the lock go, as [A
commit](#a-commit) describes. In a probe that commits single keys through eight shared handles of
one file in turn from one thread, so that each commit follows seven frames and waits for nothing,
user-space instructions fell from 260,260 a commit to 254,630, and with one handle they went from
125,660 to 125,744. At eight processes on one branch each process made 8.6 reads a commit where it
made 15.3.

Other work kept the machine's 32 cores busy, at load averages from 12 to 60, so each round ran the
build before the changes and the build with them at the same time, and the time each process held
its branch lock came from timers around the kernel lock in a build kept for the measurement. Median
of eight rounds on one branch, five on branches of their own and six for one writer, the ratio the
median of each round's ratio, commits a second and µs the branch lock was held a commit:

| Branches | Writers | Processes, before | After | Ratio | Held, before | After | Ratio |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| one | 1 | 37,318 | 37,666 | 1.001 | 23.2 | 19.4 | 0.846 |
| one | 2 | 17,550 | 18,528 | 1.047 | 52.0 | 47.7 | 0.943 |
| one | 8 | 11,486 | 12,218 | 1.027 | 80.5 | 74.0 | 0.921 |
| own | 2 | 31,973 | 32,391 | 1.022 | 46.3 | 44.7 | 0.874 |
| own | 4 | 41,389 | 41,207 | 1.009 | 56.5 | 56.5 | 0.904 |
| own | 8 | 20,978 | 26,967 | 1.121 | 187.1 | 182.9 | 0.868 |

Threads of one process, whose lane still holds the branch while they cache, moved by at most 2% at
the median ratio, from 21,494 and 19,340 commits a second to 21,871 and 19,184 at two and eight on
one branch, and from 57,497 to 62,987 at four on branches of their own. An earlier run the same
day, against the tree before the frame that stores version 0 went in, gave ratios of 1.065 and
1.037 in commits a second and 0.930 and 0.939 in time held at two and eight processes on one
branch.

## Measured and left out

- Shared writers by default. On one process the option costs what its locks and reads cost, 2.5 to
  4.5 µs of CPU a booking commit and 10% of single-key Buffered commits a second, measured above,
  and a file opened without it keeps every promise it made before: a writer keeps the file to
  itself, a read-only follower is the only other process that can open it, and nothing in a commit
  takes a lock or reads past the writer's own end. So it stays an option.
- One lock across processes for every closure, in place of a lock per branch, built by giving every
  branch name the same byte. It is simpler, but closures on different branches in different
  processes then run one at a time. With a branch per process, median of three rounds alternating
  the two, before a followed frame's header was read only once, commits a second:

  | Durability | Closure | Writers | One lock | A lock per branch |
  | --- | ---: | ---: | ---: | ---: |
  | Buffered | none | 2 | 38,373 | 57,146 |
  | Buffered | none | 4 | 32,005 | 72,640 |
  | Buffered | none | 8 | 23,284 | 44,840 |
  | Buffered | 50 µs | 2 | 13,020 | 23,809 |
  | Buffered | 50 µs | 4 | 12,106 | 42,306 |
  | Buffered | 50 µs | 8 | 10,558 | 47,390 |
  | Fsync | none | 2 | 2,600 | 2,661 |
  | Fsync | none | 4 | 5,004 | 5,133 |
  | Fsync | none | 8 | 9,746 | 9,347 |

  Under Fsync the disk hides it. Elsewhere only one closure runs at a time across all the
  processes, so with 50 µs closures they make about 13,000 commits a second however many there
  are, and fewer as they add handoffs.
- Closures checked at placement and run again when another process moved their branch. Every write
  takes an `FnOnce` closure, so running one twice would change the API, and a closure on a busy
  branch could run any number of times. Not built.
- Each process syncing for itself, which the first version did. Single-key commits with fsync,
  median of three alternating runs, commits a second:

  | Branches | Writers | Each syncing alone | One sync for all |
  | --- | ---: | ---: | ---: |
  | one | 2 | 2,778 | 2,654 |
  | one | 4 | 3,179 | 5,094 |
  | one | 8 | 3,532 | 9,922 |
  | own | 2 | 2,629 | 2,617 |
  | own | 4 | 3,172 | 5,334 |
  | own | 8 | 3,487 | 9,972 |

  Alone, each process's fdatasync covered little but its own frames, so 8 processes made about a
  quarter more commits than 2, where 8 threads made 3.8 times as many as 2.
- A mark in the file's header naming the last frame placed, which each placer would read and
  rewrite. It saves reading another process's frames to find their end, but costs a write on every
  commit where reading the header at `placed` costs a read, and a process that dies between writing
  its frame and the mark leaves a whole frame for the next one to write over. Not built.
- Writing frames after the placement lock, as within a process. Another process cannot wait for this
  one's `written`, so it would have to poll for frames it can see placed but not yet whole, and a
  sync would have to as well. Writing the frame under the lock costs its check and copy there, about
  1.5 µs for a 2.5 KB frame. Not built.
- A small shared memory file beside the database, as SQLite's WAL mode keeps, for the locks and the
  end of the log. It would make an uncontended lock an atomic instruction, but a mutex in shared
  memory is left locked by a process that dies holding it unless it is robust, which only Linux
  offers, and it is a second file to keep beside the database. Kernel locks are released on death.
  Not built.
- Following with `Options::follow` so that a writer's thread warms the next turn. The follower
  looks at most once a millisecond while commits keep coming, and only up to the frame a
  superblock names, so at eight processes on one branch a turn still followed 6.95 frames under
  the branch lock where it had followed 7.0, and the processes made 30,738 commits a second at two
  and 21,232 at eight, against about 32,000 and 22,000 without it.
- Catching up before waiting for the branch lock, with a try first and a catch-up when it fails. A
  writer comes back to the lock straight after its own commit, when at most one other frame has
  landed, and the frames it will need land while it waits in `F_OFD_SETLKW`, where it can read
  none of them. Not built.
- Putting every object of a followed frame in the cache as the catch-up reads it, decoded or as a
  checked record that the first read decodes. The nodes a turn reads back were rewritten by any
  commit since this process last touched them, and few of them by the frames it has just
  followed: a node of its path is stale when another process rewrote it since, about
  (n - 1)/n of the time at every level below the root, so of the 5.3 nodes a turn read at eight
  processes about 2.2 were in the seven frames it followed. The store does not know the types to
  decode the rest, a checked record would still be decoded and cached on its first read, and the
  other processes' frames hold about ten objects each, so at eight processes each would hash and
  cache some 70 objects for each commit of its own to save the read and the hash of the one or two
  of them, at levels two to four, that it reads back before the cache evicts them. A frame's xxh3
  checks do not stand in for the hash: they find a torn or damaged write, not a frame made to
  match them, and an object cached under a hash it does not have would answer every later read of
  that hash. Not built.
- Reads that skip the hash, measured only as a bound on what moving the hash off the branch lock
  could gain. With both changes of [Reading back what other processes
  wrote](#reading-back-what-other-processes-wrote), in an earlier form, and no hash on any read of
  a shared writer, two, four and eight processes on one branch made 36,942, 31,369 and 27,016
  commits a second, against 34,453, 28,275 and 24,497 with the hash, median of three alternating
  rounds: 7 to 10%. Every read hashes its object.
- The frames a catch-up keeps in one buffer, grown as it reads each frame, rather than a buffer for
  each. On branches of their own, where every placement follows a frame or two under the placement
  lock, the buffer's reallocations cost four processes 6%: 82,409 commits a second against 87,224,
  median of three alternating rounds, swapped in through an `ArcSwap`, where a buffer for each
  frame and a mutex made 87,395. Sized for the first frame and swapped under a mutex, one buffer
  still made 84,422 against 86,602 over five rounds, though at eight processes on one branch it
  made 24,807 against 23,995 for a buffer each. The buffer for each frame stays, since it costs a
  writer on its own branch nothing, and with one for each frame the `ArcSwapOption` that reads now
  load measured the same as the mutex on one branch and on branches of their own, as above.
- glibc's `trim_threshold` and `top_pad`, through `GLIBC_TUNABLES`, since the profile showed
  `mprotect` from growing a thread's arena under the 8 KiB hash table that LZ4 allocates for each
  compression: 32,306 and 22,141 commits a second at two and eight processes against 32,106 and
  22,030, within the spread of the rounds. The table is `lz4_flex`'s and the same with threads.

## Limits

- All the processes must be on one machine. The file system's page cache is what makes a frame one
  process wrote readable by another, and network file systems give no such guarantee, nor reliable
  locks.
- Each process has its own cache, so on one branch a process reads back the nodes another process
  wrote before its closure can change them.
- A process sees other processes' commits only when it commits, attaches a branch or calls
  `refresh`, unless it follows the file with `Options::follow`. One that does none of these holds
  the file a cycle replaced, as a read-only handle that never refreshes does.
- A follower is woken by inotify only on 64-bit Linux, with one inotify instance and two watches
  for each open that follows, which count against the user's `max_user_instances` and
  `max_user_watches`. Elsewhere, and past those limits, it looks every 10 ms, as described under
  [Followers](#followers).
- Two shared `Db`s of one file in one process behave as two processes. A closure that writes through
  the other one to the same branch waits for itself, and `Error::Reentrant` cannot see it.
- An open of a Buffered file with live writers scans up to `checkpoint_bytes`.
- Only 64-bit Linux. Windows has byte-range locks, `LockFileEx`, but a blocking one on a handle
  opened without `FILE_FLAG_OVERLAPPED`, as `std` opens every file, holds up every other I/O call on
  that file object while it waits, so a writer waiting for another process's branch would stall this
  process's readers, and two processes could deadlock on each other's reads. Doing it right means
  a second handle for the locks, or overlapped waits, and none of it can be run here: CI only checks
  that the Windows build compiles, so the option fails with `Unsupported` there until a Windows run
  tests it. macOS and the BSDs have no open file description locks, and 32-bit Linux has an `off_t`
  too narrow for the offsets.
