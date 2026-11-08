# Changing your types

Your struct is the schema. Fields are stored by name hash, so old data keeps loading after you edit
the struct:

- Reordering fields is always safe.
- A removed field is skipped when old data is read. List its stored name on the struct, as
  `#[ferro(removed("title_score"))]`, as described below.
- A new field needs a default: `Option`, `Vec`, `BTreeMap`, `BTreeSet`, `Bytes`, `Map`, `Set`,
  `List` and `Table` start empty, a `Box` or `Arc` of one of them does too, and anything else takes
  `#[ferro(default)]` or `#[ferro(default = "path::to::fn")]`.
- To rename a field, use `#[ferro(rename = "old_name")]`.

A field or variant written as a raw identifier, such as `r#type`, is stored under its name without
the `r#`, here `type`, and its indexes and lookups are named the same way, as `by_type`. Earlier
builds stored it under `r#type` and could not index it, so to read data they wrote, mark the field
`#[ferro(rename = "r#type")]`.

Do not change the encoding or `Ord` of a key type on existing data. Tree placement depends on both.

A field is stored under a 16-bit hash of its name, so a field you add has a 1 in 65,536 chance of
hashing to the tag of a field you removed. Old data then still holds bytes under that tag, and the
new field reads them: the removed field's value if the bytes decode as the new field's type, and a
failed read if they do not. To rule that out, list every removed field's stored name on the struct:

```rust
#[derive(Data, Clone)]
#[ferro(removed("title_score"))]
struct Post {
    id: u32,
    #[ferro(default)]
    flags: u32,
}
```

The derive then refuses a field whose stored name has the tag of a listed one, here `flags`, and
`#[ferro(rename = "...")]` stores it under another name. It refuses a listed name the struct still
has too, so take a name off the list to bring its field back. The same attribute on an enum lists
its removed variants, whose old data otherwise decodes as a new variant with the same tag, and on a
variant its removed fields. The derive cannot see a field that has gone, so it checks only the names
you list.

A field marked `#[ferro(skip)]` is not stored and reads back as its `Default`. The `Record` derive
refuses `#[key]` or an index of any kind on one, alone or in a composite, and a `Summary` must not
depend on one, as described under [Summaries](../internals/summaries.md#how-a-summary-is-kept).

A map stores the fold of every subtree, and nothing in the file says which summary type `S` made it.
A new `S` that encodes differently fails to decode, as a changed `V` does. One that encodes the same
way, such as another transparent `u64`, or the same type with another `of` or `combine`, reads the
old folds as its own: `summary()`, and a range summary wherever it takes a stored fold, return what
the old `S` folded, with no error, until `verify_typed` reports them. So whenever `S` or what it
folds changes, give the field a new stored name, so that the map starts empty, and fill it from the
old field, which reads as before under its old name with the old `S` until you remove it.

## Indexes

Each index of a `Record` is stored with a fingerprint of its kind, the key's type, and the stored
names and types of the fields it is keyed on and the fields it covers. When a table is read, an
index whose stored fingerprint differs from the one the struct gives is rebuilt from the rows, and
the next commit stores it. The table's other indexes are kept. A fingerprint costs eight bytes per
index in every commit. Any of these rebuilds that one index:

- switching between `#[index]`, `#[index(each)]` and `#[unique]`;
- adding, removing or reordering the fields in its `cover(...)` list;
- giving one of its fields another stored name with `#[ferro(rename)]`;
- changing the type of one of its fields, or of the key;
- changing what the `#[ferro(default)]` or `#[ferro(default = "...")]` of one of its fields returns,
  which is what rows written before that field existed hold in the index;
- changing what such a default returns on a field of a struct or enum that one of its fields'
  types holds, at any depth, as `Meta` is held in `Option<Meta>` or `Vec<Meta>`;
- changing what `Data::missing` returns for a type with a hand-written `Data` impl that one of its
  fields is or holds, at any depth, unless that impl overrides `describe`.

These rebuild nothing: renaming a field in Rust while `#[ferro(rename)]` keeps its stored name,
writing a type through an alias, a `Box`, an `Arc` or a `#[ferro(transparent)]` wrapper, renaming a
struct, reordering a struct's fields, and pointing a default at another function that returns the
same value. That holds for an `Option` too: a unique index leaves `None` out however the `Option` is
written, and only the generated lookups change, as described under [Unique
indexes](tables.md#unique-indexes). A file written before indexes told an `Option` by its type
rebuilds, once, the first time it is read, each unique index on an `Option` and each index on an
`Option` written other than as `Option<T>`. A default on a field an index is keyed on or covers must
return the same value every time it is called, or the index is rebuilt every time the program
starts. A cover list naming only the index's own fields stores nothing and rebuilds nothing. Turning
an `#[index]` into a `#[unique]` fails to open if two rows share a value, as described under [Unique
indexes](../internals/schema-and-unique-indexes.md#unique-indexes).

The fingerprint follows encodings, not ordering. The values of an indexed field are keys of its
index, so the rule above for key types applies to them: changing how an indexed type compares, for
example reordering the fields of a struct that derives `Ord`, is not noticed and leaves lookups
missing rows. A type with a hand-written `Data` impl is fingerprinted by its type name and the value
its `missing` returns, so give it a `describe` of its own if its encoding can change while its name
stays, and have that `describe` include the value `missing` returns, if it returns one, since an
index holds that value for rows written before the field existed. [Index
fingerprints](../internals/schema-and-unique-indexes.md#index-fingerprints) has the details.

A file written before indexes carried fingerprints opens normally. Its indexes are rebuilt the first
time it is read and stored by the next commit. An older version read through `at`, `as_of`, history
or a merge rebuilds its indexes each time it is read when they were written under another layout,
which includes every version written before fingerprints.

[Schema evolution](../internals/schema-and-unique-indexes.md#schema-evolution) describes the
encoding.

### Index changes and sync

A pull refuses a table that holds an index the receiving struct does not declare, a struct with no
index at all included, with `Error::Bundle`, because nothing could check that index as it landed,
and a later version of the struct that declared it would trust what it holds. A pull also refuses
a table with a row stored under a key other than its own, whether or not the struct has indexes.
So when peers sync a table, upgrade in this order:

- When a table gains an index, or an index changes kind (`#[index]`, `#[index(each)]`,
  `#[unique]`), fields, cover list, or the stored names, types or defaults of its fields, the
  receiving side must run the new schema before it pulls from a peer that already does.
- The older direction still syncs: a side on the new schema pulls from a peer still on the old one,
  whose tables lack the index, and each such table lands and rebuilds the index when it is read. A
  new unique index that two rows share a value of fails that read, and the pull lands nothing.
- A side that dropped or changed an index refuses tables that still hold the old one, so a changed
  index syncs only between peers that both run the change, and a clone into the new schema of a
  history whose versions hold tables written under the old one is refused, until the sender has
  written those tables again under the new schema and pruned the versions before, as below.

A table is stored as it was written until a commit writes it again. A table held in the root struct,
directly or inside an `Option`, a `Vec`, a tuple or another struct, is written again by the next
commit, whatever that commit changes, with its indexes as its struct now declares them, so it needs
no step of its own. Its rows, like the values of a `Map` or a `List`, are written again only where
a commit changes the node that holds them. So after an index change most tables in the values of a
map, or in the rows of another table, keep the old index in every new version too, and a clone into
the new schema stays refused however much history is pruned. So after an index change, rewrite each
map whose values hold such tables, and each table whose rows hold them, in one commit, then prune
the versions before it:

```rust
db.update(|root| {
    root.teams.rewrite();
    root.projects.rewrite();
})?;
db.compact(Keep::Last(1))?;
```

`Map::rewrite`, `List::rewrite` and `Table::rewrite` read every value or row, which rebuilds the
changed indexes of the tables in it and drops the indexes their struct no longer declares, and the
commit writes every node again. They hold the whole map or table in memory until the commit. A
collection inside a value is written as it was, so a map whose values hold maps of tables, or a
table whose rows hold tables of tables, is rewritten a level at a time, rewriting each inner map or
table in the same commit. Until the versions before the rewrite are pruned, a clone of them is
refused, and a peer that already holds them pulls the new versions as usual.

A struct's own file is never refused: its older versions keep their indexes under the old
fingerprints and rebuild them when read through `at`. [Indexes that agree with their
rows](../internals/sync-receive.md#indexes-that-agree-with-their-rows) describes the check.
