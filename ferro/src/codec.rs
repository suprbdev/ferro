use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::store::Store;

pub type Hash = [u8; 32];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ptr {
    pub hash: Hash,
    pub offset: u64,
}

pub struct Object {
    pub body: Vec<u8>,
    pub links: Vec<Ptr>,
}

impl Object {
    pub fn hash(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&(self.body.len() as u32).to_le_bytes());
        hasher.update(&(self.links.len() as u32).to_le_bytes());
        hasher.update(&self.body);
        for link in &self.links {
            hasher.update(&link.hash);
        }
        *hasher.finalize().as_bytes()
    }
}

pub trait Sink {
    fn put(&mut self, object: Object, hash: Hash) -> Result<Ptr>;
    fn remember(&mut self, _ptr: Ptr, _value: Arc<dyn Any + Send + Sync>, _weight: u32) {}
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

    fn store_id(&self) -> u64 {
        0
    }
}

pub trait Data: Clone + Send + Sync + 'static {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()>;
    fn decode(r: &mut Decoder<'_>) -> Result<Self>;
    fn missing() -> Option<Self> {
        None
    }
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
    value
        .or_else(T::missing)
        .ok_or(Error::MissingField(name))
}

pub struct Encoder<'a> {
    buf: Vec<u8>,
    links: Vec<Ptr>,
    sink: &'a mut dyn Sink,
    unsettled: bool,
}

fn push_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
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
        let head = self.buf.len();
        let links_before = self.links.len();
        f(self)?;
        let len = (self.buf.len() - head) as u64;
        let link_count = (self.links.len() - links_before) as u64;
        let mut prefix = Vec::with_capacity(4);
        push_varint(&mut prefix, (len << 1) | u64::from(link_count > 0));
        if link_count > 0 {
            push_varint(&mut prefix, link_count);
        }
        self.buf.splice(head..head, prefix);
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

pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    links: &'a [Ptr],
    cursor: usize,
    store: Option<&'a Store>,
    empty_items: u64,
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
        }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
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

    pub fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift == 63 && byte > 1 {
                return Err(Error::Decode("varint overflow".into()));
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                return Err(Error::Decode("varint overflow".into()));
            }
        }
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

    pub fn store(&self) -> Option<Store> {
        self.store.cloned()
    }

    pub fn fields(
        &mut self,
        mut f: impl FnMut(u16, &mut Decoder<'a>) -> Result<()>,
    ) -> Result<()> {
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
            };
            self.cursor = end;
            f(tag, &mut sub)?;
            self.empty_items = sub.empty_items;
        }
        Ok(())
    }
}

impl Data for Ptr {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.link(*self);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        r.link()
    }
}

impl Data for () {
    fn encode(&self, _w: &mut Encoder<'_>) -> Result<()> {
        Ok(())
    }

    fn decode(_r: &mut Decoder<'_>) -> Result<Self> {
        Ok(())
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
}

impl Data for u8 {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.u8(*self);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        r.u8()
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
}

impl Data for i128 {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.raw(&self.to_le_bytes());
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let bytes: [u8; 16] = r.take(16)?.try_into().expect("slice length is 16");
        Ok(i128::from_le_bytes(bytes))
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
        String::from_utf8(bytes.to_vec()).map_err(|e| Error::Decode(e.to_string()))
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

    fn missing() -> Option<Self> {
        Some(None)
    }
}

impl<T: Data> Data for Vec<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        w.varint(self.len() as u64);
        for item in self {
            item.encode(w)?;
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let len = r.varint()? as usize;
        let room = r.remaining() / size_of::<T>().max(1);
        let mut out = Vec::with_capacity(len.min(room));
        for _ in 0..len {
            out.push(r.item(T::decode)?);
        }
        Ok(out)
    }

    fn missing() -> Option<Self> {
        Some(Vec::new())
    }
}

impl<T: Data, const N: usize> Data for [T; N] {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        for item in self {
            item.encode(w)?;
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let mut out = Vec::with_capacity(N);
        for _ in 0..N {
            out.push(T::decode(r)?);
        }
        out.try_into()
            .map_err(|_| Error::Decode("array length mismatch".into()))
    }
}

impl<T: Data> Data for Box<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        (**self).encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        Ok(Box::new(T::decode(r)?))
    }

    fn missing() -> Option<Self> {
        T::missing().map(Box::new)
    }
}

impl<T: Data> Data for Arc<T> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        (**self).encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        Ok(Arc::new(T::decode(r)?))
    }

    fn missing() -> Option<Self> {
        T::missing().map(Arc::new)
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
        Ok(out)
    }

    fn missing() -> Option<Self> {
        Some(BTreeMap::new())
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
        Ok(out)
    }

    fn missing() -> Option<Self> {
        Some(BTreeSet::new())
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
        }
    };
}

tuple!(A 0);
tuple!(A 0, B 1);
tuple!(A 0, B 1, C 2);
tuple!(A 0, B 1, C 2, D 3);
tuple!(A 0, B 1, C 2, D 3, E 4);
tuple!(A 0, B 1, C 2, D 3, E 4, F 5);
