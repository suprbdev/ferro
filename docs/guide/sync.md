# Sync between files

Two files that share a history, or have none yet, can exchange commits, so each device works offline
and catches up later.

## Pulling

```rust
let laptop = Db::<Shop>::open("laptop.ferro")?;
let phone = Db::<Shop>::open("phone.ferro")?;

let pulled = laptop.pull(&phone, "phone", |shop, base, theirs| {
    let applied = shop.orders.merge(&base.orders, &theirs.orders, |conflict| newer(conflict));
    ("sync from the phone", applied)
})?;
println!("{:?}, {} bytes in", pulled.outcome, pulled.received_bytes);
```

`pull` copies the other file's branch into this one as the branch `phone`, then brings this branch
up to it. It returns a `Pulled` with the `outcome`, the merge closure's `value` if it ran, the
`version` this branch is at afterwards, the number of `objects` the bundle carried, and the
`sent_bytes` and `received_bytes` of the transfer. The outcome is `UpToDate`, `Ahead` when this
branch already has all of it, `FastForward` when this branch has nothing it lacks, or `Merged`. Only
a merge calls your closure. It works as in `merge_with`: the merge commit links the remote commit it
merged, and the base is a best common ancestor over the merge graph of every file the commits passed
through, or the virtual base of all of them after a criss-cross, which devices that sync both ways
make, so a change that reaches you through a third device is merged once, and a revert routed
through one is not lost. `push` is `pull` from the other side. `try_pull`, `try_push`, `try_receive`
and `try_receive_from` take a closure that returns a `Result`. An `Err` leaves this branch and its
merge records as they were and comes back as your error, while the other side's commits stay landed
on `phone`, so the next pull sends nothing new and merges them again.

## Over a network

Over a network the same pull is three calls, and only bytes cross: the phone sends its `have`, the
laptop answers with a `bundle`, and the phone applies it with `receive`.

```rust
let have: Vec<u8> = phone.have()?.into();
let bundle: Vec<u8> = laptop.bundle(&Have::from(have))?.into();
phone.receive("laptop", &Bundle::from(bundle), merge)?;
```

A bundle can also stream, so neither side holds it whole. On the laptop, `bundle_to` writes it to
any `io::Write`, such as a socket or a response body, and returns the bytes written. On the phone,
`receive_from` reads it from any `io::Read` to its end:

```rust
let have = phone.have()?;
let sent = laptop.bundle_to(&have, BufWriter::new(response))?;
let pulled = phone.receive_from("laptop", BufReader::new(download), merge)?;
```

## What it costs and what it checks

`bundle` and `receive` are the same calls on a byte buffer, and `pull` streams between the two
files. Cloning a 330 MB file of a million keys took 0.91 s and 65 MiB at peak on 2026-11-04, and a
pull of three small commits into it 1.3 ms with fsync. The in-memory version that streaming
replaced took 1.1 GB, and 2.6 s where streaming took 2.3 s in the same runs, with small pulls as
fast. Staging the bundle in a scratch file, so that commits go on while it arrives, added about a
seventh to a clone and nothing to its memory.

A have lists the commits the pulling file holds, a few hundred bytes. A bundle carries the commits
it lacks and the objects their trees reach that it does not hold, so it costs what changed: three
commits on a 9 MB file of 100,000 keys sent 15 KB. The first pull into an empty file carries
everything. `receive` hashes every object and checks every link as it reads, and checks every object
it is sent as the type that will read it: each map node, spilled value, nested collection, table
index and summary decodes, and each node's count, level and key range agree with its parent. It
costs about half again the time of applying a bundle, and nothing for the objects it already holds.
Objects are staged as they arrive, in memory up to 1 MiB and past that in a scratch file beside the
database, and `receive` holds no writer lock while it reads, so commits on every branch go on
however slowly the bundle comes. Only once the whole bundle has passed are its objects copied into
the file, beside those commits, where nothing can find them until one frame points the remote branch
at them, so a crash leaves the previous head. Only that frame takes the writer locks, and it checks
again that the commits the bundle builds on are still here. A damaged or forged bundle is rejected
with `Error::Bundle` and never reaches the file. `receive` uses a fixed buffer, the object it is
reading and under 500 bytes for each object the bundle carries, not memory for its bytes or for what
it claims, and refuses commits dated more than a day past this machine's clock.

## When the other side has moved

If this file does not hold the commit a bundle builds on, because the bundle was built for another
file, or this one pruned or dropped that commit after sending its have, even while the bundle was
arriving, `receive` returns `Error::StaleHave` and lands nothing: build a new bundle from a fresh
have and try again. `pull` returns the same error rather than retrying, so call it again, as
`examples/notes` does once.

## Versions and hashes across files

A pulled commit keeps the version, time, message and hash it had. A fast-forward adopts the other
branch, so afterwards `head_hash` is the same in both files. A merge commits at this branch's next
version, and the other side's commits stay on the `phone` branch under their own numbers, so after a
divergence compare commits across files by hash, not by version. A pull that brings new commits
replaces the `phone` branch, so do not commit to it. A bundle that arrives late or twice changes
nothing. With bounded history, keep at least what each device commits between syncs: a pull whose
base only pruned history could show, such as one from a device that has pruned past the commit you
last shared, lands the other branch and returns `NoCommonAncestor` instead of merging against
nothing, which would bring back what the other device deleted.

## Converging devices

Devices converge only if your closure gives the same result whichever side is `ours`, as
last-writer-wins on a clock does. `examples/notes` is an offline-first notes app built this way:
each device directory holds its own file, and `sync` exchanges changes both ways.

```sh
cargo run --release -p notes -- --device laptop add "buy milk" --tag home
cargo run --release -p notes -- --device phone sync laptop
cargo run --release -p notes --bin notes-scale
```

## Limits

Sync streams, but keeps under 500 bytes per object received and 110 per object sent, and holds each
object whole, so a clone of tens of millions of objects or of one huge value still needs memory to
match. A bundle over 1 MiB is written twice, to a scratch file beside the database while it arrives
and then into the file, so it needs disk for both until it lands. A crash while it is copied in, or
a bundle refused because this file pruned what it builds on while it arrived, leaves the copy in the
file until the next reclamation cycle. Files that share no commit send everything on their first
pull.

[Limits](../internals/sync.md#limits) in the internals lists the rest.

## How it works

- [Sync](../internals/sync.md): haves, bundles, the checks on landing and bringing the branch up.
- [Receiving a bundle](../internals/sync-receive.md): staging, commits stored twice and type checks.
