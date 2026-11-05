use std::any::{Any, TypeId, type_name};
use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Deref;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::shared::Erased;
use crate::store::Store;

pub const HASH_BYTES: usize = 20;

pub type Hash = [u8; HASH_BYTES];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ptr {
    pub hash: Hash,
    pub offset: u64,
}

pub struct Object {
    pub body: Vec<u8>,
    pub links: Vec<Ptr>,
}

const JOINED_BYTES: usize = 4096;
const SCRATCH_BYTES: usize = 4096;

thread_local! {
    static JOINED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static SCRATCH: RefCell<[Vec<u8>; 2]> = const { RefCell::new([Vec::new(), Vec::new()]) };
}

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
    pub fn hash(&self) -> Hash {
        hash_parts(&self.body, self.links.iter().map(|link| &link.hash))
    }
}

#[derive(Clone, Copy)]
pub struct Staged<'a> {
    pub raw: usize,
    pub payload: &'a [u8],
    pub packed: bool,
    pub links: &'a [Ptr],
}

impl Staged<'_> {
    pub fn body(&self) -> Result<Vec<u8>> {
        if !self.packed {
            return Ok(self.payload.to_vec());
        }
        lz4_flex::block::decompress(self.payload, self.raw)
            .map_err(|error| Error::Corrupt(format!("bad staged body: {error}")))
    }
}

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

pub trait Sink {
    fn put(&mut self, object: Object, hash: Hash) -> Result<Ptr>;
    fn packs(&self) -> bool {
        false
    }
    fn put_staged(&mut self, hash: Hash, staged: Staged<'_>) -> Result<Ptr> {
        let object = Object {
            body: staged.body()?,
            links: staged.links.to_vec(),
        };
        self.put(object, hash)
    }
    fn check_link(&self, _ptr: Ptr) -> Result<()> {
        Ok(())
    }
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
    fn store_id(&self) -> u64;
    fn store(&self) -> Option<Store> {
        None
    }
}

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

pub trait Data: Clone + Send + Sync + 'static {
    #[doc(hidden)]
    const LOCATED: bool = false;
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()>;
    fn decode(r: &mut Decoder<'_>) -> Result<Self>;
    fn visit(r: &mut Decoder<'_>) -> Result<()> {
        Self::decode(r).map(drop)
    }
    fn missing() -> Option<Self> {
        None
    }
    fn describe(d: &mut Description) {
        d.word("rust");
        d.word(type_name::<Self>());
    }
    fn same_encoding(&self, other: &Self) -> bool {
        encodings_match(self, other)
    }
    fn encode_slice(items: &[Self], w: &mut Encoder<'_>) -> Result<()> {
        for item in items {
            item.encode(w)?;
        }
        Ok(())
    }
    fn decode_vec(r: &mut Decoder<'_>, len: usize) -> Result<Vec<Self>> {
        let room = r.remaining() / size_of::<Self>().max(1);
        let mut out = Vec::with_capacity(len.min(room));
        for _ in 0..len {
            out.push(r.item(Self::decode)?);
        }
        Ok(out)
    }
}

#[derive(Default)]
pub struct Description {
    bytes: Vec<u8>,
    open: Vec<TypeId>,
}

const FINGERPRINT_CONTEXT: &str = "ferro 2026-11-04 index fingerprint";

impl Description {
    pub fn new() -> Self {
        Description::default()
    }

    pub fn word(&mut self, word: &str) {
        self.bytes.push(0);
        push_varint(&mut self.bytes, word.len() as u64);
        self.bytes.extend_from_slice(word.as_bytes());
    }

    pub fn count(&mut self, count: u64) {
        self.bytes.push(1);
        push_varint(&mut self.bytes, count);
    }

    pub fn value<D: Data>(&mut self, value: &D) {
        self.bytes.push(2);
        self.bytes.extend_from_slice(&hash_of(value));
    }

    pub fn of<D: Data>(&mut self) {
        D::describe(self);
    }

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

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn fingerprint(&self) -> u64 {
        let mut hasher = blake3::Hasher::new_derive_key(FINGERPRINT_CONTEXT);
        hasher.update(&self.bytes);
        let hash = hasher.finalize();
        let mut head = [0u8; 8];
        head.copy_from_slice(&hash.as_bytes()[..8]);
        u64::from_le_bytes(head)
    }
}

pub fn try_hash_of<D: Data>(value: &D) -> Result<Hash> {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder)?;
    Ok(encoder.finish()?.hash)
}

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

pub fn required<T: Data>(value: Option<T>, name: &'static str) -> Result<T> {
    value.or_else(T::missing).ok_or(Error::MissingField(name))
}

pub struct Encoder<'a> {
    buf: Vec<u8>,
    links: Vec<Ptr>,
    sink: &'a mut dyn Sink,
    unsettled: bool,
}

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
    pub fn new(sink: &'a mut dyn Sink) -> Self {
        Encoder {
            buf: Vec::new(),
            links: Vec::new(),
            sink,
            unsettled: false,
        }
    }

    pub fn with_capacity(sink: &'a mut dyn Sink, bytes: usize, links: usize) -> Self {
        Encoder {
            buf: Vec::with_capacity(bytes),
            links: Vec::with_capacity(links),
            sink,
            unsettled: false,
        }
    }

    pub fn reusing(sink: &'a mut dyn Sink, mut buf: Vec<u8>) -> Self {
        buf.clear();
        Encoder {
            buf,
            links: Vec::new(),
            sink,
            unsettled: false,
        }
    }

    pub fn into_buf(self) -> Vec<u8> {
        self.buf
    }

    pub fn mark_unsettled(&mut self) {
        self.unsettled = true;
    }

    pub fn is_unsettled(&self) -> bool {
        self.unsettled
    }

    pub fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    pub fn varint(&mut self, value: u64) {
        push_varint(&mut self.buf, value);
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn link(&mut self, ptr: Ptr) {
        self.links.push(ptr);
    }

    pub fn sink(&mut self) -> &mut dyn Sink {
        &mut *self.sink
    }

    pub fn store_id(&self) -> u64 {
        self.sink.store_id()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn links(&self) -> &[Ptr] {
        &self.links
    }

    pub fn mark(&self) -> (usize, usize) {
        (self.buf.len(), self.links.len())
    }

    pub fn since(&self, mark: (usize, usize)) -> usize {
        self.buf.len() - mark.0
    }

    pub fn rewind(&mut self, mark: (usize, usize)) {
        self.buf.truncate(mark.0);
        self.links.truncate(mark.1);
    }

    pub fn child(&mut self, f: impl FnOnce(&mut Encoder<'_>) -> Result<()>) -> Result<Ptr> {
        let mut encoder = Encoder::new(&mut *self.sink);
        f(&mut encoder)?;
        self.unsettled |= encoder.unsettled;
        encoder.finish()
    }

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

    pub fn into_object(self) -> Object {
        Object {
            body: self.buf,
            links: self.links,
        }
    }

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
pub(crate) const ARC_HEADER: usize = 2 * size_of::<usize>();
const TREE_HEADER: usize = 16;
const TREE_FILL: usize = 6;
const TREE_CAPACITY: usize = 11;

fn allocation(bytes: usize) -> usize {
    bytes.saturating_add(8).next_multiple_of(16).max(32)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) count: u64,
    pub(crate) level: u8,
}

#[derive(Clone)]
pub(crate) struct Kind {
    pub(crate) id: TypeId,
    pub(crate) name: &'static str,
    pub(crate) check: fn(&Kind, &mut Decoder<'_>) -> Result<Shape>,
    pub(crate) held: fn(&Kind, &Store, Ptr) -> Result<Shape>,
    pub(crate) within: fn(&Kind, &Kind) -> bool,
    pub(crate) count: Option<u64>,
    pub(crate) below: Option<u8>,
    pub(crate) range: Option<Arc<dyn Any + Send + Sync>>,
}

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
        }
    }

    pub(crate) fn fits(&self, shape: Shape) -> bool {
        self.count.is_none_or(|count| count == shape.count)
            && self.below.is_none_or(|below| shape.level < below)
    }

    pub(crate) fn implies(&self, want: &Kind) -> bool {
        self.id == want.id
            && want.count.is_none_or(|count| self.count == Some(count))
            && want
                .below
                .is_none_or(|below| self.below.is_some_and(|own| own <= below))
            && (self.within)(self, want)
    }

    pub(crate) fn check(&self, body: &[u8], links: &[Ptr]) -> Result<(Shape, Found)> {
        let found = RefCell::new(Vec::new());
        let mut decoder = Decoder::new(body, links, None);
        decoder.found = Some(&found);
        let shape = (self.check)(self, &mut decoder)?;
        Ok((shape, found.into_inner()))
    }
}

#[derive(Default)]
pub(crate) struct Checked {
    seen: HashMap<(Hash, TypeId), (Shape, Vec<Kind>)>,
}

impl Checked {
    pub(crate) fn get(&self, hash: &Hash, kind: &Kind) -> Option<Shape> {
        let (shape, kinds) = self.seen.get(&(*hash, kind.id))?;
        kinds
            .iter()
            .any(|seen| (seen.within)(seen, kind))
            .then_some(*shape)
    }

    pub(crate) fn insert(&mut self, hash: Hash, kind: &Kind, shape: Shape) {
        let entry = self.seen.entry((hash, kind.id));
        entry
            .or_insert_with(|| (shape, Vec::with_capacity(1)))
            .1
            .push(kind.clone());
    }
}

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

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn lone_varint(&self) -> bool {
        let rest = &self.buf[self.pos..];
        let end = rest.iter().position(|byte| byte & 0x80 == 0);
        end.is_some() && end == rest.len().checked_sub(1)
    }

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

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

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

    pub fn link(&mut self) -> Result<Ptr> {
        let ptr = self
            .links
            .get(self.cursor)
            .copied()
            .ok_or_else(|| Error::Decode("link table exhausted".into()))?;
        self.cursor += 1;
        Ok(ptr)
    }

    pub(crate) fn typed_link(&mut self, kind: impl FnOnce() -> Kind) -> Result<Ptr> {
        let index = self.base + self.cursor;
        let ptr = self.link()?;
        if let Some(found) = self.found {
            found.borrow_mut().push((index, kind()));
        }
        Ok(ptr)
    }

    pub fn store(&self) -> Option<Store> {
        self.store.map(Store::share)
    }

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

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(Arc<[u8]>);

impl Bytes {
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
