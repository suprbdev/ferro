//! How values become bytes: the [`Data`] trait and the encoding it writes.
//!
//! Everything ferro stores is an [`Object`]: a body of bytes and a list of links, each a [`Ptr`] to
//! another object. An object's [`Hash`](type@Hash) covers its body and the hashes of its links, so
//! the hash of a value covers everything it links to, and two values with the same contents have
//! the same hash wherever they are stored. Root hashes, `==` on collections, the skip of a commit
//! that changes nothing, [`Map::diff`](crate::Map::diff) and merges all go by these hashes and
//! encodings.
//!
//! A type is stored through its [`Data`] impl, which writes a value into an [`Encoder`] and reads
//! it back from a [`Decoder`]. ferro implements it for integers, floats, `bool`, `char`, `String`,
//! [`Bytes`], `Option`, `Vec`, arrays, tuples of up to twelve fields, `Box`, `Arc`, `BTreeMap`,
//! `BTreeSet` and its own collections, and [`#[derive(Data)]`](derive@crate::Data) implements it
//! for a struct or an enum, storing each field under a tag hashed from its name so that the type
//! can change while old data keeps loading. An encoder hands each object it finishes to a [`Sink`]:
//! a commit's sink writes it to the file, and a [`HashSink`] keeps only its hash.
//!
//! ```
//! use ferro::codec::{Decoder, Encoder, HashSink};
//! use ferro::{Data, hash_of};
//!
//! let value = (7u32, "seven".to_string(), vec![1u8, 2]);
//!
//! let mut sink = HashSink;
//! let mut encoder = Encoder::new(&mut sink);
//! value.encode(&mut encoder)?;
//! let object = encoder.into_object();
//! assert_eq!(object.hash(), hash_of(&value));
//!
//! let mut decoder = Decoder::new(&object.body, &object.links, None);
//! assert_eq!(<(u32, String, Vec<u8>)>::decode(&mut decoder)?, value);
//! # Ok::<(), ferro::Error>(())
//! ```
//!
//! [Changing your types](https://github.com/archlemon/ferro/blob/main/docs/guide/schema-evolution.md)
//! has the rules for evolving a stored type, and
//! [the storage internals](https://github.com/archlemon/ferro/blob/main/docs/internals/storage.md)
//! describe objects, hashes and how they are laid out in the file.

use std::any::{Any, TypeId, type_name};
use std::borrow::Borrow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::{Deref, Range};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use crate::error::{Error, Result};
use crate::map::unwound;
use crate::pool::{self, Pool};
use crate::shared::Erased;
use crate::stack;
use crate::store::Store;

/// The length of a [`Hash`](type@Hash) in bytes.
pub const HASH_BYTES: usize = 20;

/// The most levels a value may nest within one object.
///
/// Each `Box`, `Arc`, `Vec`, `BTreeMap` and `BTreeSet` a value holds opens a level for what it
/// holds, and so does each [`Encoder::nested`] and [`Decoder::nested`] call of a hand-written impl.
/// A type can hold itself only behind a pointer or in a collection, so the count bounds how deep
/// decoding, checking, pairing and encoding any value recurse. Each level runs on the thread's
/// stack while it has room, and on a segment of stack mapped for the thread once it does not, so a
/// value at the limit reads and writes on any thread, and the count bounds the stack a recursion
/// takes. A [`Decoder`] refuses a value that nests deeper with [`Error::Decode`], so that a forged
/// object cannot make it take more, and an [`Encoder`] refuses to write one with
/// [`Error::TooDeep`]. An encoding counts on through the nodes of the collections it writes: the
/// values in the nodes of a map, set, list or table that it writes nest two levels below the
/// value that holds the collection.
pub const MAX_DEPTH: usize = 4096;

/// The hash of an [`Object`]: the first [`HASH_BYTES`] bytes of a BLAKE3 hash over its body
/// length, link count, body and the hashes of its links.
///
/// [`hash_of`] gives the hash of any value, and [`Map::root_hash`](crate::Map::root_hash) that of a
/// whole map.
pub type Hash = [u8; HASH_BYTES];

/// A link to an object: its hash, and where it lies in the file.
///
/// The hash names the content, and the offset lets a reader go straight to it. Only the hash is
/// part of the hash of the object that links here, so content hashes the same wherever it is
/// stored, and an offset means something only inside the file it was read from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ptr {
    /// The hash of the object linked to.
    pub hash: Hash,
    /// The offset of the object's record in the file, or 0 for an object that was only hashed.
    pub offset: u64,
}

/// An encoded value: its bytes, and its links to the objects it holds.
///
/// Links are kept apart from the body, in the order the encoder wrote them and the decoder reads
/// them, so a value's bytes do not depend on where the objects it links to are stored.
pub struct Object {
    /// The bytes the value encoded to.
    pub body: Vec<u8>,
    /// The links the value encoded, in order.
    pub links: Vec<Ptr>,
}

const JOINED_BYTES: usize = 4096;
const SCRATCH_BYTES: usize = 4096;

thread_local! {
    static JOINED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static SCRATCH: RefCell<[Vec<u8>; 2]> = const { RefCell::new([Vec::new(), Vec::new()]) };
}

/// The [`Hash`](type@Hash) of an object with this body and links with these hashes, as
/// [`Object::hash`] computes it, for callers that hold the parts apart.
pub(crate) fn hash_parts<'a>(body: &[u8], links: impl ExactSizeIterator<Item = &'a Hash>) -> Hash {
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(&(body.len() as u32).to_le_bytes());
    header[4..].copy_from_slice(&(links.len() as u32).to_le_bytes());
    let total = header.len() + body.len() + links.len() * HASH_BYTES;
    let full = if total <= JOINED_BYTES {
        JOINED.with_borrow_mut(|joined| {
            joined.clear();
            joined.extend_from_slice(&header);
            joined.extend_from_slice(body);
            for link in links {
                joined.extend_from_slice(link);
            }
            blake3::hash(joined)
        })
    } else {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&header);
        hasher.update(body);
        for link in links {
            hasher.update(link);
        }
        hasher.finalize()
    };
    let mut out = [0u8; HASH_BYTES];
    out.copy_from_slice(&full.as_bytes()[..HASH_BYTES]);
    out
}

impl Object {
    /// The object's [`Hash`](type@Hash): BLAKE3 over the body length, the link count, the body
    /// and the hashes of the links, cut to [`HASH_BYTES`]. The links' offsets take no part.
    pub fn hash(&self) -> Hash {
        hash_parts(&self.body, self.links.iter().map(|link| &link.hash))
    }
}

/// An object already hashed, and compressed if that made it smaller, ready for a [`Sink`] to
/// store.
///
/// A commit that writes a large tree encodes and compresses its new nodes on several threads, then
/// hands them to [`Sink::put_staged`] in order, borrowing their bytes from one buffer.
#[derive(Clone, Copy)]
pub struct Staged<'a> {
    /// The length of the body before compression.
    pub raw: usize,
    /// The body as it is to be stored: LZ4 compressed if `packed`, as is otherwise.
    pub payload: &'a [u8],
    /// Whether `payload` is compressed.
    pub packed: bool,
    /// The object's links, in order.
    pub links: &'a [Ptr],
}

impl Staged<'_> {
    /// The body before compression.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Corrupt`] if a compressed payload does not decompress to `raw` bytes.
    pub fn body(&self) -> Result<Vec<u8>> {
        if !self.packed {
            return Ok(self.payload.to_vec());
        }
        lz4_flex::block::decompress(self.payload, self.raw)
            .map_err(|error| Error::Corrupt(format!("bad staged body: {error}")))
    }
}

/// A value that a store's cache can hold: every [`Data`] type, and the store's own records.
pub trait Cached: Any + Send + Sync {}

impl<T: Data> Cached for T {}

#[doc(hidden)]
pub struct Moves {
    from: u64,
    old: Vec<u64>,
    new: Vec<u64>,
}

impl Moves {
    pub(crate) fn new(from: u64, old: Vec<u64>, new: Vec<u64>) -> Moves {
        Moves { from, old, new }
    }

    pub(crate) fn offset(&self, offset: u64) -> Option<u64> {
        if offset < self.from {
            return Some(offset);
        }
        self.old.binary_search(&offset).ok().map(|at| self.new[at])
    }

    pub(crate) fn ptr(&self, ptr: Ptr) -> Option<Ptr> {
        Some(Ptr {
            hash: ptr.hash,
            offset: self.offset(ptr.offset)?,
        })
    }
}

#[doc(hidden)]
pub type Moved = HashMap<usize, Erased>;

#[doc(hidden)]
pub type Relocate = fn(&Erased, usize, &Moves, &Moved) -> Option<Erased>;

/// Where an [`Encoder`] puts the objects it finishes.
///
/// A commit writes into a sink that appends each object to the file, a reclamation cycle into one
/// that copies into a new file, and [`HashSink`] keeps nothing but the hashes, for
/// [`hash_of`] and for comparing encodings.
pub trait Sink {
    /// Stores `object`, whose hash is `hash`, and returns the link to it.
    ///
    /// # Errors
    ///
    /// Returns the error that stopped the object being stored, such as [`Error::Io`] or
    /// [`Error::TooLarge`].
    fn put(&mut self, object: Object, hash: Hash) -> Result<Ptr>;
    /// Whether the sink stores bodies compressed, so that a staged object should be compressed
    /// before it is handed over. The default is `false`.
    fn packs(&self) -> bool {
        false
    }
    /// Stores an object that was hashed, and maybe compressed, ahead of time, and returns the link
    /// to it.
    ///
    /// The default decompresses the body and calls [`put`](Sink::put).
    ///
    /// # Errors
    ///
    /// As for [`put`](Sink::put), and [`Error::Corrupt`] if the body does not decompress.
    fn put_staged(&mut self, hash: Hash, staged: Staged<'_>) -> Result<Ptr> {
        let object = Object {
            body: staged.body()?,
            links: staged.links.to_vec(),
        };
        self.put(object, hash)
    }
    /// Checks that `ptr`, which a value being encoded holds, names an object in the sink's file.
    /// The default accepts every link.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StalePointer`] for a link that names no object in the file, such as one
    /// into an earlier generation of it, and the error of reading the file if that fails.
    fn check_link(&self, _ptr: Ptr) -> Result<()> {
        Ok(())
    }
    /// Offers the sink the in-memory node it has just stored at `ptr`, with its weight in the
    /// cache, so that a store can keep it cached rather than read it back. The default drops it.
    fn remember(&mut self, _ptr: Ptr, _value: Erased, _weight: u32) {}
    #[doc(hidden)]
    fn remember_moving(
        &mut self,
        ptr: Ptr,
        value: Erased,
        weight: u32,
        _relocate: (Relocate, usize),
    ) {
        self.remember(ptr, value, weight);
    }
    /// The id of the store the sink writes into, or 0 for a sink with none.
    ///
    /// A collection compares it with the id of the store its own nodes come from, and copies
    /// itself entry by entry into a sink of another store, since a link means something only
    /// inside its own file.
    fn store_id(&self) -> u64;
    /// The store the sink writes into, if any, through which nodes it stores can be decoded
    /// again. The default is `None`.
    fn store(&self) -> Option<Store> {
        None
    }
}

/// A [`Sink`] that stores nothing and returns each object's hash with offset 0.
///
/// [`hash_of`] encodes into one, and so does [`Map::root_hash`](crate::Map::root_hash) for a map
/// with changes that are not yet committed.
pub struct HashSink;

impl Sink for HashSink {
    fn put(&mut self, _object: Object, hash: Hash) -> Result<Ptr> {
        Ok(Ptr { hash, offset: 0 })
    }

    fn put_staged(&mut self, hash: Hash, _staged: Staged<'_>) -> Result<Ptr> {
        Ok(Ptr { hash, offset: 0 })
    }

    fn store_id(&self) -> u64 {
        0
    }
}

/// A type that ferro can store: how its values encode to bytes and links, and decode back.
///
/// Derive it with [`#[derive(Data)]`](derive@crate::Data) for a struct or an enum. The derive
/// stores each field under a tag hashed from its name, so fields can be reordered, added with a
/// default and removed while old data keeps loading, as its docs describe. Implement it by hand
/// only for a type whose encoding you want to control, such as a compact key.
///
/// # What an implementation must guarantee
///
/// - **Decoding inverts encoding.** [`decode`](Data::decode) of what [`encode`](Data::encode)
///   wrote gives back a value that encodes to the same bytes and links.
/// - **Items are counted alike.** An impl that reads the items of a sequence with
///   [`Decoder::item`] writes them with [`Encoder::item`], as `Vec`, arrays, `BTreeMap` and
///   `BTreeSet` do. A decoder refuses an object with more than 2^24 items that take no bytes and
///   no links, so that a forged length cannot keep it running, and the encoder refuses to write
///   one, with [`Error::TooManyZeroWidth`].
/// - **Nesting is counted alike.** A value nests at most [`MAX_DEPTH`] levels within one object,
///   counted at each `Box`, `Arc`, `Vec`, `BTreeMap` and `BTreeSet`. An impl for a pointer or a
///   collection of its own, or for a type that reads a value of its own type without going through
///   one of those, such as one that boxes what it decodes itself, writes and reads it through
///   [`Encoder::nested`] and [`Decoder::nested`], which count the level and make room on the stack
///   for it, so that neither a deep value nor a forged object can run the thread out of stack.
/// - **The encoding is canonical.** It depends only on the value, never on how the value was
///   built, its capacity or what it shared. Root hashes, `==` on collections, the skip of a
///   commit that changes nothing, [`Map::diff`](crate::Map::diff) and merges all go by
///   encodings, so a value that encodes two ways looks changed when it is not. Decoding reads a
///   value only from the one form its encoder writes: the impls ferro provides refuse a varint
///   longer than its value needs and a `BTreeMap` or `BTreeSet` that holds two equal keys,
///   [`Decoder::fields`] refuses fields out of order of tag, the derive refuses a field its type
///   does not read to the end, and the checks of [`Db::receive`](crate::Db::receive) and
///   [`Db::verify_typed`](crate::Db::verify_typed) refuse an object with bytes or links left
///   after its value. A hand-written impl that reads fields with [`Decoder::fields`] can call
///   [`Decoder::end`] on each field it knows, as the derive does. The one other form read is the
///   keys of a `BTreeMap` or `BTreeSet` out of order, which a file written before their type's
///   `Ord` changed holds: they are sorted as they are read, and the object that holds them keeps
///   its hash until the value is written again.
/// - **A key's encoding agrees with its `Ord`.** A key type's values must compare equal exactly
///   when they encode the same. A key's place in a map's tree comes from a hash of its encoding
///   and its order from `Ord`, so two equal keys that encode differently can land in different
///   nodes and be missed by lookups. Neither the encoding nor the `Ord` of the key type of a map,
///   a set or a table index may change on existing data. A `BTreeMap` or `BTreeSet` sorts its keys
///   as it reads them, so its key type's `Ord` may change, but a commit that writes two of its
///   keys that compare unequal and encode the same, as a type whose `Ord` reads a
///   `#[ferro(skip)]` field can hold, fails with [`Error::Decode`].
/// - **The encoding evolves by name.** Old data must keep decoding as the type changes. The
///   derive gets this by storing each field under a tag hashed from its name, which
///   [`Encoder::field`] and [`Decoder::fields`] offer a hand-written impl too; one that writes
///   bare values must not change what it writes once data is stored.
/// - [`same_encoding`](Data::same_encoding), when overridden, gives the answer comparing the two
///   encodings would, and [`describe`](Data::describe), when overridden, changes whenever the
///   encoding does, and whenever the value [`missing`](Data::missing) returns does.
///
/// # Nesting depth
///
/// A value nests at most [`MAX_DEPTH`] levels within one object, one for each `Box`, `Arc`, `Vec`,
/// `BTreeMap` and `BTreeSet` on the way down to its deepest part: a chain of 4,096
/// `Option<Box<Node>>`, or a tree of 4,095 levels of `Vec<Node>` children. A decoder refuses a
/// value that nests deeper with [`Error::Decode`], whether it comes from the file or from a sync
/// bundle, which [`Db::receive`](crate::Db::receive) then refuses whole, and writing such a value
/// fails with [`Error::TooDeep`], so that no commit stores what cannot be read back. A commit
/// counts on through the collections it writes as it encodes: the values in the nodes of a map,
/// set, list or table whose nodes the commit writes nest two levels below the value that holds
/// it. A collection read from the file encodes as its root link alone, and its nodes count apart,
/// as they decode apart.
///
/// Encoding, decoding, visiting and pairing recurse once for each level. A level that finds less
/// than 128 KiB of the thread's stack left runs on a segment of stack that ferro maps for the
/// thread, on Linux, Android, macOS and iOS, so a value at the limit reads and writes on any
/// thread, in any build, and takes up to a few KB of stack a level, about 12 MB for a decode at
/// the limit in a release build. On Windows ferro does not switch stacks, and such a level fails
/// with [`Error::Decode`] or [`Error::TooDeep`] rather than overflow the stack.
///
/// Dropping, cloning and comparing a value recurse through it too, in code that Rust generates,
/// which checks for nothing. Commits and merges, which clone the root, run with at least 12 MiB of
/// stack, enough to clone a value at the limit, but ferro does not grow the stack for the drops of
/// the values it read and the roots it replaced, nor for clones of your own. A drop takes at most
/// a few hundred bytes a level in a release build, so a value at the limit drops on a spawned
/// thread's 2 MiB stack, but a debug build can take more, and so can a derived `Clone` of a large
/// struct. A type meant to hold long chains can drop them in a loop, and a deep value can be
/// cloned on a thread with a larger stack.
/// [Nesting depth](https://github.com/archlemon/ferro/blob/main/docs/internals/storage.md#nesting-depth)
/// gives the stack each shape takes.
///
/// # Examples
///
/// An IPv4 address stored as its four bytes, with no field tags, which suits a key that never
/// changes shape:
///
/// ```
/// use ferro::codec::{Decoder, Description, Encoder};
/// use ferro::{Data, Map, Result};
///
/// #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// struct Ip([u8; 4]);
///
/// impl Data for Ip {
///     fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
///         w.raw(&self.0);
///         Ok(())
///     }
///
///     fn decode(r: &mut Decoder<'_>) -> Result<Self> {
///         let bytes = r.take(4)?;
///         Ok(Ip([bytes[0], bytes[1], bytes[2], bytes[3]]))
///     }
///
///     fn describe(d: &mut Description) {
///         d.word("ipv4");
///     }
///
///     fn same_encoding(&self, other: &Self) -> bool {
///         self == other
///     }
/// }
///
/// let mut seen: Map<Ip, u32> = Map::new();
/// seen.insert(Ip([10, 0, 0, 1]), 3);
/// seen.insert(Ip([10, 0, 0, 2]), 1);
/// assert_eq!(seen.get(&Ip([10, 0, 0, 1])).as_deref(), Some(&3));
/// ```
pub trait Data: Clone + Send + Sync + 'static {
    #[doc(hidden)]
    const LOCATED: bool = false;
    /// Whether a value of this type can stand for no value, as `None` does, which a `#[unique]`
    /// index leaves out, so that any number of rows can hold it.
    ///
    /// `Option` sets it, `Box`, `Arc` and a `#[ferro(transparent)]` type take it from what they
    /// hold, and every other type leaves it `false`, so an index treats an `Option` the same
    /// however its type is spelled. An index's fingerprint holds it.
    #[doc(hidden)]
    const OPTIONAL: bool = false;
    /// Whether `self` stands for no value, as `None` does, for a type that sets `OPTIONAL`.
    #[doc(hidden)]
    fn absent(&self) -> bool {
        false
    }
    /// Writes the value into `w`: its bytes with [`Encoder::u8`], [`Encoder::varint`] and the
    /// like, and its links with [`Encoder::link`].
    ///
    /// # Errors
    ///
    /// Returns the errors of what it writes: of a nested value that cannot encode, such as a
    /// collection whose nodes cannot be read, of [`Encoder::item`], [`Error::TooManyZeroWidth`]
    /// for more zero-width items in one object than a decoder reads back, [`Error::TooDeep`] for
    /// a value that nests deeper than [`MAX_DEPTH`] levels, and of the sink, such as
    /// [`Error::StalePointer`] for a [`Ptr`] into an earlier generation of the file.
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()>;
    /// Reads a value that [`encode`](Data::encode) wrote, consuming exactly what it wrote.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if the bytes do not form a value of the type or nest deeper than
    /// [`MAX_DEPTH`] levels, and [`Error::MissingField`] for a stored struct that lacks a field
    /// with no default.
    fn decode(r: &mut Decoder<'_>) -> Result<Self>;
    /// Checks a value's encoding and moves past it, without keeping the value.
    ///
    /// [`Db::verify_typed`](crate::Db::verify_typed) and the checks on a received sync bundle
    /// call it. The default decodes the value and drops it; an override can check without
    /// allocating, but must accept and reject exactly what [`decode`](Data::decode) does.
    ///
    /// # Errors
    ///
    /// As for [`decode`](Data::decode).
    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        Self::decode(r).map(drop)
    }
    /// The value a struct field of this type reads as when stored data lacks the field, or `None`,
    /// the default, if such a field must be present.
    ///
    /// The collections, `Option`, `Vec`, `BTreeMap`, `BTreeSet` and [`Bytes`] return their empty
    /// value, so a field of one of them can be added to a type that already has data, and `Box`
    /// and `Arc` follow what they hold.
    ///
    /// An index holds this value for the rows that lack the field, so it must be rebuilt when the
    /// value changes. The default [`describe`](Data::describe) describes it; an impl that
    /// overrides both must describe it too.
    fn missing() -> Option<Self> {
        None
    }
    /// Describes the type's encoding into `d`, for the fingerprints that tell a table when an
    /// index must be rebuilt.
    ///
    /// The derive describes a struct by its stored field names, their types and the values their
    /// defaults return, and an enum by its variants the same way. The default describes a type by
    /// its Rust name and, when [`missing`](Data::missing) returns a value, by the hash of that
    /// value. A hand-written impl whose encoding can change while its name stays the same should
    /// describe what it writes, and the value `missing` returns if there is one.
    ///
    /// # Panics
    ///
    /// The default panics if the value `missing` returns fails to encode, as
    /// [`Description::value`] does.
    fn describe(d: &mut Description) {
        d.word("rust");
        d.word(type_name::<Self>());
        if let Some(value) = Self::missing() {
            d.word("missing");
            d.value(&value);
        }
    }
    /// Whether `self` and `other` encode to the same bytes and links, the test that
    /// [`Map::diff`](crate::Map::diff) and merges compare values by.
    ///
    /// The default encodes both into reused buffers and compares them. The impls ferro provides
    /// answer without encoding: integers, strings and [`Bytes`] by `==`, floats by their bits,
    /// containers part by part, and collections by length and root hash. An override must give
    /// the same answer as comparing encodings, which `==` does not always: `0.0 == -0.0`, and a
    /// hand-written `PartialEq` may skip a field.
    fn same_encoding(&self, other: &Self) -> bool {
        encodings_match(self, other)
    }
    /// Adds to `t` whether a value of this type can hold a [`Table`](crate::Table), by naming
    /// the types it holds with [`Tabled::of`], or with [`Tabled::table`] for a table itself.
    ///
    /// The check that the indexes of a received table agree with its rows reads only the values
    /// that can hold one when it pairs each table with the table at the same place in the
    /// version before. The default holds none, so a table inside a type whose `Data` is written
    /// by hand is checked against all its rows.
    #[doc(hidden)]
    fn tables(_t: &mut Tabled) {}
    /// Reads one value of this type from each of `old` and `new`, the encodings it has at the
    /// same place in two versions, and adds to the [`Pairing`] each table the later one holds with
    /// the table at the same place in the earlier one: the same field of a struct, of the same
    /// variant of an enum, the same element of a sequence, or the one it most likely was before
    /// the sequence gained or lost elements, or the value under the same key of a map.
    ///
    /// The default visits both and pairs nothing.
    ///
    /// # Errors
    ///
    /// The error of a value that does not decode, or of a node of a map that cannot be read.
    #[doc(hidden)]
    fn pair(
        old: &mut Decoder<'_>,
        new: &mut Decoder<'_>,
        _pairing: &mut Pairing<'_>,
    ) -> Result<()> {
        Self::visit(old)?;
        Self::visit(new)
    }
    /// Writes `items` one after another, as `Vec` and arrays store their items.
    ///
    /// The default encodes each item in turn through [`Encoder::item`]. An override, such as
    /// `u8`'s, which copies the bytes at once, must write exactly what the default would, and
    /// count the items that [`decode_vec`](Data::decode_vec) counts.
    ///
    /// # Errors
    ///
    /// As for [`encode`](Data::encode), and [`Error::TooManyZeroWidth`] for more zero-width items
    /// in one object than a decoder reads back.
    fn encode_slice(items: &[Self], w: &mut Encoder<'_>) -> Result<()> {
        for item in items {
            w.item(|w| item.encode(w))?;
        }
        Ok(())
    }
    /// Reads `len` items that [`encode_slice`](Data::encode_slice) wrote.
    ///
    /// The default decodes each item in turn, reserving room for no more items than the remaining
    /// bytes could hold, so a damaged length cannot make it allocate far ahead of what it decodes.
    ///
    /// # Errors
    ///
    /// As for [`decode`](Data::decode).
    fn decode_vec(r: &mut Decoder<'_>, len: usize) -> Result<Vec<Self>> {
        let room = r.remaining() / size_of::<Self>().max(1);
        let mut out = Vec::with_capacity(len.min(room));
        for _ in 0..len {
            out.push(r.item(Self::decode)?);
        }
        Ok(out)
    }
}

/// A description of how a type encodes, written by [`Data::describe`].
///
/// A table stores a [`fingerprint`](Description::fingerprint) of each index's description beside
/// it, and rebuilds the index from its rows when the fingerprint the code gives differs, so a
/// description must change whenever the encoding it describes does, and should not change
/// otherwise. It is a sequence of words, counts, value hashes and the descriptions of other types,
/// each marked with its kind so that no two sequences run together.
///
/// # Examples
///
/// ```
/// use ferro::codec::Description;
///
/// let mut first = Description::new();
/// first.of::<Vec<(u32, String)>>();
/// let mut second = Description::new();
/// second.of::<Vec<(u32, u64)>>();
/// assert_ne!(first.fingerprint(), second.fingerprint());
/// ```
#[derive(Default)]
pub struct Description {
    bytes: Vec<u8>,
    open: Vec<TypeId>,
}

const FINGERPRINT_CONTEXT: &str = "ferro 2026-11-04 index fingerprint";

impl Description {
    /// Creates an empty description.
    pub fn new() -> Self {
        Description::default()
    }

    /// Adds a word, such as the name of a type's shape or of a field.
    pub fn word(&mut self, word: &str) {
        self.bytes.push(0);
        push_varint(&mut self.bytes, word.len() as u64);
        self.bytes.extend_from_slice(word.as_bytes());
    }

    /// Adds a number, such as the length of an array or the number of fields.
    pub fn count(&mut self, count: u64) {
        self.bytes.push(1);
        push_varint(&mut self.bytes, count);
    }

    /// Adds the hash of `value`, such as the default a field reads as when stored data lacks it.
    ///
    /// # Panics
    ///
    /// Panics if `value` fails to encode, as [`hash_of`] does.
    pub fn value<D: Data>(&mut self, value: &D) {
        self.bytes.push(2);
        self.bytes.extend_from_slice(&hash_of(value));
    }

    /// Adds the description of `D`, as its [`Data::describe`] writes it.
    pub fn of<D: Data>(&mut self) {
        D::describe(self);
    }

    /// Adds the description of the named type `D` that `describe` writes, or a reference back to
    /// it if `D` is already being described further out, so that a recursive type's description
    /// ends.
    ///
    /// The derive wraps every struct and enum in it.
    pub fn named<D: Data>(&mut self, describe: impl FnOnce(&mut Description)) {
        let id = TypeId::of::<D>();
        if let Some(at) = self.open.iter().rposition(|open| *open == id) {
            let depth = self.open.len() - at;
            self.word("recurse");
            self.count(depth as u64);
            return;
        }
        self.open.push(id);
        describe(self);
        self.open.pop();
    }

    /// The description as written so far.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// A 64-bit fingerprint of the description: the first eight bytes of a BLAKE3 hash in key
    /// derivation mode, under a context string kept for index fingerprints, read as a
    /// little-endian integer.
    pub fn fingerprint(&self) -> u64 {
        let mut hasher = blake3::Hasher::new_derive_key(FINGERPRINT_CONTEXT);
        hasher.update(&self.bytes);
        let hash = hasher.finalize();
        let mut head = [0u8; 8];
        head.copy_from_slice(&hash.as_bytes()[..8]);
        u64::from_le_bytes(head)
    }
}

/// The [`Hash`](type@Hash) that `value` has as an object of its own, or the error that stopped it
/// encoding.
///
/// # Errors
///
/// Returns the error of [`Data::encode`], such as one from a nested collection whose nodes cannot
/// be read.
pub fn try_hash_of<D: Data>(value: &D) -> Result<Hash> {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder)?;
    Ok(encoder.finish()?.hash)
}

/// The [`Hash`](type@Hash) that `value` has as an object of its own, whether or not it is ever
/// stored as one.
///
/// Two values that encode the same have the same hash, so it suits an HTTP `ETag`, and
/// [`Map::value_hash`](crate::Map::value_hash) gives the same hash for a value in a map. Nothing
/// is written.
///
/// # Panics
///
/// Panics if `value` fails to encode, where [`try_hash_of`] returns the error.
pub fn hash_of<D: Data>(value: &D) -> Hash {
    crate::map::fatal(try_hash_of(value))
}

fn scratch(buf: Vec<u8>) -> Vec<u8> {
    if buf.capacity() > SCRATCH_BYTES {
        Vec::new()
    } else {
        buf
    }
}

/// What `same` answers to whether two values encode alike, asked with room on the stack for it to
/// recurse into what they hold, or `false` where no room can be had, as on Windows.
#[inline(always)]
fn alike(same: impl FnOnce() -> bool) -> bool {
    stack::with_room(same).unwrap_or(false)
}

fn encodings_match<D: Data>(a: &D, b: &D) -> bool {
    let [left, right] = SCRATCH.take();
    let (mut left_sink, mut right_sink) = (HashSink, HashSink);
    let mut left = Encoder::reusing(&mut left_sink, left);
    let mut right = Encoder::reusing(&mut right_sink, right);
    let same = a.encode(&mut left).is_ok()
        && b.encode(&mut right).is_ok()
        && left.bytes() == right.bytes()
        && left.links().len() == right.links().len()
        && left
            .links()
            .iter()
            .zip(right.links())
            .all(|(a, b)| a.hash == b.hash);
    SCRATCH.set([scratch(left.into_buf()), scratch(right.into_buf())]);
    same
}

/// The 16-bit tag that [`#[derive(Data)]`](derive@crate::Data) stores a field or an enum variant
/// under: an FNV-1a hash of its stored name, folded to 16 bits.
pub const fn field_tag(name: &str) -> u16 {
    let bytes = name.as_bytes();
    let mut hash: u32 = 0x811c9dc5;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u32;
        hash = hash.wrapping_mul(0x01000193);
        i += 1;
    }
    ((hash >> 16) ^ (hash & 0xffff)) as u16
}

/// Decodes a value of `T` from `r`, the decoder of one field, into `slot`, as the derive reads
/// each stored field.
///
/// # Errors
///
/// As for [`Data::decode`], and [`Error::Decode`] if the value does not read the field to its
/// end.
#[doc(hidden)]
pub fn decode_into<T: Data>(slot: &mut Option<T>, r: &mut Decoder<'_>) -> Result<()> {
    *slot = Some(T::decode(r)?);
    r.end()
}

/// Visits a value of `T` in `r`, the decoder of one field, as the derive checks each stored
/// field.
///
/// # Errors
///
/// As for [`decode_into`].
#[doc(hidden)]
pub fn visit_field<T: Data>(r: &mut Decoder<'_>) -> Result<()> {
    T::visit(r)?;
    r.end()
}

/// The value of the field `name`: `value` if the stored data held the field, and otherwise the
/// type's [`missing`](Data::missing) value.
///
/// The derive calls it for each field without a default.
///
/// # Errors
///
/// Returns [`Error::MissingField`] with `name` if the field is absent and its type has no value
/// for that.
pub fn required<T: Data>(value: Option<T>, name: &'static str) -> Result<T> {
    value.or_else(T::missing).ok_or(Error::MissingField(name))
}

/// Writes one object: a body of bytes and a list of links.
///
/// [`Data::encode`] writes a value into one. An encoder hands each nested object it finishes,
/// and itself when [`finish`](Encoder::finish) is called, to its [`Sink`]. The impls ferro provides
/// write most integers as LEB128 varints, signed ones zigzagged first, and the derive writes a
/// struct as its fields, each through [`field`](Encoder::field).
pub struct Encoder<'a> {
    buf: Vec<u8>,
    links: Vec<Ptr>,
    sink: &'a mut dyn Sink,
    unsettled: bool,
    empty_items: u64,
    depth: usize,
}

/// Appends `value` to `buf` as an LEB128 varint, as [`Encoder::varint`] writes it.
pub(crate) fn push_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

fn varint_into(out: &mut [u8], mut value: u64) -> usize {
    let mut used = 0;
    while value >= 0x80 {
        out[used] = (value as u8) | 0x80;
        value >>= 7;
        used += 1;
    }
    out[used] = value as u8;
    used + 1
}

impl<'a> Encoder<'a> {
    /// Creates an encoder of an empty object, which hands what it finishes to `sink`.
    pub fn new(sink: &'a mut dyn Sink) -> Self {
        Encoder {
            buf: Vec::new(),
            links: Vec::new(),
            sink,
            unsettled: false,
            empty_items: 0,
            depth: 0,
        }
    }

    /// Creates an encoder with room for `bytes` bytes and `links` links before it reallocates.
    pub fn with_capacity(sink: &'a mut dyn Sink, bytes: usize, links: usize) -> Self {
        Encoder {
            buf: Vec::with_capacity(bytes),
            links: Vec::with_capacity(links),
            sink,
            unsettled: false,
            empty_items: 0,
            depth: 0,
        }
    }

    /// Creates an encoder that writes into `buf`, cleared first, to reuse its allocation;
    /// [`into_buf`](Encoder::into_buf) gives it back.
    pub fn reusing(sink: &'a mut dyn Sink, mut buf: Vec<u8>) -> Self {
        buf.clear();
        Encoder {
            buf,
            links: Vec::new(),
            sink,
            unsettled: false,
            empty_items: 0,
            depth: 0,
        }
    }

    /// The bytes written, dropping the links, for reuse by [`reusing`](Encoder::reusing).
    pub fn into_buf(self) -> Vec<u8> {
        self.buf
    }

    /// Records that the value written holds something not yet stored in the sink's file, such as
    /// a collection with uncommitted changes or one from another file.
    ///
    /// A map node holding such a value is then decoded from what was written before it is cached,
    /// rather than kept as it was, so that the collections inside it come back clean and read
    /// from this file. [`child`](Encoder::child) passes the mark up.
    pub fn mark_unsettled(&mut self) {
        self.unsettled = true;
    }

    /// Whether [`mark_unsettled`](Encoder::mark_unsettled) was called on this encoder or a child
    /// of it.
    pub fn is_unsettled(&self) -> bool {
        self.unsettled
    }

    /// Writes one byte.
    pub fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    /// Writes two bytes, little-endian.
    pub fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes `value` as an LEB128 varint: seven bits a byte, the lowest first, with the top bit
    /// of each byte but the last set. Values below 128 take one byte.
    pub fn varint(&mut self, value: u64) {
        push_varint(&mut self.buf, value);
    }

    /// Writes `bytes` as they are, with no length.
    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Adds a link to the object `ptr` names. Links take no room in the body, and a decoder reads
    /// them back in the order they were added.
    pub fn link(&mut self, ptr: Ptr) {
        self.links.push(ptr);
    }

    /// The sink this encoder hands its objects to.
    pub fn sink(&mut self) -> &mut dyn Sink {
        &mut *self.sink
    }

    /// The id of the sink's store, as [`Sink::store_id`] gives it.
    pub fn store_id(&self) -> u64 {
        self.sink.store_id()
    }

    /// The bytes written so far.
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// The links written so far.
    pub fn links(&self) -> &[Ptr] {
        &self.links
    }

    /// The current position, as counts of bytes and links written, for
    /// [`since`](Encoder::since) and [`rewind`](Encoder::rewind).
    pub fn mark(&self) -> (usize, usize) {
        (self.buf.len(), self.links.len())
    }

    /// How many bytes were written since `mark`.
    pub fn since(&self, mark: (usize, usize)) -> usize {
        self.buf.len() - mark.0
    }

    /// Drops everything written since `mark`, bytes and links.
    ///
    /// The zero-width items that [`item`](Encoder::item) counted since `mark` stay counted.
    pub fn rewind(&mut self, mark: (usize, usize)) {
        self.buf.truncate(mark.0);
        self.links.truncate(mark.1);
    }

    /// Writes one item of a sequence with `write`, counting the items that write no bytes and no
    /// links, as [`Decoder::item`] counts them when it reads them back.
    ///
    /// Use it for each item of a sequence that a decoder reads with [`Decoder::item`], so that a
    /// value the decoder would refuse, such as a `Vec<()>` of more than 2^24 items, fails to
    /// encode rather than encodes to an object that does not decode.
    ///
    /// # Errors
    ///
    /// Returns the error of `write`, and [`Error::TooManyZeroWidth`] once more than 2^24
    /// zero-width items have been written into one object.
    pub fn item(&mut self, write: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        let before = self.mark();
        write(self)?;
        if self.mark() == before {
            self.add_zero_width(1)?;
        }
        Ok(())
    }

    /// Writes with `write` one level deeper, as [`Decoder::nested`] reads it back.
    ///
    /// The impls of `Box`, `Arc`, `Vec`, `BTreeMap` and `BTreeSet` each write what they hold
    /// through it, so a hand-written impl needs it only where it holds a value of its own type
    /// without going through one of them, as [`MAX_DEPTH`] describes. `write` runs on a segment
    /// of stack mapped for the thread if the thread's stack has less than 128 KiB left.
    ///
    /// # Errors
    ///
    /// Returns the error of `write`, and [`Error::TooDeep`] if the value would nest deeper than
    /// [`MAX_DEPTH`] levels, or the stack is short and cannot grow, as on Windows.
    #[inline(always)]
    pub fn nested<T>(&mut self, write: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.deeper()?;
        let written = match stack::room() {
            true => write(self),
            false => stack::elsewhere(|| write(self)).unwrap_or(Err(Error::TooDeep)),
        };
        self.depth -= 1;
        written
    }

    /// Opens one more level of nesting, or returns [`Error::TooDeep`] if [`MAX_DEPTH`] are open.
    #[inline]
    fn deeper(&mut self) -> Result<()> {
        if self.depth >= MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        self.depth += 1;
        Ok(())
    }

    /// How many levels of nesting are open where the encoder is writing, counting those of the
    /// values that hold this object in the same encoding.
    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    /// This encoder, writing `depth` levels deep, for the object of a collection that a value
    /// holds at that depth.
    pub(crate) fn at_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    /// Writes with `write` into this object, but counts the zero-width items it writes apart from
    /// the object's and returns them, for a value that may yet move into an object of its own.
    /// [`add_zero_width`](Encoder::add_zero_width) adds them to the object's count if it stays.
    ///
    /// # Errors
    ///
    /// Returns the error of `write`, such as [`Error::TooManyZeroWidth`] for more than 2^24
    /// zero-width items in what it writes.
    pub(crate) fn zero_width_apart(
        &mut self,
        write: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<u64> {
        let outer = std::mem::take(&mut self.empty_items);
        let written = write(self);
        let inner = std::mem::replace(&mut self.empty_items, outer);
        written.map(|()| inner)
    }

    /// Adds `count` zero-width items to this object's count, as if they were written here.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TooManyZeroWidth`] if the object then holds more than 2^24.
    pub(crate) fn add_zero_width(&mut self, count: u64) -> Result<()> {
        self.empty_items = self.empty_items.saturating_add(count);
        if self.empty_items > MAX_EMPTY_ITEMS {
            return Err(Error::TooManyZeroWidth);
        }
        Ok(())
    }

    /// The zero-width items written into this object so far.
    pub(crate) fn zero_width(&self) -> u64 {
        self.empty_items
    }

    /// Encodes a separate object with `f`, hands it to the sink and returns the link to it,
    /// which the caller records with [`link`](Encoder::link).
    ///
    /// The child starts as deep as this encoder is, so that the levels of a value and of the
    /// objects it writes as it encodes count together against [`MAX_DEPTH`].
    ///
    /// # Errors
    ///
    /// Returns the errors of `f` and of the sink.
    pub fn child(&mut self, f: impl FnOnce(&mut Encoder<'_>) -> Result<()>) -> Result<Ptr> {
        let mut encoder = Encoder::new(&mut *self.sink).at_depth(self.depth);
        f(&mut encoder)?;
        self.unsettled |= encoder.unsettled;
        encoder.finish()
    }

    /// Writes one field of a struct under `tag`, as the derive stores fields.
    ///
    /// The field is written as `tag`, then a varint of the length of what `f` writes, with a flag
    /// and a count when `f` writes links, then what `f` writes, so that a reader that does not
    /// know the tag can skip it. A struct writes the number of fields with
    /// [`varint`](Encoder::varint) and then each field in strictly increasing order of tag, which
    /// keeps its encoding canonical, and [`Decoder::fields`] reads them back, refusing them in any
    /// other order. The derive tags each field with the [`field_tag`] of its stored name.
    ///
    /// # Errors
    ///
    /// Returns the error of `f`.
    pub fn field(
        &mut self,
        tag: u16,
        f: impl FnOnce(&mut Encoder<'a>) -> Result<()>,
    ) -> Result<()> {
        self.u16(tag);
        let at = self.buf.len();
        self.buf.push(0);
        let links_before = self.links.len();
        f(self)?;
        let len = (self.buf.len() - at - 1) as u64;
        let link_count = (self.links.len() - links_before) as u64;
        let mut prefix = [0u8; 20];
        let mut used = varint_into(&mut prefix, (len << 1) | u64::from(link_count > 0));
        if link_count > 0 {
            used += varint_into(&mut prefix[used..], link_count);
        }
        if used == 1 {
            self.buf[at] = prefix[0];
        } else {
            self.buf.splice(at..at + 1, prefix[..used].iter().copied());
        }
        Ok(())
    }

    /// The object written, without hashing it or handing it to the sink.
    pub fn into_object(self) -> Object {
        Object {
            body: self.buf,
            links: self.links,
        }
    }

    /// Hashes the object written and hands it to the sink, returning the link to it.
    ///
    /// # Errors
    ///
    /// Returns the error of [`Sink::put`].
    pub fn finish(self) -> Result<Ptr> {
        let Encoder {
            buf, links, sink, ..
        } = self;
        let object = Object { body: buf, links };
        let hash = object.hash();
        sink.put(object, hash)
    }
}

const MAX_EMPTY_ITEMS: u64 = 1 << 24;

/// The error of a decoder asked to read deeper than [`MAX_DEPTH`] levels.
#[cold]
fn too_deep() -> Error {
    Error::Decode(format!("a value nests deeper than {MAX_DEPTH} levels"))
}

/// The error of a decoder that reads `depth` levels deep on a stack that has no room left for
/// them and cannot grow, as on Windows.
#[cold]
fn no_room(depth: usize) -> Error {
    Error::Decode(format!(
        "the stack has no room to read a value nested {depth} levels deep"
    ))
}

/// The error of a varint that takes more bytes than its value needs.
#[cold]
fn overlong() -> Error {
    Error::Decode("overlong varint".into())
}

/// The error of a `BTreeMap` or `BTreeSet` that stores two keys its key type's `Ord` holds equal.
#[cold]
fn repeated() -> Error {
    Error::Decode("a key is stored more than once".into())
}

/// Room for the entries of a `BTreeMap` or `BTreeSet` of `len` entries in what is left of `r`,
/// reserving no more than the remaining bytes could hold, as [`Data::decode_vec`] does, so that a
/// damaged length cannot make it allocate far ahead of what it decodes.
fn entries_for<T>(r: &Decoder<'_>, len: u64) -> Vec<T> {
    let room = r.remaining() / size_of::<T>().max(1);
    Vec::with_capacity(usize::try_from(len).unwrap_or(usize::MAX).min(room))
}

/// Whether `key` sorts after the key `key_of` gives for the last of `entries`, as each key does
/// when a `BTreeMap` or `BTreeSet` is stored in the order it encodes its keys.
#[inline]
fn after<T, K: Ord>(entries: &[T], key: &K, key_of: impl Fn(&T) -> &K) -> bool {
    entries.last().is_none_or(|last| key_of(last) < key)
}

/// Sorts `entries`, read in the order they were stored, by the key `key_of` gives, unless they
/// were stored `ascending`, and refuses two whose keys are equal.
///
/// A `BTreeMap` or `BTreeSet` reads its keys in any order, since the `Ord` of a key type can change
/// as the type evolves, but never two equal keys, which no encoder writes and of which a map would
/// keep one.
///
/// # Errors
///
/// [`Error::Decode`] if two keys are equal.
fn sort_distinct<T, K: Ord>(
    entries: &mut [T],
    ascending: bool,
    key_of: impl Fn(&T) -> &K,
) -> Result<()> {
    if ascending {
        return Ok(());
    }
    entries.sort_unstable_by(|a, b| key_of(a).cmp(key_of(b)));
    match entries
        .windows(2)
        .any(|pair| key_of(&pair[0]).cmp(key_of(&pair[1])).is_eq())
    {
        true => Err(repeated()),
        false => Ok(()),
    }
}

/// Reads the `len` entries of a `BTreeMap` or `BTreeSet` with `entry`, in the order they were
/// stored, and sorts them by the key `key_of` gives, refusing two equal keys.
///
/// # Errors
///
/// The error of an entry that does not decode, and [`Error::Decode`] for two equal keys.
fn sorted_entries<E, K: Ord>(
    r: &mut Decoder<'_>,
    len: u64,
    mut entry: impl FnMut(&mut Decoder<'_>) -> Result<E>,
    key_of: impl Fn(&E) -> &K,
) -> Result<Vec<E>> {
    let mut entries: Vec<E> = entries_for(r, len);
    let mut ascending = true;
    for _ in 0..len {
        let read = r.item(&mut entry)?;
        ascending = ascending && after(&entries, key_of(&read), &key_of);
        entries.push(read);
    }
    sort_distinct(&mut entries, ascending, key_of)?;
    Ok(entries)
}

/// Visits the entries of a `BTreeMap` or `BTreeSet` after its opening level, reading each key with
/// `key` and the value after it with `value`, and refuses two equal keys in whatever order they are
/// stored.
///
/// Each value is visited once. Keys stored in order, as every encoder writes them, cost a
/// comparison each and are not kept: only where each entry begins is. At the first key out of
/// order it reads the keys before it again from there, apart from this decoder, so that nothing
/// they hold is noted or counted twice, keeps them and the keys that follow, and sorts them.
///
/// # Errors
///
/// The error of an entry that does not decode, and [`Error::Decode`] for two equal keys.
#[inline(never)]
fn visit_entries<K: Ord>(
    r: &mut Decoder<'_>,
    mut key: impl FnMut(&mut Decoder<'_>) -> Result<K>,
    mut value: impl FnMut(&mut Decoder<'_>) -> Result<()>,
) -> Result<()> {
    let len = r.varint()?;
    let mut starts = Starts::for_entries(len);
    let mut last: Option<K> = None;
    let mut outcome = Ok(());
    for index in 0..len {
        let before = r.read_so_far();
        let read = match key(r).and_then(|read| value(r).map(|()| read)) {
            Ok(read) => read,
            Err(error) => {
                outcome = Err(error);
                break;
            }
        };
        if let Err(error) = r.counted(before) {
            outcome = Err(error);
            break;
        }
        if last.as_ref().is_some_and(|last| *last >= read) {
            outcome = visit_unordered(r, len - index - 1, &starts, read, &mut key, &mut value);
            break;
        }
        starts.push(before);
        last = Some(read);
    }
    starts.release();
    outcome
}

/// Visits the `rest` entries of a `BTreeMap` or `BTreeSet` that follow `read`, the first key out
/// of order, as [`visit_entries`] does, keeping every key: those before it, read again from
/// `starts`, `read` and those after it. Then sorts them and refuses two equal keys.
///
/// # Errors
///
/// As for [`visit_entries`].
#[cold]
#[inline(never)]
fn visit_unordered<K: Ord>(
    r: &mut Decoder<'_>,
    rest: u64,
    starts: &Starts,
    read: K,
    mut key: impl FnMut(&mut Decoder<'_>) -> Result<K>,
    mut value: impl FnMut(&mut Decoder<'_>) -> Result<()>,
) -> Result<()> {
    let mut keys: Vec<K> = entries_for(r, rest);
    keys.reserve(starts.len() + 1);
    for &at in starts.iter() {
        keys.push(r.reread(at, &mut key)?);
    }
    keys.push(read);
    for _ in 0..rest {
        let before = r.read_so_far();
        let read = key(r)?;
        value(r)?;
        r.counted(before)?;
        keys.push(read);
    }
    sort_distinct(&mut keys, false, |key| key)
}

/// The number of entry starts a [`Starts`] holds in place before it takes a vector.
const STARTS_NEAR: usize = 8;

/// The most entry starts whose room a thread keeps for the next large collection it visits.
const STARTS_KEPT: usize = 1 << 16;

thread_local! {
    static STARTS: Cell<Vec<(usize, usize)>> = const { Cell::new(Vec::new()) };
}

/// Where each entry of a `BTreeMap` or `BTreeSet` that a visit read begins, as
/// [`Decoder::read_so_far`] gives it: the first [`STARTS_NEAR`] in place, so that a small
/// collection allocates nothing, and the rest in a vector that the thread keeps between visits.
struct Starts {
    near: [(usize, usize); STARTS_NEAR],
    held: usize,
    far: Vec<(usize, usize)>,
}

impl Starts {
    /// Room for the starts of a collection of `len` entries, which takes the vector the thread
    /// kept if they do not all fit in place.
    #[inline]
    fn for_entries(len: u64) -> Starts {
        let far = match len > STARTS_NEAR as u64 {
            true => STARTS.take(),
            false => Vec::new(),
        };
        Starts {
            near: [(0, 0); STARTS_NEAR],
            held: 0,
            far,
        }
    }

    /// Notes where the next entry begins.
    #[inline]
    fn push(&mut self, at: (usize, usize)) {
        match self.near.get_mut(self.held) {
            Some(slot) => {
                *slot = at;
                self.held += 1;
            }
            None => self.far.push(at),
        }
    }

    /// Gives the vector back for the thread to keep, unless it has grown past [`STARTS_KEPT`].
    #[inline]
    fn release(mut self) {
        if self.far.capacity() > 0 && self.far.capacity() <= STARTS_KEPT {
            self.far.clear();
            STARTS.set(std::mem::take(&mut self.far));
        }
    }

    fn len(&self) -> usize {
        self.held + self.far.len()
    }

    fn iter(&self) -> impl Iterator<Item = &(usize, usize)> {
        self.near[..self.held].iter().chain(&self.far)
    }
}

/// The error of a value that did not read all of what holds it: `bytes` bytes and `links` links
/// left over.
#[cold]
fn unread(bytes: usize, links: usize) -> Error {
    Error::Decode(format!(
        "{bytes} bytes and {links} links are left after the value"
    ))
}

/// The most entries each generation of a [`Recalled`] keeps.
const RECALLED: usize = 1 << 14;
/// The bytes an [`Arc`] allocation spends on its counts, for estimating what a value holds on the
/// heap.
pub(crate) const ARC_HEADER: usize = 2 * size_of::<usize>();
const TREE_HEADER: usize = 16;
const TREE_FILL: usize = 6;
const TREE_CAPACITY: usize = 11;

fn allocation(bytes: usize) -> usize {
    bytes.saturating_add(8).next_multiple_of(16).max(32)
}

/// What a typed check learned about an object: for a map node, how many entries lie in and below
/// it and its level, and zero for any other value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) count: u64,
    pub(crate) level: u8,
    /// Whether the summary the kind claims is still to be confirmed, by checking the object again
    /// with a store once every object the check reaches has passed, because a value it summarizes
    /// holds a collection or decodes only with a store, or a summary it folds holds a collection.
    pub(crate) deferred: bool,
}

/// What a link must lead to, for the typed checks of
/// [`Db::verify_typed`](crate::Db::verify_typed) and of a received sync bundle.
///
/// It names the type stored there and, for a map node, the entry count its parent's link records,
/// the level the node must be below, the range of keys it must keep within and the summary the
/// link records, which the check recomputes from the node's entries.
#[derive(Clone)]
pub(crate) struct Kind {
    /// The type stored at the link.
    pub(crate) id: TypeId,
    /// The type's name, for error messages.
    pub(crate) name: &'static str,
    /// Checks an object's body as this kind, recording each typed link it finds in the decoder.
    pub(crate) check: fn(&Kind, &mut Decoder<'_>) -> Result<Shape>,
    /// Checks an object that the receiving store already holds, given by its link, as this kind.
    pub(crate) held: fn(&Kind, &Store, Ptr) -> Result<Shape>,
    /// Whether an object checked as the first kind is known to be good as the second, apart from
    /// the [`claim`](Kind::claim), which [`vouched_by`](Kind::vouched_by) compares.
    pub(crate) within: fn(&Kind, &Kind) -> bool,
    /// The entry count the object must hold, if the parent records one.
    pub(crate) count: Option<u64>,
    /// The level the object's node must be below, if any.
    pub(crate) below: Option<u8>,
    /// The keys the object's node must keep within, as the parent gives them, if any.
    pub(crate) range: Option<Arc<dyn Any + Send + Sync>>,
    /// The summary the parent records for what the link leads to, which the check recomputes
    /// and must find, if the parent records one.
    pub(crate) claim: Option<Arc<dyn Claim>>,
}

/// A summary that a parent records for what it links to, held in a [`Kind`]'s
/// [`claim`](Kind::claim): a map node's summary, which a check of the node recomputes from its
/// entries, or the key and summary of the entry whose spilled value the object is.
pub(crate) trait Claim: Any + Send + Sync {
    /// Whether this claim says what `confirmed`, a claim a check found true, does: the same
    /// type, and the same encoding.
    fn agrees(&self, confirmed: &dyn Claim) -> bool;

    /// The hash of what this claim encodes, which with its type tells it from any claim that
    /// does not agree with it, or `None` if it does not encode.
    fn fingerprint(&self) -> Option<Hash>;
}

/// The typed links a check found in an object: each one's index in the object's links, with the
/// [`Kind`] it must lead to.
pub(crate) type Found = Vec<(usize, Kind)>;

/// What checks that the indexes of a table agree with its rows: given the table, a table of the
/// same type to work from, whose indexes are known to agree with its rows, the store both read
/// from, the pool whose threads share the work, and whether an index under a fingerprint the
/// type does not know fails it, it returns [`Error::Decode`] for an index that does not hold
/// exactly the entries the rows give.
pub(crate) type Agree =
    for<'p> fn(&FoundTable, Option<&FoundTable>, &'p Store, &Pool<'p>, bool) -> Result<()>;

/// A table value that a typed check found, kept for the check that its indexes agree with its
/// rows, which reads the rows and so counts only once every object the check reaches has passed.
#[derive(Clone)]
pub(crate) struct FoundTable {
    /// The table's type.
    pub(crate) id: TypeId,
    /// The table type's name, for error messages.
    pub(crate) name: &'static str,
    /// Checks the table's indexes against its rows.
    pub(crate) agree: Agree,
    /// Whether the check against the table of the version before may run before the rows it
    /// reads have passed their own checks, which it may unless working out the entries of a row
    /// reads the file.
    pub(crate) beside: bool,
    /// The table value's bytes, as its parent encodes it.
    pub(crate) body: Arc<[u8]>,
    /// The table value's links.
    pub(crate) links: Arc<[Ptr]>,
    /// The hash of the value's bytes and the hashes of its links, which with
    /// [`id`](FoundTable::id) tells this table value from any other.
    pub(crate) hash: Hash,
}

impl FoundTable {
    /// The type and hash that tell this table value from any other.
    pub(crate) fn identity(&self) -> (TypeId, Hash) {
        (self.id, self.hash)
    }
}

/// The table values that typed checks found, each kept once, in the order first found.
#[derive(Default)]
pub(crate) struct Tables {
    seen: HashSet<(TypeId, Hash)>,
    found: Vec<FoundTable>,
}

impl Tables {
    /// Keeps each table in `found` that was not found before.
    pub(crate) fn note(&mut self, found: Vec<FoundTable>) {
        for table in found {
            if self.seen.insert(table.identity()) {
                self.found.push(table);
            }
        }
    }

    /// Whether a table value with this identity was found.
    pub(crate) fn holds(&self, table: &FoundTable) -> bool {
        self.seen.contains(&table.identity())
    }

    /// Whether no table was found.
    pub(crate) fn is_empty(&self) -> bool {
        self.found.is_empty()
    }

    /// The tables found, in the order first found.
    pub(crate) fn into_found(self) -> Vec<FoundTable> {
        self.found
    }
}

/// How the check of one table value goes.
enum Plan {
    /// The value is known to agree, as part of a version the receiver holds.
    Trusted,
    /// It is checked against the rows that changed from this table, whose check does not depend
    /// on it.
    From(FoundTable),
    /// Its indexes are built from its rows.
    Built,
}

/// How the check of each table value that a typed check found goes: against the table at the
/// same place in its version's parent, as a [`Pairing`] finds it, which costs what changed
/// between the two, or by building its indexes from its rows, as a table with no plan is checked.
///
/// A table is checked against another only if that one's own check is already planned, so no
/// chain of checks leads back to where it started and every table that is checked rests on one
/// whose indexes were built or that was trusted.
#[derive(Default)]
pub(crate) struct Plans(HashMap<(TypeId, Hash), Plan>);

impl Plans {
    /// Takes the table value with this type and hash, as [`FoundTable::identity`] gives them, as
    /// agreeing with its rows, unless its check is already planned.
    pub(crate) fn trust(&mut self, identity: (TypeId, Hash)) {
        self.0.entry(identity).or_insert(Plan::Trusted);
    }

    /// Plans to build the indexes of `table` from its rows, unless its check is already planned.
    pub(crate) fn build(&mut self, table: &FoundTable) {
        self.0.entry(table.identity()).or_insert(Plan::Built);
    }

    /// Plans to check `table` against `base`, the table at the same place in the version before,
    /// unless the check of `table` is already planned, that of `base` is not, or the two are one
    /// value. Returns whether it planned it.
    pub(crate) fn from(&mut self, table: &FoundTable, base: &FoundTable) -> bool {
        if base.identity() == table.identity()
            || self.0.contains_key(&table.identity())
            || !self.0.contains_key(&base.identity())
        {
            return false;
        }
        self.0.insert(table.identity(), Plan::From(base.clone()));
        true
    }

    /// How to check `table`: `None` if it is trusted, and otherwise the table to check it
    /// against, if any.
    pub(crate) fn base(&self, table: &FoundTable) -> Option<Option<&FoundTable>> {
        match self.0.get(&table.identity()) {
            Some(Plan::Trusted) => None,
            Some(Plan::From(base)) => Some(Some(base)),
            Some(Plan::Built) | None => Some(None),
        }
    }

    /// Checks, as planned, each table among `found` that is not trusted, reading through `store`,
    /// and returns the first, in the order of `found`, whose check failed, with its error.
    ///
    /// The checks run on the threads of a [`pool::scope`], as [`Checks::finish`] runs them.
    pub(crate) fn check<'a>(
        &self,
        found: &'a [FoundTable],
        store: &Store,
    ) -> Option<(&'a FoundTable, Error)> {
        pool::scope(|pool| Checks::default().finish(pool, store, self, found))
    }
}

/// The checks that tables agree with their rows that are queued on a [`Pool`], each once, by the
/// table it checks and the table it checks it against, so that a receive can queue the check of a
/// version's tables as soon as it knows how to check them and run it beside its walk.
#[derive(Default)]
pub(crate) struct Checks {
    queued: HashMap<CheckOf, Arc<Verdict>>,
    strict: bool,
}

/// The table a check of [`Checks`] checks and the table it checks it against, by type and hash.
type CheckOf = ((TypeId, Hash), Option<(TypeId, Hash)>);

/// What one check of [`Checks`] found, once it has run.
#[derive(Default)]
struct Verdict {
    done: AtomicBool,
    failed: Mutex<Option<Error>>,
}

impl Checks {
    /// Checks for a receive, which also fail a table that holds an index under a fingerprint its
    /// type does not know: nothing checked such an index as it lands, so the file must not hold
    /// it for a later version of the type that knows the fingerprint to trust.
    pub(crate) fn strict() -> Checks {
        Checks {
            queued: HashMap::new(),
            strict: true,
        }
    }

    /// Queues on `pool` the check of `table` against `base`, as [`Plans::base`] gives it, reading
    /// through `store`, unless it is queued already.
    pub(crate) fn start<'p>(
        &mut self,
        pool: &Pool<'p>,
        store: &'p Store,
        table: &FoundTable,
        base: Option<&FoundTable>,
    ) {
        self.queued(pool, store, table, base);
    }

    fn queued<'p>(
        &mut self,
        pool: &Pool<'p>,
        store: &'p Store,
        table: &FoundTable,
        base: Option<&FoundTable>,
    ) -> Arc<Verdict> {
        let (verdict, fresh) = self.verdict(table, base);
        if let Some(check) = fresh {
            self.queue_group(pool, store, vec![check]);
        }
        verdict
    }

    /// The verdict of the check of `table` against `base`, and the check to queue for it unless
    /// one is queued already.
    fn verdict(
        &mut self,
        table: &FoundTable,
        base: Option<&FoundTable>,
    ) -> (Arc<Verdict>, Option<Unqueued>) {
        let key = (table.identity(), base.map(FoundTable::identity));
        if let Some(verdict) = self.queued.get(&key) {
            return (verdict.clone(), None);
        }
        let verdict = Arc::new(Verdict::default());
        self.queued.insert(key, verdict.clone());
        let check = (table.clone(), base.cloned(), verdict.clone());
        (verdict, Some(check))
    }

    /// Queues on `pool` one check that makes each of `group` in turn, so that many small tables
    /// cost a few jobs rather than one each.
    fn queue_group<'p>(&self, pool: &Pool<'p>, store: &'p Store, group: Vec<Unqueued>) {
        let strict = self.strict;
        pool.check(pool::job(move |pool| {
            for (table, base, told) in &group {
                if !pool.stopped() {
                    let agreed = unwound("checking its indexes", || {
                        (table.agree)(table, base.as_ref(), store, pool, strict)
                    });
                    if let Err(error) = agreed.and_then(|agreed| agreed) {
                        *told.failed.lock() = Some(error);
                    }
                }
                told.done.store(true, Ordering::Release);
            }
            pool.wake();
        }));
    }

    /// Checks each table among `found` that `plans` does not trust, against the table it plans,
    /// queuing on `pool` those checks not queued yet, waits for them while it helps run what is
    /// queued, and returns the first table, in the order of `found`, whose check failed, with its
    /// error.
    ///
    /// The checks not queued yet are queued in groups, up to [`GROUPED`] at a time and fewer
    /// where there are not many, each group as one job.
    pub(crate) fn finish<'a, 'p>(
        mut self,
        pool: &Pool<'p>,
        store: &'p Store,
        plans: &Plans,
        found: &'a [FoundTable],
    ) -> Option<(&'a FoundTable, Error)> {
        let mut fresh = Vec::new();
        let waiting: Vec<_> = found
            .iter()
            .filter_map(|table| {
                let base = plans.base(table)?;
                let (verdict, check) = self.verdict(table, base);
                fresh.extend(check);
                Some((table, verdict))
            })
            .collect();
        let size = (fresh.len() / GROUPS).clamp(1, GROUPED);
        let mut fresh = fresh.into_iter().peekable();
        while fresh.peek().is_some() {
            self.queue_group(pool, store, fresh.by_ref().take(size).collect());
        }
        let passed = Cell::new(0);
        pool.help_until(true, || {
            let mut at = passed.get();
            while waiting
                .get(at)
                .is_some_and(|(_, verdict)| verdict.done.load(Ordering::Acquire))
            {
                at += 1;
            }
            passed.set(at);
            at == waiting.len()
        });
        waiting
            .into_iter()
            .find_map(|(table, verdict)| verdict.failed.lock().take().map(|error| (table, error)))
    }
}

/// A check that [`Checks`] has a verdict for but has not queued: the table, the table to check it
/// against, and the verdict.
type Unqueued = (FoundTable, Option<FoundTable>, Arc<Verdict>);

/// About how many jobs [`Checks::finish`] splits the checks it queues into.
const GROUPS: usize = 128;

/// The most checks [`Checks::finish`] queues as one job.
const GROUPED: usize = 64;

/// Whether a type's values can hold a [`Table`](crate::Table), as [`Data::tables`] adds it up.
#[doc(hidden)]
#[derive(Default)]
pub struct Tabled {
    open: Vec<TypeId>,
    found: bool,
}

impl Tabled {
    /// Adds whether `D` can hold a table, as its [`Data::tables`] says.
    pub fn of<D: Data>(&mut self) {
        if !self.found {
            D::tables(self);
        }
    }

    /// Adds what the named type `D` holds, as `holds` adds it, unless `D` is already being looked
    /// into further out, so that a recursive type's search ends.
    ///
    /// The derive wraps every struct and enum in it.
    pub fn named<D: Data>(&mut self, holds: impl FnOnce(&mut Tabled)) {
        let id = TypeId::of::<D>();
        if self.found || self.open.contains(&id) {
            return;
        }
        self.open.push(id);
        holds(self);
        self.open.pop();
    }

    /// Marks the type as one whose values can hold a table.
    pub fn table(&mut self) {
        self.found = true;
    }
}

/// Whether values of `D` can hold a [`Table`](crate::Table), as [`Data::tables`] says, worked out
/// once for each type.
pub(crate) fn holds_tables<D: Data>() -> bool {
    static KNOWN: std::sync::OnceLock<parking_lot::RwLock<HashMap<TypeId, bool>>> =
        std::sync::OnceLock::new();
    let known = KNOWN.get_or_init(Default::default);
    let id = TypeId::of::<D>();
    if let Some(holds) = known.read().get(&id) {
        return *holds;
    }
    let mut tabled = Tabled::default();
    tabled.of::<D>();
    known.write().insert(id, tabled.found);
    tabled.found
}

/// Pairs each table of one version with the table at the same place in the version before, the
/// place being the field tags, variant tags, element positions and map keys from the root down,
/// which [`Data::pair`] reads from the two encodings side by side.
///
/// It reads through a store, the view of a receive or the file `verify_typed` checks, and skips
/// what the two versions share: a value both encode alike holds the same tables, and a map whose
/// root is the same in both holds the same entries. Of a map, it reads only the nodes the two do
/// not share, as a diff does, and pairs the values of each key both hold. The diff of one map
/// opens each node once on each side, and one that reaches a node its side opened already, which
/// no map a write builds does, is left unpaired from there. Maps that share nodes, as clones of
/// one map do, are each paired in full, opening again the nodes another map opened, as long as
/// the nodes opened again number no more than those opened once; a map that would open more is
/// left unpaired from there, so a pairing opens at most twice as many nodes as the two versions
/// hold, however the maps share them. Two maps paired once are not paired again.
///
/// A map is paired after the value that holds it, one map at a time, so that maps nested in the
/// values of maps, however deep, do not deepen the stack: only the levels of one value at a time
/// do, as [`MAX_DEPTH`] bounds them. The maps it queues so count their nodes under the same bound
/// as the others.
///
/// It keeps a table that both versions hold at a place only by its type and hash, and only when
/// asked to, since only a parent the receiver holds vouches for such a table, and it builds a
/// [`FoundTable`] only for the two tables of a pair that differ.
#[doc(hidden)]
pub struct Pairing<'s> {
    store: &'s Store,
    pairs: Vec<(FoundTable, FoundTable)>,
    same: Option<Vec<(TypeId, Hash)>>,
    maps: HashSet<(Hash, Hash)>,
    opened: [HashSet<Hash>; 2],
    again: usize,
    waiting: Vec<Waiting>,
    aligning: bool,
}

/// What pairs the tables of two versions of a map, given their roots with the entry counts their
/// links record.
pub(crate) type PairMaps = for<'s> fn(&mut Pairing<'s>, (Ptr, u64), (Ptr, u64)) -> Result<()>;

/// The pairing of two versions of a map that a [`Pairing`] has queued: what pairs them, and the
/// root of each with the entry count its link records.
type Waiting = (PairMaps, (Ptr, u64), (Ptr, u64));

/// The tables a [`Pairing`] paired: each table of the later version with the one at the same
/// place in the earlier version, when the two differ, and each table that is the same at its
/// place in both, if the pairing kept them.
#[derive(Default)]
pub(crate) struct Paired {
    /// Each table of the earlier version, with the table at its place in the later one.
    pub(crate) pairs: Vec<(FoundTable, FoundTable)>,
    /// The type and hash of each table that both versions hold at the same place, as
    /// [`FoundTable::identity`] gives them, if the pairing kept them.
    pub(crate) same: Vec<(TypeId, Hash)>,
}

impl<'s> Pairing<'s> {
    /// A pairing that reads through `store`, which with `keep_same` keeps the tables that both
    /// versions hold at the same place as well as those it pairs.
    pub(crate) fn new(store: &'s Store, keep_same: bool) -> Pairing<'s> {
        Pairing {
            store,
            pairs: Vec::new(),
            same: keep_same.then(Vec::new),
            maps: HashSet::new(),
            opened: [HashSet::new(), HashSet::new()],
            again: 0,
            waiting: Vec::new(),
            aligning: false,
        }
    }

    /// The store the pairing reads through.
    pub(crate) fn store(&self) -> &'s Store {
        self.store
    }

    /// What the pairing paired.
    pub(crate) fn into_paired(self) -> Paired {
        Paired {
            pairs: self.pairs,
            same: self.same.unwrap_or_default(),
        }
    }

    /// Reads the fields of a struct from `old` and `new`, as [`Decoder::fields`] does, and calls
    /// `f` on each tag both hold, with a decoder over that field on each side, in the order of
    /// their tags.
    ///
    /// It merges the two lists in the order they are stored, which the derive writes sorted by
    /// tag, so it reads each field of either once and holds none it is not pairing. A tag stored
    /// more than once is paired in the order stored, and of a list stored out of order it pairs
    /// only the fields the merge meets.
    ///
    /// The zero-width items `f` reads count as those of the object each side's field lies in, as
    /// [`Decoder::item`] counts them, so the fields of one object share the limit.
    ///
    /// # Errors
    ///
    /// The error of a field list that does not decode, and the first error of `f`, such as
    /// [`Error::Decode`] once more zero-width items than one object may hold have been read.
    pub fn fields<'a, 'b>(
        &mut self,
        old: &mut Decoder<'a>,
        new: &mut Decoder<'b>,
        mut f: impl FnMut(&mut Self, u16, &mut Decoder<'a>, &mut Decoder<'b>) -> Result<()>,
    ) -> Result<()> {
        let (mut olds, news) = (old.varint()?, new.varint()?);
        let mut ahead: Option<(u16, Decoder<'a>)> = None;
        for _ in 0..news {
            let (tag, mut new_field) = new.next_field()?;
            while olds > 0 && ahead.as_ref().is_none_or(|(other, _)| *other < tag) {
                olds -= 1;
                ahead = Some(old.next_field()?);
            }
            if let Some((_, mut old_field)) = ahead.take_if(|(other, _)| *other == tag) {
                old_field.empty_items = old.empty_items;
                new_field.empty_items = new.empty_items;
                f(self, tag, &mut old_field, &mut new_field)?;
                old.empty_items = old_field.empty_items;
                new.empty_items = new_field.empty_items;
            }
        }
        for _ in 0..olds {
            old.next_field()?;
        }
        Ok(())
    }

    /// Pairs the tables of a value of `D` that `old` and `new` each hold to their end, as a field
    /// does, unless `D` cannot hold a table, when it reads neither.
    ///
    /// # Errors
    ///
    /// As for [`Data::pair`].
    pub fn field<D: Data>(&mut self, old: &mut Decoder<'_>, new: &mut Decoder<'_>) -> Result<()> {
        if !holds_tables::<D>() {
            return Ok(());
        }
        D::pair(old, new, self)
    }

    /// Reads the variant tag of an enum `D` from `old` and `new`, and calls `same` with it when
    /// the two match, to pair what follows, or otherwise visits the rest of each as a `D`.
    ///
    /// # Errors
    ///
    /// The error of a tag or value that does not decode, and that of `same`.
    pub fn variant<'a, 'b, D: Data>(
        &mut self,
        old: &mut Decoder<'a>,
        new: &mut Decoder<'b>,
        same: impl FnOnce(&mut Self, u16, &mut Decoder<'a>, &mut Decoder<'b>) -> Result<()>,
    ) -> Result<()> {
        let (from_old, from_new) = (old.checkpoint(), new.checkpoint());
        let (tag, other) = (old.u16()?, new.u16()?);
        if tag == other {
            return same(self, tag, old, new);
        }
        old.rewind(from_old);
        new.rewind(from_new);
        D::visit(old)?;
        D::visit(new)
    }

    /// Records the table of type `id` that `new` read since `from_new`, a table of the later
    /// version, as paired with the one `old` read since `from_old`, the table at its place in the
    /// earlier one, each as `found` makes a [`FoundTable`] of it, or as the same table when the two
    /// encode alike.
    pub(crate) fn table(
        &mut self,
        id: TypeId,
        (old, from_old): (&Decoder<'_>, Checkpoint),
        (new, from_new): (&Decoder<'_>, Checkpoint),
        found: impl Fn(&Decoder<'_>, Checkpoint) -> FoundTable,
    ) {
        let (old_body, old_links) = old.read_since(from_old);
        let (new_body, new_links) = new.read_since(from_new);
        let alike = old_body == new_body
            && old_links
                .iter()
                .map(|link| link.hash)
                .eq(new_links.iter().map(|link| link.hash));
        if !alike {
            self.pairs
                .push((found(old, from_old), found(new, from_new)));
        } else if let Some(same) = &mut self.same {
            let hash = hash_parts(new_body, new_links.iter().map(|link| &link.hash));
            same.push((id, hash));
        }
    }

    /// Whether the maps with roots `old` and `new` are yet to be paired, which it notes they no
    /// longer are.
    pub(crate) fn unpaired_maps(&mut self, old: Hash, new: Hash) -> bool {
        self.maps.insert((old, new))
    }

    /// Whether the pairing of one map may open the node `hash` of the earlier version, or with
    /// `later` of the later one, which it then notes as opened in `map`, the nodes that map's
    /// pairing opened on each side: a node is opened once in each map, and a node another map
    /// opened is opened again only while the nodes opened again number no more than those opened
    /// once.
    pub(crate) fn may_open(
        &mut self,
        map: &mut [HashSet<Hash>; 2],
        later: bool,
        hash: Hash,
    ) -> bool {
        let side = usize::from(later);
        if !map[side].insert(hash) {
            return false;
        }
        if self.opened[side].insert(hash) {
            return true;
        }
        if self.again >= self.opened[0].len() + self.opened[1].len() {
            return false;
        }
        self.again += 1;
        true
    }

    /// Queues the pairing of two versions of a map, with roots `old` and `new`, by `pair`, for
    /// [`settle`](Pairing::settle) to make once the value that holds them is paired.
    pub(crate) fn defer(&mut self, pair: PairMaps, old: (Ptr, u64), new: (Ptr, u64)) {
        self.waiting.push((pair, old, new));
    }

    /// Pairs the maps queued by [`defer`](Pairing::defer), and those their values queue in turn,
    /// until none is left.
    ///
    /// # Errors
    ///
    /// The first error of a map's pairing.
    pub(crate) fn settle(&mut self) -> Result<()> {
        while let Some((pair, old, new)) = self.waiting.pop() {
            pair(self, old, new)?;
        }
        Ok(())
    }
}

/// Pairs the tables of the `olds` items of a sequence of `T` that `old` holds with those of the
/// `news` items that `new` holds, in order, as if the longer had gained or lost its extra items in
/// one run, which it visits.
///
/// Where the run lies is found by [`split`] when the two differ in length, `T` can hold a table,
/// neither holds more than [`ALIGNED`] items and the pairing is not already pairing the items of
/// such a sequence; anywhere else it lies at the end, and the items pair by position.
///
/// # Errors
///
/// The error of an item that does not decode.
fn paired_items<T: Data>(
    old: &mut Decoder<'_>,
    new: &mut Decoder<'_>,
    pairing: &mut Pairing<'_>,
    olds: u64,
    news: u64,
) -> Result<()> {
    let tabled = holds_tables::<T>();
    let both = olds.min(news);
    let aligning = tabled && olds != news && !pairing.aligning && olds.max(news) <= ALIGNED;
    let run = if aligning {
        split::<T>(old, new, olds, news)?
    } else {
        both
    };
    let outer = pairing.aligning;
    pairing.aligning = outer || aligning;
    let paired = (|| {
        for at in 0..=both {
            if at == run {
                for _ in both..olds {
                    old.item(T::visit)?;
                }
                for _ in both..news {
                    new.item(T::visit)?;
                }
            }
            if at == both {
                break;
            }
            if tabled {
                old.item(|old| new.item(|new| T::pair(old, new, pairing)))?;
            } else {
                old.item(T::visit)?;
                new.item(T::visit)?;
            }
        }
        Ok(())
    })();
    pairing.aligning = outer;
    paired
}

/// The most items of a sequence whose items [`paired_items`] aligns rather than pairs by
/// position.
const ALIGNED: u64 = 1 << 10;

/// Where the items that the longer of two sequences of `T` holds beyond the shorter's length most
/// likely came or went, as the position among the shorter's items before which they lie: the one
/// at which the most items pair with an item they share a link with, or encode alike, the last of
/// those that tie.
///
/// It reads the `olds` items of `old` and the `news` items of `new`, counting their zero-width
/// items as [`Decoder::item`] does, and leaves both where they started.
///
/// # Errors
///
/// The error of an item that does not decode.
fn split<T: Data>(
    old: &mut Decoder<'_>,
    new: &mut Decoder<'_>,
    olds: u64,
    news: u64,
) -> Result<u64> {
    let (from_old, from_new) = (old.checkpoint(), new.checkpoint());
    let (olds, news) = (traits::<T>(old, olds)?, traits::<T>(new, news)?);
    old.rewind(from_old);
    new.rewind(from_new);
    let (short, long) = if olds.len() <= news.len() {
        (&olds, &news)
    } else {
        (&news, &olds)
    };
    let extra = long.len() - short.len();
    let akin = |(bytes, links): &(&[u8], Vec<Hash>), (other, others): &(&[u8], Vec<Hash>)| {
        let alike = bytes == other && links == others;
        u64::from(alike || links.iter().any(|link| others.binary_search(link).is_ok()))
    };
    let after: u64 = (0..short.len())
        .map(|at| akin(&short[at], &long[at + extra]))
        .sum();
    let (mut best, mut run) = (after, 0);
    let (mut before, mut passed) = (0, 0);
    for at in 0..short.len() {
        before += akin(&short[at], &long[at]);
        passed += akin(&short[at], &long[at + extra]);
        if before + after - passed >= best {
            (best, run) = (before + after - passed, at + 1);
        }
    }
    Ok(run as u64)
}

/// The bytes of each of the `count` items of `T` that `r` holds, with the sorted hashes of its
/// links, which [`split`] compares.
///
/// # Errors
///
/// The error of an item that does not decode.
fn traits<'a, T: Data>(r: &mut Decoder<'a>, count: u64) -> Result<Vec<(&'a [u8], Vec<Hash>)>> {
    let mut out = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for _ in 0..count {
        let start = r.checkpoint();
        r.item(T::visit)?;
        let (bytes, links) = r.read_since(start);
        let mut hashes: Vec<Hash> = links.iter().map(|link| link.hash).collect();
        hashes.sort_unstable();
        out.push((bytes, hashes));
    }
    Ok(out)
}

/// What pairs the tables of two versions' roots, given their links: [`pair_roots`] for the type of
/// the root.
pub(crate) type PairRoots = for<'s> fn(&mut Pairing<'s>, Ptr, Ptr) -> Result<()>;

/// Pairs each table of the root at `new`, a value of `D`, with the table at the same place in the
/// root at `old`, as [`Data::pair`] pairs them, reading both through the pairing's store.
///
/// The zero-width items it reads from a root that the store, a view, reads at a charge count
/// against what the view may still decode, as [`Store::count_zero_width`] counts them.
///
/// # Errors
///
/// The error of a read, of a value or node that does not decode, and [`Error::Bundle`] once the
/// view cannot cover the zero-width items read.
pub(crate) fn pair_roots<D: Data>(pairing: &mut Pairing<'_>, old: Ptr, new: Ptr) -> Result<()> {
    if old.hash == new.hash || !holds_tables::<D>() {
        return Ok(());
    }
    let store = pairing.store();
    let (old, old_charged) = store.read_fresh(old)?;
    let (new, new_charged) = store.read_fresh(new)?;
    let mut old = Decoder::new(&old.body, &old.links, Some(store));
    let mut new = Decoder::new(&new.body, &new.links, Some(store));
    D::pair(&mut old, &mut new, pairing)?;
    for (charged, read) in [(old_charged, &old), (new_charged, &new)] {
        if charged {
            store.count_zero_width(read.zero_width())?;
        }
    }
    pairing.settle()
}

fn check_value<D: Data>(_: &Kind, r: &mut Decoder<'_>) -> Result<Shape> {
    D::visit(r)?;
    Ok(Shape::default())
}

fn held_value(_: &Kind, _: &Store, _: Ptr) -> Result<Shape> {
    Ok(Shape::default())
}

fn anywhere(_: &Kind, _: &Kind) -> bool {
    true
}

impl Kind {
    /// The kind of a link to a value of type `D`, checked by [`Data::visit`], with no count, level,
    /// range or summary to meet.
    pub(crate) fn value<D: Data>() -> Kind {
        Kind {
            id: TypeId::of::<D>(),
            name: type_name::<D>(),
            check: check_value::<D>,
            held: held_value,
            within: anywhere,
            count: None,
            below: None,
            range: None,
            claim: None,
        }
    }

    /// This kind without its [`claim`](Kind::claim), for a check that only needs to learn the
    /// kinds of an object's links.
    pub(crate) fn unclaimed(&self) -> Kind {
        Kind {
            claim: None,
            ..self.clone()
        }
    }

    /// Whether a checked object's shape has the count and the level this kind asks for.
    pub(crate) fn fits(&self, shape: Shape) -> bool {
        self.count.is_none_or(|count| count == shape.count)
            && self.below.is_none_or(|below| shape.level < below)
    }

    /// Whether an object that passed a check as `seen` is known to have the summary this kind
    /// claims: this kind claims none, or `seen` claimed one that says the same.
    pub(crate) fn vouched_by(&self, seen: &Kind) -> bool {
        match (&self.claim, &seen.claim) {
            (None, _) => true,
            (Some(want), Some(had)) => want.agrees(&**had),
            (Some(_), None) => false,
        }
    }

    /// Whether an object that passed a check as this kind needs no check as `want`: the same
    /// type, the count `want` asks for, a level bound at least as tight, a key range within
    /// `want`'s and the summary `want` claims.
    pub(crate) fn implies(&self, want: &Kind) -> bool {
        self.id == want.id
            && want.count.is_none_or(|count| self.count == Some(count))
            && want
                .below
                .is_none_or(|below| self.below.is_some_and(|own| own <= below))
            && (self.within)(self, want)
            && want.vouched_by(self)
    }

    /// Checks an object with this body and these links as this kind, returning its shape, the
    /// typed links found in it, which the caller checks in turn, and the zero-width items it
    /// read, as [`Decoder::item`] counts them.
    ///
    /// Without a store, a check that would need one to confirm the summary the kind claims
    /// returns a [`Shape`] marked [`deferred`](Shape::deferred). With `store`, the values and
    /// summaries a summary is recomputed from decode with it, so that `of` and `combine` can read
    /// the collections they hold, and the check never defers.
    ///
    /// # Errors
    ///
    /// Returns the error of the first part that does not decode, or does not meet what the kind
    /// asks of it, and [`Error::Decode`] for an object with bytes or links left after it.
    pub(crate) fn check(
        &self,
        body: &[u8],
        links: &[Ptr],
        store: Option<&Store>,
    ) -> Result<(Shape, Found, u64)> {
        self.checked(body, links, store, None)
    }

    /// Checks an object without a store, as [`check`](Kind::check) does, and also adds to
    /// `tables` each table value that the check reads, in the order it reads them.
    ///
    /// # Errors
    ///
    /// As for [`check`](Kind::check).
    pub(crate) fn check_tables(
        &self,
        body: &[u8],
        links: &[Ptr],
        tables: &RefCell<Vec<FoundTable>>,
    ) -> Result<(Shape, Found, u64)> {
        self.checked(body, links, None, Some(tables))
    }

    fn checked(
        &self,
        body: &[u8],
        links: &[Ptr],
        store: Option<&Store>,
        tables: Option<&RefCell<Vec<FoundTable>>>,
    ) -> Result<(Shape, Found, u64)> {
        let found = RefCell::new(Vec::new());
        let mut decoder = Decoder::new(body, links, store);
        decoder.found = Some(&found);
        decoder.tables = tables;
        let shape = (self.check)(self, &mut decoder)?;
        decoder.end()?;
        let zero_width = decoder.zero_width();
        Ok((shape, found.into_inner(), zero_width))
    }

    /// Checks an object again with `store`, as [`check`](Kind::check) does, once the check
    /// without one deferred it, without collecting the typed links that check found, and returns
    /// its shape and the zero-width items it read in this object. With `recalled`, it takes the
    /// summary of each inline entry from there where an earlier recheck computed it, and keeps
    /// there those it computes.
    ///
    /// # Errors
    ///
    /// As for [`check`](Kind::check).
    pub(crate) fn recheck(
        &self,
        body: &[u8],
        links: &[Ptr],
        store: &Store,
        recalled: Option<&Recalled>,
    ) -> Result<(Shape, u64)> {
        let mut decoder = Decoder::new(body, links, Some(store));
        decoder.recalled = recalled;
        let shape = (self.check)(self, &mut decoder)?;
        decoder.end()?;
        Ok((shape, decoder.zero_width()))
    }
}

/// The summaries of single entries that the rechecks of deferred objects computed, by the types
/// of the map and the hash of the entry's key and encoded value, so that an entry met again in
/// another node, as in each version of a node a bundle rewrites, is summarized once.
///
/// It keeps two generations of at most [`RECALLED`] entries each, and drops the older one when
/// the newer fills, keeping what is found in it.
#[derive(Default)]
pub(crate) struct Recalled {
    generations: RefCell<[Generation; 2]>,
}

/// The summaries one generation of a [`Recalled`] keeps, by the type of the map and the hash of
/// the entry.
type Generation = HashMap<(TypeId, Hash), Box<dyn Any + Send + Sync>>;

impl Recalled {
    /// The summary of type `S` kept for the entry `id`, if there is one.
    pub(crate) fn get<S: Clone + Send + Sync + 'static>(&self, id: &(TypeId, Hash)) -> Option<S> {
        let mut generations = self.generations.borrow_mut();
        if let Some(summary) = generations[0].get(id) {
            return summary.downcast_ref::<S>().cloned();
        }
        let summary = generations[1].remove(id)?;
        let found = summary.downcast_ref::<S>().cloned();
        Self::keep(&mut generations, *id, summary);
        found
    }

    /// Keeps `summary` for the entry `id`.
    pub(crate) fn put<S: Send + Sync + 'static>(&self, id: (TypeId, Hash), summary: S) {
        Self::keep(&mut self.generations.borrow_mut(), id, Box::new(summary));
    }

    fn keep(
        generations: &mut [Generation; 2],
        id: (TypeId, Hash),
        summary: Box<dyn Any + Send + Sync>,
    ) {
        if generations[0].len() >= RECALLED {
            generations[1] = std::mem::take(&mut generations[0]);
        }
        generations[0].insert(id, summary);
    }
}

/// The objects a typed check has already passed, by hash and type, with the kinds they passed
/// as, so that an object linked from many places is checked once for each key range it must keep
/// within rather than once for each link, and again only for a link whose claimed summary no
/// earlier check confirmed.
#[derive(Default)]
pub(crate) struct Checked {
    seen: HashMap<(Hash, TypeId), (Shape, Vec<Kind>)>,
}

impl Checked {
    /// The shape of the object `hash`, if it already passed a check that covers `kind`, and
    /// whether one of its checks confirmed the summary `kind` claims.
    pub(crate) fn get(&self, hash: &Hash, kind: &Kind) -> Option<(Shape, bool)> {
        let (shape, kinds) = self.seen.get(&(*hash, kind.id))?;
        kinds
            .iter()
            .any(|seen| (seen.within)(seen, kind))
            .then(|| (*shape, kinds.iter().any(|seen| kind.vouched_by(seen))))
    }

    /// Records that the object `hash` passed a check as `kind` with `shape`.
    pub(crate) fn insert(&mut self, hash: Hash, kind: &Kind, shape: Shape) {
        let entry = self.seen.entry((hash, kind.id));
        entry
            .or_insert_with(|| (shape, Vec::with_capacity(1)))
            .1
            .push(kind.clone());
    }
}

/// A place in a [`Decoder`] that [`Decoder::rewind`] goes back to.
#[derive(Clone, Copy)]
pub(crate) struct Checkpoint {
    pos: usize,
    cursor: usize,
    empty_items: u64,
    heap: usize,
}

/// Reads one object's body and links, in the order an [`Encoder`] wrote them.
///
/// [`Data::decode`] reads a value from one. Every read checks its bounds and returns
/// [`Error::Decode`] past the end, so a damaged object fails to decode rather than panics.
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    links: &'a [Ptr],
    cursor: usize,
    store: Option<&'a Store>,
    empty_items: u64,
    heap: usize,
    base: usize,
    found: Option<&'a RefCell<Found>>,
    tables: Option<&'a RefCell<Vec<FoundTable>>>,
    recalled: Option<&'a Recalled>,
    rebuilds: bool,
    depth: usize,
}

impl<'a> Decoder<'a> {
    /// Creates a decoder of the object with body `buf` and links `links`.
    ///
    /// `store` is the store the object was read from, which the collections inside it read their
    /// nodes from. With `None`, a collection decoded from the object fails with
    /// [`Error::Detached`] once it needs one of its nodes.
    pub fn new(buf: &'a [u8], links: &'a [Ptr], store: Option<&'a Store>) -> Self {
        Decoder {
            buf,
            pos: 0,
            links,
            cursor: 0,
            store,
            empty_items: 0,
            heap: 0,
            base: 0,
            found: None,
            tables: None,
            recalled: None,
            rebuilds: true,
            depth: 0,
        }
    }

    /// Makes this decoder leave empty each index of a table it decodes that the table does not
    /// store under a fingerprint its type knows, rather than rebuild it from the rows.
    ///
    /// The check that a table's indexes agree with its rows decodes the rows it reads with one: it
    /// uses only the fields their indexes name, and a table nested in a row would otherwise read
    /// its own rows, which the walk may not have checked yet, to rebuild an index.
    pub(crate) fn skip_rebuilds(&mut self) {
        self.rebuilds = false;
    }

    /// Whether a table this decoder decodes rebuilds the indexes it does not store, as
    /// [`Data::decode`] does unless the decoder belongs to the check that a table's indexes agree
    /// with its rows.
    #[doc(hidden)]
    pub fn rebuilds(&self) -> bool {
        self.rebuilds
    }

    /// Where the decoder is, for [`rewind`](Decoder::rewind) to come back to.
    #[inline]
    pub(crate) fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            pos: self.pos,
            cursor: self.cursor,
            empty_items: self.empty_items,
            heap: self.heap,
        }
    }

    /// Goes back to `checkpoint`, forgetting the typed links recorded since, which are those at
    /// or past the link the decoder was at then.
    pub(crate) fn rewind(&mut self, checkpoint: Checkpoint) {
        self.pos = checkpoint.pos;
        self.cursor = checkpoint.cursor;
        self.empty_items = checkpoint.empty_items;
        self.heap = checkpoint.heap;
        if let Some(found) = self.found {
            let from = self.base + checkpoint.cursor;
            found.borrow_mut().retain(|(index, _)| *index < from);
        }
    }

    /// Reads with `read` from `at`, a place [`read_so_far`](Decoder::read_so_far) gave, apart from
    /// this decoder, which stays where it is: the typed links, tables and zero-width items `read`
    /// meets are not noted or counted.
    ///
    /// # Errors
    ///
    /// The error of `read`.
    fn reread<T>(
        &self,
        at: (usize, usize),
        read: impl FnOnce(&mut Decoder<'a>) -> Result<T>,
    ) -> Result<T> {
        let mut apart = Decoder {
            buf: self.buf,
            pos: at.0,
            links: self.links,
            cursor: at.1,
            store: self.store,
            empty_items: 0,
            heap: 0,
            base: self.base,
            found: None,
            tables: None,
            recalled: None,
            rebuilds: self.rebuilds,
            depth: self.depth,
        };
        read(&mut apart)
    }

    /// How much has been read: the bytes of the body and the links, from the start of this
    /// decoder's object.
    #[inline]
    pub(crate) fn read_so_far(&self) -> (usize, usize) {
        (self.pos, self.cursor)
    }

    /// Whether a link, typed or not, was read since `checkpoint`.
    #[inline]
    pub(crate) fn linked_since(&self, checkpoint: Checkpoint) -> bool {
        self.cursor != checkpoint.cursor
    }

    /// The bytes and the links read since `checkpoint`.
    pub(crate) fn read_since(&self, checkpoint: Checkpoint) -> (&'a [u8], &'a [Ptr]) {
        (
            &self.buf[checkpoint.pos..self.pos],
            &self.links[checkpoint.cursor..self.cursor],
        )
    }

    /// Notes the table value read since `start`, of the table type `id` named `name`, for
    /// `agree` to check once the typed check this decoder belongs to has passed, or beside it
    /// with `beside`, as [`FoundTable::beside`] says, if it collects tables.
    pub(crate) fn found_table(
        &self,
        start: Checkpoint,
        id: TypeId,
        name: &'static str,
        agree: Agree,
        beside: bool,
    ) {
        let Some(tables) = self.tables else {
            return;
        };
        tables
            .borrow_mut()
            .push(self.table_since(start, id, name, agree, beside));
    }

    /// The table value read since `start`, of the table type `id` named `name`, as
    /// [`found_table`](Decoder::found_table) notes it.
    pub(crate) fn table_since(
        &self,
        start: Checkpoint,
        id: TypeId,
        name: &'static str,
        agree: Agree,
        beside: bool,
    ) -> FoundTable {
        let (body, links) = self.read_since(start);
        let hash = hash_parts(body, links.iter().map(|link| &link.hash));
        FoundTable {
            id,
            name,
            agree,
            beside,
            body: body.into(),
            links: links.into(),
            hash,
        }
    }

    /// Where a [`Kind::recheck`] keeps the summaries of the entries it computed, or `None` for
    /// any other decoder.
    pub(crate) fn recalled(&self) -> Option<&'a Recalled> {
        self.recalled
    }

    /// How many links have been read, which a value that holds a collection or a raw [`Ptr`]
    /// moves on.
    #[inline]
    pub(crate) fn links_read(&self) -> usize {
        self.cursor
    }

    /// Whether the decoder reads from a store, which the collections it decodes can read their
    /// nodes from.
    pub(crate) fn has_store(&self) -> bool {
        self.store.is_some()
    }

    /// The number of body bytes not yet read.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Checks that every byte of the body and every link has been read, as they are once the value
    /// that fills an object, or a field that [`fields`](Decoder::fields) hands over, has been
    /// decoded from it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if bytes or links are left.
    #[inline]
    pub fn end(&self) -> Result<()> {
        if self.pos != self.buf.len() || self.cursor != self.links.len() {
            return Err(unread(self.remaining(), self.links.len() - self.cursor));
        }
        Ok(())
    }

    /// Whether what is left of the body is exactly one varint, as the body of a map without a
    /// summary is, which lets the branch table tell a whole table from a step on top of one.
    pub(crate) fn lone_varint(&self) -> bool {
        let rest = &self.buf[self.pos..];
        let end = rest.iter().position(|byte| byte & 0x80 == 0);
        end.is_some() && end == rest.len().checked_sub(1)
    }

    /// An estimate of the heap bytes the values decoded so far allocated, which weighs them in
    /// the cache.
    pub(crate) fn heap(&self) -> usize {
        self.heap
    }

    /// The zero-width items [`item`](Decoder::item) has counted in this object so far, less those
    /// read since a checkpoint it went back to.
    pub(crate) fn zero_width(&self) -> u64 {
        self.empty_items
    }

    fn allocated(&mut self, count: usize, bytes: usize) {
        if count > 0 && bytes > 0 {
            self.heap = self
                .heap
                .saturating_add(count.saturating_mul(allocation(bytes)));
        }
    }

    fn tree<K, V>(&mut self, len: usize) {
        let node = TREE_HEADER + TREE_CAPACITY * (size_of::<K>() + size_of::<V>());
        self.allocated(len.div_ceil(TREE_FILL), node);
    }

    /// The next `n` bytes of the body.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if fewer than `n` bytes are left.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(Error::Decode(format!(
                "wanted {n} bytes, {} left",
                self.remaining()
            )));
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// The next byte.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] at the end of the body.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// The next two bytes, as a little-endian `u16`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if fewer than two bytes are left.
    pub fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// The next LEB128 varint, as [`Encoder::varint`] writes it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] for a varint that runs past the end of the body or past 64 bits,
    /// and for one that takes more bytes than its value needs, which ends in a zero byte, so that
    /// a value is read from the one form an encoder writes for it.
    #[inline]
    pub fn varint(&mut self) -> Result<u64> {
        match self.buf.get(self.pos) {
            Some(&byte) if byte < 0x80 => {
                self.pos += 1;
                Ok(u64::from(byte))
            }
            _ => self.long_varint(),
        }
    }

    fn long_varint(&mut self) -> Result<u64> {
        let rest = &self.buf[self.pos..];
        let mut value = 0u64;
        let mut shift = 0u32;
        for (index, &byte) in rest.iter().enumerate() {
            if index == 8
                && let Some(&tenth) = rest.get(9)
            {
                let more = byte >> 7;
                let last = if more == 0 { byte } else { tenth };
                if (last != 0) & ((more == 0) | (last == 1)) {
                    self.pos += 9 + usize::from(more);
                    return Ok(value
                        | u64::from(byte & 0x7f) << 56
                        | u64::from(tenth & more) << 63);
                }
            }
            if shift == 63 && byte > 1 {
                return Err(Error::Decode("varint overflow".into()));
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                if byte == 0 {
                    return Err(overlong());
                }
                self.pos += index + 1;
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                return Err(Error::Decode("varint overflow".into()));
            }
        }
        self.pos = self.buf.len();
        Err(Error::Decode(
            "varint runs off the end of its object".into(),
        ))
    }

    /// Reads one item of a sequence with `read`, counting the items that consume no bytes and no
    /// links.
    ///
    /// Use it for each item of a sequence whose length was decoded, so that a forged length of
    /// zero-width items, such as a billion `()`s, fails rather than runs on. The count is per
    /// object: the keys, values and summaries of one map node share it, and a value spilled into
    /// an object of its own, or a nested collection's nodes, count apart. [`Encoder::item`] counts
    /// items as they are written and refuses an object with more, so what encodes decodes.
    ///
    /// # Errors
    ///
    /// Returns the error of `read`, and [`Error::Decode`] once more than 2^24 zero-width items have
    /// been read from one object.
    pub fn item<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let before = self.read_so_far();
        let value = read(self)?;
        self.counted(before)?;
        Ok(value)
    }

    /// Counts one item of a sequence that began where the decoder was at `before`, as
    /// [`item`](Decoder::item) counts the item it reads, for an item read in parts.
    ///
    /// # Errors
    ///
    /// As for [`item`](Decoder::item).
    #[inline]
    fn counted(&mut self, before: (usize, usize)) -> Result<()> {
        if self.read_so_far() == before {
            self.empty_items += 1;
            if self.empty_items > MAX_EMPTY_ITEMS {
                return Err(Error::Decode("too many zero-width items".into()));
            }
        }
        Ok(())
    }

    /// Reads with `read` one level deeper, as [`Encoder::nested`] wrote it.
    ///
    /// The impls of `Box`, `Arc`, `Vec`, `BTreeMap` and `BTreeSet` each read what they hold
    /// through it, so a hand-written impl needs it only where it holds a value of its own type
    /// without going through one of them, as [`MAX_DEPTH`] describes. `read` runs on a segment of
    /// stack mapped for the thread if the thread's stack has less than 128 KiB left.
    ///
    /// # Errors
    ///
    /// Returns the error of `read`, and [`Error::Decode`] if the value nests deeper than
    /// [`MAX_DEPTH`] levels, or the stack is short and cannot grow, as on Windows.
    #[inline(always)]
    pub fn nested<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.depth = self.deeper()?;
        let value = match stack::room() {
            true => read(self),
            false => stack::elsewhere(|| read(self)).unwrap_or_else(|| Err(no_room(self.depth))),
        };
        self.depth -= 1;
        value
    }

    /// The depth one level below where the decoder is reading, or [`Error::Decode`] if that is
    /// past [`MAX_DEPTH`].
    #[inline]
    fn deeper(&self) -> Result<usize> {
        if self.depth >= MAX_DEPTH {
            return Err(too_deep());
        }
        Ok(self.depth + 1)
    }

    /// The next link, in the order [`Encoder::link`] added them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if every link has been read.
    pub fn link(&mut self) -> Result<Ptr> {
        let ptr = self
            .links
            .get(self.cursor)
            .copied()
            .ok_or_else(|| Error::Decode("link table exhausted".into()))?;
        self.cursor += 1;
        Ok(ptr)
    }

    /// The next link, as [`link`](Decoder::link) reads it, which during a typed check is recorded
    /// with the [`Kind`] that `kind` makes, for the check to follow.
    ///
    /// # Errors
    ///
    /// As for [`link`](Decoder::link).
    pub(crate) fn typed_link(&mut self, kind: impl FnOnce() -> Kind) -> Result<Ptr> {
        let index = self.base + self.cursor;
        let ptr = self.link()?;
        if let Some(found) = self.found {
            found.borrow_mut().push((index, kind()));
        }
        Ok(ptr)
    }

    /// The next link, as [`typed_link`](Decoder::typed_link) reads it, and what `read` then
    /// reads after it, where the [`Kind`] of the link depends on what `read` returns.
    ///
    /// # Errors
    ///
    /// As for [`link`](Decoder::link), and the error of `read`.
    pub(crate) fn typed_link_before<T>(
        &mut self,
        read: impl FnOnce(&mut Self) -> Result<T>,
        kind: impl FnOnce(&T) -> Kind,
    ) -> Result<T> {
        let index = self.base + self.cursor;
        self.link()?;
        let after = read(self)?;
        if let Some(found) = self.found {
            found.borrow_mut().push((index, kind(&after)));
        }
        Ok(after)
    }

    /// A handle on the store the object was read from, for a collection to read its nodes from,
    /// or `None` for an object decoded from memory.
    pub fn store(&self) -> Option<Store> {
        self.store.map(Store::share)
    }

    /// Reads a struct's fields as [`Encoder::field`] wrote them: a varint count, then each field,
    /// in strictly increasing order of tag.
    ///
    /// `f` gets each field's tag and a decoder over that field's bytes and links alone. What `f`
    /// leaves unread is skipped, so a field the type no longer has can be ignored, and a missing
    /// field is one whose tag never comes. The derive reads each field it knows to its end, and
    /// refuses one that its type does not read whole, as [`end`](Decoder::end) checks.
    ///
    /// # Errors
    ///
    /// Returns the error of `f`, and [`Error::Decode`] if a field runs past the end of the body or
    /// the links, flags links and holds none, or has a tag no greater than the one before it.
    pub fn fields(&mut self, f: impl FnMut(u16, &mut Decoder<'a>) -> Result<()>) -> Result<()> {
        self.read_fields(false, f)
    }

    /// Reads fields as [`fields`](Decoder::fields) does, but lets several fields in a row share
    /// a tag, as a table stores each of its indexes under one tag.
    ///
    /// # Errors
    ///
    /// As for [`fields`](Decoder::fields), refusing a tag lower than the one before it.
    #[doc(hidden)]
    pub fn fields_sharing_tags(
        &mut self,
        f: impl FnMut(u16, &mut Decoder<'a>) -> Result<()>,
    ) -> Result<()> {
        self.read_fields(true, f)
    }

    fn read_fields(
        &mut self,
        sharing: bool,
        mut f: impl FnMut(u16, &mut Decoder<'a>) -> Result<()>,
    ) -> Result<()> {
        let count = self.varint()?;
        let mut floor = 0u32;
        for _ in 0..count {
            let (tag, body, links) = self.field_parts()?;
            if u32::from(tag) < floor {
                return Err(Error::Decode(format!(
                    "field tag {tag} is stored out of order"
                )));
            }
            floor = u32::from(tag) + u32::from(!sharing);
            let mut sub = Decoder {
                buf: body,
                pos: 0,
                links: &self.links[links.clone()],
                cursor: 0,
                store: self.store,
                empty_items: self.empty_items,
                heap: self.heap,
                base: self.base + links.start,
                found: self.found,
                tables: self.tables,
                recalled: self.recalled,
                rebuilds: self.rebuilds,
                depth: self.depth,
            };
            f(tag, &mut sub)?;
            self.empty_items = sub.empty_items;
            self.heap = sub.heap;
        }
        Ok(())
    }

    /// Reads the next field of a struct, one of those that [`fields`](Decoder::fields) reads
    /// after their count, and returns its tag with a decoder over that field's bytes and links
    /// alone.
    ///
    /// The decoder reads at this decoder's depth of nesting, as [`fields`](Decoder::fields)
    /// does, and starts its count of zero-width items at nothing: one read as part of this
    /// decoder's object takes the count from it first and gives it back after, as
    /// [`fields`](Decoder::fields) does, so that all the fields of one object share it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] for a field that runs past the end of the object.
    pub(crate) fn next_field(&mut self) -> Result<(u16, Decoder<'a>)> {
        let (tag, body, links) = self.field_parts()?;
        let mut field = Decoder::new(body, &self.links[links], self.store);
        field.rebuilds = self.rebuilds;
        field.depth = self.depth;
        Ok((tag, field))
    }

    /// Reads the tag, the length and the link count of the next field of a struct, and returns
    /// the tag, the field's bytes and the range of its links among this decoder's, moving past
    /// both.
    fn field_parts(&mut self) -> Result<(u16, &'a [u8], Range<usize>)> {
        let tag = self.u16()?;
        let prefix = self.varint()?;
        let link_count = if prefix & 1 == 1 {
            match self.varint()? {
                0 => return Err(Error::Decode("a field flags links it does not hold".into())),
                count => count as usize,
            }
        } else {
            0
        };
        let body = self.take((prefix >> 1) as usize)?;
        let start = self.cursor;
        let end = start
            .checked_add(link_count)
            .filter(|end| *end <= self.links.len())
            .ok_or_else(|| Error::Decode("field link count out of range".into()))?;
        self.cursor = end;
        Ok((tag, body, start..end))
    }
}

impl Data for Ptr {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.sink().check_link(*self)?;
        w.link(*self);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        r.link()
    }

    fn describe(d: &mut Description) {
        d.word("ptr");
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

impl Data for () {
    fn encode(&self, _w: &mut Encoder<'_>) -> Result<()> {
        Ok(())
    }

    fn decode(_r: &mut Decoder<'_>) -> Result<Self> {
        Ok(())
    }

    fn describe(d: &mut Description) {
        d.word("unit");
    }

    fn same_encoding(&self, _other: &Self) -> bool {
        true
    }
}

impl Data for bool {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.u8(u8::from(*self));
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        match r.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(Error::Decode(format!("invalid bool {other}"))),
        }
    }

    fn describe(d: &mut Description) {
        d.word("bool");
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self == other
    }
}

impl Data for u8 {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.u8(*self);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        r.u8()
    }

    fn describe(d: &mut Description) {
        d.word("u8");
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self == other
    }

    fn encode_slice(items: &[Self], w: &mut Encoder<'_>) -> Result<()> {
        w.raw(items);
        Ok(())
    }

    fn decode_vec(r: &mut Decoder<'_>, len: usize) -> Result<Vec<Self>> {
        Ok(r.take(len)?.to_vec())
    }
}

macro_rules! unsigned {
    ($($ty:ty),*) => {$(
        impl Data for $ty {
            fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
                w.varint(*self as u64);
                Ok(())
            }

            fn decode(r: &mut Decoder<'_>) -> Result<Self> {
                let value = r.varint()?;
                <$ty>::try_from(value)
                    .map_err(|_| Error::Decode(format!("{value} out of range for {}", stringify!($ty))))
            }

            fn describe(d: &mut Description) {
                d.word(stringify!($ty));
            }

            fn same_encoding(&self, other: &Self) -> bool {
                self == other
            }
        }
    )*};
}

unsigned!(u16, u32, u64, usize);

macro_rules! signed {
    ($($ty:ty),*) => {$(
        impl Data for $ty {
            fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
                let wide = *self as i64;
                w.varint(((wide << 1) ^ (wide >> 63)) as u64);
                Ok(())
            }

            fn decode(r: &mut Decoder<'_>) -> Result<Self> {
                let raw = r.varint()?;
                let wide = ((raw >> 1) as i64) ^ -((raw & 1) as i64);
                <$ty>::try_from(wide)
                    .map_err(|_| Error::Decode(format!("{wide} out of range for {}", stringify!($ty))))
            }

            fn describe(d: &mut Description) {
                d.word(stringify!($ty));
            }

            fn same_encoding(&self, other: &Self) -> bool {
                self == other
            }
        }
    )*};
}

signed!(i8, i16, i32, i64, isize);

impl Data for u128 {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.raw(&self.to_le_bytes());
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let bytes: [u8; 16] = r.take(16)?.try_into().expect("slice length is 16");
        Ok(u128::from_le_bytes(bytes))
    }

    fn describe(d: &mut Description) {
        d.word("u128");
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self == other
    }
}

impl Data for i128 {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        let bits = ((*self << 1) ^ (*self >> 127)) as u128;
        w.varint(bits as u64);
        w.varint((bits >> 64) as u64);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let bits = u128::from(r.varint()?) | (u128::from(r.varint()?) << 64);
        Ok(((bits >> 1) as i128) ^ -((bits & 1) as i128))
    }

    fn describe(d: &mut Description) {
        d.word("i128");
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self == other
    }
}

impl Data for f32 {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.raw(&self.to_le_bytes());
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let bytes: [u8; 4] = r.take(4)?.try_into().expect("slice length is 4");
        Ok(f32::from_le_bytes(bytes))
    }

    fn describe(d: &mut Description) {
        d.word("f32");
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.to_bits() == other.to_bits()
    }
}

impl Data for f64 {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.raw(&self.to_le_bytes());
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let bytes: [u8; 8] = r.take(8)?.try_into().expect("slice length is 8");
        Ok(f64::from_le_bytes(bytes))
    }

    fn describe(d: &mut Description) {
        d.word("f64");
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.to_bits() == other.to_bits()
    }
}

impl Data for char {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.varint(u64::from(u32::from(*self)));
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let raw = r.varint()?;
        u32::try_from(raw)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(|| Error::Decode(format!("invalid char {raw}")))
    }

    fn describe(d: &mut Description) {
        d.word("char");
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self == other
    }
}

impl Data for String {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.varint(self.len() as u64);
        w.raw(self.as_bytes());
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let len = r.varint()? as usize;
        let bytes = r.take(len)?;
        let text = String::from_utf8(bytes.to_vec()).map_err(|e| Error::Decode(e.to_string()))?;
        r.allocated(1, text.capacity());
        Ok(text)
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        let len = r.varint()? as usize;
        std::str::from_utf8(r.take(len)?).map_err(|e| Error::Decode(e.to_string()))?;
        Ok(())
    }

    fn describe(d: &mut Description) {
        d.word("string");
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self == other
    }
}

impl<T: Data> Data for Option<T> {
    const OPTIONAL: bool = true;

    fn absent(&self) -> bool {
        self.is_none()
    }

    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        match self {
            None => w.u8(0),
            Some(value) => {
                w.u8(1);
                value.encode(w)?;
            }
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        match r.u8()? {
            0 => Ok(None),
            1 => Ok(Some(T::decode(r)?)),
            other => Err(Error::Decode(format!("invalid option tag {other}"))),
        }
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        match r.u8()? {
            0 => Ok(()),
            1 => T::visit(r),
            other => Err(Error::Decode(format!("invalid option tag {other}"))),
        }
    }

    fn missing() -> Option<Self> {
        Some(None)
    }

    fn describe(d: &mut Description) {
        d.word("option");
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        match (self, other) {
            (None, None) => true,
            (Some(a), Some(b)) => a.same_encoding(b),
            _ => false,
        }
    }

    fn tables(t: &mut Tabled) {
        t.of::<T>();
    }

    fn pair(old: &mut Decoder<'_>, new: &mut Decoder<'_>, pairing: &mut Pairing<'_>) -> Result<()> {
        let (from_old, from_new) = (old.checkpoint(), new.checkpoint());
        if (old.u8()?, new.u8()?) == (1, 1) {
            return T::pair(old, new, pairing);
        }
        old.rewind(from_old);
        new.rewind(from_new);
        Self::visit(old)?;
        Self::visit(new)
    }
}

impl<T: Data> Data for Vec<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.nested(|w| {
            w.varint(self.len() as u64);
            T::encode_slice(self, w)
        })
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        r.nested(|r| {
            let len = r.varint()? as usize;
            let items = T::decode_vec(r, len)?;
            r.allocated(1, items.capacity() * size_of::<T>());
            Ok(items)
        })
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        r.nested(|r| {
            let len = r.varint()?;
            for _ in 0..len {
                r.item(T::visit)?;
            }
            Ok(())
        })
    }

    fn missing() -> Option<Self> {
        Some(Vec::new())
    }

    fn describe(d: &mut Description) {
        d.word("vec");
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.len() == other.len()
            && alike(|| self.iter().zip(other).all(|(a, b)| a.same_encoding(b)))
    }

    fn tables(t: &mut Tabled) {
        t.of::<T>();
    }

    fn pair(old: &mut Decoder<'_>, new: &mut Decoder<'_>, pairing: &mut Pairing<'_>) -> Result<()> {
        old.nested(|old| {
            new.nested(|new| {
                let (olds, news) = (old.varint()?, new.varint()?);
                paired_items::<T>(old, new, pairing, olds, news)
            })
        })
    }
}

/// An immutable byte buffer that clones in O(1).
///
/// It stores as a `Vec<u8>` does, its length and then its bytes, so a field can move between the
/// two without changing what is stored. It derefs to `[u8]` and borrows as one, so a
/// `Map<Bytes, V>` is looked up by a byte slice. Decoding copies the bytes once out of the object
/// they were read from.
///
/// # Examples
///
/// ```
/// use ferro::{Bytes, Map, hash_of};
///
/// let mut blobs: Map<Bytes, u32> = Map::new();
/// blobs.insert(Bytes::from(&b"key"[..]), 1);
/// assert_eq!(blobs.get(&b"key"[..]).as_deref(), Some(&1));
///
/// let bytes = Bytes::from(vec![1, 2, 3]);
/// assert_eq!(&bytes[..], [1, 2, 3]);
/// assert_eq!(hash_of(&bytes), hash_of(&vec![1u8, 2, 3]));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(Arc<[u8]>);

impl Bytes {
    /// Creates an empty buffer.
    pub fn new() -> Self {
        Bytes::default()
    }
}

impl Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Borrow<[u8]> for Bytes {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Self {
        Bytes(bytes.into())
    }
}

impl From<&[u8]> for Bytes {
    fn from(bytes: &[u8]) -> Self {
        Bytes(bytes.into())
    }
}

impl Data for Bytes {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.varint(self.len() as u64);
        w.raw(self);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let len = r.varint()? as usize;
        let bytes = Bytes(r.take(len)?.into());
        r.allocated(1, ARC_HEADER + len);
        Ok(bytes)
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        let len = r.varint()? as usize;
        r.take(len).map(drop)
    }

    fn missing() -> Option<Self> {
        Some(Bytes::new())
    }

    fn describe(d: &mut Description) {
        d.word("bytes");
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self == other
    }
}

impl<T: Data, const N: usize> Data for [T; N] {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        T::encode_slice(self, w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        T::decode_vec(r, N)?
            .try_into()
            .map_err(|_| Error::Decode("array length mismatch".into()))
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        for _ in 0..N {
            r.item(T::visit)?;
        }
        Ok(())
    }

    fn describe(d: &mut Description) {
        d.word("array");
        d.count(N as u64);
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.iter().zip(other).all(|(a, b)| a.same_encoding(b))
    }

    fn tables(t: &mut Tabled) {
        t.of::<T>();
    }

    fn pair(old: &mut Decoder<'_>, new: &mut Decoder<'_>, pairing: &mut Pairing<'_>) -> Result<()> {
        paired_items::<T>(old, new, pairing, N as u64, N as u64)
    }
}

impl<T: Data> Data for Box<T> {
    const OPTIONAL: bool = T::OPTIONAL;

    fn absent(&self) -> bool {
        (**self).absent()
    }

    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.nested(|w| (**self).encode(w))
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let value = Box::new(r.nested(T::decode)?);
        r.allocated(1, size_of::<T>());
        Ok(value)
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        r.nested(T::visit)
    }

    fn missing() -> Option<Self> {
        T::missing().map(Box::new)
    }

    fn describe(d: &mut Description) {
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        alike(|| (**self).same_encoding(other))
    }

    fn tables(t: &mut Tabled) {
        t.of::<T>();
    }

    fn pair(old: &mut Decoder<'_>, new: &mut Decoder<'_>, pairing: &mut Pairing<'_>) -> Result<()> {
        old.nested(|old| new.nested(|new| T::pair(old, new, pairing)))
    }
}

impl<T: Data> Data for Arc<T> {
    const OPTIONAL: bool = T::OPTIONAL;

    fn absent(&self) -> bool {
        (**self).absent()
    }

    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.nested(|w| (**self).encode(w))
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let value = Arc::new(r.nested(T::decode)?);
        r.allocated(1, ARC_HEADER + size_of::<T>());
        Ok(value)
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        r.nested(T::visit)
    }

    fn missing() -> Option<Self> {
        T::missing().map(Arc::new)
    }

    fn describe(d: &mut Description) {
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        Arc::ptr_eq(self, other) || alike(|| (**self).same_encoding(other))
    }

    fn tables(t: &mut Tabled) {
        t.of::<T>();
    }

    fn pair(old: &mut Decoder<'_>, new: &mut Decoder<'_>, pairing: &mut Pairing<'_>) -> Result<()> {
        old.nested(|old| new.nested(|new| T::pair(old, new, pairing)))
    }
}

impl<K: Data + Ord, V: Data> Data for BTreeMap<K, V> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.nested(|w| {
            w.varint(self.len() as u64);
            for (key, value) in self {
                w.item(|w| {
                    key.encode(w)?;
                    value.encode(w)
                })?;
            }
            Ok(())
        })
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        r.nested(|r| {
            let len = r.varint()?;
            let entry = |r: &mut Decoder<'_>| Ok((K::decode(r)?, V::decode(r)?));
            let entries = sorted_entries(r, len, entry, |(key, _)| key)?;
            r.tree::<K, V>(entries.len());
            Ok(BTreeMap::from_iter(entries))
        })
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        r.nested(|r| visit_entries(r, K::decode, V::visit))
    }

    fn missing() -> Option<Self> {
        Some(BTreeMap::new())
    }

    fn describe(d: &mut Description) {
        d.word("btreemap");
        d.of::<K>();
        d.of::<V>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.len() == other.len()
            && alike(|| {
                self.iter()
                    .zip(other)
                    .all(|(a, b)| a.0.same_encoding(b.0) && a.1.same_encoding(b.1))
            })
    }

    fn tables(t: &mut Tabled) {
        t.of::<V>();
    }

    fn pair(old: &mut Decoder<'_>, new: &mut Decoder<'_>, pairing: &mut Pairing<'_>) -> Result<()> {
        if !holds_tables::<V>() {
            Self::visit(old)?;
            return Self::visit(new);
        }
        old.nested(|old| {
            new.nested(|new| {
                let (mut olds, news) = (old.varint()?, new.varint()?);
                let mut ahead: Option<(K, (usize, usize))> = None;
                for _ in 0..news {
                    let at = new.read_so_far();
                    let key = K::decode(new)?;
                    loop {
                        if ahead.is_none() {
                            if olds == 0 {
                                break;
                            }
                            olds -= 1;
                            let at = old.read_so_far();
                            ahead = Some((K::decode(old)?, at));
                        }
                        match &ahead {
                            Some((other, at)) if *other < key => {
                                let at = *at;
                                V::visit(old)?;
                                old.counted(at)?;
                                ahead = None;
                            }
                            _ => break,
                        }
                    }
                    match ahead.take() {
                        Some((other, at)) if other == key => {
                            V::pair(old, new, pairing)?;
                            old.counted(at)?;
                        }
                        other => {
                            ahead = other;
                            V::visit(new)?;
                        }
                    }
                    new.counted(at)?;
                }
                if let Some((_, at)) = ahead {
                    V::visit(old)?;
                    old.counted(at)?;
                }
                for _ in 0..olds {
                    old.item(|old| {
                        K::visit(old)?;
                        V::visit(old)
                    })?;
                }
                Ok(())
            })
        })
    }
}

impl<T: Data + Ord> Data for BTreeSet<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.nested(|w| {
            w.varint(self.len() as u64);
            for item in self {
                w.item(|w| item.encode(w))?;
            }
            Ok(())
        })
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        r.nested(|r| {
            let len = r.varint()?;
            let items = sorted_entries(r, len, T::decode, |item| item)?;
            r.tree::<T, ()>(items.len());
            Ok(BTreeSet::from_iter(items))
        })
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        r.nested(|r| visit_entries(r, T::decode, |_| Ok(())))
    }

    fn missing() -> Option<Self> {
        Some(BTreeSet::new())
    }

    fn describe(d: &mut Description) {
        d.word("btreeset");
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.len() == other.len()
            && alike(|| self.iter().zip(other).all(|(a, b)| a.same_encoding(b)))
    }
}

macro_rules! tuple {
    ($($name:ident $index:tt),+) => {
        impl<$($name: Data),+> Data for ($($name,)+) {
            fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
                $(self.$index.encode(w)?;)+
                Ok(())
            }

            fn decode(r: &mut Decoder<'_>) -> Result<Self> {
                Ok(($($name::decode(r)?,)+))
            }

            fn visit(r: &mut Decoder<'_>) -> Result<()> {
                $($name::visit(r)?;)+
                Ok(())
            }

            fn describe(d: &mut Description) {
                d.word("tuple");
                d.count([$($index),+].len() as u64);
                $(d.of::<$name>();)+
            }

            fn same_encoding(&self, other: &Self) -> bool {
                $(self.$index.same_encoding(&other.$index))&&+
            }

            fn tables(t: &mut Tabled) {
                $(t.of::<$name>();)+
            }

            fn pair(
                old: &mut Decoder<'_>,
                new: &mut Decoder<'_>,
                pairing: &mut Pairing<'_>,
            ) -> Result<()> {
                $($name::pair(old, new, pairing)?;)+
                Ok(())
            }
        }
    };
}

tuple!(A 0);
tuple!(A 0, B 1);
tuple!(A 0, B 1, C 2);
tuple!(A 0, B 1, C 2, D 3);
tuple!(A 0, B 1, C 2, D 3, E 4);
tuple!(A 0, B 1, C 2, D 3, E 4, F 5);
tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6);
tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);
tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10);
tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11);
