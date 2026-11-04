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
so rename the field to rebuild it. Turning an `#[index]` into a `#[unique]` rebuilds the index, and
fails to open if two rows share a value, as described under [Unique
indexes](../internals/schema-and-unique-indexes.md#unique-indexes). Adding, removing or reordering
the fields in an index's `cover(...)` list rebuilds the table's indexes from the rows when the table
is read, and the next commit stores them, as described under [Covering
indexes](../internals/schema-and-unique-indexes.md#covering-indexes). A list naming only the
index's own fields stores nothing and rebuilds nothing.

[Schema evolution](../internals/schema-and-unique-indexes.md#schema-evolution) describes the
encoding.
