use std::any::{Any, TypeId, type_name};
use std::borrow::Borrow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::convert::Infallible;
use std::fmt;
use std::hash;
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, needs_drop};
use std::ops::{Bound, ControlFlow, Deref, Range, RangeBounds};
use std::ptr::NonNull;
use std::sync::atomic::AtomicPtr;
use std::sync::{Arc, OnceLock};
use std::vec;

use crate::codec::{
    Data, Decoder, Description, Encoder, Hash, HashSink, Kind, Moved, Moves, Object, Ptr, Shape,
    Sink, Staged, try_hash_of,
};
use crate::epoch::{self, Guard, HAZARDS, Hazards, ROOT, STORE};
use crate::error::{Error, Result};
use crate::prefix::Prefix as KeyPrefix;
use crate::shared::{Erased, Shared, Weak};
use crate::store::{Lent, MAX_RATIO, Store, compressed, heap_weight};
use xxhash_rust::xxh3::xxh3_64;

pub(crate) const SPILL_BYTES: usize = 512;
const LEVEL_BITS: u32 = 3;
const LEVELS: u8 = (u64::BITS / LEVEL_BITS) as u8 + 1;
const PIN_DEPTH: usize = 3;
const ARC_HEADER: usize = 16;
const ALLOCATION_BYTES: usize = 16;
const ITEM_BYTES: usize = 32;
const PATH_FRAMES: usize = 8;
const PARALLEL_KEYS: u64 = 4096;
const PARALLEL_NODES: usize = 256;
const LINE_BYTES: usize = 64;
const SPAN_LINES: i32 = 15;
const LINEAR_KEYS: usize = 16;
const LINEAR_KEY_BYTES: usize = 16;
const PARALLEL_ITEMS: usize = 1 << 14;
const MAX_WORKERS: usize = 16;
const TEARDOWN_KEYS: u64 = 4096;
const TEARDOWN_AHEAD: usize = 16;
const TEARDOWN_PATHS: usize = 4;
const HINTED_LEVEL: u8 = 3;
const LEAF_CAPACITY: usize = 8;
const FRONT: usize = 2;
const SPAN: usize = (HAZARDS - FRONT) / 2;
const BACK: usize = FRONT + SPAN;
const LEAF: usize = 2;
const PARENT: usize = 3;

pub(crate) fn fatal<T>(result: Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("ferro: {error}"),
    }
}

thread_local! {
    static KEY_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

pub trait Summary<K, V>: Data {
    fn of(key: &K, value: &V) -> Self;
    fn combine(&self, other: &Self) -> Self;
}

impl<K, V> Summary<K, V> for () {
    #[inline]
    fn of(_key: &K, _value: &V) {}

    #[inline]
    fn combine(&self, _other: &Self) {}
}

#[inline]
fn fold<K, V, S: Summary<K, V>>(total: Option<S>, next: &S) -> Option<S> {
    Some(match total {
        Some(total) => total.combine(next),
        None => next.clone(),
    })
}

fn level_of<K: Data>(key: &K) -> Result<u8> {
    let mut sink = HashSink;
    let mut encoder = Encoder::reusing(&mut sink, KEY_SCRATCH.take());
    let encoded = key.encode(&mut encoder);
    let buf = encoder.into_buf();
    let word = xxh3_64(&buf);
    KEY_SCRATCH.set(buf);
    encoded?;
    Ok((word.trailing_zeros() / LEVEL_BITS) as u8)
}

const fn by_copy<V>() -> bool {
    !needs_drop::<V>() && size_of::<V>() <= 2 * size_of::<usize>()
}

pub struct Val<V>(Held<V>);

enum Held<V> {
    Copied(ManuallyDrop<V>),
    Column(Arc<[V]>, u32),
    Single(Arc<V>),
}

impl<V: Clone> Val<V> {
    #[inline]
    fn of_column(column: &Arc<[V]>, index: usize) -> Self {
        if by_copy::<V>() {
            Val(Held::Copied(ManuallyDrop::new(column[index].clone())))
        } else {
            Val(Held::Column(column.clone(), index as u32))
        }
    }

    #[inline]
    fn of_single(value: &Arc<V>) -> Self {
        if by_copy::<V>() {
            Val(Held::Copied(ManuallyDrop::new((**value).clone())))
        } else {
            Val(Held::Single(value.clone()))
        }
    }

    pub fn new(value: V) -> Self {
        if by_copy::<V>() {
            Val(Held::Copied(ManuallyDrop::new(value)))
        } else {
            Val(Held::Single(Arc::new(value)))
        }
    }

    pub fn into_owned(self) -> V {
        match self.0 {
            Held::Copied(value) => ManuallyDrop::into_inner(value),
            Held::Column(column, index) => column[index as usize].clone(),
            Held::Single(value) => Arc::unwrap_or_clone(value),
        }
    }

    pub fn into_arc(self) -> Arc<V> {
        match self.0 {
            Held::Single(value) => value,
            other => Arc::new(Val(other).into_owned()),
        }
    }
}

impl<V> Val<V> {
    fn shares(&self, other: &Val<V>) -> bool {
        match (&self.0, &other.0) {
            (Held::Single(a), Held::Single(b)) => Arc::ptr_eq(a, b),
            (Held::Column(a, i), Held::Column(b, j)) => i == j && Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

pub(crate) fn same_value<V: Data>(a: Option<&Val<V>>, b: Option<&Val<V>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.shares(b) || V::same_encoding(a, b),
        _ => false,
    }
}

impl<V> Deref for Val<V> {
    type Target = V;

    #[inline]
    fn deref(&self) -> &V {
        match &self.0 {
            Held::Copied(value) => value,
            Held::Column(column, index) => &column[*index as usize],
            Held::Single(value) => value,
        }
    }
}

impl<V: Clone> Clone for Val<V> {
    #[inline]
    fn clone(&self) -> Self {
        Val(match &self.0 {
            Held::Copied(value) => Held::Copied(value.clone()),
            Held::Column(column, index) => Held::Column(column.clone(), *index),
            Held::Single(value) => Held::Single(value.clone()),
        })
    }
}

impl<V> From<Arc<V>> for Val<V> {
    fn from(value: Arc<V>) -> Self {
        Val(Held::Single(value))
    }
}

impl<V> AsRef<V> for Val<V> {
    fn as_ref(&self) -> &V {
        self
    }
}

impl<V> Borrow<V> for Val<V> {
    fn borrow(&self) -> &V {
        self
    }
}

impl<V: fmt::Debug> fmt::Debug for Val<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<V: fmt::Display> fmt::Display for Val<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<V: PartialEq> PartialEq for Val<V> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<V: Eq> Eq for Val<V> {}

impl<V: PartialOrd> PartialOrd for Val<V> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        (**self).partial_cmp(&**other)
    }
}

impl<V: Ord> Ord for Val<V> {
    fn cmp(&self, other: &Self) -> Ordering {
        (**self).cmp(&**other)
    }
}

impl<V: hash::Hash> hash::Hash for Val<V> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

enum Slot<V, S> {
    Packed(Arc<[V]>, u32),
    Fresh(Arc<V>),
    Spilled(Ptr, S),
}

impl<V, S> Slot<V, S> {
    #[inline]
    fn held(&self) -> Option<&V> {
        match self {
            Slot::Packed(column, index) => Some(&column[*index as usize]),
            Slot::Fresh(value) => Some(value),
            Slot::Spilled(..) => None,
        }
    }

    fn shares(&self, other: &Self) -> bool {
        match (self, other) {
            (Slot::Packed(a, i), Slot::Packed(b, j)) => i == j && Arc::ptr_eq(a, b),
            (Slot::Fresh(a), Slot::Fresh(b)) => Arc::ptr_eq(a, b),
            (Slot::Spilled(a, _), Slot::Spilled(b, _)) => a.hash == b.hash,
            _ => false,
        }
    }

    #[inline]
    fn summary<K>(&self, key: &K) -> S
    where
        S: Summary<K, V>,
    {
        match self {
            Slot::Spilled(_, summary) => summary.clone(),
            held => S::of(
                key,
                held.held()
                    .expect("a slot that is not spilled holds its value"),
            ),
        }
    }
}

impl<V, S: Clone> Clone for Slot<V, S> {
    fn clone(&self) -> Self {
        match self {
            Slot::Packed(column, index) => Slot::Packed(column.clone(), *index),
            Slot::Fresh(value) => Slot::Fresh(value.clone()),
            Slot::Spilled(ptr, summary) => Slot::Spilled(*ptr, summary.clone()),
        }
    }
}

enum Cells<V, S> {
    Column(Arc<[V]>),
    Slots(Vec<Slot<V, S>>),
}

impl<V: Clone, S: Clone> Cells<V, S> {
    fn settled(&self, spills: Vec<(usize, Slot<V, S>)>, decoded: Vec<(usize, V)>) -> Self {
        let width = match self {
            Cells::Column(_) if spills.is_empty() && decoded.is_empty() => return self.clone(),
            Cells::Column(column) => column.len(),
            Cells::Slots(slots) => slots.len(),
        };
        let mut spills = spills.into_iter().peekable();
        let mut decoded = decoded.into_iter().peekable();
        let mut column = Vec::with_capacity(width);
        let mut spilled = Vec::new();
        for index in 0..width {
            if let Some((_, value)) = decoded.next_if(|(at, _)| *at == index) {
                column.push(value);
                continue;
            }
            match (spills.next_if(|(at, _)| *at == index), self) {
                (Some(spill), _) => spilled.push(spill),
                (None, Cells::Column(values)) => column.push(values[index].clone()),
                (None, Cells::Slots(slots)) => match slots[index].held() {
                    Some(value) => column.push(value.clone()),
                    None => spilled.push((index, slots[index].clone())),
                },
            }
        }
        Cells::spliced(column.into(), spilled)
    }
}

impl<V, S: Clone> Clone for Cells<V, S> {
    fn clone(&self) -> Self {
        match self {
            Cells::Column(column) => Cells::Column(column.clone()),
            Cells::Slots(slots) => Cells::Slots(slots.clone()),
        }
    }
}

impl<V, S> Cells<V, S> {
    fn spliced(column: Arc<[V]>, spilled: Vec<(usize, Slot<V, S>)>) -> Self {
        if spilled.is_empty() {
            return Cells::Column(column);
        }
        let width = column.len() + spilled.len();
        let mut slots = Vec::with_capacity(width);
        let mut spilled = spilled.into_iter().peekable();
        let mut held = 0;
        for index in 0..width {
            match spilled.next_if(|(at, _)| *at == index) {
                Some((_, slot)) => slots.push(slot),
                None => {
                    slots.push(Slot::Packed(column.clone(), held));
                    held += 1;
                }
            }
        }
        Cells::Slots(slots)
    }

    fn slots(&mut self) -> &mut Vec<Slot<V, S>> {
        if let Cells::Column(column) = self {
            let packed = (0..column.len() as u32).map(|index| Slot::Packed(column.clone(), index));
            *self = Cells::Slots(packed.collect());
        }
        match self {
            Cells::Slots(slots) => slots,
            Cells::Column(_) => unreachable!(),
        }
    }

    fn into_slots(mut self) -> Vec<Slot<V, S>> {
        std::mem::take(self.slots())
    }
}

enum Memo<K, V, S> {
    Pinned(Shared<Node<K, V, S>>),
    Weak(Weak<Node<K, V, S>>, Hint),
}

impl<K, V, S> Clone for Memo<K, V, S> {
    fn clone(&self) -> Self {
        match self {
            Memo::Pinned(node) => Memo::Pinned(node.clone()),
            Memo::Weak(node, hint) => Memo::Weak(node.clone(), *hint),
        }
    }
}

#[derive(Clone, Copy)]
struct Hint {
    keys: i32,
    rest: i32,
}

impl Hint {
    const NONE: Hint = Hint { keys: 0, rest: 0 };

    fn of<K, V, S>(node: &Shared<Node<K, V, S>>) -> Hint {
        Hint::spans(node, false)
    }

    fn dirty<K, V, S>(node: &Node<K, V, S>) -> Hint {
        if node.level >= HINTED_LEVEL {
            return Hint::NONE;
        }
        Hint::spans(node, true)
    }

    fn spans<K, V, S>(node: &Node<K, V, S>, dirty: bool) -> Hint {
        let base = std::ptr::from_ref(node).addr();
        let span = |start: usize, bytes: usize| -> i32 {
            let Ok(offset) = i32::try_from(start.wrapping_sub(base) as isize) else {
                return 0;
            };
            let offset = offset & !SPAN_LINES;
            let first = base.wrapping_add_signed(offset as isize) / LINE_BYTES;
            let last = (start + bytes.max(1) - 1) / LINE_BYTES;
            let lines = (last + 1).saturating_sub(first).min(SPAN_LINES as usize);
            offset | lines as i32
        };
        let rest = match &node.cells {
            _ if !node.children.is_empty() => span(
                node.children.as_ptr().addr(),
                node.children.len() * size_of::<Link<K, V, S>>(),
            ),
            Cells::Column(column) => span(
                column.as_ptr().addr().wrapping_sub(ARC_HEADER),
                ARC_HEADER + column.len() * size_of::<V>(),
            ),
            Cells::Slots(slots) if dirty => {
                span(slots.as_ptr().addr(), slots.len() * size_of::<Slot<V, S>>())
            }
            Cells::Slots(_) => 0,
        };
        Hint {
            keys: span(node.keys.as_ptr().addr(), node.keys.len() * size_of::<K>()),
            rest,
        }
    }

    fn renew<K, V, S>(&mut self, node: &Node<K, V, S>, before: [usize; 3]) {
        if node.buffers() != before {
            *self = Hint::dirty(node);
        }
    }

    #[inline]
    fn prefetch<T>(self, node: &Weak<T>) {
        self.prefetch_at(node.as_ptr().addr());
    }

    #[inline]
    fn prefetch_at(self, base: usize) {
        for span in [self.keys, self.rest] {
            let start = base.wrapping_add_signed((span & !SPAN_LINES) as isize);
            for line in 0..(span & SPAN_LINES) as usize {
                prefetch(start.wrapping_add(line * LINE_BYTES));
            }
        }
    }
}

#[inline(always)]
fn prefetch(address: usize) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch::<_MM_HINT_T0>(std::ptr::without_provenance(address));
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = address;
}

fn prefetch_items<T>(items: &[T]) {
    let start = items.as_ptr().addr();
    for line in (0..size_of_val(items)).step_by(LINE_BYTES) {
        prefetch(start + line);
    }
}

enum Link<K, V, S> {
    Empty,
    Stored {
        ptr: Ptr,
        count: u64,
        ceiling: u8,
        summary: S,
        memo: OnceLock<Memo<K, V, S>>,
    },
    Mem(Shared<Node<K, V, S>>, Hint),
}

impl<K, V, S: Clone> Clone for Link<K, V, S> {
    fn clone(&self) -> Self {
        match self {
            Link::Empty => Link::Empty,
            Link::Stored {
                ptr,
                count,
                ceiling,
                summary,
                ..
            } => Link::under(*ptr, *count, *ceiling, summary.clone()),
            Link::Mem(node, hint) => Link::Mem(node.clone(), *hint),
        }
    }
}

impl<K, V, S> Link<K, V, S> {
    fn mem(node: Shared<Node<K, V, S>>) -> Self {
        let hint = Hint::dirty(&node);
        Link::Mem(node, hint)
    }

    fn stored(ptr: Ptr, count: u64, summary: S) -> Self {
        Link::under(ptr, count, LEVELS, summary)
    }

    fn under(ptr: Ptr, count: u64, ceiling: u8, summary: S) -> Self {
        Link::Stored {
            ptr,
            count,
            ceiling,
            summary,
            memo: OnceLock::new(),
        }
    }

    fn with_memo(&self) -> Self
    where
        S: Clone,
    {
        match self {
            Link::Stored {
                ptr,
                count,
                ceiling,
                summary,
                memo,
            } => Link::Stored {
                ptr: *ptr,
                count: *count,
                ceiling: *ceiling,
                summary: summary.clone(),
                memo: memo.clone(),
            },
            other => other.clone(),
        }
    }

    fn count(&self) -> u64 {
        match self {
            Link::Empty => 0,
            Link::Stored { count, .. } => *count,
            Link::Mem(node, _) => node.count,
        }
    }

    fn summary(&self) -> Option<S>
    where
        S: Summary<K, V>,
    {
        match self {
            Link::Empty => None,
            Link::Stored { summary, .. } => Some(summary.clone()),
            Link::Mem(node, _) => node.summary(),
        }
    }

    fn take(&mut self) -> Self {
        std::mem::replace(self, Link::Empty)
    }

    fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Link::Empty, Link::Empty) => true,
            (Link::Stored { ptr: a, .. }, Link::Stored { ptr: b, .. }) => a.hash == b.hash,
            (Link::Mem(a, _), Link::Mem(b, _)) => Shared::ptr_eq(a, b),
            _ => false,
        }
    }
}

struct Prefix {
    bytes: Box<[u8]>,
    links: Box<[Ptr]>,
}

impl Prefix {
    fn of(object: &Object, at: (usize, usize)) -> Arc<Prefix> {
        Arc::new(Prefix {
            bytes: object.body[..at.0].into(),
            links: object.links[..at.1].into(),
        })
    }

    fn heap(&self) -> usize {
        ARC_HEADER + 2 * ALLOCATION_BYTES + self.bytes.len() + size_of_val(&*self.links)
    }
}

#[repr(C)]
struct Node<K, V, S> {
    keys: Vec<K>,
    children: Vec<Link<K, V, S>>,
    cells: Cells<V, S>,
    count: u64,
    level: u8,
    origin: Option<Ptr>,
    prefix: Option<Arc<Prefix>>,
}

impl<K: Clone, V, S: Clone> Clone for Node<K, V, S> {
    fn clone(&self) -> Self {
        Node {
            level: self.level,
            keys: self.keys.clone(),
            cells: self.cells.clone(),
            children: self.children.clone(),
            count: self.count,
            origin: self.origin,
            prefix: self.prefix.clone(),
        }
    }
}

impl<K, V, S> Node<K, V, S> {
    fn search<Q>(&self, key: &Q) -> std::result::Result<usize, usize>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let small = !needs_drop::<K>() && size_of::<K>() <= LINEAR_KEY_BYTES;
        if small && self.keys.len() <= LINEAR_KEYS {
            let position = self
                .keys
                .iter()
                .filter(|probe| (*probe).borrow() < key)
                .count();
            return match self.keys.get(position) {
                Some(probe) if probe.borrow() == key => Ok(position),
                _ => Err(position),
            };
        }
        self.keys.binary_search_by(|probe| probe.borrow().cmp(key))
    }

    fn buffers(&self) -> [usize; 3] {
        let slots = match &self.cells {
            Cells::Slots(slots) => slots.as_ptr().addr(),
            Cells::Column(_) => 0,
        };
        [
            self.keys.as_ptr().addr(),
            self.children.as_ptr().addr(),
            slots,
        ]
    }

    fn child(&self, position: usize) -> &Link<K, V, S> {
        self.children.get(position).unwrap_or(&Link::Empty)
    }

    fn children_mut(&mut self) -> &mut Vec<Link<K, V, S>> {
        if self.children.is_empty() {
            self.children
                .resize_with(self.keys.len() + 1, || Link::Empty);
        }
        &mut self.children
    }

    #[inline]
    fn slot(&self, index: usize) -> Slot<V, S>
    where
        S: Clone,
    {
        match &self.cells {
            Cells::Column(column) => Slot::Packed(column.clone(), index as u32),
            Cells::Slots(slots) => slots[index].clone(),
        }
    }

    #[inline]
    fn entry_summary(&self, index: usize) -> S
    where
        S: Summary<K, V>,
    {
        match &self.cells {
            Cells::Column(column) => S::of(&self.keys[index], &column[index]),
            Cells::Slots(slots) => slots[index].summary(&self.keys[index]),
        }
    }

    #[inline]
    fn is_fresh(&self, index: usize) -> bool {
        match &self.cells {
            Cells::Column(_) => false,
            Cells::Slots(slots) => matches!(slots[index], Slot::Fresh(_)),
        }
    }

    fn recount(&mut self) {
        self.count = self.keys.len() as u64 + self.children.iter().map(Link::count).sum::<u64>();
    }

    fn summary(&self) -> Option<S>
    where
        S: Summary<K, V>,
    {
        let mut total = self.child(0).summary();
        for index in 0..self.keys.len() {
            total = fold(total, &self.entry_summary(index));
            if let Some(next) = self.child(index + 1).summary() {
                total = fold(total, &next);
            }
        }
        total
    }

    fn fixed_bytes(&self) -> usize {
        let inner = |owns_heap: bool, bytes: usize| if owns_heap { bytes } else { 0 };
        let values = match &self.cells {
            Cells::Column(column) => {
                let each = inner(needs_drop::<V>(), size_of::<V>() + ITEM_BYTES);
                ARC_HEADER + ALLOCATION_BYTES + column.len() * (size_of::<V>() + each)
            }
            Cells::Slots(slots) => {
                let held = slots.iter().filter(|slot| slot.held().is_some());
                slots.capacity() * size_of::<Slot<V, S>>()
                    + held.count() * (ARC_HEADER + ALLOCATION_BYTES + size_of::<V>() + ITEM_BYTES)
            }
        };
        size_of::<Self>()
            + self.keys.capacity() * (size_of::<K>() + inner(needs_drop::<K>(), ITEM_BYTES))
            + self.children.capacity() * size_of::<Link<K, V, S>>()
            + values
            + self.prefix.as_ref().map_or(0, |prefix| prefix.heap())
    }

    fn weight(&self, body_bytes: usize, _heap: usize) -> u32 {
        heap_weight(self.fixed_bytes(), body_bytes)
    }
}

impl<K, V: Data, S> Node<K, V, S> {
    #[inline]
    fn value(&self, store: Option<&Store>, index: usize) -> Result<Val<V>> {
        match &self.cells {
            Cells::Column(column) => Ok(Val::of_column(column, index)),
            Cells::Slots(slots) => read_slot(store, &slots[index]),
        }
    }

    #[inline]
    fn read<R>(&self, store: Option<&Store>, index: usize, f: impl FnOnce(&V) -> R) -> Result<R> {
        match &self.cells {
            Cells::Column(column) => Ok(f(&column[index])),
            Cells::Slots(slots) => match &slots[index] {
                Slot::Spilled(ptr, _) => Ok(f(&*store.ok_or(Error::Detached)?.load::<V>(*ptr)?)),
                held => held.held().map(f).ok_or(Error::Detached),
            },
        }
    }
}

impl<K: Data, V: Data, S: Data> Node<K, V, S> {
    fn decode(decoder: &mut Decoder<'_>, origin: Ptr) -> Result<Self> {
        let level = decoder.u8()?;
        let n = decoder.varint()? as usize;
        if n > decoder.remaining() {
            return Err(Error::Decode(format!("node claims {n} keys")));
        }
        let mut keys = Vec::with_capacity(n);
        for _ in 0..n {
            keys.push(K::decode(decoder)?);
        }
        let mut column = Vec::with_capacity(keys.len());
        let mut spilled = Vec::new();
        for index in 0..n {
            match decoder.u8()? {
                0 => column.push(V::decode(decoder)?),
                1 => spilled.push((index, Slot::Spilled(decoder.link()?, S::decode(decoder)?))),
                other => return Err(Error::Decode(format!("invalid slot tag {other}"))),
            }
        }
        let cells = Cells::spliced(column.into(), spilled);
        let mut children = Vec::new();
        let mut total = n as u64;
        for position in 0..=n {
            let count = decoder.varint()?;
            if count != 0 {
                children.reserve_exact(n + 1 - children.len());
                children.resize_with(position, || Link::Empty);
                let summary = S::decode(decoder)?;
                children.push(Link::under(decoder.link()?, count, level, summary));
                total = total
                    .checked_add(count)
                    .ok_or_else(|| Error::Decode("node counts overflow".into()))?;
            }
        }
        if !children.is_empty() {
            children.resize_with(n + 1, || Link::Empty);
        }
        Ok(Node {
            level,
            keys,
            cells,
            children,
            count: total,
            origin: Some(origin),
            prefix: None,
        })
    }
}

type Fetched<K, V, S> = (Shared<Node<K, V, S>>, bool);

fn fetch_node<K: Data, V: Data, S: Data>(store: &Store, ptr: Ptr) -> Result<Fetched<K, V, S>> {
    let decode = |decoder: &mut Decoder<'_>| {
        let node = Node::decode(decoder, ptr)?;
        let written = store.written();
        if node.count > written.saturating_mul(MAX_RATIO as u64) {
            return Err(Error::Decode(format!(
                "a map node claims {} entries, more than a log of {written} bytes holds",
                node.count
            )));
        }
        Ok(node)
    };
    let (node, cached) = store.fetch_node(ptr, decode, Node::weight)?;
    if (K::LOCATED || V::LOCATED) && node.origin != Some(ptr) {
        return Ok((Shared::new(store.decode_at(ptr, decode)?), false));
    }
    Ok((node, cached))
}

fn admit<K, V, S>(node: &Node<K, V, S>, count: u64, ceiling: u8) -> Result<()> {
    if node.level >= ceiling {
        return Err(Error::Decode(format!(
            "a map node at level {} is not below level {ceiling}",
            node.level
        )));
    }
    if node.count != count {
        return Err(Error::Decode(format!(
            "a map link claims {count} entries for a node that holds {}",
            node.count
        )));
    }
    Ok(())
}

type Span<K> = (Option<K>, Option<K>);

fn node_kind<K: Data + Ord, V: Data, S: Data>(
    count: u64,
    below: Option<u8>,
    span: Option<Span<K>>,
) -> Kind {
    Kind {
        id: TypeId::of::<Node<K, V, S>>(),
        name: type_name::<Node<K, V, S>>(),
        check: check_node::<K, V, S>,
        held: held_node::<K, V, S>,
        within: span_within::<K>,
        count: Some(count),
        below,
        range: span.map(|span| Arc::new(span) as Arc<dyn Any + Send + Sync>),
    }
}

fn span_of<K: 'static>(kind: &Kind) -> Option<&Span<K>> {
    kind.range
        .as_ref()
        .and_then(|range| range.downcast_ref::<Span<K>>())
}

fn span_within<K: Ord + 'static>(inner: &Kind, outer: &Kind) -> bool {
    let Some((low, high)) = span_of::<K>(outer) else {
        return true;
    };
    let (inner_low, inner_high) = match span_of::<K>(inner) {
        Some((low, high)) => (low.as_ref(), high.as_ref()),
        None => (None, None),
    };
    let above = match (low, inner_low) {
        (None, _) => true,
        (Some(low), Some(inner)) => low <= inner,
        (Some(_), None) => false,
    };
    let under = match (high, inner_high) {
        (None, _) => true,
        (Some(high), Some(inner)) => inner <= high,
        (Some(_), None) => false,
    };
    above && under
}

fn outside<K: Ord>(span: Option<&Span<K>>, first: &K, last: &K) -> bool {
    span.is_some_and(|(low, high)| {
        low.as_ref().is_some_and(|low| first <= low)
            || high.as_ref().is_some_and(|high| last >= high)
    })
}

fn check_node<K: Data + Ord, V: Data, S: Data>(kind: &Kind, r: &mut Decoder<'_>) -> Result<Shape> {
    let level = r.u8()?;
    let n = r.varint()?;
    if n == 0 || n > r.remaining() as u64 {
        return Err(Error::Decode(format!("node claims {n} keys")));
    }
    let mut keys: Vec<K> = Vec::new();
    for _ in 0..n {
        let key = K::decode(r)?;
        if level_of(&key)? != level {
            return Err(Error::Decode(
                "a key sits at a level it does not hash to".into(),
            ));
        }
        if keys.last().is_some_and(|previous| *previous >= key) {
            return Err(Error::Decode("node keys are out of order".into()));
        }
        keys.push(key);
    }
    let span = span_of::<K>(kind);
    if outside(span, &keys[0], &keys[keys.len() - 1]) {
        return Err(Error::Decode(
            "node keys fall outside the range their parent gives them".into(),
        ));
    }
    for _ in 0..n {
        match r.u8()? {
            0 => V::visit(r)?,
            1 => {
                r.typed_link(Kind::value::<V>)?;
                S::visit(r)?;
            }
            other => return Err(Error::Decode(format!("invalid slot tag {other}"))),
        }
    }
    let mut count = n;
    for position in 0..keys.len() + 1 {
        let below = r.varint()?;
        if below != 0 {
            S::visit(r)?;
            r.typed_link(|| {
                let low = match position {
                    0 => span.and_then(|(low, _)| low.clone()),
                    _ => Some(keys[position - 1].clone()),
                };
                let high = match keys.get(position) {
                    Some(key) => Some(key.clone()),
                    None => span.and_then(|(_, high)| high.clone()),
                };
                node_kind::<K, V, S>(below, Some(level), Some((low, high)))
            })?;
            count = count
                .checked_add(below)
                .ok_or_else(|| Error::Decode("node counts overflow".into()))?;
        }
    }
    Ok(Shape { count, level })
}

fn extreme<K: Data, V: Data, S: Data>(
    store: &Store,
    top: &Shared<Node<K, V, S>>,
    last: bool,
) -> Result<K> {
    let mut node = top.clone();
    loop {
        let child = node.child(if last { node.keys.len() } else { 0 });
        match load(Some(store), child)? {
            Some(next) => node = next,
            None => break,
        }
    }
    let key = if last {
        node.keys.last()
    } else {
        node.keys.first()
    };
    key.cloned()
        .ok_or_else(|| Error::Corrupt("a stored map node holds no keys".into()))
}

fn held_node<K: Data + Ord, V: Data, S: Data>(
    kind: &Kind,
    store: &Store,
    ptr: Ptr,
) -> Result<Shape> {
    let (node, _) = fetch_node::<K, V, S>(store, ptr)?;
    let span = span_of::<K>(kind);
    if span.is_some() {
        let (first, last) = (extreme(store, &node, false)?, extreme(store, &node, true)?);
        if outside(span, &first, &last) {
            return Err(Error::Decode(
                "held keys fall outside the range a new parent gives them".into(),
            ));
        }
    }
    Ok(Shape {
        count: node.count,
        level: node.level,
    })
}

enum Reach<'a, 'g, K, V, S> {
    Absent,
    Borrowed(&'a Shared<Node<K, V, S>>),
    Owned(Shared<Node<K, V, S>>),
    Held(&'g Node<K, V, S>),
}

#[inline]
fn reach<'a, 'g, K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    link: &'a Link<K, V, S>,
    depth: usize,
    guard: Option<&'g Guard>,
) -> Result<Reach<'a, 'g, K, V, S>> {
    let memo = match link {
        Link::Empty => return Ok(Reach::Absent),
        Link::Mem(node, _) => return Ok(Reach::Borrowed(node)),
        Link::Stored { memo, .. } => memo,
    };
    match memo.get() {
        Some(Memo::Pinned(node)) => return Ok(Reach::Borrowed(node)),
        Some(Memo::Weak(weak, hint)) => {
            hint.prefetch(weak);
            if let Some(node) = guard.and_then(|guard| weak.lend(guard)) {
                return Ok(Reach::Held(node));
            }
            if let Some(node) = weak.upgrade() {
                return Ok(Reach::Owned(node));
            }
        }
        None => {}
    }
    fetch_link(store, link, depth)
}

enum At<'g, K, V, S> {
    Ref(&'g Node<K, V, S>),
    Shared(Shared<Node<K, V, S>>),
}

impl<'g, K, V, S> At<'g, K, V, S> {
    #[inline]
    fn of(reached: Reach<'g, 'g, K, V, S>) -> Option<Self> {
        match reached {
            Reach::Absent => None,
            Reach::Borrowed(node) => Some(At::Ref(node)),
            Reach::Held(node) => Some(At::Ref(node)),
            Reach::Owned(node) => Some(At::Shared(node)),
        }
    }

    #[inline]
    fn owned(reached: Reach<'_, 'g, K, V, S>) -> Option<Self> {
        match reached {
            Reach::Absent => None,
            Reach::Borrowed(node) => Some(At::Shared(node.clone())),
            Reach::Held(node) => Some(At::Ref(node)),
            Reach::Owned(node) => Some(At::Shared(node)),
        }
    }

    #[inline]
    fn node(&self) -> &Node<K, V, S> {
        match self {
            At::Ref(node) => node,
            At::Shared(node) => node,
        }
    }
}

#[cold]
#[inline(never)]
fn fetch_link<'a, 'g, K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    link: &'a Link<K, V, S>,
    depth: usize,
) -> Result<Reach<'a, 'g, K, V, S>> {
    let Link::Stored {
        ptr,
        count,
        ceiling,
        memo,
        ..
    } = link
    else {
        return Err(Error::Detached);
    };
    let store = store.ok_or(Error::Detached)?;
    let (node, cached) = fetch_node(store, *ptr)?;
    admit(&node, *count, *ceiling)?;
    if depth < PIN_DEPTH {
        if let Memo::Pinned(node) = memo.get_or_init(|| Memo::Pinned(node.clone())) {
            return Ok(Reach::Borrowed(node));
        }
    } else if cached {
        memo.get_or_init(|| Memo::Weak(Shared::downgrade(&node), Hint::of(&node)));
    }
    Ok(Reach::Owned(node))
}

fn resident<K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    link: &Link<K, V, S>,
    depth: usize,
) -> Result<Option<Shared<Node<K, V, S>>>> {
    Ok(match reach(store, link, depth, None)? {
        Reach::Absent => None,
        Reach::Borrowed(node) => Some(node.clone()),
        Reach::Owned(node) => Some(node),
        Reach::Held(_) => unreachable!("an unguarded reach holds no node"),
    })
}

fn load<K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    link: &Link<K, V, S>,
) -> Result<Option<Shared<Node<K, V, S>>>> {
    match link {
        Link::Empty => Ok(None),
        Link::Mem(node, _) => Ok(Some(node.clone())),
        Link::Stored {
            ptr,
            count,
            ceiling,
            ..
        } => {
            let (node, _) = fetch_node(store.ok_or(Error::Detached)?, *ptr)?;
            admit(&node, *count, *ceiling)?;
            Ok(Some(node))
        }
    }
}

enum Visit<R> {
    Done(R),
    Descend(usize),
}

fn node_mut<'a, K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    link: &'a mut Link<K, V, S>,
) -> Result<&'a mut Node<K, V, S>> {
    dirty_mut(store, link).map(|(node, _)| node)
}

fn dirty_mut<'a, K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    link: &'a mut Link<K, V, S>,
) -> Result<(&'a mut Node<K, V, S>, &'a mut Hint)> {
    if let Link::Stored { .. } = link {
        let node = load(store, link)?.ok_or(Error::Detached)?;
        *link = Link::Mem(node, Hint::NONE);
    }
    match link {
        Link::Mem(node, hint) => {
            let shared = Shared::as_ptr(node);
            let node = Shared::make_mut(node);
            if !std::ptr::eq(shared, node) {
                *hint = Hint::NONE;
            }
            Ok((node, hint))
        }
        _ => Err(Error::Detached),
    }
}

fn link_from<K, V, S>(
    level: u8,
    keys: Vec<K>,
    slots: Vec<Slot<V, S>>,
    mut children: Vec<Link<K, V, S>>,
    origin: Option<Ptr>,
) -> Link<K, V, S> {
    if keys.is_empty() {
        return children.pop().unwrap_or(Link::Empty);
    }
    if children.iter().all(|child| matches!(child, Link::Empty)) {
        children = Vec::new();
    }
    let mut node = Node {
        level,
        keys,
        cells: Cells::Slots(slots),
        children,
        count: 0,
        origin,
        prefix: None,
    };
    node.recount();
    Link::mem(Shared::new(node))
}

type Parts<K, V, S> = (Link<K, V, S>, Link<K, V, S>);

fn tail<T>(items: &mut Vec<T>, at: usize) -> Vec<T> {
    if at == items.len() {
        return Vec::new();
    }
    let mut tail = Vec::with_capacity((items.len() - at).max(LEAF_CAPACITY));
    tail.extend(items.drain(at..));
    tail
}

fn split<K: Data + Ord, V: Data, S: Data>(
    store: Option<&Store>,
    link: Link<K, V, S>,
    side: &impl Fn(&K) -> Ordering,
) -> Result<Parts<K, V, S>> {
    let Some(node) = load(store, &link)? else {
        return Ok((Link::Empty, Link::Empty));
    };
    drop(link);
    let Node {
        level,
        mut keys,
        cells,
        mut children,
        origin,
        ..
    } = Shared::unwrap_or_clone(node);
    let mut slots = cells.into_slots();
    let position = keys
        .binary_search_by(side)
        .err()
        .ok_or_else(|| Error::Corrupt("key found at a level it does not hash to".into()))?;
    let right_keys = tail(&mut keys, position);
    let right_slots = tail(&mut slots, position);
    if children.is_empty() {
        return Ok((
            link_from(level, keys, slots, children, origin),
            link_from(level, right_keys, right_slots, Vec::new(), origin),
        ));
    }
    let mut right_children = children.split_off(position + 1);
    let middle = children.pop().ok_or(Error::Detached)?;
    let (left_part, right_part) = split(store, middle, side)?;
    children.push(left_part);
    right_children.insert(0, right_part);
    Ok((
        link_from(level, keys, slots, children, origin),
        link_from(level, right_keys, right_slots, right_children, origin),
    ))
}

fn merge<K: Data + Ord, V: Data, S: Data>(
    store: Option<&Store>,
    a: Link<K, V, S>,
    b: Link<K, V, S>,
) -> Result<Link<K, V, S>> {
    let (Some(x), Some(y)) = (load(store, &a)?, load(store, &b)?) else {
        return Ok(if a.count() == 0 { b } else { a });
    };
    match x.level.cmp(&y.level) {
        Ordering::Greater => {
            drop(a);
            let mut node = Shared::unwrap_or_clone(x);
            let last = node.children_mut().pop().ok_or(Error::Detached)?;
            node.children.push(merge(store, last, b)?);
            node.recount();
            Ok(Link::mem(Shared::new(node)))
        }
        Ordering::Less => {
            drop(b);
            let mut node = Shared::unwrap_or_clone(y);
            let first = node.children_mut()[0].take();
            node.children[0] = merge(store, a, first)?;
            node.recount();
            Ok(Link::mem(Shared::new(node)))
        }
        Ordering::Equal => {
            drop(a);
            drop(b);
            let mut left = Shared::unwrap_or_clone(x);
            let mut right = Shared::unwrap_or_clone(y);
            let inner_left = left.children_mut().pop().ok_or(Error::Detached)?;
            let inner_right = right.children_mut()[0].take();
            left.children.push(merge(store, inner_left, inner_right)?);
            left.children.extend(right.children.drain(1..));
            left.keys.append(&mut right.keys);
            left.cells.slots().append(right.cells.slots());
            left.origin = None;
            left.recount();
            Ok(Link::mem(Shared::new(left)))
        }
    }
}

fn insert_at<K: Data + Ord, V: Data, S: Data>(
    store: Option<&Store>,
    link: &mut Link<K, V, S>,
    key: K,
    level: u8,
    value: Arc<V>,
) -> Result<Option<Slot<V, S>>> {
    if let Link::Stored { .. } = link {
        let node = load(store, link)?.ok_or(Error::Detached)?;
        *link = Link::Mem(node, Hint::NONE);
    }
    let Link::Mem(current, _) = &*link else {
        let mut keys = Vec::with_capacity(LEAF_CAPACITY);
        keys.push(key);
        let mut slots = Vec::with_capacity(LEAF_CAPACITY);
        slots.push(Slot::Fresh(value));
        *link = link_from(level, keys, slots, Vec::new(), None);
        return Ok(None);
    };
    if level > current.level {
        let (left, right) = split(store, link.take(), &|probe: &K| probe.cmp(&key))?;
        *link = link_from(
            level,
            vec![key],
            vec![Slot::Fresh(value)],
            vec![left, right],
            None,
        );
        return Ok(None);
    }
    let (node, hint) = dirty_mut(store, link)?;
    match node.search(&key) {
        Ok(index) => {
            let before = node.buffers();
            let replaced = std::mem::replace(&mut node.cells.slots()[index], Slot::Fresh(value));
            hint.renew(node, before);
            Ok(Some(replaced))
        }
        Err(position) if level == node.level => {
            let before = node.buffers();
            if !node.children.is_empty() {
                let (left, right) = split(store, node.children[position].take(), &|probe: &K| {
                    probe.cmp(&key)
                })?;
                node.children[position] = left;
                node.children.insert(position + 1, right);
            }
            node.keys.insert(position, key);
            node.cells.slots().insert(position, Slot::Fresh(value));
            node.count += 1;
            hint.renew(node, before);
            Ok(None)
        }
        Err(position) => {
            if node.children.is_empty() {
                node.children_mut();
                *hint = Hint::dirty(node);
            }
            if node.level <= HINTED_LEVEL
                && let Link::Mem(child, hint) = &node.children[position]
            {
                hint.prefetch_at(Shared::as_ptr(child).addr());
            }
            let replaced = insert_at(store, &mut node.children[position], key, level, value)?;
            if replaced.is_none() {
                node.count += 1;
            }
            Ok(replaced)
        }
    }
}

fn remove_at<K, V, S, Q>(
    store: Option<&Store>,
    link: &mut Link<K, V, S>,
    key: &Q,
) -> Result<Option<Slot<V, S>>>
where
    K: Data + Ord + Borrow<Q>,
    V: Data,
    S: Data,
    Q: Ord + ?Sized,
{
    if matches!(link, Link::Empty) {
        return Ok(None);
    }
    let node = node_mut(store, link)?;
    match node.search(key) {
        Ok(index) => {
            node.keys.remove(index);
            let slot = node.cells.slots().remove(index);
            if !node.children.is_empty() {
                let right = node.children.remove(index + 1);
                let left = node.children[index].take();
                node.children[index] = merge(store, left, right)?;
            }
            node.count -= 1;
            if node.keys.is_empty() {
                *link = node.children.first_mut().map_or(Link::Empty, Link::take);
            }
            Ok(Some(slot))
        }
        Err(position) => {
            let removed = remove_at(store, &mut node.children_mut()[position], key)?;
            if removed.is_some() {
                node.count -= 1;
            }
            Ok(removed)
        }
    }
}

fn find_mut<'a, K, V, S, Q>(
    store: Option<&Store>,
    link: &'a mut Link<K, V, S>,
    key: &Q,
) -> Result<Option<&'a mut V>>
where
    K: Data + Ord + Borrow<Q>,
    V: Data,
    S: Data,
    Q: Ord + ?Sized,
{
    if matches!(link, Link::Empty) {
        return Ok(None);
    }
    let node = node_mut(store, link)?;
    match node.search(key) {
        Ok(index) => {
            let slot = &mut node.cells.slots()[index];
            let value = match &*slot {
                Slot::Packed(column, position) => {
                    Some(Arc::new(column[*position as usize].clone()))
                }
                Slot::Fresh(_) => None,
                Slot::Spilled(ptr, _) => Some(store.ok_or(Error::Detached)?.load::<V>(*ptr)?),
            };
            if let Some(value) = value {
                *slot = Slot::Fresh(value);
            }
            match slot {
                Slot::Fresh(value) => Ok(Some(Arc::make_mut(value))),
                _ => Err(Error::Detached),
            }
        }
        Err(position) => find_mut(store, &mut node.children_mut()[position], key),
    }
}

fn read_slot<V: Data, S>(store: Option<&Store>, slot: &Slot<V, S>) -> Result<Val<V>> {
    match slot {
        Slot::Packed(column, index) => Ok(Val::of_column(column, *index as usize)),
        Slot::Fresh(value) => Ok(Val::of_single(value)),
        Slot::Spilled(ptr, _) => Ok(Val(Held::Single(
            store.ok_or(Error::Detached)?.load::<V>(*ptr)?,
        ))),
    }
}

fn verify<T: Data>(encoder: &Encoder<'_>, mark: (usize, usize), store: &Store) -> Result<T> {
    let mut decoder = Decoder::new(
        &encoder.bytes()[mark.0..],
        &encoder.links()[mark.1..],
        Some(store),
    );
    T::decode(&mut decoder)
}

struct Probe(u64);

impl Sink for Probe {
    fn put(&mut self, _object: Object, _hash: Hash) -> Result<Ptr> {
        Err(Error::Detached)
    }

    fn check_link(&self, _ptr: Ptr) -> Result<()> {
        Err(Error::Detached)
    }

    fn store_id(&self) -> u64 {
        self.0
    }
}

fn flush<K: Data, V: Data, S: Summary<K, V>>(
    link: &Link<K, V, S>,
    sink: &mut dyn Sink,
    depth: usize,
    dense: bool,
) -> Result<Link<K, V, S>> {
    flush_with(link, sink, depth, dense, true)
}

fn flush_with<K: Data, V: Data, S: Summary<K, V>>(
    link: &Link<K, V, S>,
    sink: &mut dyn Sink,
    depth: usize,
    dense: bool,
    mut fan: bool,
) -> Result<Link<K, V, S>> {
    let Link::Mem(node, _) = link else {
        return Ok(link.clone());
    };
    if fan && fans_out(node) {
        let stager = Stager {
            id: sink.store_id(),
            store: sink.store(),
            pack: sink.packs(),
        };
        if let Ok(mut staged) = stage_tree(node, &stager) {
            emit_staged(&mut staged, &mut Arena::default(), sink)?;
            let written = Link::stored(staged.ptr, node.count, staged.total.clone());
            if let Some(store) = &stager.store {
                let settled = settle_tree(&mut staged, depth, store)?;
                if dense && depth >= PIN_DEPTH {
                    memoize(&written, &settled);
                }
            }
            remember(staged, sink);
            return Ok(written);
        }
        fan = false;
    }
    let flushed = prepare(node, sink, depth, fan)?;
    emit(flushed, sink, depth, dense)
}

fn memoize<K, V, S>(link: &Link<K, V, S>, node: &Shared<Node<K, V, S>>) {
    if let Link::Stored { memo, .. } = link {
        memo.get_or_init(|| Memo::Weak(Shared::downgrade(node), Hint::of(node)));
    }
}

fn relocate<K: Data, V: Data, S: Data>(
    old: &Erased,
    links: usize,
    moves: &Moves,
    moved: &Moved,
) -> Option<Erased> {
    let old = old.downcast_ref::<Node<K, V, S>>()?;
    let children = old
        .children
        .iter()
        .filter(|child| matches!(child, Link::Stored { .. }));
    let slots = match &old.cells {
        Cells::Slots(slots) => slots.as_slice(),
        Cells::Column(_) => &[],
    };
    let spilled = slots
        .iter()
        .filter(|slot| matches!(slot, Slot::Spilled(..)));
    if children.count() + spilled.count() != links {
        return None;
    }
    let mut node = old.clone();
    node.origin = Some(moves.ptr(old.origin?)?);
    for child in &mut node.children {
        if let Link::Stored { ptr, .. } = child {
            *ptr = moves.ptr(*ptr)?;
        }
    }
    if let Cells::Slots(slots) = &mut node.cells {
        for slot in slots {
            if let Slot::Spilled(ptr, _) = slot {
                *ptr = moves.ptr(*ptr)?;
            }
        }
    }
    let node = Shared::new(node);
    for (link, was) in node.children.iter().zip(&old.children) {
        if let Link::Stored { memo, .. } = was
            && let Some(Memo::Weak(child, _)) = memo.get()
            && let Some(child) = moved.get(&child.as_ptr().addr())
            && let Ok(child) = child.clone().downcast()
        {
            memoize(link, &child);
        }
    }
    Some(Shared::erase(node))
}

fn fans_out<K, V, S>(node: &Node<K, V, S>) -> bool {
    let mut budget = PARALLEL_NODES;
    node.count >= PARALLEL_KEYS
        && node
            .children
            .iter()
            .filter(|child| matches!(child, Link::Mem(..)))
            .nth(1)
            .is_some()
        && busy(node, &mut budget)
}

fn busy<K, V, S>(node: &Node<K, V, S>, budget: &mut usize) -> bool {
    *budget = budget.saturating_sub(1);
    *budget == 0 || dirty(node).any(|child| busy(child, budget))
}

struct Flushed<K, V, S> {
    object: Object,
    hash: Hash,
    cached: Option<Node<K, V, S>>,
    count: u64,
    total: S,
    origin: Option<Ptr>,
}

fn prepare<K: Data, V: Data, S: Summary<K, V>>(
    node: &Node<K, V, S>,
    sink: &mut dyn Sink,
    depth: usize,
    fan: bool,
) -> Result<Flushed<K, V, S>> {
    let store = sink.store();
    let caching = store.is_some();
    let width = node.keys.len();
    let fresh = (0..width).filter(|&index| node.is_fresh(index)).count();
    let decoding = caching && fresh > 0 && 2 * fresh >= width;
    let reused = match (&node.cells, &node.prefix) {
        (Cells::Column(_), Some(prefix)) => Some(prefix.clone()),
        _ => None,
    };
    let bytes = reused.as_ref().map_or(64 + 24 * width, |prefix| {
        prefix.bytes.len() + 24 * (width + 1)
    });
    let mut encoder = Encoder::with_capacity(&mut *sink, bytes, width + 1);
    let (spilled, checked) = match &reused {
        Some(prefix) => {
            encoder.raw(&prefix.bytes);
            for link in &prefix.links {
                encoder.link(*link);
            }
            (Vec::new(), Vec::new())
        }
        None => entries(node, &mut encoder, store.as_ref(), caching, decoding)?,
    };
    let entries_end = encoder.mark();
    let branching = caching && !node.children.is_empty();
    let rewritten = dirty(node).count();
    let mut stored: Vec<Link<K, V, S>> = Vec::with_capacity(if branching { width + 1 } else { 0 });
    let mut total = None;
    for position in 0..=width {
        let child = node.child(position);
        let flushed = flush_with(child, encoder.sink(), depth + 1, rewritten > 1, fan)?;
        match &flushed {
            Link::Empty => encoder.varint(0),
            Link::Stored {
                ptr,
                count,
                summary,
                ..
            } => {
                encoder.varint(*count);
                summary.encode(&mut encoder)?;
                encoder.link(*ptr);
                total = fold(total, summary);
            }
            Link::Mem(..) => return Err(Error::Detached),
        }
        if branching {
            stored.push(flushed);
        }
        if position < width {
            total = fold(total, &node.entry_summary(position));
        }
    }
    let total = total.ok_or(Error::Detached)?;
    let unsettled = encoder.is_unsettled();
    let object = encoder.into_object();
    let hash = object.hash();
    let cached = match &store {
        None => None,
        Some(store) if unsettled || decoding => {
            let mut decoder = Decoder::new(&object.body, &object.links, Some(store));
            let mut decoded = Node::<K, V, S>::decode(&mut decoder, Ptr { hash, offset: 0 })?;
            if branching && !unsettled {
                decoded.children = stored;
            }
            Some(decoded)
        }
        Some(_) => Some(Node {
            level: node.level,
            keys: node.keys.clone(),
            cells: node.cells.settled(spilled, checked),
            children: stored,
            count: node.count,
            origin: None,
            prefix: None,
        }),
    };
    let cached = cached.map(|mut cached| {
        if let Cells::Column(_) = cached.cells {
            cached.prefix = Some(reused.unwrap_or_else(|| Prefix::of(&object, entries_end)));
        }
        cached
    });
    Ok(Flushed {
        object,
        hash,
        cached,
        count: node.count,
        total,
        origin: node.origin,
    })
}

type Entries<V, S> = (Vec<(usize, Slot<V, S>)>, Vec<(usize, V)>);

fn entries<K: Data, V: Data, S: Summary<K, V>>(
    node: &Node<K, V, S>,
    encoder: &mut Encoder<'_>,
    store: Option<&Store>,
    caching: bool,
    decoding: bool,
) -> Result<Entries<V, S>> {
    encoder.u8(node.level);
    encoder.varint(node.keys.len() as u64);
    for (index, key) in node.keys.iter().enumerate() {
        let mark = encoder.mark();
        key.encode(encoder)?;
        if let (true, false, Some(store)) = (node.is_fresh(index), decoding, store) {
            verify::<K>(encoder, mark, store)?;
        }
    }
    let mut spilled = Vec::new();
    let mut checked = Vec::new();
    for (index, key) in node.keys.iter().enumerate() {
        let value = match &node.cells {
            Cells::Column(column) => &column[index],
            Cells::Slots(slots) => match &slots[index] {
                Slot::Spilled(ptr, summary) => {
                    encoder.u8(1);
                    encoder.link(*ptr);
                    summary.encode(encoder)?;
                    continue;
                }
                Slot::Packed(column, position) => &column[*position as usize],
                Slot::Fresh(value) => &**value,
            },
        };
        let mark = encoder.mark();
        encoder.u8(0);
        let body = encoder.mark();
        value.encode(encoder)?;
        let spill = encoder.since(mark) > SPILL_BYTES;
        if let (true, true, Some(store)) = (node.is_fresh(index), spill || !decoding, store) {
            let decoded = verify::<V>(encoder, body, store)?;
            if !spill && caching && !decoding {
                checked.push((index, decoded));
            }
        }
        if spill {
            encoder.rewind(mark);
            let ptr = encoder.child(|inner| value.encode(inner))?;
            let summary = S::of(key, value);
            encoder.u8(1);
            encoder.link(ptr);
            summary.encode(encoder)?;
            if caching && !decoding {
                spilled.push((index, Slot::Spilled(ptr, summary)));
            }
        }
    }
    Ok((spilled, checked))
}

fn emit<K: Data, V: Data, S: Data>(
    flushed: Flushed<K, V, S>,
    sink: &mut dyn Sink,
    depth: usize,
    dense: bool,
) -> Result<Link<K, V, S>> {
    let Flushed {
        object,
        hash,
        cached,
        count,
        total,
        origin,
    } = flushed;
    let body_bytes = object.body.len();
    let links = object.links.len();
    let ptr = match origin {
        Some(origin) if origin.hash == hash => origin,
        _ => sink.put(object, hash)?,
    };
    let written = Link::stored(ptr, count, total);
    if let Some(mut decoded) = cached {
        decoded.origin = Some(ptr);
        let weight = heap_weight(decoded.fixed_bytes(), body_bytes);
        let decoded = Shared::new(decoded);
        if dense && depth >= PIN_DEPTH {
            memoize(&written, &decoded);
        }
        sink.remember_moving(
            ptr,
            Shared::erase(decoded),
            weight,
            (relocate::<K, V, S>, links),
        );
    }
    Ok(written)
}

struct Stager {
    id: u64,
    store: Option<Store>,
    pack: bool,
}

#[derive(Default)]
struct Arena {
    bytes: Vec<u8>,
    links: Vec<Ptr>,
}

struct Placed {
    hash: Hash,
    raw: usize,
    payload: Range<usize>,
    packed: bool,
    links: Range<usize>,
}

impl Arena {
    fn place(&mut self, object: &Object, pack: bool) -> Placed {
        let packed = if pack { compressed(&object.body) } else { None };
        let start = self.bytes.len();
        self.bytes
            .extend_from_slice(packed.as_deref().unwrap_or(&object.body));
        let payload = start..self.bytes.len();
        let start = self.links.len();
        self.links.extend_from_slice(&object.links);
        Placed {
            hash: object.hash(),
            raw: object.body.len(),
            payload,
            packed: packed.is_some(),
            links: start..self.links.len(),
        }
    }

    fn staged(&self, placed: &Placed) -> Staged<'_> {
        Staged {
            raw: placed.raw,
            payload: &self.bytes[placed.payload.clone()],
            packed: placed.packed,
            links: &self.links[placed.links.clone()],
        }
    }

    fn put(&self, placed: &Placed, sink: &mut dyn Sink) -> Result<Ptr> {
        sink.put_staged(placed.hash, self.staged(placed))
    }
}

type Settled<K, V, S> = (Ptr, Shared<Node<K, V, S>>, u32);
type Written<K, V, S> = (usize, Shared<Node<K, V, S>>);

struct Deferred<'a, K, V, S> {
    node: &'a Node<K, V, S>,
    arena: Option<Box<Arena>>,
    placed: Placed,
    spills: Vec<(usize, Placed)>,
    children: Vec<(usize, usize, Deferred<'a, K, V, S>)>,
    total: S,
    ptr: Ptr,
    settled: Vec<Settled<K, V, S>>,
}

fn dirty<K, V, S>(node: &Node<K, V, S>) -> impl Iterator<Item = &Node<K, V, S>> {
    node.children.iter().filter_map(|child| match child {
        Link::Mem(child, _) => Some(&**child),
        _ => None,
    })
}

fn frontier<'a, K, V, S>(node: &'a Node<K, V, S>, tasks: &mut Vec<&'a Node<K, V, S>>) {
    for child in dirty(node) {
        if child.count >= PARALLEL_KEYS {
            frontier(child, tasks);
        } else {
            tasks.push(child);
        }
    }
}

fn stage_tree<'a, K: Data, V: Data, S: Summary<K, V>>(
    node: &'a Node<K, V, S>,
    stager: &Stager,
) -> Result<Deferred<'a, K, V, S>> {
    let mut tasks = Vec::new();
    frontier(node, &mut tasks);
    let mut staged = parallel(tasks, |task| {
        let mut arena = Arena::default();
        let mut staged = stage(task, stager, &mut arena)?;
        staged.arena = Some(Box::new(arena));
        Ok(staged)
    })
    .into_iter();
    let mut arena = Arena::default();
    let mut root = stage_spine(node, stager, &mut arena, &mut staged)?;
    root.arena = Some(Box::new(arena));
    Ok(root)
}

fn stage_spine<'a, K: Data, V: Data, S: Summary<K, V>>(
    node: &'a Node<K, V, S>,
    stager: &Stager,
    arena: &mut Arena,
    staged: &mut impl Iterator<Item = Result<Deferred<'a, K, V, S>>>,
) -> Result<Deferred<'a, K, V, S>> {
    let mut children = Vec::new();
    for child in dirty(node) {
        children.push(if child.count >= PARALLEL_KEYS {
            stage_spine(child, stager, arena, staged)?
        } else {
            staged.next().ok_or(Error::Detached)??
        });
    }
    seal(node, children, stager, arena)
}

fn stage<'a, K: Data, V: Data, S: Summary<K, V>>(
    node: &'a Node<K, V, S>,
    stager: &Stager,
    arena: &mut Arena,
) -> Result<Deferred<'a, K, V, S>> {
    let children = dirty(node)
        .map(|child| stage(child, stager, arena))
        .collect::<Result<Vec<_>>>()?;
    seal(node, children, stager, arena)
}

fn seal<'a, K: Data, V: Data, S: Summary<K, V>>(
    node: &'a Node<K, V, S>,
    mut children: Vec<Deferred<'a, K, V, S>>,
    stager: &Stager,
    arena: &mut Arena,
) -> Result<Deferred<'a, K, V, S>> {
    children.reverse();
    let mut probe = Probe(stager.id);
    let width = node.keys.len();
    let mut encoder = Encoder::with_capacity(&mut probe, 64 + 24 * width, width + 1);
    encoder.u8(node.level);
    encoder.varint(width as u64);
    for key in &node.keys {
        key.encode(&mut encoder)?;
    }
    let mut spills = Vec::new();
    for (index, key) in node.keys.iter().enumerate() {
        let value = match &node.cells {
            Cells::Column(column) => &column[index],
            Cells::Slots(slots) => match &slots[index] {
                Slot::Spilled(ptr, summary) => {
                    encoder.u8(1);
                    encoder.link(*ptr);
                    summary.encode(&mut encoder)?;
                    continue;
                }
                Slot::Packed(column, position) => &column[*position as usize],
                Slot::Fresh(value) => &**value,
            },
        };
        let mark = encoder.mark();
        encoder.u8(0);
        value.encode(&mut encoder)?;
        if encoder.since(mark) > SPILL_BYTES {
            encoder.rewind(mark);
            let mut inner = Probe(stager.id);
            let mut spill = Encoder::new(&mut inner);
            value.encode(&mut spill)?;
            let object = spill.into_object();
            if let (true, Some(store)) = (node.is_fresh(index), &stager.store) {
                V::decode(&mut Decoder::new(&object.body, &object.links, Some(store)))?;
            }
            let placed = arena.place(&object, stager.pack);
            encoder.u8(1);
            spills.push((encoder.links().len(), placed));
            encoder.link(Ptr {
                hash: object.hash(),
                offset: 0,
            });
            S::of(key, value).encode(&mut encoder)?;
        }
    }
    let mut linked = Vec::with_capacity(children.len());
    let mut total = None;
    for position in 0..=width {
        match node.child(position) {
            Link::Empty => encoder.varint(0),
            Link::Stored {
                ptr,
                count,
                summary,
                ..
            } => {
                encoder.varint(*count);
                summary.encode(&mut encoder)?;
                encoder.link(*ptr);
                total = fold(total, summary);
            }
            Link::Mem(child, _) => {
                let deferred = children.pop().ok_or(Error::Detached)?;
                encoder.varint(child.count);
                deferred.total.encode(&mut encoder)?;
                total = fold(total, &deferred.total);
                let hash = deferred.placed.hash;
                linked.push((encoder.links().len(), position, deferred));
                encoder.link(Ptr { hash, offset: 0 });
            }
        }
        if position < width {
            total = fold(total, &node.entry_summary(position));
        }
    }
    if encoder.is_unsettled() {
        return Err(Error::Detached);
    }
    let object = encoder.into_object();
    let placed = arena.place(&object, stager.pack);
    Ok(Deferred {
        node,
        arena: None,
        ptr: Ptr {
            hash: placed.hash,
            offset: 0,
        },
        placed,
        spills,
        children: linked,
        total: total.ok_or(Error::Detached)?,
        settled: Vec::new(),
    })
}

fn emit_staged<K, V, S>(
    deferred: &mut Deferred<'_, K, V, S>,
    outer: &mut Arena,
    sink: &mut dyn Sink,
) -> Result<()> {
    let Deferred {
        node,
        arena,
        placed,
        spills,
        children,
        ptr,
        ..
    } = deferred;
    let arena = arena.as_deref_mut().unwrap_or(outer);
    let base = placed.links.start;
    for (at, spill) in spills.iter() {
        arena.links[base + at] = arena.put(spill, sink)?;
    }
    for (at, _, child) in children.iter_mut() {
        emit_staged(child, arena, sink)?;
        arena.links[base + *at] = child.ptr;
    }
    *ptr = match node.origin {
        Some(origin) if origin.hash == placed.hash => origin,
        _ => arena.put(placed, sink)?,
    };
    Ok(())
}

fn settle_tree<K: Data, V: Data, S: Data>(
    root: &mut Deferred<'_, K, V, S>,
    depth: usize,
    store: &Store,
) -> Result<Shared<Node<K, V, S>>> {
    let mut tasks = Vec::new();
    for (_, _, child) in &mut root.children {
        owned(child, depth + 1, &mut tasks);
    }
    parallel(tasks, |(task, depth)| settle_owned(task, depth, store))
        .into_iter()
        .collect::<Result<()>>()?;
    let arena = root.arena.take().ok_or(Error::Detached)?;
    settle_spine(root, depth, &arena, store, &mut Vec::new())
}

fn owned<'t, 'a, K, V, S>(
    deferred: &'t mut Deferred<'a, K, V, S>,
    depth: usize,
    tasks: &mut Vec<(&'t mut Deferred<'a, K, V, S>, usize)>,
) {
    if deferred.arena.is_some() {
        tasks.push((deferred, depth));
    } else {
        for (_, _, child) in &mut deferred.children {
            owned(child, depth + 1, tasks);
        }
    }
}

fn settle_owned<K: Data, V: Data, S: Data>(
    task: &mut Deferred<'_, K, V, S>,
    depth: usize,
    store: &Store,
) -> Result<()> {
    let arena = task.arena.take().ok_or(Error::Detached)?;
    let mut settled = Vec::new();
    let outcome = settle(task, depth, &arena, store, &mut Vec::new(), &mut settled);
    task.arena = Some(Box::default());
    task.children = Vec::new();
    task.settled = settled;
    outcome.map(drop)
}

fn settle<K: Data, V: Data, S: Data>(
    deferred: &Deferred<'_, K, V, S>,
    depth: usize,
    arena: &Arena,
    store: &Store,
    scratch: &mut Vec<u8>,
    settled: &mut Vec<Settled<K, V, S>>,
) -> Result<Shared<Node<K, V, S>>> {
    let mut below = Vec::with_capacity(deferred.children.len());
    for (_, position, child) in &deferred.children {
        below.push((
            *position,
            settle(child, depth + 1, arena, store, scratch, settled)?,
        ));
    }
    let entry = decoded(deferred, depth, &below, arena, store, scratch)?;
    let node = entry.1.clone();
    settled.push(entry);
    Ok(node)
}

fn settle_spine<K: Data, V: Data, S: Data>(
    deferred: &mut Deferred<'_, K, V, S>,
    depth: usize,
    arena: &Arena,
    store: &Store,
    scratch: &mut Vec<u8>,
) -> Result<Shared<Node<K, V, S>>> {
    let mut below = Vec::with_capacity(deferred.children.len());
    for (_, position, child) in &mut deferred.children {
        let node = if child.arena.is_none() {
            settle_spine(child, depth + 1, arena, store, scratch)?
        } else {
            let last = child.settled.last().ok_or(Error::Detached)?;
            last.1.clone()
        };
        below.push((*position, node));
    }
    let entry = decoded(deferred, depth, &below, arena, store, scratch)?;
    let node = entry.1.clone();
    deferred.settled = vec![entry];
    Ok(node)
}

fn decoded<K: Data, V: Data, S: Data>(
    deferred: &Deferred<'_, K, V, S>,
    depth: usize,
    below: &[Written<K, V, S>],
    arena: &Arena,
    store: &Store,
    scratch: &mut Vec<u8>,
) -> Result<Settled<K, V, S>> {
    let placed = &deferred.placed;
    let payload = &arena.bytes[placed.payload.clone()];
    let body = if placed.packed {
        scratch.resize(placed.raw, 0);
        let filled = lz4_flex::block::decompress_into(payload, scratch)
            .map_err(|error| Error::Corrupt(format!("bad staged body: {error}")))?;
        &scratch[..filled]
    } else {
        payload
    };
    let mut decoder = Decoder::new(body, &arena.links[placed.links.clone()], Some(store));
    let node = Node::<K, V, S>::decode(&mut decoder, deferred.ptr)?;
    if below.len() > 1 && depth + 1 >= PIN_DEPTH {
        for (position, child) in below {
            memoize(node.child(*position), child);
        }
    }
    let weight = heap_weight(node.fixed_bytes(), placed.raw);
    Ok((deferred.ptr, Shared::new(node), weight))
}

fn remember<K: Data, V: Data, S: Data>(deferred: Deferred<'_, K, V, S>, sink: &mut dyn Sink) {
    for (_, _, child) in deferred.children {
        remember(child, sink);
    }
    for (ptr, node, weight) in deferred.settled {
        sink.remember(ptr, Shared::erase(node), weight);
    }
}

fn workers() -> usize {
    static WORKERS: OnceLock<usize> = OnceLock::new();
    *WORKERS.get_or_init(|| {
        std::thread::available_parallelism().map_or(1, |cores| cores.get().min(MAX_WORKERS))
    })
}

fn parallel<T: Send, R: Send>(tasks: Vec<T>, work: impl Fn(T) -> R + Sync) -> Vec<R> {
    let threads = workers().min(tasks.len());
    if threads < 2 {
        return tasks.into_iter().map(work).collect();
    }
    let queue = parking_lot::Mutex::new(tasks.into_iter().enumerate());
    let drain = || {
        let mut done = Vec::new();
        loop {
            let next = queue.lock().next();
            let Some((index, task)) = next else {
                return done;
            };
            done.push((index, work(task)));
        }
    };
    let mut done = std::thread::scope(|scope| {
        let helpers: Vec<_> = (1..threads).map(|_| scope.spawn(drain)).collect();
        let mut done = drain();
        for helper in helpers {
            match helper.join() {
                Ok(more) => done.extend(more),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
        done
    });
    done.sort_unstable_by_key(|(index, _)| *index);
    done.into_iter().map(|(_, result)| result).collect()
}

struct Edit<K, V> {
    key: K,
    level: u8,
    value: Option<Arc<V>>,
}

struct Open<K, V, S> {
    level: u8,
    keys: Vec<K>,
    slots: Vec<Slot<V, S>>,
    children: Vec<Link<K, V, S>>,
}

impl<K, V, S> Open<K, V, S> {
    fn new(level: u8) -> Self {
        Open {
            level,
            keys: Vec::new(),
            slots: Vec::new(),
            children: Vec::new(),
        }
    }

    fn adopt(&mut self, child: Link<K, V, S>) {
        if self.children.is_empty() {
            if matches!(child, Link::Empty) {
                return;
            }
            self.children.resize_with(self.keys.len(), || Link::Empty);
        }
        self.children.push(child);
    }

    fn push(&mut self, carry: Link<K, V, S>, key: K, value: Arc<V>) {
        self.adopt(carry);
        self.keys.push(key);
        self.slots.push(Slot::Fresh(value));
    }

    fn close(mut self, last: Link<K, V, S>) -> Link<K, V, S> {
        self.adopt(last);
        link_from(self.level, self.keys, self.slots, self.children, None)
    }
}

fn rise<K: Data + Ord, V: Data, S: Data>(
    store: Option<&Store>,
    link: &mut Link<K, V, S>,
    level: u8,
    edits: &mut vec::IntoIter<Edit<K, V>>,
    count: usize,
) -> Result<()> {
    let mut left = count;
    let mut keys = Vec::new();
    let mut slots = Vec::new();
    let mut children = Vec::new();
    let mut piece = link.take();
    while let Some(before) = edits.as_slice()[..left]
        .iter()
        .position(|edit| edit.level == level && edit.value.is_some())
    {
        let key = &edits.as_slice()[before].key;
        let (mut low, high) = split(store, piece, &|probe: &K| probe.cmp(key))?;
        absorb(store, &mut low, edits, before)?;
        children.push(low);
        let edit = edits.next().ok_or(Error::Detached)?;
        keys.push(edit.key);
        slots.push(Slot::Fresh(edit.value.ok_or(Error::Detached)?));
        piece = high;
        left -= before + 1;
    }
    absorb(store, &mut piece, edits, left)?;
    children.push(piece);
    *link = link_from(level, keys, slots, children, None);
    Ok(())
}

fn absorb<K: Data + Ord, V: Data, S: Data>(
    store: Option<&Store>,
    link: &mut Link<K, V, S>,
    edits: &mut vec::IntoIter<Edit<K, V>>,
    mut count: usize,
) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    if matches!(link, Link::Empty) {
        *link = build_sorted(edits.by_ref().take(count));
        return Ok(());
    }
    if let Link::Stored { .. } = link {
        *link = Link::Mem(load(store, link)?.ok_or(Error::Detached)?, Hint::NONE);
    }
    let top = edits.as_slice()[..count]
        .iter()
        .filter(|edit| edit.value.is_some())
        .map(|edit| edit.level)
        .max();
    if let (Link::Mem(current, _), Some(level)) = (&*link, top)
        && level > current.level
    {
        return rise(store, link, level, edits, count);
    }
    let node = node_mut(store, link)?;
    let level = node.level;
    let mut position = 0;
    while count > 0 {
        let pending = &edits.as_slice()[..count];
        position += node.keys[position..].partition_point(|key| *key < pending[0].key);
        if node.keys.get(position) == Some(&pending[0].key) {
            count -= 1;
            match edits.next().and_then(|edit| edit.value) {
                Some(value) => {
                    node.cells.slots()[position] = Slot::Fresh(value);
                    position += 1;
                }
                None => {
                    node.keys.remove(position);
                    node.cells.slots().remove(position);
                    if !node.children.is_empty() {
                        let right = node.children.remove(position + 1);
                        let left = node.children[position].take();
                        node.children[position] = merge(store, left, right)?;
                    }
                }
            }
            continue;
        }
        let group = match node.keys.get(position) {
            Some(limit) => pending.partition_point(|edit| edit.key < *limit),
            None => count,
        };
        let found = pending[..group]
            .iter()
            .position(|edit| edit.level == level && edit.value.is_some());
        match found {
            None => {
                absorb(store, &mut node.children_mut()[position], edits, group)?;
                count -= group;
            }
            Some(before) => {
                if node.children.is_empty() && before == 0 {
                    let edit = edits.next().ok_or(Error::Detached)?;
                    node.keys.insert(position, edit.key);
                    node.cells
                        .slots()
                        .insert(position, Slot::Fresh(edit.value.ok_or(Error::Detached)?));
                } else {
                    let key = &pending[before].key;
                    let piece = node.children_mut()[position].take();
                    let (mut low, high) = split(store, piece, &|probe: &K| probe.cmp(key))?;
                    absorb(store, &mut low, edits, before)?;
                    let edit = edits.next().ok_or(Error::Detached)?;
                    node.keys.insert(position, edit.key);
                    node.cells
                        .slots()
                        .insert(position, Slot::Fresh(edit.value.ok_or(Error::Detached)?));
                    node.children[position] = low;
                    node.children.insert(position + 1, high);
                }
                position += 1;
                count -= before + 1;
            }
        }
    }
    node.recount();
    if node.keys.is_empty() {
        *link = node.children.first_mut().map_or(Link::Empty, Link::take);
    }
    Ok(())
}

fn build_sorted<K: Data + Ord, V: Data, S: Data>(
    edits: impl Iterator<Item = Edit<K, V>>,
) -> Link<K, V, S> {
    let mut stack: Vec<Open<K, V, S>> = Vec::new();
    for edit in edits {
        let Edit {
            key,
            level,
            value: Some(value),
        } = edit
        else {
            continue;
        };
        let mut carry = Link::Empty;
        while stack.last().is_some_and(|top| top.level < level) {
            if let Some(top) = stack.pop() {
                carry = top.close(carry);
            }
        }
        match stack.last_mut() {
            Some(top) if top.level == level => top.push(carry, key, value),
            _ => {
                let mut open = Open::new(level);
                open.push(carry, key, value);
                stack.push(open);
            }
        }
    }
    let mut carry = Link::Empty;
    while let Some(top) = stack.pop() {
        carry = top.close(carry);
    }
    carry
}

fn sorted_unique<K: Ord, T>(mut items: Vec<(K, T)>) -> Vec<(K, T)> {
    if items.windows(2).all(|pair| pair[0].0 < pair[1].0) {
        return items;
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items.dedup_by(|later, kept| {
        let same = later.0 == kept.0;
        if same {
            std::mem::swap(later, kept);
        }
        same
    });
    items
}

fn levels<K: Data, T: Sync>(items: &[(K, T)]) -> Vec<u8> {
    let chunk = |chunk: &[(K, T)]| -> Vec<u8> {
        chunk.iter().map(|(key, _)| fatal(level_of(key))).collect()
    };
    if items.len() < PARALLEL_ITEMS {
        return chunk(items);
    }
    parallel(items.chunks(PARALLEL_ITEMS).collect(), chunk).concat()
}

fn below<K, Q>(key: &K, start: Bound<&Q>) -> bool
where
    K: Borrow<Q>,
    Q: Ord + ?Sized,
{
    match start {
        Bound::Included(bound) => key.borrow() < bound,
        Bound::Excluded(bound) => key.borrow() <= bound,
        Bound::Unbounded => false,
    }
}

fn within<K, Q>(key: &K, end: Bound<&Q>) -> bool
where
    K: Borrow<Q>,
    Q: Ord + ?Sized,
{
    match end {
        Bound::Included(bound) => key.borrow() <= bound,
        Bound::Excluded(bound) => key.borrow() < bound,
        Bound::Unbounded => true,
    }
}

type Cut<'a, K> = Option<&'a dyn Fn(&K) -> bool>;

fn gather<K: Data, V: Data, S: Summary<K, V>>(
    store: Option<&Store>,
    guard: Option<&Guard>,
    link: &Link<K, V, S>,
    depth: usize,
    start: Cut<'_, K>,
    end: Cut<'_, K>,
    total: Option<S>,
) -> Result<Option<S>> {
    if start.is_none() && end.is_none() {
        return Ok(match link.summary() {
            Some(whole) => fold(total, &whole),
            None => total,
        });
    }
    let reached = reach(store, link, depth, guard)?;
    let node = match &reached {
        Reach::Absent => return Ok(total),
        Reach::Borrowed(node) => &***node,
        Reach::Owned(node) => &**node,
        Reach::Held(node) => *node,
    };
    let low = start.map_or(0, |below| node.keys.partition_point(below));
    let high = end.map_or(node.keys.len(), |within| node.keys.partition_point(within));
    if low > high {
        return Ok(total);
    }
    if low == high {
        return gather(store, guard, node.child(low), depth + 1, start, end, total);
    }
    let mut total = gather(store, guard, node.child(low), depth + 1, start, None, total)?;
    for index in low..high {
        total = fold(total, &node.entry_summary(index));
        let last = if index + 1 == high { end } else { None };
        total = gather(
            store,
            guard,
            node.child(index + 1),
            depth + 1,
            None,
            last,
            total,
        )?;
    }
    Ok(total)
}

fn scan<K, V, S, B>(
    store: Option<&Store>,
    link: &Link<K, V, S>,
    depth: usize,
    start: Cut<'_, K>,
    end: Cut<'_, K>,
    visit: &mut impl FnMut(&K, &V) -> ControlFlow<B>,
) -> Result<ControlFlow<B>>
where
    K: Data,
    V: Data,
    S: Data,
{
    let reached = reach(store, link, depth, None)?;
    let node = match &reached {
        Reach::Absent => return Ok(ControlFlow::Continue(())),
        Reach::Borrowed(node) => &***node,
        Reach::Owned(node) => &**node,
        Reach::Held(node) => *node,
    };
    let low = start.map_or(0, |below| node.keys.partition_point(below));
    let high = end.map_or(node.keys.len(), |within| node.keys.partition_point(within));
    if low > high {
        return Ok(ControlFlow::Continue(()));
    }
    let leaf = node.children.is_empty();
    if leaf && let Cells::Column(column) = &node.cells {
        for (key, value) in node.keys[low..high].iter().zip(&column[low..high]) {
            if let ControlFlow::Break(out) = visit(key, value) {
                return Ok(ControlFlow::Break(out));
            }
        }
        return Ok(ControlFlow::Continue(()));
    }
    if !leaf {
        let last = if low == high { end } else { None };
        if let ControlFlow::Break(out) =
            scan(store, node.child(low), depth + 1, start, last, visit)?
        {
            return Ok(ControlFlow::Break(out));
        }
    }
    for index in low..high {
        let outcome = node.read(store, index, |value| visit(&node.keys[index], value))?;
        if let ControlFlow::Break(out) = outcome {
            return Ok(ControlFlow::Break(out));
        }
        if !leaf {
            let last = if index + 1 == high { end } else { None };
            if let ControlFlow::Break(out) =
                scan(store, node.child(index + 1), depth + 1, None, last, visit)?
            {
                return Ok(ControlFlow::Break(out));
            }
        }
    }
    Ok(ControlFlow::Continue(()))
}

fn left_if(left: bool) -> Ordering {
    if left {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

pub struct Map<K, V, S = ()> {
    root: Link<K, V, S>,
    store: Option<Store>,
}

impl<K, V, S: Clone> Clone for Map<K, V, S> {
    fn clone(&self) -> Self {
        Map {
            root: self.root.with_memo(),
            store: self.store.clone(),
        }
    }
}

impl<K, V, S> Default for Map<K, V, S> {
    fn default() -> Self {
        Map {
            root: Link::Empty,
            store: None,
        }
    }
}

impl<K, V> Map<K, V> {
    pub fn new() -> Self {
        Map::default()
    }
}

impl<K: Data + Ord, V: Data, S: Summary<K, V>> Map<K, V, S> {
    pub fn len(&self) -> usize {
        self.root.count() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.root.count() == 0
    }

    pub fn is_dirty(&self) -> bool {
        matches!(self.root, Link::Mem(..))
    }

    pub fn clear(&mut self) {
        self.root = Link::Empty;
    }

    fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    fn lend_store(&self, hazards: Option<&Hazards>) -> Option<Lent> {
        let hazard = hazards.map(|hazards| hazards.entry(STORE));
        self.store.as_ref().map(|store| store.lend(hazard))
    }

    fn lend_root(&self, hazards: Option<&Hazards>) -> Option<Hold<K, V, S>> {
        let hazards = hazards?;
        let Link::Stored { memo, .. } = &self.root else {
            return None;
        };
        let Some(Memo::Pinned(node)) = memo.get() else {
            return None;
        };
        Some(Hold::lent(Shared::lend_to(node, hazards.entry(ROOT))))
    }

    fn walk<R>(&self, mut visit: impl FnMut(&Node<K, V, S>) -> Visit<R>) -> Result<Option<R>> {
        let store = self.store();
        let guard = epoch::pin();
        let guard = guard.as_ref();
        let mut depth = 0;
        let Some(mut at) = At::of(reach(store, &self.root, depth, guard)?) else {
            return Ok(None);
        };
        loop {
            depth += 1;
            let position = match visit(at.node()) {
                Visit::Done(result) => return Ok(Some(result)),
                Visit::Descend(position) => position,
            };
            let next = match &at {
                At::Ref(node) => At::of(reach(store, node.child(position), depth, guard)?),
                At::Shared(node) => At::owned(reach(store, node.child(position), depth, guard)?),
            };
            match next {
                Some(next) => at = next,
                None => return Ok(None),
            }
        }
    }

    pub fn try_get<Q>(&self, key: &Q) -> Result<Option<Val<V>>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.walk(|node| match node.search(key) {
            Ok(index) => Visit::Done(node.value(self.store(), index)),
            Err(position) => Visit::Descend(position),
        })?
        .transpose()
    }

    pub fn get<Q>(&self, key: &Q) -> Option<Val<V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        fatal(self.try_get(key))
    }

    pub fn value_hash<Q>(&self, key: &Q) -> Option<Hash>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let found = self.walk(|node| match node.search(key) {
            Ok(index) => Visit::Done(match &node.cells {
                Cells::Column(column) => try_hash_of(&column[index]),
                Cells::Slots(slots) => match &slots[index] {
                    Slot::Spilled(ptr, _) => Ok(ptr.hash),
                    held => held.held().map_or(Err(Error::Detached), try_hash_of),
                },
            }),
            Err(position) => Visit::Descend(position),
        });
        fatal(fatal(found).transpose())
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let found = self.walk(|node| match node.search(key) {
            Ok(_) => Visit::Done(()),
            Err(position) => Visit::Descend(position),
        });
        fatal(found).is_some()
    }

    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if !self.contains_key(key) {
            return None;
        }
        fatal(find_mut(self.store.as_ref(), &mut self.root, key))
    }

    pub fn get_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> &mut V {
        if !self.contains_key(&key) {
            self.insert(key.clone(), make());
        }
        self.get_mut(&key).expect("key was just inserted")
    }

    pub fn update<Q, R>(&mut self, key: &Q, f: impl FnOnce(&mut V) -> R) -> Option<R>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.get_mut(key).map(f)
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<Val<V>> {
        self.insert_shared(key, Arc::new(value))
    }

    pub(crate) fn insert_shared(&mut self, key: K, value: Arc<V>) -> Option<Val<V>> {
        let level = fatal(level_of(&key));
        let store = self.store.as_ref();
        let replaced = fatal(insert_at(store, &mut self.root, key, level, value));
        replaced.map(|slot| fatal(read_slot(store, &slot)))
    }

    pub(crate) fn try_insert(&mut self, key: K, value: V) -> Result<()> {
        let level = level_of(&key)?;
        insert_at(
            self.store.as_ref(),
            &mut self.root,
            key,
            level,
            Arc::new(value),
        )?;
        Ok(())
    }

    pub fn remove<Q>(&mut self, key: &Q) -> Option<Val<V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if !self.contains_key(key) {
            return None;
        }
        let store = self.store.as_ref();
        let removed = fatal(remove_at(store, &mut self.root, key));
        removed.map(|slot| fatal(read_slot(store, &slot)))
    }

    pub fn first(&self) -> Option<(K, Val<V>)> {
        self.iter().next()
    }

    pub fn last(&self) -> Option<(K, Val<V>)> {
        self.iter().next_back()
    }

    pub fn pop_first(&mut self) -> Option<(K, Val<V>)> {
        let (key, value) = self.first()?;
        self.remove(&key);
        Some((key, value))
    }

    pub fn pop_last(&mut self) -> Option<(K, Val<V>)> {
        let (key, value) = self.last()?;
        self.remove(&key);
        Some((key, value))
    }

    pub fn nth(&self, index: usize) -> Option<(K, Val<V>)> {
        let mut remaining = index as u64;
        let found = self.walk(|node| {
            for position in 0..=node.keys.len() {
                let size = node.child(position).count();
                if remaining < size {
                    return Visit::Descend(position);
                }
                remaining -= size;
                if position < node.keys.len() {
                    if remaining == 0 {
                        let value = node.value(self.store(), position);
                        return Visit::Done(Some((node.keys[position].clone(), value)));
                    }
                    remaining -= 1;
                }
            }
            Visit::Done(None)
        });
        fatal(found)
            .flatten()
            .map(|(key, value)| (key, fatal(value)))
    }

    pub fn range_by(
        &self,
        below: impl Fn(&K) -> bool,
        within: impl Fn(&K) -> bool,
    ) -> Iter<K, V, S> {
        let build = || -> Result<Iter<K, V, S>> {
            let store = self.store();
            let hazards = Hazards::new();
            let ahead = Slots::of(&hazards, FRONT);
            let behind = Slots::of(&hazards, BACK);
            let mut front = Vec::with_capacity(PATH_FRAMES);
            let mut back = Vec::with_capacity(PATH_FRAMES);
            let mut remaining = 0;
            let mut depth = 0;
            let root = match reach(store, &self.root, depth, None)? {
                Reach::Absent => None,
                Reach::Borrowed(node) => self
                    .lend_root(hazards.as_ref())
                    .or_else(|| Some(Hold::owned(node.clone()))),
                Reach::Owned(node) => Some(Hold::owned(node)),
                Reach::Held(_) => unreachable!("an unguarded reach holds no node"),
            };
            let mut current = root
                .as_deref()
                .map(|root| (Hold::anchored_at(root), Hold::anchored_at(root)));
            while let Some((node, twin)) = current.take() {
                let low = node.keys.partition_point(|key| below(key));
                let high = node.keys.partition_point(|key| within(key));
                let anchored = node.anchored();
                depth += 1;
                if low >= high {
                    if low == high {
                        current = twins(store, (&node, &twin), low, depth, (ahead, behind))?;
                    }
                    back.push(Frame {
                        node: twin,
                        position: 2 * high as isize - 1,
                    });
                    front.push(Frame {
                        node,
                        position: 2 * low as isize + 1,
                    });
                    continue;
                }
                back.push(Frame {
                    node: twin.share(),
                    position: 2 * high as isize - 1,
                });
                front.push(Frame {
                    node: node.share(),
                    position: 2 * low as isize + 1,
                });
                let inner = (low + 1..high).map(|position| node.child(position).count());
                let skipped = edge(
                    store,
                    (node.child(low), node.level),
                    anchored,
                    depth,
                    &below,
                    1,
                    &mut front,
                    ahead,
                )?;
                let kept = edge(
                    store,
                    (twin.child(high), twin.level),
                    twin.anchored(),
                    depth,
                    &within,
                    -1,
                    &mut back,
                    behind,
                )?;
                remaining = (high - low) as u64
                    + inner.sum::<u64>()
                    + kept
                    + node.child(low).count().saturating_sub(skipped);
            }
            Ok(Iter {
                store: self.lend_store(hazards.as_ref()),
                front,
                back,
                hazards,
                _root: root,
                remaining,
            })
        };
        fatal(build())
    }

    pub fn iter(&self) -> Iter<K, V, S> {
        self.range_by(|_| false, |_| true)
    }

    fn top(&self, hazards: Option<&Hazards>) -> Top<K, V, S> {
        match self.lend_root(hazards) {
            Some(root) => Top::Lent(root),
            None => Top::Link(self.root.with_memo()),
        }
    }

    pub(crate) fn finder(&self) -> Finder<K, V, S> {
        let hazards = Hazards::new();
        Finder {
            store: self.lend_store(hazards.as_ref()),
            leaf: None,
            parent: None,
            root: self.top(hazards.as_ref()),
            hazards,
        }
    }

    pub fn cursor(&self) -> Cursor<K, V, S> {
        let hazards = Hazards::new();
        Cursor {
            store: self.lend_store(hazards.as_ref()),
            path: Vec::with_capacity(PATH_FRAMES),
            root: self.top(hazards.as_ref()),
            hazards,
            index: None,
        }
    }

    pub fn floor<Q>(&self, key: &Q) -> Option<(K, Val<V>)>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        fatal(self.try_floor(key))
    }

    fn try_floor<Q>(&self, key: &Q) -> Result<Option<(K, Val<V>)>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let store = self.store();
        let entry = |node: &Node<K, V, S>, index: usize| -> Result<(K, Val<V>)> {
            Ok((node.keys[index].clone(), node.value(store, index)?))
        };
        let guard = epoch::pin();
        let guard = guard.as_ref();
        let mut depth = 0;
        let mut best = None;
        let mut current = At::of(reach(store, &self.root, depth, guard)?);
        while let Some(at) = current {
            depth += 1;
            let position = match at.node().search(key) {
                Ok(index) => return entry(at.node(), index).map(Some),
                Err(position) => position,
            };
            current = match &at {
                At::Ref(node) => At::of(reach(store, node.child(position), depth, guard)?),
                At::Shared(node) => At::owned(reach(store, node.child(position), depth, guard)?),
            };
            if position > 0 {
                best = Some((at, position - 1));
            }
        }
        match best {
            Some((at, index)) => entry(at.node(), index).map(Some),
            None => Ok(None),
        }
    }

    pub fn get_many<I>(
        &self,
        keys: I,
    ) -> impl Iterator<Item = (I::Item, Option<Val<V>>)> + use<I, K, V, S>
    where
        I: IntoIterator,
        I::Item: Borrow<K>,
    {
        let mut cursor = self.cursor();
        keys.into_iter().map(move |key| {
            let value = cursor.seek(key.borrow()).then(|| cursor.value()).flatten();
            (key, value)
        })
    }

    pub fn contains_many<I>(
        &self,
        keys: I,
    ) -> impl Iterator<Item = (I::Item, bool)> + use<I, K, V, S>
    where
        I: IntoIterator,
        I::Item: Borrow<K>,
    {
        let mut cursor = self.cursor();
        keys.into_iter().map(move |key| {
            let found = cursor.descend(key.borrow());
            (key, found)
        })
    }

    pub fn for_each(&self, mut f: impl FnMut(&K, &V)) {
        let _ = self.try_for_each(|key, value| -> ControlFlow<()> {
            f(key, value);
            ControlFlow::Continue(())
        });
    }

    pub fn try_for_each<B>(&self, mut f: impl FnMut(&K, &V) -> ControlFlow<B>) -> ControlFlow<B> {
        fatal(scan(self.store(), &self.root, 0, None, None, &mut f))
    }

    fn try_walk(&self, mut visit: impl FnMut(&Node<K, V, S>, usize) -> Result<()>) -> Result<()> {
        let store = self.store();
        let mut path = Vec::new();
        if let Some(root) = resident(store, &self.root, 0)? {
            path.push((root, 0));
        }
        loop {
            let depth = path.len();
            let Some((node, position)) = path.last_mut() else {
                break;
            };
            let at = *position;
            if at > 2 * node.keys.len() {
                path.pop();
                continue;
            }
            *position += 1;
            if at % 2 == 1 {
                visit(node, at / 2)?;
                continue;
            }
            if let Some(child) = resident(store, node.child(at / 2), depth)? {
                path.push((child, 0));
            }
        }
        Ok(())
    }

    pub(crate) fn try_entries(&self) -> Result<Vec<(K, V)>> {
        let store = self.store();
        let mut entries = Vec::new();
        self.try_walk(|node, index| {
            entries.push((node.keys[index].clone(), node.read(store, index, V::clone)?));
            Ok(())
        })?;
        Ok(entries)
    }

    pub(crate) fn try_values(&self) -> Result<Vec<Val<V>>> {
        let store = self.store();
        let mut values = Vec::new();
        self.try_walk(|node, index| {
            values.push(node.value(store, index)?);
            Ok(())
        })?;
        Ok(values)
    }

    pub fn for_each_range<Q, R>(&self, range: R, mut f: impl FnMut(&K, &V))
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        let _ = self.try_for_each_range::<Q, R, ()>(range, |key, value| {
            f(key, value);
            ControlFlow::Continue(())
        });
    }

    pub fn try_for_each_range<Q, R, B>(
        &self,
        range: R,
        mut f: impl FnMut(&K, &V) -> ControlFlow<B>,
    ) -> ControlFlow<B>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        let start = range.start_bound();
        let end = range.end_bound();
        let below = |key: &K| below(key, start);
        let within = |key: &K| within(key, end);
        fatal(scan(
            self.store(),
            &self.root,
            0,
            Some(&below),
            Some(&within),
            &mut f,
        ))
    }

    pub fn keys(&self) -> Keys<K, V, S> {
        self.iter().into_keys()
    }

    pub fn values(&self) -> Values<K, V, S> {
        Values(self.iter())
    }

    pub fn range<Q, R>(&self, range: R) -> Iter<K, V, S>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        let start = range.start_bound();
        let end = range.end_bound();
        self.range_by(|key| below(key, start), |key| within(key, end))
    }

    pub fn summary(&self) -> Option<S> {
        self.root.summary()
    }

    pub fn summary_by(&self, below: impl Fn(&K) -> bool, within: impl Fn(&K) -> bool) -> Option<S> {
        let guard = epoch::pin();
        let store = self.store();
        fatal(gather(
            store,
            guard.as_ref(),
            &self.root,
            0,
            Some(&below),
            Some(&within),
            None,
        ))
    }

    pub fn range_summary<Q, R>(&self, range: R) -> Option<S>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        let start = range.start_bound();
        let end = range.end_bound();
        self.summary_by(|key| below(key, start), |key| within(key, end))
    }

    pub fn prefix<Q>(&self, head: &Q) -> Iter<K, V, S>
    where
        K: KeyPrefix<Q>,
        Q: Ord + ?Sized,
    {
        self.range_by(|key| key.head() < head, |key| key.head() <= head)
    }

    pub fn prefix_summary<Q>(&self, head: &Q) -> Option<S>
    where
        K: KeyPrefix<Q>,
        Q: Ord + ?Sized,
    {
        self.summary_by(|key| key.head() < head, |key| key.head() <= head)
    }

    pub fn take_range<Q, R>(&mut self, range: R) -> Map<K, V, S>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        let start = range.start_bound();
        let end = range.end_bound();
        let empty = self
            .range_by(|key| below(key, start), |key| within(key, end))
            .len()
            == 0;
        let mut taken = Map {
            root: Link::Empty,
            store: self.store.clone(),
        };
        if empty {
            return taken;
        }
        let store = self.store.as_ref();
        let (kept, rest) = fatal(split(store, self.root.take(), &|key| {
            left_if(below(key, start))
        }));
        let (middle, tail) = fatal(split(store, rest, &|key| left_if(within(key, end))));
        self.root = fatal(merge(store, kept, tail));
        taken.root = middle;
        taken
    }

    pub fn remove_range<Q, R>(&mut self, range: R) -> usize
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        self.take_range::<Q, R>(range).len()
    }

    pub fn split_off<Q>(&mut self, key: &Q) -> Map<K, V, S>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let start = Bound::Included(key);
        let store = self.store.as_ref();
        let (kept, tail) = fatal(split(store, self.root.take(), &|probe| {
            left_if(below(probe, start))
        }));
        self.root = kept;
        Map {
            root: tail,
            store: self.store.clone(),
        }
    }

    pub fn append(&mut self, other: &mut Map<K, V, S>) {
        if other.is_empty() {
            return;
        }
        if self.is_empty() {
            std::mem::swap(self, other);
            other.clear();
            return;
        }
        let joinable = self
            .store
            .as_ref()
            .zip(other.store.as_ref())
            .is_none_or(|(ours, theirs)| ours.id() == theirs.id());
        let ascending = self.keys().next_back() < other.keys().next();
        let descending = other.keys().next_back() < self.keys().next();
        if joinable && (ascending || descending) {
            let store = self.store.clone().or_else(|| other.store.clone());
            let (low, high) = if ascending {
                (self.root.take(), other.root.take())
            } else {
                (other.root.take(), self.root.take())
            };
            self.root = fatal(merge(store.as_ref(), low, high));
            self.store = store;
            return;
        }
        self.batch(
            other
                .iter()
                .map(|(key, value)| (key, Some(value.into_arc())))
                .collect(),
        );
        other.clear();
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let doomed: Vec<(K, Option<Arc<V>>)> = self
            .iter()
            .filter(|(key, value)| !keep(key, value))
            .map(|(key, _)| (key, None))
            .collect();
        self.batch(doomed);
    }

    pub fn apply(&mut self, edits: impl IntoIterator<Item = (K, Option<V>)>) {
        self.batch(
            edits
                .into_iter()
                .map(|(key, value)| (key, value.map(Arc::new)))
                .collect(),
        );
    }

    fn batch(&mut self, items: Vec<(K, Option<Arc<V>>)>) {
        let items = sorted_unique(items);
        let count = items.len();
        let levels = levels(&items);
        let mut edits: vec::IntoIter<Edit<K, V>> = items
            .into_iter()
            .zip(levels)
            .map(|((key, value), level)| Edit { key, level, value })
            .collect::<Vec<_>>()
            .into_iter();
        let store = self.store.as_ref();
        fatal(absorb(store, &mut self.root, &mut edits, count));
    }

    pub fn diff(&self, newer: &Map<K, V, S>) -> Diff<K, V, S> {
        Diff {
            old_store: self.store.clone(),
            new_store: newer.store.clone(),
            old: vec![Step::Sub(self.root.clone())],
            new: vec![Step::Sub(newer.root.clone())],
        }
    }

    pub fn merge(
        &mut self,
        base: &Map<K, V, S>,
        theirs: &Map<K, V, S>,
        mut resolve: impl FnMut(Conflict<K, V>) -> Option<V>,
    ) -> usize {
        let Ok(written) = self.try_merge(base, theirs, |conflict| {
            Ok::<_, Infallible>(resolve(conflict))
        });
        written
    }

    pub fn try_merge<E>(
        &mut self,
        base: &Map<K, V, S>,
        theirs: &Map<K, V, S>,
        resolve: impl FnMut(Conflict<K, V>) -> std::result::Result<Option<V>, E>,
    ) -> std::result::Result<usize, E> {
        let kept = self.clone();
        let merged = three_way(
            self,
            base,
            theirs,
            |map, key| map.get(key),
            |map, key, value| match value {
                Some(value) => {
                    map.insert(key, value);
                }
                None => {
                    map.remove(&key);
                }
            },
            resolve,
        );
        if merged.is_err() {
            *self = kept;
        }
        merged
    }

    pub fn root_hash(&self) -> Option<Hash> {
        match &self.root {
            Link::Empty => None,
            Link::Stored { ptr, .. } => Some(ptr.hash),
            Link::Mem(..) => {
                let mut sink = HashSink;
                match flush(&self.root, &mut sink, 0, false) {
                    Ok(Link::Stored { ptr, .. }) => Some(ptr.hash),
                    Ok(_) => None,
                    Err(error) => panic!("ferro: {error}"),
                }
            }
        }
    }

    fn rebuilt(&self) -> Map<K, V, S> {
        self.iter()
            .map(|(key, value)| (key, (*value).clone()))
            .collect()
    }
}

impl<K: Data + Ord, V: Data, S: Summary<K, V>> Data for Map<K, V, S> {
    const LOCATED: bool = K::LOCATED || V::LOCATED;

    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        let target = w.store_id();
        let foreign = target != 0
            && self
                .store
                .as_ref()
                .is_some_and(|store| store.id() != target);
        if foreign || matches!(self.root, Link::Mem(..)) {
            w.mark_unsettled();
        }
        let stored = if foreign {
            flush(&self.rebuilt().root, w.sink(), 0, false)?
        } else {
            flush(&self.root, w.sink(), 0, false)?
        };
        match stored {
            Link::Empty => w.varint(0),
            Link::Stored {
                ptr,
                count,
                summary,
                ..
            } => {
                w.varint(count);
                summary.encode(w)?;
                w.link(ptr);
            }
            Link::Mem(..) => return Err(Error::Detached),
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let count = r.varint()?;
        let root = if count == 0 {
            Link::Empty
        } else {
            let summary = S::decode(r)?;
            Link::stored(
                r.typed_link(|| node_kind::<K, V, S>(count, None, None))?,
                count,
                summary,
            )
        };
        Ok(Map {
            root,
            store: r.store(),
        })
    }

    fn missing() -> Option<Self> {
        Some(Map::default())
    }

    fn describe(d: &mut Description) {
        d.word("map");
        d.of::<K>();
        d.of::<V>();
        d.of::<S>();
    }

    fn same_encoding(&self, other: &Self) -> bool {
        self.len() == other.len()
            && (self.root.same_as(&other.root) || self.root_hash() == other.root_hash())
    }
}

impl<K: Data + Ord, V: Data, S: Summary<K, V>> PartialEq for Map<K, V, S> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.root_hash() == other.root_hash()
    }
}

impl<K: Data + Ord, V: Data, S: Summary<K, V>> Eq for Map<K, V, S> {}

impl<K, V, S> fmt::Debug for Map<K, V, S>
where
    K: Data + Ord + fmt::Debug,
    V: Data + fmt::Debug,
    S: Summary<K, V>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K, V, S> Drop for Map<K, V, S> {
    fn drop(&mut self) {
        if let Link::Mem(root, _) = self.root.take()
            && root.count >= TEARDOWN_KEYS
            && scattered(&root)
        {
            teardown(root);
        }
    }
}

fn scattered<K, V, S>(root: &Node<K, V, S>) -> bool {
    let (mut rising, mut falling) = (0, 0);
    for path in 0..TEARDOWN_PATHS {
        let mut node = root;
        loop {
            if let Cells::Slots(slots) = &node.cells {
                let mut last = None;
                for slot in slots {
                    if let Slot::Fresh(value) = slot {
                        let at = Arc::as_ptr(value).addr();
                        match last {
                            Some(before) if at > before => rising += 1,
                            Some(_) => falling += 1,
                            None => {}
                        }
                        last = Some(at);
                    }
                }
            }
            let position = node.children.len() * path / TEARDOWN_PATHS;
            match node.children.get(position) {
                Some(Link::Mem(child, _)) => node = child,
                _ => break,
            }
        }
    }
    falling * 4 > rising
}

fn teardown<K, V, S>(root: Shared<Node<K, V, S>>) {
    let mut nodes = vec![root];
    let mut values = Vec::new();
    let mut keys = Vec::new();
    let mut slots = Vec::new();
    let mut children = Vec::new();
    let mut index = 0;
    while index < nodes.len() {
        if let Some(ahead) = nodes.get(index + TEARDOWN_AHEAD) {
            let base = Shared::as_ptr(ahead).addr();
            prefetch(base);
            prefetch(base + LINE_BYTES);
        }
        if let Some(ahead) = nodes.get(index + TEARDOWN_AHEAD / 2) {
            prefetch_items(&ahead.children);
            if let Cells::Slots(held) = &ahead.cells {
                prefetch_items(held);
            }
        }
        let Some(node) = Shared::get_mut(&mut nodes[index]) else {
            index += 1;
            continue;
        };
        keys.push(std::mem::take(&mut node.keys));
        let mut links = std::mem::take(&mut node.children);
        let cells = std::mem::replace(&mut node.cells, Cells::Slots(Vec::new()));
        for link in &mut links {
            if let Link::Mem(..) = link
                && let Link::Mem(child, _) = link.take()
            {
                nodes.push(child);
            }
        }
        children.push(links);
        if let Cells::Slots(mut held) = cells {
            values.extend(held.drain(..).filter_map(|slot| match slot {
                Slot::Fresh(value) => Some(value),
                _ => None,
            }));
            slots.push(held);
        }
        index += 1;
    }
    values.sort_unstable_by_key(|value| Arc::as_ptr(value).addr());
    keys.sort_unstable_by_key(|keys| keys.as_ptr().addr());
    slots.sort_unstable_by_key(|slots| slots.as_ptr().addr());
    children.sort_unstable_by_key(|children| children.as_ptr().addr());
    nodes.sort_unstable_by_key(|node| Shared::as_ptr(node).addr());
    let mut values = values.into_iter().peekable();
    let mut keys = keys.into_iter().peekable();
    let mut slots = slots.into_iter().peekable();
    let mut children = children.into_iter().peekable();
    let mut nodes = nodes.into_iter().peekable();
    loop {
        let heads = [
            values
                .peek()
                .map_or(usize::MAX, |value| Arc::as_ptr(value).addr()),
            keys.peek().map_or(usize::MAX, |keys| keys.as_ptr().addr()),
            slots
                .peek()
                .map_or(usize::MAX, |slots| slots.as_ptr().addr()),
            children
                .peek()
                .map_or(usize::MAX, |children| children.as_ptr().addr()),
            nodes
                .peek()
                .map_or(usize::MAX, |node| Shared::as_ptr(node).addr()),
        ];
        let lowest = heads.iter().copied().min().unwrap_or(usize::MAX);
        if lowest == usize::MAX {
            break;
        }
        match heads.iter().position(|&head| head == lowest) {
            Some(0) => drop(values.next()),
            Some(1) => drop(keys.next()),
            Some(2) => drop(slots.next()),
            Some(3) => drop(children.next()),
            _ => drop(nodes.next()),
        }
    }
}

impl<K: Data + Ord, V: Data, S: Summary<K, V>> Extend<(K, V)> for Map<K, V, S> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, items: I) {
        if !self.is_empty() {
            self.apply(items.into_iter().map(|(key, value)| (key, Some(value))));
            return;
        }
        let items = sorted_unique(items.into_iter().collect());
        let levels = levels(&items);
        self.root = build_sorted(
            items
                .into_iter()
                .zip(levels)
                .map(|((key, value), level)| Edit {
                    key,
                    level,
                    value: Some(Arc::new(value)),
                }),
        );
    }
}

impl<K: Data + Ord, V: Data, S: Summary<K, V>> FromIterator<(K, V)> for Map<K, V, S> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(items: I) -> Self {
        let mut map = Map::default();
        map.extend(items);
        map
    }
}

impl<K: Data + Ord, V: Data, S: Summary<K, V>> IntoIterator for &Map<K, V, S> {
    type Item = (K, Val<V>);
    type IntoIter = Iter<K, V, S>;

    fn into_iter(self) -> Iter<K, V, S> {
        self.iter()
    }
}

struct Hold<K, V, S> {
    tagged: NonNull<Node<K, V, S>>,
    _owns: PhantomData<Shared<Node<K, V, S>>>,
}

unsafe impl<K, V, S> Send for Hold<K, V, S> where Shared<Node<K, V, S>>: Send {}

unsafe impl<K, V, S> Sync for Hold<K, V, S> where Shared<Node<K, V, S>>: Sync {}

impl<K, V, S> Deref for Hold<K, V, S> {
    type Target = Node<K, V, S>;

    #[inline]
    fn deref(&self) -> &Node<K, V, S> {
        unsafe { &*self.node() }
    }
}

impl<K, V, S> Drop for Hold<K, V, S> {
    #[inline]
    fn drop(&mut self) {
        if !self.anchored() {
            drop(unsafe { Shared::from_raw(self.node()) });
        }
    }
}

impl<K, V, S> Hold<K, V, S> {
    #[inline]
    fn owned(node: Shared<Node<K, V, S>>) -> Self {
        Hold {
            tagged: unsafe { NonNull::new_unchecked(Shared::into_raw(node).cast_mut()) },
            _owns: PhantomData,
        }
    }

    #[inline]
    fn anchored_at(node: &Node<K, V, S>) -> Self {
        Hold::lent(NonNull::from(node))
    }

    #[inline]
    fn lent(node: NonNull<Node<K, V, S>>) -> Self {
        Hold {
            tagged: node.map_addr(|addr| addr | 1),
            _owns: PhantomData,
        }
    }

    #[inline]
    fn node(&self) -> *const Node<K, V, S> {
        self.tagged.as_ptr().map_addr(|addr| addr & !1).cast_const()
    }

    #[inline]
    fn anchored(&self) -> bool {
        self.tagged.addr().get() & 1 == 1
    }

    #[inline]
    fn share(&self) -> Self {
        if !self.anchored() {
            unsafe { Shared::increment_strong_count(self.node()) };
        }
        Hold {
            tagged: self.tagged,
            _owns: PhantomData,
        }
    }
}

#[derive(Clone, Copy)]
struct Slots<'a> {
    hazards: &'a Option<Hazards>,
    base: usize,
}

impl<'a> Slots<'a> {
    #[inline]
    fn of(hazards: &'a Option<Hazards>, base: usize) -> Self {
        Slots { hazards, base }
    }

    #[inline]
    fn at(self, level: u8) -> Option<&'a AtomicPtr<()>> {
        let index = usize::from(level);
        let hazards = self.hazards.as_ref()?;
        (index < SPAN).then(|| hazards.entry(self.base + index))
    }

    #[inline]
    fn below(self, parent: u8) -> Option<&'a AtomicPtr<()>> {
        self.at(parent.wrapping_sub(1))
    }
}

#[inline(always)]
fn hold<K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    link: &Link<K, V, S>,
    depth: usize,
    anchored: bool,
    hazard: Option<&AtomicPtr<()>>,
) -> Result<Option<Hold<K, V, S>>> {
    if let Some(hazard) = hazard
        && let Link::Stored { memo, .. } = link
        && let Some(Memo::Weak(weak, hint)) = memo.get()
    {
        hint.prefetch(weak);
        prefetch(weak.flags_addr());
        if let Some(node) = weak.lend_to(hazard) {
            return Ok(Some(Hold::lent(node)));
        }
    }
    Ok(match reach(store, link, depth, None)? {
        Reach::Absent => None,
        Reach::Borrowed(node) if anchored => Some(Hold::anchored_at(node)),
        Reach::Borrowed(node) => Some(Hold::owned(node.clone())),
        Reach::Owned(node) => Some(Hold::owned(node)),
        Reach::Held(_) => unreachable!("an unguarded reach holds no node"),
    })
}

struct Frame<K, V, S> {
    node: Hold<K, V, S>,
    position: isize,
}

#[inline]
fn step<K: Data, V: Data, S: Data>(
    store: &Option<Lent>,
    path: &mut Vec<Frame<K, V, S>>,
    slots: Slots<'_>,
) -> Option<usize> {
    loop {
        let depth = path.len();
        let frame = path.last_mut()?;
        if frame.position > 2 * frame.node.keys.len() as isize {
            path.pop();
            continue;
        }
        let position = frame.position;
        if position % 2 == 1 {
            frame.position += if frame.node.children.is_empty() { 2 } else { 1 };
            return Some((position / 2) as usize);
        }
        frame.position += 1;
        enter(store, path, (position / 2) as usize, depth, slots);
    }
}

#[inline(never)]
fn enter<K: Data, V: Data, S: Data>(
    store: &Option<Lent>,
    path: &mut Vec<Frame<K, V, S>>,
    position: usize,
    depth: usize,
    slots: Slots<'_>,
) {
    let Some(frame) = path.last() else {
        return;
    };
    let child = frame.node.child(position);
    let anchored = frame.node.anchored();
    let hazard = slots.below(frame.node.level);
    if let Some(node) = fatal(hold(store.as_deref(), child, depth, anchored, hazard)) {
        path.push(Frame { node, position: 0 });
    }
}

type Twins<K, V, S> = (Hold<K, V, S>, Hold<K, V, S>);

fn twins<K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    (node, twin): (&Hold<K, V, S>, &Hold<K, V, S>),
    position: usize,
    depth: usize,
    (ahead, behind): (Slots<'_>, Slots<'_>),
) -> Result<Option<Twins<K, V, S>>> {
    let hazard = ahead.below(node.level);
    let Some(next) = hold(store, node.child(position), depth, node.anchored(), hazard)? else {
        return Ok(None);
    };
    if !next.anchored() {
        let other = next.share();
        return Ok(Some((next, other)));
    }
    let hazard = behind.below(twin.level);
    let other = hold(store, twin.child(position), depth, twin.anchored(), hazard)?;
    Ok(other.map(|other| (next, other)))
}

#[allow(clippy::too_many_arguments)]
fn edge<K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    (link, above): (&Link<K, V, S>, u8),
    anchored: bool,
    mut depth: usize,
    before: &impl Fn(&K) -> bool,
    offset: isize,
    path: &mut Vec<Frame<K, V, S>>,
    slots: Slots<'_>,
) -> Result<u64> {
    let mut rank = 0u64;
    let mut current = hold(store, link, depth, anchored, slots.below(above))?;
    while let Some(node) = current {
        let position = node.keys.partition_point(|key| before(key));
        rank += position as u64;
        rank += node
            .children
            .iter()
            .take(position)
            .map(Link::count)
            .sum::<u64>();
        depth += 1;
        current = hold(
            store,
            node.child(position),
            depth,
            node.anchored(),
            slots.below(node.level),
        )?;
        path.push(Frame {
            node,
            position: 2 * position as isize + offset,
        });
    }
    Ok(rank)
}

enum Top<K, V, S> {
    Lent(Hold<K, V, S>),
    Link(Link<K, V, S>),
}

impl<K: Data, V: Data, S: Data> Top<K, V, S> {
    fn hold(&self, store: Option<&Store>) -> Option<Hold<K, V, S>> {
        match self {
            Top::Lent(root) => Some(Hold::anchored_at(root)),
            Top::Link(link) => fatal(hold(store, link, 0, true, None)),
        }
    }
}

pub struct Cursor<K, V, S = ()> {
    store: Option<Lent>,
    path: Vec<Frame<K, V, S>>,
    hazards: Option<Hazards>,
    root: Top<K, V, S>,
    index: Option<usize>,
}

impl<K: Data + Ord, V: Data, S: Data> Cursor<K, V, S> {
    pub fn rewind(&mut self) {
        self.path.clear();
        if let Some(node) = self.root.hold(self.store.as_deref()) {
            self.path.push(Frame { node, position: 0 });
        }
        let slots = Slots::of(&self.hazards, FRONT);
        self.index = step(&self.store, &mut self.path, slots);
    }

    fn climb<Q>(&mut self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        while let [.., _, frame] = self.path.as_slice() {
            let keys = &frame.node.keys;
            let inside = keys.first().is_some_and(|first| first.borrow() <= key)
                && keys.last().is_some_and(|last| key <= last.borrow());
            if inside {
                return;
            }
            self.path.pop();
        }
        if self.path.is_empty()
            && let Some(node) = self.root.hold(self.store.as_deref())
        {
            self.path.push(Frame { node, position: 0 });
        }
    }

    fn descend<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.climb(key);
        let store = self.store.as_deref();
        let slots = Slots::of(&self.hazards, FRONT);
        loop {
            let depth = self.path.len();
            let Some(frame) = self.path.last_mut() else {
                return false;
            };
            match frame.node.search(key) {
                Ok(index) => {
                    frame.position = 2 * index as isize + 2;
                    self.index = Some(index);
                    return true;
                }
                Err(position) => {
                    frame.position = 2 * position as isize + 1;
                    let anchored = frame.node.anchored();
                    let child = frame.node.child(position);
                    let hazard = slots.below(frame.node.level);
                    match fatal(hold(store, child, depth, anchored, hazard)) {
                        Some(node) => self.path.push(Frame { node, position: 0 }),
                        None => return false,
                    }
                }
            }
        }
    }

    pub fn seek<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if self.descend(key) {
            return true;
        }
        let slots = Slots::of(&self.hazards, FRONT);
        self.index = step(&self.store, &mut self.path, slots);
        false
    }

    pub fn seek_back<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if self.descend(key) {
            return true;
        }
        self.index = None;
        while let Some(frame) = self.path.last_mut() {
            let position = (frame.position / 2) as usize;
            if position > 0 {
                frame.position = 2 * position as isize;
                self.index = Some(position - 1);
                break;
            }
            self.path.pop();
        }
        false
    }

    pub fn advance(&mut self) -> Option<&K> {
        self.index?;
        let slots = Slots::of(&self.hazards, FRONT);
        self.index = step(&self.store, &mut self.path, slots);
        self.key()
    }

    pub fn key(&self) -> Option<&K> {
        let index = self.index?;
        self.path.last().map(|frame| &frame.node.keys[index])
    }

    pub fn value(&self) -> Option<Val<V>> {
        let index = self.index?;
        self.path
            .last()
            .map(|frame| fatal(frame.node.value(self.store.as_deref(), index)))
    }

    pub fn read<R>(&self, f: impl FnOnce(&K, &V) -> R) -> Option<R> {
        let index = self.index?;
        let node = &self.path.last()?.node;
        Some(fatal(node.read(self.store.as_deref(), index, |value| {
            f(&node.keys[index], value)
        })))
    }
}

pub(crate) struct Finder<K, V, S = ()> {
    store: Option<Lent>,
    leaf: Option<Hold<K, V, S>>,
    parent: Parent<K, V, S>,
    hazards: Option<Hazards>,
    root: Top<K, V, S>,
}

type Parent<K, V, S> = Option<(Hold<K, V, S>, usize)>;

type Found<K, V, S> = (Option<Val<V>>, Option<Hold<K, V, S>>, Parent<K, V, S>);

fn strong<K, V, S>(link: &Link<K, V, S>) -> Option<Shared<Node<K, V, S>>> {
    match link {
        Link::Empty => None,
        Link::Mem(node, _) => Some(node.clone()),
        Link::Stored { memo, .. } => match memo.get()? {
            Memo::Pinned(node) => Some(node.clone()),
            Memo::Weak(node, _) => node.upgrade(),
        },
    }
}

fn kept<K: Data, V: Data, S: Data>(
    at: &At<'_, K, V, S>,
    link: &Link<K, V, S>,
    hazard: Option<&AtomicPtr<()>>,
) -> Option<Hold<K, V, S>> {
    let hazard = match (at, hazard) {
        (At::Shared(node), _) => return Some(Hold::owned(node.clone())),
        (At::Ref(_), None) => return strong(link).map(Hold::owned),
        (At::Ref(_), Some(hazard)) => hazard,
    };
    let node = match link {
        Link::Empty => return None,
        Link::Mem(node, _) => Shared::lend_to(node, hazard),
        Link::Stored { memo, .. } => match memo.get()? {
            Memo::Pinned(node) => Shared::lend_to(node, hazard),
            // SAFETY: `at` borrows this node under the descent's guard, which is still pinned
            // and keeps it alive until the flag is set.
            Memo::Weak(node, _) => unsafe { node.lend_held(hazard) },
        },
    };
    Some(Hold::lent(node))
}

fn spans<K, V, S, Q>(node: &Node<K, V, S>, key: &Q) -> bool
where
    K: Borrow<Q>,
    Q: Ord + ?Sized,
{
    node.keys.first().is_some_and(|first| first.borrow() <= key)
        && node.keys.last().is_some_and(|last| key <= last.borrow())
}

impl<K: Data + Ord, V: Data, S: Data> Finder<K, V, S> {
    pub(crate) fn get<Q>(&mut self, key: &Q) -> Option<Val<V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if let Some(leaf) = &self.leaf
            && spans(leaf, key)
        {
            let index = leaf.search(key).ok()?;
            return Some(fatal(leaf.value(self.store.as_deref(), index)));
        }
        let held_leaf = self.leaf.take();
        let held_parent = self.parent.take();
        let start = match &held_parent {
            Some((parent, depth)) if spans(parent, key) => Some((&**parent, *depth)),
            _ => None,
        };
        let slots = self
            .hazards
            .as_ref()
            .map(|hazards| (hazards.entry(LEAF), hazards.entry(PARENT)));
        let (found, leaf, parent) = fatal(self.descend(start, key, slots));
        self.leaf = leaf.or(held_leaf);
        self.parent = parent.or(held_parent);
        found
    }

    fn descend<Q>(
        &self,
        start: Option<(&Node<K, V, S>, usize)>,
        key: &Q,
        slots: Option<(&AtomicPtr<()>, &AtomicPtr<()>)>,
    ) -> Result<Found<K, V, S>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let store = self.store.as_deref();
        let guard = epoch::pin();
        let guard = guard.as_ref();
        let top = match (start, &self.root) {
            (Some((node, depth)), _) => Some((At::Ref(node), depth)),
            (None, Top::Lent(root)) => Some((At::Ref(&**root), 0)),
            (None, Top::Link(link)) => At::of(reach(store, link, 0, guard)?).map(|at| (at, 0)),
        };
        let Some((mut at, mut depth)) = top else {
            return Ok((None, None, None));
        };
        let mut leaf = None;
        let mut parent = None;
        loop {
            depth += 1;
            let node = at.node();
            let position = match node.search(key) {
                Ok(index) => return Ok((Some(node.value(store, index)?), leaf, parent)),
                Err(_) if node.children.is_empty() => return Ok((None, leaf, parent)),
                Err(position) => position,
            };
            let link = node.child(position);
            let next = match &at {
                At::Ref(node) => At::of(reach(store, node.child(position), depth, guard)?),
                At::Shared(_) => At::owned(reach(store, link, depth, guard)?),
            };
            let Some(next) = next else {
                return Ok((None, leaf, parent));
            };
            let child = next.node();
            if child.children.is_empty() {
                leaf = kept(&next, link, slots.map(|(leaf, _)| leaf));
            } else if child.level <= 1 {
                parent =
                    kept(&next, link, slots.map(|(_, parent)| parent)).map(|node| (node, depth));
            }
            at = next;
        }
    }
}

enum Run<'a, V, S> {
    Column(&'a Arc<[V]>, &'a [V], usize),
    Slots(&'a [Slot<V, S>]),
}

impl<'a, V: Data, S> Run<'a, V, S> {
    #[inline]
    fn of(cells: &'a Cells<V, S>, run: Range<usize>) -> Self {
        match cells {
            Cells::Column(column) => Run::Column(column, &column[run.clone()], run.start),
            Cells::Slots(slots) => Run::Slots(&slots[run]),
        }
    }

    #[inline(always)]
    fn value(&self, store: Option<&Store>, offset: usize) -> Val<V> {
        match self {
            Run::Column(_, values, _) if by_copy::<V>() => {
                Val(Held::Copied(ManuallyDrop::new(values[offset].clone())))
            }
            Run::Column(column, _, start) => {
                Val(Held::Column(Arc::clone(column), (start + offset) as u32))
            }
            Run::Slots(slots) => fatal(read_slot(store, &slots[offset])),
        }
    }
}

pub struct Iter<K, V, S = ()> {
    store: Option<Lent>,
    front: Vec<Frame<K, V, S>>,
    back: Vec<Frame<K, V, S>>,
    hazards: Option<Hazards>,
    _root: Option<Hold<K, V, S>>,
    remaining: u64,
}

impl<K: Data + Ord, V: Data, S: Data> Iter<K, V, S> {
    pub(crate) fn into_keys(self) -> Keys<K, V, S> {
        Keys(self)
    }

    pub(crate) fn into_values(self) -> Values<K, V, S> {
        Values(self)
    }

    #[inline]
    fn advance(&mut self) -> Option<usize> {
        if self.remaining == 0 {
            return None;
        }
        let slots = Slots::of(&self.hazards, FRONT);
        let index = step(&self.store, &mut self.front, slots)?;
        self.remaining -= 1;
        Some(index)
    }

    fn drain<B>(
        &mut self,
        init: B,
        mut visit: impl FnMut(B, Option<&Store>, &Node<K, V, S>, Range<usize>) -> B,
    ) -> B {
        let mut total = init;
        let store = self.store.as_deref();
        let slots = Slots::of(&self.hazards, FRONT);
        while self.remaining > 0 {
            let depth = self.front.len();
            let Some(frame) = self.front.last_mut() else {
                break;
            };
            let width = frame.node.keys.len();
            if frame.position > 2 * width as isize {
                self.front.pop();
                continue;
            }
            if frame.node.children.is_empty() {
                let start = (frame.position / 2) as usize;
                let stop = width.min(start.saturating_add(self.remaining as usize));
                total = visit(total, store, &frame.node, start..stop);
                self.remaining -= (stop - start) as u64;
                frame.position = 2 * stop as isize + 1;
                continue;
            }
            let position = frame.position;
            frame.position += 1;
            if position % 2 == 1 {
                self.remaining -= 1;
                let index = (position / 2) as usize;
                total = visit(total, store, &frame.node, index..index + 1);
                continue;
            }
            let child = frame.node.child((position / 2) as usize);
            let anchored = frame.node.anchored();
            let hazard = slots.below(frame.node.level);
            if let Some(node) = fatal(hold(store, child, depth, anchored, hazard)) {
                self.front.push(Frame { node, position: 0 });
            }
        }
        total
    }

    fn retreat(&mut self) -> Option<usize> {
        if self.remaining == 0 {
            return None;
        }
        loop {
            let depth = self.back.len();
            let frame = self.back.last_mut()?;
            if frame.position < 0 {
                self.back.pop();
                continue;
            }
            let position = frame.position;
            if position % 2 == 1 {
                frame.position -= if frame.node.children.is_empty() { 2 } else { 1 };
                self.remaining -= 1;
                return Some((position / 2) as usize);
            }
            frame.position -= 1;
            let child = frame.node.child((position / 2) as usize);
            let anchored = frame.node.anchored();
            let hazard = Slots::of(&self.hazards, BACK).below(frame.node.level);
            if let Some(node) = fatal(hold(self.store.as_deref(), child, depth, anchored, hazard)) {
                let position = 2 * node.keys.len() as isize;
                self.back.push(Frame { node, position });
            }
        }
    }

    pub(crate) fn next_key_with<R>(&mut self, f: impl FnOnce(&K) -> R) -> Option<R> {
        let index = self.advance()?;
        self.front.last().map(|frame| f(&frame.node.keys[index]))
    }

    pub(crate) fn next_back_key_with<R>(&mut self, f: impl FnOnce(&K) -> R) -> Option<R> {
        let index = self.retreat()?;
        self.back.last().map(|frame| f(&frame.node.keys[index]))
    }

    pub(crate) fn next_with<R>(&mut self, f: impl FnOnce(&K, &V) -> R) -> Option<R> {
        let index = self.advance()?;
        let frame = self.front.last()?;
        let key = &frame.node.keys[index];
        Some(fatal(frame.node.read(
            self.store.as_deref(),
            index,
            |value| f(key, value),
        )))
    }

    pub(crate) fn next_back_with<R>(&mut self, f: impl FnOnce(&K, &V) -> R) -> Option<R> {
        let index = self.retreat()?;
        let frame = self.back.last()?;
        let key = &frame.node.keys[index];
        Some(fatal(frame.node.read(
            self.store.as_deref(),
            index,
            |value| f(key, value),
        )))
    }

    fn value_of(&self, frame: Option<&Frame<K, V, S>>, index: usize) -> Option<Val<V>> {
        frame.map(|frame| fatal(frame.node.value(self.store.as_deref(), index)))
    }

    fn pair(&self, frame: Option<&Frame<K, V, S>>, index: usize) -> Option<(K, Val<V>)> {
        let value = self.value_of(frame, index)?;
        frame.map(|frame| (frame.node.keys[index].clone(), value))
    }
}

impl<K: Data + Ord, V: Data, S: Data> Iterator for Iter<K, V, S> {
    type Item = (K, Val<V>);

    fn next(&mut self) -> Option<Self::Item> {
        let index = self.advance()?;
        self.pair(self.front.last(), index)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining as usize, Some(self.remaining as usize))
    }

    fn fold<B, F: FnMut(B, Self::Item) -> B>(mut self, init: B, mut f: F) -> B {
        self.drain(init, |total, store, node, run| {
            let keys = &node.keys[run.clone()];
            let values = Run::of(&node.cells, run);
            (0..keys.len()).fold(total, |total, offset| {
                f(total, (keys[offset].clone(), values.value(store, offset)))
            })
        })
    }
}

impl<K: Data + Ord, V: Data, S: Data> DoubleEndedIterator for Iter<K, V, S> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let index = self.retreat()?;
        self.pair(self.back.last(), index)
    }
}

impl<K: Data + Ord, V: Data, S: Data> ExactSizeIterator for Iter<K, V, S> {}

pub struct Keys<K, V, S = ()>(Iter<K, V, S>);

impl<K: Data + Ord, V: Data, S: Data> Iterator for Keys<K, V, S> {
    type Item = K;

    fn next(&mut self) -> Option<K> {
        let index = self.0.advance()?;
        self.0
            .front
            .last()
            .map(|frame| frame.node.keys[index].clone())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    fn fold<B, F: FnMut(B, K) -> B>(mut self, init: B, mut f: F) -> B {
        self.0.drain(init, |total, _, node, run| {
            node.keys[run].iter().cloned().fold(total, &mut f)
        })
    }
}

impl<K: Data + Ord, V: Data, S: Data> DoubleEndedIterator for Keys<K, V, S> {
    fn next_back(&mut self) -> Option<K> {
        let index = self.0.retreat()?;
        self.0
            .back
            .last()
            .map(|frame| frame.node.keys[index].clone())
    }
}

impl<K: Data + Ord, V: Data, S: Data> ExactSizeIterator for Keys<K, V, S> {}

pub struct Values<K, V, S = ()>(Iter<K, V, S>);

impl<K: Data + Ord, V: Data, S: Data> Iterator for Values<K, V, S> {
    type Item = Val<V>;

    fn next(&mut self) -> Option<Val<V>> {
        let index = self.0.advance()?;
        self.0.value_of(self.0.front.last(), index)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    fn fold<B, F: FnMut(B, Val<V>) -> B>(mut self, init: B, mut f: F) -> B {
        self.0.drain(init, |total, store, node, run| {
            let values = Run::of(&node.cells, run.clone());
            (0..run.len()).fold(total, |total, offset| f(total, values.value(store, offset)))
        })
    }
}

impl<K: Data + Ord, V: Data, S: Data> DoubleEndedIterator for Values<K, V, S> {
    fn next_back(&mut self) -> Option<Val<V>> {
        let index = self.0.retreat()?;
        self.0.value_of(self.0.back.last(), index)
    }
}

impl<K: Data + Ord, V: Data, S: Data> ExactSizeIterator for Values<K, V, S> {}

#[derive(Clone, Debug, PartialEq)]
pub enum Change<K, V> {
    Added(K, Val<V>),
    Removed(K, Val<V>),
    Changed(K, Val<V>, Val<V>),
}

impl<K, V> Change<K, V> {
    pub fn key(&self) -> &K {
        match self {
            Change::Added(key, _) | Change::Removed(key, _) | Change::Changed(key, _, _) => key,
        }
    }

    pub fn before(&self) -> Option<&Val<V>> {
        match self {
            Change::Added(..) => None,
            Change::Removed(_, value) | Change::Changed(_, value, _) => Some(value),
        }
    }

    pub fn after(&self) -> Option<&Val<V>> {
        match self {
            Change::Removed(..) => None,
            Change::Added(_, value) | Change::Changed(_, _, value) => Some(value),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Conflict<K, V> {
    pub key: K,
    pub base: Option<Val<V>>,
    pub ours: Option<Val<V>>,
    pub theirs: Option<Val<V>>,
}

impl<K, V: Clone> Conflict<K, V> {
    pub fn keep_ours(self) -> Option<V> {
        self.ours.map(Val::into_owned)
    }

    pub fn keep_theirs(self) -> Option<V> {
        self.theirs.map(Val::into_owned)
    }
}

pub(crate) fn three_way<T, K, V, S, E>(
    target: &mut T,
    base: &Map<K, V, S>,
    theirs: &Map<K, V, S>,
    read: impl Fn(&T, &K) -> Option<Val<V>>,
    mut write: impl FnMut(&mut T, K, Option<V>),
    mut resolve: impl FnMut(Conflict<K, V>) -> std::result::Result<Option<V>, E>,
) -> std::result::Result<usize, E>
where
    K: Data + Ord,
    V: Data,
    S: Summary<K, V>,
{
    let mut written = 0;
    for change in base.diff(theirs) {
        let key = change.key().clone();
        let before = change.before().cloned();
        let after = change.after().cloned();
        let ours = read(target, &key);
        if same_value(ours.as_ref(), after.as_ref()) {
            continue;
        }
        let value = if same_value(ours.as_ref(), before.as_ref()) {
            after.map(|value| (*value).clone())
        } else {
            resolve(Conflict {
                key: key.clone(),
                base: before,
                ours,
                theirs: after,
            })?
        };
        write(target, key, value);
        written += 1;
    }
    Ok(written)
}

enum Step<K, V, S> {
    Sub(Link<K, V, S>),
    Entry(K, Slot<V, S>),
}

pub struct Diff<K, V, S = ()> {
    old_store: Option<Store>,
    new_store: Option<Store>,
    old: Vec<Step<K, V, S>>,
    new: Vec<Step<K, V, S>>,
}

enum Action {
    Done,
    ExpandOld,
    ExpandNew,
    SkipBoth,
    TakeOld,
    TakeNew,
    Compare,
}

fn expand<K: Data + Ord, V: Data, S: Data>(store: Option<&Store>, steps: &mut Vec<Step<K, V, S>>) {
    let Some(Step::Sub(link)) = steps.pop() else {
        return;
    };
    let Some(node) = fatal(load(store, &link)) else {
        return;
    };
    for index in (0..=node.keys.len()).rev() {
        steps.push(Step::Sub(node.child(index).clone()));
        if index > 0 {
            steps.push(Step::Entry(
                node.keys[index - 1].clone(),
                node.slot(index - 1),
            ));
        }
    }
}

impl<K: Data + Ord, V: Data, S: Data> Diff<K, V, S> {
    fn action(&self) -> Action {
        match (self.old.last(), self.new.last()) {
            (None, None) => Action::Done,
            (Some(Step::Sub(_)), None) => Action::ExpandOld,
            (None, Some(Step::Sub(_))) => Action::ExpandNew,
            (Some(Step::Entry(..)), None) => Action::TakeOld,
            (None, Some(Step::Entry(..))) => Action::TakeNew,
            (Some(Step::Sub(_)), Some(Step::Entry(..))) => Action::ExpandOld,
            (Some(Step::Entry(..)), Some(Step::Sub(_))) => Action::ExpandNew,
            (Some(Step::Sub(a)), Some(Step::Sub(b))) => {
                if a.same_as(b) {
                    Action::SkipBoth
                } else if a.count() >= b.count() {
                    Action::ExpandOld
                } else {
                    Action::ExpandNew
                }
            }
            (Some(Step::Entry(a, _)), Some(Step::Entry(b, _))) => match a.cmp(b) {
                Ordering::Less => Action::TakeOld,
                Ordering::Greater => Action::TakeNew,
                Ordering::Equal => Action::Compare,
            },
        }
    }
}

impl<K: Data + Ord, V: Data, S: Data> Iterator for Diff<K, V, S> {
    type Item = Change<K, V>;

    fn next(&mut self) -> Option<Change<K, V>> {
        loop {
            match self.action() {
                Action::Done => return None,
                Action::ExpandOld => expand(self.old_store.as_ref(), &mut self.old),
                Action::ExpandNew => expand(self.new_store.as_ref(), &mut self.new),
                Action::SkipBoth => {
                    self.old.pop();
                    self.new.pop();
                }
                Action::TakeOld => {
                    if let Some(Step::Entry(key, slot)) = self.old.pop() {
                        let value = fatal(read_slot(self.old_store.as_ref(), &slot));
                        return Some(Change::Removed(key, value));
                    }
                }
                Action::TakeNew => {
                    if let Some(Step::Entry(key, slot)) = self.new.pop() {
                        let value = fatal(read_slot(self.new_store.as_ref(), &slot));
                        return Some(Change::Added(key, value));
                    }
                }
                Action::Compare => {
                    let old = self.old.pop();
                    let new = self.new.pop();
                    if let (Some(Step::Entry(key, a)), Some(Step::Entry(_, b))) = (old, new) {
                        if a.shares(&b) {
                            continue;
                        }
                        let before = fatal(read_slot(self.old_store.as_ref(), &a));
                        let after = fatal(read_slot(self.new_store.as_ref(), &b));
                        if !V::same_encoding(&before, &after) {
                            return Some(Change::Changed(key, before, after));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;
    use crate::codec::HASH_BYTES;

    const ROUNDS: usize = 40;

    fn leaf(first: u64) -> Shared<Node<u64, u64, ()>> {
        let keys: Vec<u64> = (first..first + 8).collect();
        let column: Arc<[u64]> = keys.iter().map(|key| key * 3).collect();
        let node = Node {
            count: keys.len() as u64,
            keys,
            children: Vec::new(),
            cells: Cells::Column(column),
            level: 0,
            origin: None,
            prefix: None,
        };
        Shared::new(node)
    }

    fn read(weak: &Weak<Node<u64, u64, ()>>, first: u64) -> bool {
        let Some(guard) = epoch::pin() else {
            return false;
        };
        let Some(node) = weak.lend(&guard) else {
            return false;
        };
        assert_eq!(node.keys.iter().sum::<u64>(), 8 * first + 28);
        match &node.cells {
            Cells::Column(column) => assert_eq!(column[7], (first + 7) * 3),
            Cells::Slots(_) => panic!("a leaf built from a column keeps it"),
        }
        true
    }

    fn evict(node: Shared<Node<u64, u64, ()>>) {
        if node.release() {
            epoch::retire(Shared::erase(node), 64);
        }
    }

    #[test]
    fn epoch_borrows_survive_an_eviction_that_races_them() {
        for round in 0..4u64 {
            let first = round * 100;
            let node = leaf(first);
            node.hold();
            let weak = Shared::downgrade(&node);
            let readers: Vec<_> = (0..3)
                .map(|_| {
                    let weak = weak.clone();
                    thread::spawn(move || (0..ROUNDS).filter(|_| read(&weak, first)).count())
                })
                .collect();
            thread::yield_now();
            evict(node);
            for reader in readers {
                reader.join().unwrap();
            }
            assert!(!read(&weak, first));
            epoch::flush();
            assert_eq!(weak.strong_count(), 0);
        }
    }

    #[test]
    fn epoch_readers_after_an_eviction_race_the_free_of_what_it_retired() {
        let node = leaf(500);
        node.hold();
        let weak = Shared::downgrade(&node);
        assert!(read(&weak, 500));
        evict(node);
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let weak = weak.clone();
                thread::spawn(move || (0..ROUNDS).filter(|_| read(&weak, 500)).count())
            })
            .collect();
        epoch::flush();
        for reader in readers {
            assert_eq!(reader.join().unwrap(), 0);
        }
        assert_eq!(weak.strong_count(), 0);
    }

    #[test]
    fn epoch_a_node_never_lent_is_freed_at_eviction() {
        let node = leaf(7);
        node.hold();
        let weak = Shared::downgrade(&node);
        evict(node);
        assert_eq!(weak.strong_count(), 0);
        assert!(!read(&weak, 7));
    }

    type Tree = (Shared<Node<u64, u64, ()>>, Vec<u64>);

    fn tree(depth: usize, next: &mut u64, keep: &mut Vec<Shared<Node<u64, u64, ()>>>) -> Tree {
        if depth == PIN_DEPTH + 1 {
            let node = leaf(*next);
            *next += 10;
            let keys = node.keys.clone();
            return (node, keys);
        }
        let (left, mut keys) = tree(depth + 1, next, keep);
        let key = *next;
        *next += 1;
        let (right, right_keys) = tree(depth + 1, next, keep);
        let mut link = |node: Shared<Node<u64, u64, ()>>, count: usize| {
            if depth + 1 < PIN_DEPTH {
                return Link::Mem(node, Hint::NONE);
            }
            let ptr = Ptr {
                hash: [0; HASH_BYTES],
                offset: 0,
            };
            let link = Link::under(ptr, count as u64, LEVELS, ());
            if let Link::Stored { memo, .. } = &link {
                let _ = memo.set(Memo::Weak(Shared::downgrade(&node), Hint::NONE));
            }
            keep.push(node);
            link
        };
        let children = vec![link(left, keys.len()), link(right, right_keys.len())];
        keys.push(key);
        keys.extend(right_keys);
        let node = Node {
            count: keys.len() as u64,
            keys: vec![key],
            children,
            cells: Cells::Column(Arc::from([key * 3])),
            level: (PIN_DEPTH + 1 - depth) as u8,
            origin: None,
            prefix: None,
        };
        (Shared::new(node), keys)
    }

    #[test]
    fn epoch_iterators_borrow_what_their_root_pins() {
        let mut keep = Vec::new();
        let (root, keys) = tree(0, &mut 0, &mut keep);
        let map: Map<u64, u64> = Map {
            root: Link::Mem(root, Hint::NONE),
            store: None,
        };
        let pairs: Vec<(u64, u64)> = keys.iter().map(|key| (*key, key * 3)).collect();
        let seen: Vec<(u64, u64)> = map.iter().map(|(key, value)| (key, *value)).collect();
        assert_eq!(seen, pairs);
        let back: Vec<u64> = map.keys().rev().collect();
        assert!(back.iter().rev().eq(keys.iter()));
        let (low, high) = (keys[5], keys[keys.len() - 7]);
        let inner: Vec<u64> = keys
            .iter()
            .copied()
            .filter(|key| (low..high).contains(key))
            .collect();
        assert_eq!(
            map.range(low..high).map(|(key, _)| key).collect::<Vec<_>>(),
            inner
        );
        assert!(
            map.range(low..high)
                .rev()
                .map(|(key, _)| key)
                .eq(inner.iter().rev().copied())
        );
        let narrow = map.range(keys[1]..keys[4]);
        assert!(narrow.map(|(key, _)| key).eq(keys[1..4].iter().copied()));
        let mut cursor = map.cursor();
        for key in keys.iter().rev() {
            assert!(cursor.seek(key));
            assert_eq!(cursor.value().map(|value| *value), Some(key * 3));
        }
        let mut walk = map.iter();
        let mut edited = map.clone();
        let middle = keys[keys.len() / 2];
        *edited.get_mut(&middle).unwrap() = 1;
        drop(map);
        drop(cursor);
        assert_eq!(
            walk.next().map(|(key, value)| (key, *value)),
            Some(pairs[0])
        );
        assert_eq!(edited.get(&middle).map(|value| *value), Some(1));
        drop(edited);
        let rest: Vec<(u64, u64)> = walk.map(|(key, value)| (key, *value)).collect();
        assert_eq!(rest, pairs[1..]);
        assert!(keep.iter().all(|node| Shared::strong_count(node) == 1));
    }

    #[test]
    fn epoch_a_lent_node_waits_for_the_reader_that_borrowed_it() {
        let node = leaf(30);
        node.hold();
        let weak = Shared::downgrade(&node);
        let guard = epoch::pin().unwrap();
        let borrowed = weak.lend(&guard).unwrap();
        evict(node);
        let flusher = thread::spawn(epoch::flush);
        assert_eq!(borrowed.keys[0], 30);
        assert_eq!(weak.strong_count(), 1);
        drop(guard);
        flusher.join().unwrap();
        epoch::flush();
        assert_eq!(weak.strong_count(), 0);
    }

    fn cache(keep: &[Shared<Node<u64, u64, ()>>]) -> Vec<Erased> {
        keep.iter()
            .map(|node| {
                let entry = Shared::erase(node.clone());
                entry.hold();
                entry
            })
            .collect()
    }

    fn evict_all(entries: Vec<Erased>) {
        for entry in entries {
            if entry.release() {
                epoch::retire(entry, 64);
            }
        }
    }

    fn uncounted(path: &[Frame<u64, u64, ()>]) -> impl Iterator<Item = *const Node<u64, u64, ()>> {
        path.iter()
            .filter(|frame| frame.node.anchored())
            .map(|frame| frame.node.node())
    }

    fn values(walk: impl Iterator<Item = (u64, Val<u64>)>) -> Vec<(u64, u64)> {
        walk.map(|(key, value)| (key, *value)).collect()
    }

    #[test]
    fn epoch_iterators_keep_borrowed_nodes_past_their_eviction() {
        let mut keep = Vec::new();
        let (root, keys) = tree(0, &mut 0, &mut keep);
        let entries = cache(&keep);
        let map: Map<u64, u64> = Map {
            root: Link::Mem(root, Hint::NONE),
            store: None,
        };
        let pairs: Vec<(u64, u64)> = keys.iter().map(|key| (*key, key * 3)).collect();
        let mut walk = map.iter();
        let mut seen = values(walk.by_ref().take(3));
        let (low, high) = (keys[1], keys[keys.len() - 2]);
        let mut back = map.range(low..high);
        let last = back.next_back().map(|(key, value)| (key, *value));
        let middle = keys[keys.len() / 2];
        let mut cursor = map.cursor();
        assert!(cursor.seek(&middle));
        let mut finder = map.finder();
        assert_eq!(finder.get(&keys[9]).map(|value| *value), Some(keys[9] * 3));
        let held: Vec<*const Node<u64, u64, ()>> = uncounted(&walk.front)
            .chain(uncounted(&back.front))
            .chain(uncounted(&back.back))
            .chain(uncounted(&cursor.path))
            .chain(finder.leaf.iter().map(|leaf| leaf.node()))
            .collect();
        let watched: Vec<Weak<Node<u64, u64, ()>>> = keep
            .iter()
            .filter(|node| held.contains(&Shared::as_ptr(node)))
            .map(Shared::downgrade)
            .collect();
        assert!(watched.len() >= 2);
        evict_all(entries);
        keep.retain(|node| !held.contains(&Shared::as_ptr(node)));
        epoch::flush();
        assert!(watched.iter().all(|weak| weak.strong_count() == 1));
        assert_eq!(cursor.value().map(|value| *value), Some(middle * 3));
        assert_eq!(finder.get(&keys[9]).map(|value| *value), Some(keys[9] * 3));
        seen.extend(values(walk));
        assert_eq!(seen, pairs);
        let mut inner = values(back);
        inner.extend(last);
        let want: Vec<(u64, u64)> = pairs
            .iter()
            .copied()
            .filter(|(key, _)| (low..high).contains(key))
            .collect();
        assert_eq!(inner, want);
        assert!(watched.iter().any(|weak| weak.strong_count() == 1));
        drop(cursor);
        drop(finder);
        epoch::flush();
        assert!(watched.iter().all(|weak| weak.strong_count() == 0));
    }

    #[test]
    fn epoch_iterators_dropped_mid_walk_let_their_nodes_go() {
        let mut keep = Vec::new();
        let (root, keys) = tree(0, &mut 0, &mut keep);
        let entries = cache(&keep);
        let map: Map<u64, u64> = Map {
            root: Link::Mem(root, Hint::NONE),
            store: None,
        };
        let walks: Vec<Iter<u64, u64>> = (0..3)
            .map(|skip| {
                let mut walk = map.range(keys[skip]..);
                walk.by_ref().take(5 + skip).for_each(drop);
                walk
            })
            .collect();
        let watched: Vec<Weak<Node<u64, u64, ()>>> = keep.iter().map(Shared::downgrade).collect();
        evict_all(entries);
        drop(keep);
        epoch::flush();
        assert!(watched.iter().any(|weak| weak.strong_count() == 1));
        drop(walks);
        epoch::flush();
        assert!(watched.iter().all(|weak| weak.strong_count() == 0));
    }

    #[test]
    fn epoch_a_stored_root_waits_for_the_iterators_that_borrow_it() {
        let mut keep = Vec::new();
        let (root, keys) = tree(0, &mut 0, &mut keep);
        let below = match root.child(0) {
            Link::Mem(child, _) => Shared::downgrade(child),
            _ => panic!("the test tree links its top levels in memory"),
        };
        let watch = Shared::downgrade(&root);
        let ptr = Ptr {
            hash: [0; HASH_BYTES],
            offset: 0,
        };
        let link = Link::under(ptr, keys.len() as u64, LEVELS, ());
        if let Link::Stored { memo, .. } = &link {
            let _ = memo.set(Memo::Pinned(root));
        }
        let map: Map<u64, u64> = Map {
            root: link,
            store: None,
        };
        let pairs: Vec<(u64, u64)> = keys.iter().map(|key| (*key, key * 3)).collect();
        let mut walk = map.iter();
        let mut seen = values(walk.by_ref().take(2));
        let mut cursor = map.cursor();
        let mut finder = map.finder();
        assert!(matches!(walk._root, Some(ref root) if root.anchored()));
        drop(map);
        assert_eq!(watch.strong_count(), 1);
        assert_eq!(below.strong_count(), 1);
        epoch::flush();
        seen.extend(values(walk));
        assert_eq!(seen, pairs);
        for key in keys.iter().rev() {
            assert!(cursor.seek(key));
            assert_eq!(cursor.value().map(|value| *value), Some(key * 3));
            assert_eq!(finder.get(key).map(|value| *value), Some(key * 3));
        }
        drop(cursor);
        assert_eq!(below.strong_count(), 1);
        drop(finder);
        assert_eq!(below.strong_count(), 0);
        assert!(keep.iter().all(|node| Shared::strong_count(node) == 1));
    }

    #[test]
    fn epoch_a_finder_keeps_a_leaf_that_only_a_borrowed_node_pinned() {
        let ptr = Ptr {
            hash: [0; HASH_BYTES],
            offset: 0,
        };
        let pinned = |node: Shared<Node<u64, u64, ()>>| {
            let link = Link::under(ptr, node.count, LEVELS, ());
            if let Link::Stored { memo, .. } = &link {
                let _ = memo.set(Memo::Pinned(node));
            }
            link
        };
        let (first, second) = (leaf(0), leaf(10));
        let watch = Shared::downgrade(&first);
        let middle = Shared::new(Node {
            count: 17,
            keys: vec![9],
            children: vec![pinned(first), pinned(second)],
            cells: Cells::Column(Arc::from([27])),
            level: 2,
            origin: None,
            prefix: None,
        });
        let entry = Shared::erase(middle.clone());
        entry.hold();
        let link = Link::under(ptr, 17, LEVELS, ());
        if let Link::Stored { memo, .. } = &link {
            let _ = memo.set(Memo::Weak(Shared::downgrade(&middle), Hint::NONE));
        }
        let root = Shared::new(Node {
            count: 18,
            keys: vec![100],
            children: vec![link, Link::Empty],
            cells: Cells::Column(Arc::from([300])),
            level: 3,
            origin: None,
            prefix: None,
        });
        let map: Map<u64, u64> = Map {
            root: Link::Mem(root, Hint::NONE),
            store: None,
        };
        let mut finder = map.finder();
        assert_eq!(finder.get(&3).map(|value| *value), Some(9));
        assert!(finder.parent.is_none());
        evict_all(vec![entry]);
        drop(middle);
        epoch::flush();
        assert_eq!(watch.strong_count(), 1);
        for key in 0..8 {
            assert_eq!(finder.get(&key).map(|value| *value), Some(key * 3));
        }
        drop(finder);
        epoch::flush();
        assert!(watch.upgrade().is_none());
    }

    #[test]
    fn epoch_a_root_borrowed_after_a_free_scanned_waits_for_its_iterator() {
        let mut keep = Vec::new();
        let (root, keys) = tree(0, &mut 0, &mut keep);
        let below = match root.child(0) {
            Link::Mem(child, _) => Shared::downgrade(child),
            _ => panic!("the test tree links its top levels in memory"),
        };
        let entry = Shared::erase(root.clone());
        entry.hold();
        let ptr = Ptr {
            hash: [0; HASH_BYTES],
            offset: 0,
        };
        let link = Link::under(ptr, keys.len() as u64, LEVELS, ());
        if let Link::Stored { memo, .. } = &link {
            let _ = memo.set(Memo::Pinned(root));
        }
        let map: Map<u64, u64> = Map {
            root: link,
            store: None,
        };
        drop(map.iter());
        evict_all(vec![entry]);
        let mut walk = None;
        epoch::free_after(|| {
            walk = Some(map.iter());
            drop(map);
        });
        assert_eq!(below.strong_count(), 1);
        let pairs: Vec<(u64, u64)> = keys.iter().map(|key| (*key, key * 3)).collect();
        assert_eq!(walk.map(values), Some(pairs));
    }

    #[test]
    fn epoch_a_narrow_range_holds_each_end_in_its_own_slots() {
        let mut keep = Vec::new();
        let (root, keys) = tree(0, &mut 0, &mut keep);
        let entries = cache(&keep);
        let map: Map<u64, u64> = Map {
            root: Link::Mem(root, Hint::NONE),
            store: None,
        };
        let pairs: Vec<(u64, u64)> = keys.iter().map(|key| (*key, key * 3)).collect();
        let mut narrow = map.range(keys[2]..keys[6]);
        assert_eq!(
            narrow.next().map(|(key, value)| (key, *value)),
            Some(pairs[2])
        );
        let front: Vec<*const Node<u64, u64, ()>> = uncounted(&narrow.front).collect();
        let back: Vec<*const Node<u64, u64, ()>> = uncounted(&narrow.back).collect();
        let slots = narrow.hazards.as_ref().unwrap();
        let held = |node: &*const Node<u64, u64, ()>, base: usize| {
            entries.iter().any(|entry| {
                entry.as_ptr() == node.cast()
                    && (base..base + SPAN).any(|index| {
                        slots
                            .entry(index)
                            .load(std::sync::atomic::Ordering::SeqCst)
                            .addr()
                            == entry.addr()
                    })
            })
        };
        let lent: Vec<_> = back.iter().filter(|node| held(node, BACK)).collect();
        assert!(!lent.is_empty());
        assert!(
            lent.iter()
                .all(|node| front.contains(node) && held(node, FRONT))
        );
        let watched: Vec<Weak<Node<u64, u64, ()>>> = keep
            .iter()
            .filter(|node| back.contains(&Shared::as_ptr(node)))
            .map(Shared::downgrade)
            .collect();
        evict_all(entries);
        keep.retain(|node| !back.contains(&Shared::as_ptr(node)));
        epoch::flush();
        assert!(watched.iter().all(|weak| weak.strong_count() == 1));
        let rest: Vec<(u64, u64)> = narrow.rev().map(|(key, value)| (key, *value)).collect();
        assert!(rest.iter().rev().eq(pairs[3..6].iter()));
        epoch::flush();
        assert!(watched.iter().all(|weak| weak.strong_count() == 0));
    }

    #[test]
    fn epoch_a_finder_that_panics_keeps_only_what_its_slots_hold() {
        let mut keep = Vec::new();
        let (root, keys) = tree(0, &mut 0, &mut keep);
        let mut entries = cache(&keep);
        let map: Map<u64, u64> = Map {
            root: Link::Mem(root, Hint::NONE),
            store: None,
        };
        let (first, last) = (keys[0], keys[keys.len() - 1]);
        let mut finder = map.finder();
        assert_eq!(finder.get(&first).map(|value| *value), Some(first * 3));
        assert!(finder.parent.is_some());
        let doomed = keep
            .iter()
            .position(|node| node.children.is_empty() && node.keys.contains(&last))
            .unwrap();
        evict_all(vec![entries.remove(doomed)]);
        drop(keep.remove(doomed));
        epoch::flush();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| finder.get(&last)));
        assert!(caught.is_err());
        let slot = |index: usize| {
            let hazards = finder.hazards.as_ref().unwrap();
            hazards
                .entry(index)
                .load(std::sync::atomic::Ordering::SeqCst)
                .addr()
        };
        let held = |node: &Hold<u64, u64, ()>, index: usize| {
            !node.anchored()
                || entries.iter().any(|entry| {
                    entry.as_ptr() == node.node().cast() && entry.addr() == slot(index)
                })
        };
        assert!(finder.leaf.as_ref().is_none_or(|leaf| held(leaf, LEAF)));
        assert!(
            finder
                .parent
                .as_ref()
                .is_none_or(|(parent, _)| held(parent, PARENT))
        );
        assert_eq!(finder.get(&first).map(|value| *value), Some(first * 3));
        assert_eq!(finder.get(&keys[1]).map(|value| *value), Some(keys[1] * 3));
        evict_all(entries);
    }
}
