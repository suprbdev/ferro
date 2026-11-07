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
same value. A default on a field an index is keyed on or covers must return the same value every
time it is called, or the index is rebuilt every time the program starts. A cover list naming only
the index's own fields stores nothing and rebuilds nothing. Turning an `#[index]` into a `#[unique]`
fails to open if two rows share a value, as described under [Unique
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
