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
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Deref;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::shared::Erased;
use crate::store::Store;

/// The length of a [`Hash`](type@Hash) in bytes.
pub const HASH_BYTES: usize = 20;

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
/// - **The encoding is canonical.** It depends only on the value, never on how the value was
///   built, its capacity or what it shared. Root hashes, `==` on collections, the skip of a
///   commit that changes nothing, [`Map::diff`](crate::Map::diff) and merges all go by
///   encodings, so a value that encodes two ways looks changed when it is not.
/// - **A key's encoding agrees with its `Ord`.** A key type's values must compare equal exactly
///   when they encode the same. A key's place in a map's tree comes from a hash of its encoding
///   and its order from `Ord`, so two equal keys that encode differently can land in different
///   nodes and be missed by lookups. Neither the encoding nor the `Ord` of a key type may change
///   on existing data.
/// - **The encoding evolves by name.** Old data must keep decoding as the type changes. The
///   derive gets this by storing each field under a tag hashed from its name, which
///   [`Encoder::field`] and [`Decoder::fields`] offer a hand-written impl too; one that writes
///   bare values must not change what it writes once data is stored.
/// - [`same_encoding`](Data::same_encoding), when overridden, gives the answer comparing the two
///   encodings would, and [`describe`](Data::describe), when overridden, changes whenever the
///   encoding does.
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
    /// Writes the value into `w`: its bytes with [`Encoder::u8`], [`Encoder::varint`] and the
    /// like, and its links with [`Encoder::link`].
    ///
    /// # Errors
    ///
    /// Returns the errors of what it writes: of a nested value that cannot encode, such as a
    /// collection whose nodes cannot be read, and of the sink, such as [`Error::StalePointer`] for
    /// a [`Ptr`] into an earlier generation of the file.
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()>;
    /// Reads a value that [`encode`](Data::encode) wrote, consuming exactly what it wrote.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if the bytes do not form a value of the type, and
    /// [`Error::MissingField`] for a stored struct that lacks a field with no default.
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
    fn missing() -> Option<Self> {
        None
    }
    /// Describes the type's encoding into `d`, for the fingerprints that tell a table when an
    /// index must be rebuilt.
    ///
    /// The derive describes a struct by its stored field names and their types. The default
    /// describes a type by its Rust name, so a hand-written impl whose encoding can change while
    /// its name stays the same should describe what it writes.
    fn describe(d: &mut Description) {
        d.word("rust");
        d.word(type_name::<Self>());
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
    /// Writes `items` one after another, as `Vec` and arrays store their items.
    ///
    /// The default encodes each item in turn. An override, such as `u8`'s, which copies the bytes
    /// at once, must write exactly what the default would.
    ///
    /// # Errors
    ///
    /// As for [`encode`](Data::encode).
    fn encode_slice(items: &[Self], w: &mut Encoder<'_>) -> Result<()> {
        for item in items {
            item.encode(w)?;
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
        }
    }

    /// Creates an encoder with room for `bytes` bytes and `links` links before it reallocates.
    pub fn with_capacity(sink: &'a mut dyn Sink, bytes: usize, links: usize) -> Self {
        Encoder {
            buf: Vec::with_capacity(bytes),
            links: Vec::with_capacity(links),
            sink,
            unsettled: false,
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
    pub fn rewind(&mut self, mark: (usize, usize)) {
        self.buf.truncate(mark.0);
        self.links.truncate(mark.1);
    }

    /// Encodes a separate object with `f`, hands it to the sink and returns the link to it,
    /// which the caller records with [`link`](Encoder::link).
    ///
    /// # Errors
    ///
    /// Returns the errors of `f` and of the sink.
    pub fn child(&mut self, f: impl FnOnce(&mut Encoder<'_>) -> Result<()>) -> Result<Ptr> {
        let mut encoder = Encoder::new(&mut *self.sink);
        f(&mut encoder)?;
        self.unsettled |= encoder.unsettled;
        encoder.finish()
    }

    /// Writes one field of a struct under `tag`, as the derive stores fields.
    ///
    /// The field is written as `tag`, then a varint of the length of what `f` writes, with a flag
    /// and a count when `f` writes links, then what `f` writes, so that a reader that does not
    /// know the tag can skip it. A struct writes the number of fields with
    /// [`varint`](Encoder::varint) and then each field in increasing order of tag, which keeps its
    /// encoding canonical, and [`Decoder::fields`] reads them back. The derive tags each field
    /// with the [`field_tag`] of its stored name.
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

    /// Decodes the object the link leads to, for [`confirm`](Claim::confirm) to test this claim
    /// against.
    ///
    /// # Errors
    ///
    /// The errors of decoding the object, and [`Error::Decode`] for a claim about the object
    /// alone, such as a map node's summary: an object has one such summary, so one that met a
    /// claim cannot meet another that disagrees with it.
    fn decode(&self, body: &[u8], links: &[Ptr]) -> Result<Arc<dyn Any + Send + Sync>>;

    /// Checks this claim against an object that [`decode`](Claim::decode) gave.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] if the object does not have the summary claimed.
    fn confirm(&self, decoded: &(dyn Any + Send + Sync)) -> Result<()>;
}

/// The typed links a check found in an object: each one's index in the object's links, with the
/// [`Kind`] it must lead to.
pub(crate) type Found = Vec<(usize, Kind)>;

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

    /// Checks an object with this body and these links as this kind, returning its shape and the
    /// typed links found in it, which the caller checks in turn.
    ///
    /// # Errors
    ///
    /// Returns the error of the first part that does not decode, or does not meet what the kind
    /// asks of it.
    pub(crate) fn check(&self, body: &[u8], links: &[Ptr]) -> Result<(Shape, Found)> {
        let found = RefCell::new(Vec::new());
        let mut decoder = Decoder::new(body, links, None);
        decoder.found = Some(&found);
        let shape = (self.check)(self, &mut decoder)?;
        Ok((shape, found.into_inner()))
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
    ///
    /// It keeps `kind` only if no kind it keeps for the object already covers its range, so it
    /// keeps one kind for each range however many summaries were claimed under it.
    pub(crate) fn insert(&mut self, hash: Hash, kind: &Kind, shape: Shape) {
        let (_, kinds) = self
            .seen
            .entry((hash, kind.id))
            .or_insert_with(|| (shape, Vec::with_capacity(1)));
        if !kinds.iter().any(|seen| (seen.within)(seen, kind)) {
            kinds.push(kind.clone());
        }
    }
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
        }
    }

    /// The number of body bytes not yet read.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
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
    /// Returns [`Error::Decode`] for a varint that runs past the end of the body or past 64 bits.
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
        let mut value = 0u64;
        let mut shift = 0u32;
        for (index, &byte) in self.buf[self.pos..].iter().enumerate() {
            if shift == 63 && byte > 1 {
                return Err(Error::Decode("varint overflow".into()));
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
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
    /// zero-width items, such as a billion `()`s, fails rather than runs on.
    ///
    /// # Errors
    ///
    /// Returns the error of `read`, and [`Error::Decode`] once more than 2^24 zero-width items have
    /// been read from one object.
    pub fn item<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let before = (self.pos, self.cursor);
        let value = read(self)?;
        if (self.pos, self.cursor) == before {
            self.empty_items += 1;
            if self.empty_items > MAX_EMPTY_ITEMS {
                return Err(Error::Decode("too many zero-width items".into()));
            }
        }
        Ok(value)
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

    /// A handle on the store the object was read from, for a collection to read its nodes from,
    /// or `None` for an object decoded from memory.
    pub fn store(&self) -> Option<Store> {
        self.store.map(Store::share)
    }

    /// Reads a struct's fields as [`Encoder::field`] wrote them: a varint count, then each field.
    ///
    /// `f` gets each field's tag and a decoder over that field's bytes and links alone. What `f`
    /// leaves unread is skipped, so a field the type no longer has can be ignored, and a missing
    /// field is one whose tag never comes.
    ///
    /// # Errors
    ///
    /// Returns the error of `f`, and [`Error::Decode`] if a field runs past the end of the body or
    /// the links.
    pub fn fields(&mut self, mut f: impl FnMut(u16, &mut Decoder<'a>) -> Result<()>) -> Result<()> {
        let count = self.varint()?;
        for _ in 0..count {
            let tag = self.u16()?;
            let prefix = self.varint()?;
            let link_count = if prefix & 1 == 1 {
                self.varint()? as usize
            } else {
                0
            };
            let body = self.take((prefix >> 1) as usize)?;
            let end = self
                .cursor
                .checked_add(link_count)
                .filter(|end| *end <= self.links.len())
                .ok_or_else(|| Error::Decode("field link count out of range".into()))?;
            let mut sub = Decoder {
                buf: body,
                pos: 0,
                links: &self.links[self.cursor..end],
                cursor: 0,
                store: self.store,
                empty_items: self.empty_items,
                heap: self.heap,
                base: self.base + self.cursor,
                found: self.found,
            };
            self.cursor = end;
            f(tag, &mut sub)?;
            self.empty_items = sub.empty_items;
            self.heap = sub.heap;
        }
        Ok(())
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
}

impl<T: Data> Data for Vec<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.varint(self.len() as u64);
        T::encode_slice(self, w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let len = r.varint()? as usize;
        let items = T::decode_vec(r, len)?;
        r.allocated(1, items.capacity() * size_of::<T>());
        Ok(items)
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        let len = r.varint()?;
        for _ in 0..len {
            r.item(T::visit)?;
        }
        Ok(())
    }

    fn missing() -> Option<Self> {
        Some(Vec::new())
    }

    fn describe(d: &mut Description) {
        d.word("vec");
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|(a, b)| a.same_encoding(b))
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
}

impl<T: Data> Data for Box<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        (**self).encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let value = Box::new(T::decode(r)?);
        r.allocated(1, size_of::<T>());
        Ok(value)
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        T::visit(r)
    }

    fn missing() -> Option<Self> {
        T::missing().map(Box::new)
    }

    fn describe(d: &mut Description) {
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        (**self).same_encoding(other)
    }
}

impl<T: Data> Data for Arc<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        (**self).encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let value = Arc::new(T::decode(r)?);
        r.allocated(1, ARC_HEADER + size_of::<T>());
        Ok(value)
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        T::visit(r)
    }

    fn missing() -> Option<Self> {
        T::missing().map(Arc::new)
    }

    fn describe(d: &mut Description) {
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        Arc::ptr_eq(self, other) || (**self).same_encoding(other)
    }
}

impl<K: Data + Ord, V: Data> Data for BTreeMap<K, V> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.varint(self.len() as u64);
        for (key, value) in self {
            key.encode(w)?;
            value.encode(w)?;
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let len = r.varint()?;
        let mut out = BTreeMap::new();
        for _ in 0..len {
            let (key, value) = r.item(|r| Ok((K::decode(r)?, V::decode(r)?)))?;
            out.insert(key, value);
        }
        r.tree::<K, V>(out.len());
        Ok(out)
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        let len = r.varint()?;
        for _ in 0..len {
            r.item(|r| {
                K::visit(r)?;
                V::visit(r)
            })?;
        }
        Ok(())
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
            && self
                .iter()
                .zip(other)
                .all(|(a, b)| a.0.same_encoding(b.0) && a.1.same_encoding(b.1))
    }
}

impl<T: Data + Ord> Data for BTreeSet<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.varint(self.len() as u64);
        for item in self {
            item.encode(w)?;
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let len = r.varint()?;
        let mut out = BTreeSet::new();
        for _ in 0..len {
            out.insert(r.item(T::decode)?);
        }
        r.tree::<T, ()>(out.len());
        Ok(out)
    }

    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        let len = r.varint()?;
        for _ in 0..len {
            r.item(T::visit)?;
        }
        Ok(())
    }

    fn missing() -> Option<Self> {
        Some(BTreeSet::new())
    }

    fn describe(d: &mut Description) {
        d.word("btreeset");
        d.of::<T>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|(a, b)| a.same_encoding(b))
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
