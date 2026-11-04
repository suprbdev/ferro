# Changing your types

Your struct is the schema. Fields are stored by name hash, so old data keeps loading after you edit
the struct:

- Reordering fields is always safe.
- A removed field is skipped when old data is read.
- A new field needs a default: `Option`, `Vec`, `Map`, `Set`, `List` and `Table` start empty, and
  anything else takes `#[ferro(default)]` or `#[ferro(default = "path::to::fn")]`.
- To rename a field, use `#[ferro(rename = "old_name")]`.

Do not change the encoding or `Ord` of a key type on existing data. Tree placement depends on both.

A field marked `#[ferro(skip)]` is not stored and reads back as its `Default`. A `Summary` must not
depend on one, as described under [Summaries](../internals/summaries.md#how-a-summary-is-kept).
Changing a summary type `S` changes a map's encoding under the same field tag, as changing `V` does,
so rename the field to rebuild it.

## Indexes

Each index of a `Record` is stored with a fingerprint of its kind, the key's type, and the stored
names and types of the fields it is keyed on and the fields it covers. When a table is read, an
index whose stored fingerprint differs from the one the struct gives is rebuilt from the rows, and
the next commit stores it. The table's other indexes are kept. A fingerprint costs eight bytes per
index in every commit. Any of these rebuilds that one index:

- switching between `#[index]`, `#[index(each)]` and `#[unique]`;
- adding, removing or reordering the fields in its `cover(...)` list;
- giving one of its fields another stored name with `#[ferro(rename)]`;
- changing the type of one of its fields, or of the key.

These rebuild nothing: renaming a field in Rust while `#[ferro(rename)]` keeps its stored name,
writing a type through an alias, a `Box`, an `Arc` or a `#[ferro(transparent)]` wrapper, renaming a
struct, and reordering a struct's fields. A cover list naming only the index's own fields stores
nothing and rebuilds nothing. Turning an `#[index]` into a `#[unique]` fails to open if two rows
share a value, as described under [Unique
indexes](../internals/schema-and-unique-indexes.md#unique-indexes).

The fingerprint follows encodings, not ordering. The values of an indexed field are keys of its
index, so the rule above for key types applies to them: changing how an indexed type compares, for
example reordering the fields of a struct that derives `Ord`, is not noticed and leaves lookups
missing rows. A type with a hand-written `Data` impl is fingerprinted by its type name, so give it a
`describe` of its own if its encoding can change while its name stays. [Index
fingerprints](../internals/schema-and-unique-indexes.md#index-fingerprints) has the details.

A file written before indexes carried fingerprints opens normally. Its indexes are rebuilt the first
time it is read and stored by the next commit. An older version read through `at`, `as_of`, history
or a merge rebuilds its indexes each time it is read when they were written under another layout,
which includes every version written before fingerprints.

[Schema evolution](../internals/schema-and-unique-indexes.md#schema-evolution) describes the
encoding.
