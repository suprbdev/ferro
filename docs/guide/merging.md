# Merging

```rust
let what_if = db.fork("what-if")?;
what_if.commit("try a price change", |shop| { /* ... */ })?;

db.merge_with(&what_if, |shop, base, theirs| {
    let applied = shop.orders.merge(&base.orders, &theirs.orders, |conflict| {
        conflict.ours.as_deref().cloned()
    });
    ("merge what-if", applied)
})?;
```

Merges are three-way and cost O(changes). You decide conflicts in Rust, and you merge each
collection in your root yourself. A merge commit links the commit it merged, and the base is found
over the whole merge graph, as git does, so a change is merged once however it arrives: directly,
back and forth, through a third branch or through a fork of a branch that was already merged.
Finding the base costs about two binary searches over the log, plus a step for each merge made since
the base.

## What counts as a change

Merges and `diff` compare values as they are stored, not with `==`: two values are the same when
they encode to the same bytes. A row holding NaN that neither side touched is unchanged, though NaN
is not equal to itself, and a change from `0.0` to `-0.0`, or to a field your `PartialEq` leaves
out, is a change, though `==` holds. So `diff`, `merge` and `try_merge` no longer need `PartialEq`
on values or rows. `Data::same_encoding(&a, &b)` makes the same comparison for a `resolve` closure
that compares what it is handed. Keys are still told apart by `Ord`.

## Criss-crosses

When two branches have each merged an older state of the other, they have several best common
ancestors, a criss-cross, and `common_ancestors` returns them all, best first. `merge_with` then
does what git's recursive strategy does: it first merges those ancestors into a virtual base with
your closure, and then runs your merge against that base, so a counter delta that only one of them
carries is still applied once. The closure is therefore `FnMut`. Without a criss-cross it runs once,
as before. With one it first runs once for each ancestor after the first, with a working copy of the
base built so far as `ours`, the next ancestor as `theirs` and their own base. It resolves conflicts
there the way it always does, and what it returns from those calls is dropped. The virtual base is
never written: its commit has version 0, no root, the latest time of what it merged and the message
your closure returned for it.

## Merges that can fail

A merge that finds it cannot go ahead uses `try_merge_with`, whose closure returns a `Result` as
`try_commit_with`'s does. `Err` discards the merge, including an `Err` while the virtual base is
built: nothing is written or recorded, the next merge finds the same base, and the error comes back
in your own type. `Map::try_merge` and `Table::try_merge` take a `resolve` that returns a `Result`,
stop at its first `Err` and leave the collection as it was.

## Limits

After a criss-cross, a merge closure runs more than once: once for each extra best common ancestor,
to build the virtual base, then for the merge. A closure with effects outside its return value, such
as logging each conflict, has them for the ancestors too. When the base of those ancestors could lie
in pruned history, the merge refuses with `NoCommonAncestor`, even though `common_ancestors`
answers.

## How it works

[Merging and the merge graph](../internals/merging.md) describes the base search, the virtual base
and how merges keep the commits they brought in when history is pruned.
