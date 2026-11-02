use std::borrow::Borrow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt;
use std::hash;
use std::ops::{Bound, ControlFlow, Deref, Range, RangeBounds};
use std::mem::needs_drop;
use std::sync::{Arc, OnceLock, Weak};
use std::vec;

use crate::codec::{Data, Decoder, Encoder, Hash, HashSink, Ptr, Sink, try_hash_of};
use crate::error::{Error, Result};
use crate::store::{Store, heap_weight};
use xxhash_rust::xxh3::xxh3_64;

pub(crate) const SPILL_BYTES: usize = 512;
const LEVEL_BITS: u32 = 3;
const PIN_DEPTH: usize = 3;
const ARC_HEADER: usize = 16;
const ALLOCATION_BYTES: usize = 16;
const ITEM_BYTES: usize = 32;
const PATH_FRAMES: usize = 8;

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
    Copied(V),
    Column(Arc<[V]>, u32),
    Single(Arc<V>),
}

impl<V: Clone> Val<V> {
    #[inline]
    fn of_column(column: &Arc<[V]>, index: usize) -> Self {
        if by_copy::<V>() {
            Val(Held::Copied(column[index].clone()))
        } else {
            Val(Held::Column(column.clone(), index as u32))
        }
    }

    #[inline]
    fn of_single(value: &Arc<V>) -> Self {
        if by_copy::<V>() {
            Val(Held::Copied((**value).clone()))
        } else {
            Val(Held::Single(value.clone()))
        }
    }

    pub fn new(value: V) -> Self {
        if by_copy::<V>() {
            Val(Held::Copied(value))
        } else {
            Val(Held::Single(Arc::new(value)))
        }
    }

    pub fn into_owned(self) -> V {
        match self.0 {
            Held::Copied(value) => value,
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

    #[inline]
    fn summary<K>(&self, key: &K) -> S
    where
        S: Summary<K, V>,
    {
        match self {
            Slot::Spilled(_, summary) => summary.clone(),
            held => S::of(key, held.held().expect("a slot that is not spilled holds its value")),
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
    fn settled(&self, spills: Vec<(usize, Slot<V, S>)>) -> Self {
        let width = match self {
            Cells::Column(_) if spills.is_empty() => return self.clone(),
            Cells::Column(column) => column.len(),
            Cells::Slots(slots) => slots.len(),
        };
        let mut spills = spills.into_iter().peekable();
        let mut column = Vec::with_capacity(width);
        let mut spilled = Vec::new();
        for index in 0..width {
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
    Pinned(Arc<Node<K, V, S>>),
    Weak(Weak<Node<K, V, S>>),
}

impl<K, V, S> Clone for Memo<K, V, S> {
    fn clone(&self) -> Self {
        match self {
            Memo::Pinned(node) => Memo::Pinned(node.clone()),
            Memo::Weak(node) => Memo::Weak(node.clone()),
        }
    }
}

enum Link<K, V, S> {
    Empty,
    Stored {
        ptr: Ptr,
        count: u64,
        summary: S,
        memo: OnceLock<Memo<K, V, S>>,
    },
    Mem(Arc<Node<K, V, S>>),
}

impl<K, V, S: Clone> Clone for Link<K, V, S> {
    fn clone(&self) -> Self {
        match self {
            Link::Empty => Link::Empty,
            Link::Stored {
                ptr,
                count,
                summary,
                ..
            } => Link::stored(*ptr, *count, summary.clone()),
            Link::Mem(node) => Link::Mem(node.clone()),
        }
    }
}

impl<K, V, S> Link<K, V, S> {
    fn stored(ptr: Ptr, count: u64, summary: S) -> Self {
        Link::Stored {
            ptr,
            count,
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
                summary,
                memo,
            } => Link::Stored {
                ptr: *ptr,
                count: *count,
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
            Link::Mem(node) => node.count,
        }
    }

    fn summary(&self) -> Option<S>
    where
        S: Summary<K, V>,
    {
        match self {
            Link::Empty => None,
            Link::Stored { summary, .. } => Some(summary.clone()),
            Link::Mem(node) => node.summary(),
        }
    }

    fn take(&mut self) -> Self {
        std::mem::replace(self, Link::Empty)
    }

    fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Link::Empty, Link::Empty) => true,
            (Link::Stored { ptr: a, .. }, Link::Stored { ptr: b, .. }) => a.hash == b.hash,
            (Link::Mem(a), Link::Mem(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
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
        }
    }
}

impl<K, V, S> Node<K, V, S> {
    fn search<Q>(&self, key: &Q) -> std::result::Result<usize, usize>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.keys.binary_search_by(|probe| probe.borrow().cmp(key))
    }

    fn child(&self, position: usize) -> &Link<K, V, S> {
        self.children.get(position).unwrap_or(&Link::Empty)
    }

    fn children_mut(&mut self) -> &mut Vec<Link<K, V, S>> {
        if self.children.is_empty() {
            self.children.resize_with(self.keys.len() + 1, || Link::Empty);
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
        self.count =
            self.keys.len() as u64 + self.children.iter().map(Link::count).sum::<u64>();
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
    fn read<R>(
        &self,
        store: Option<&Store>,
        index: usize,
        f: impl FnOnce(&V) -> R,
    ) -> Result<R> {
        match &self.cells {
            Cells::Column(column) => Ok(f(&column[index])),
            Cells::Slots(slots) => match &slots[index] {
                Slot::Spilled(ptr, _) => {
                    Ok(f(&*store.ok_or(Error::Detached)?.load::<V>(*ptr)?))
                }
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
        for position in 0..=n {
            let count = decoder.varint()?;
            if count != 0 {
                children.reserve_exact(n + 1 - children.len());
                children.resize_with(position, || Link::Empty);
                let summary = S::decode(decoder)?;
                children.push(Link::stored(decoder.link()?, count, summary));
            }
        }
        if !children.is_empty() {
            children.resize_with(n + 1, || Link::Empty);
        }
        let mut node = Node {
            level,
            keys,
            cells,
            children,
            count: 0,
            origin: Some(origin),
        };
        node.recount();
        Ok(node)
    }
}

enum Reach<'a, K, V, S> {
    Absent,
    Borrowed(&'a Arc<Node<K, V, S>>),
    Owned(Arc<Node<K, V, S>>),
}

fn reach<'a, K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    link: &'a Link<K, V, S>,
    depth: usize,
) -> Result<Reach<'a, K, V, S>> {
    let (ptr, memo) = match link {
        Link::Empty => return Ok(Reach::Absent),
        Link::Mem(node) => return Ok(Reach::Borrowed(node)),
        Link::Stored { ptr, memo, .. } => (*ptr, memo),
    };
    match memo.get() {
        Some(Memo::Pinned(node)) => return Ok(Reach::Borrowed(node)),
        Some(Memo::Weak(weak)) => {
            if let Some(node) = weak.upgrade() {
                return Ok(Reach::Owned(node));
            }
        }
        None => {}
    }
    let store = store.ok_or(Error::Detached)?;
    let (node, cached) = store.fetch(ptr, |decoder| Node::decode(decoder, ptr), Node::fixed_bytes)?;
    if depth < PIN_DEPTH {
        if let Memo::Pinned(node) = memo.get_or_init(|| Memo::Pinned(node.clone())) {
            return Ok(Reach::Borrowed(node));
        }
    } else if cached {
        memo.get_or_init(|| Memo::Weak(Arc::downgrade(&node)));
    }
    Ok(Reach::Owned(node))
}

fn resident<K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    link: &Link<K, V, S>,
    depth: usize,
) -> Result<Option<Arc<Node<K, V, S>>>> {
    Ok(match reach(store, link, depth)? {
        Reach::Absent => None,
        Reach::Borrowed(node) => Some(node.clone()),
        Reach::Owned(node) => Some(node),
    })
}

fn load<K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    link: &Link<K, V, S>,
) -> Result<Option<Arc<Node<K, V, S>>>> {
    match link {
        Link::Empty => Ok(None),
        Link::Mem(node) => Ok(Some(node.clone())),
        Link::Stored { ptr, .. } => {
            let store = store.ok_or(Error::Detached)?;
            let ptr = *ptr;
            let (node, _) = store.fetch(ptr, |decoder| Node::decode(decoder, ptr), Node::fixed_bytes)?;
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
    if let Link::Stored { .. } = link {
        let node = load(store, link)?.ok_or(Error::Detached)?;
        *link = Link::Mem(node);
    }
    match link {
        Link::Mem(node) => Ok(Arc::make_mut(node)),
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
    };
    node.recount();
    Link::Mem(Arc::new(node))
}

type Parts<K, V, S> = (Link<K, V, S>, Link<K, V, S>);

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
    } = Arc::unwrap_or_clone(node);
    let mut slots = cells.into_slots();
    let position = keys
        .binary_search_by(side)
        .err()
        .ok_or_else(|| Error::Corrupt("key found at a level it does not hash to".into()))?;
    let right_keys = keys.split_off(position);
    let right_slots = slots.split_off(position);
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
            let mut node = Arc::unwrap_or_clone(x);
            let last = node.children_mut().pop().ok_or(Error::Detached)?;
            node.children.push(merge(store, last, b)?);
            node.recount();
            Ok(Link::Mem(Arc::new(node)))
        }
        Ordering::Less => {
            drop(b);
            let mut node = Arc::unwrap_or_clone(y);
            let first = node.children_mut()[0].take();
            node.children[0] = merge(store, a, first)?;
            node.recount();
            Ok(Link::Mem(Arc::new(node)))
        }
        Ordering::Equal => {
            drop(a);
            drop(b);
            let mut left = Arc::unwrap_or_clone(x);
            let mut right = Arc::unwrap_or_clone(y);
            let inner_left = left.children_mut().pop().ok_or(Error::Detached)?;
            let inner_right = right.children_mut()[0].take();
            left.children.push(merge(store, inner_left, inner_right)?);
            left.children.extend(right.children.drain(1..));
            left.keys.append(&mut right.keys);
            left.cells.slots().append(right.cells.slots());
            left.origin = None;
            left.recount();
            Ok(Link::Mem(Arc::new(left)))
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
        *link = Link::Mem(node);
    }
    let Link::Mem(current) = &*link else {
        *link = link_from(
            level,
            vec![key],
            vec![Slot::Fresh(value)],
            vec![Link::Empty, Link::Empty],
            None,
        );
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
    let node = node_mut(store, link)?;
    match node.search(&key) {
        Ok(index) => Ok(Some(std::mem::replace(
            &mut node.cells.slots()[index],
            Slot::Fresh(value),
        ))),
        Err(position) if level == node.level => {
            if node.children.is_empty() {
                node.keys.insert(position, key);
                node.cells.slots().insert(position, Slot::Fresh(value));
                node.count += 1;
                return Ok(None);
            }
            let (left, right) =
                split(store, node.children[position].take(), &|probe: &K| probe.cmp(&key))?;
            node.keys.insert(position, key);
            node.cells.slots().insert(position, Slot::Fresh(value));
            node.children[position] = left;
            node.children.insert(position + 1, right);
            node.count += 1;
            Ok(None)
        }
        Err(position) => {
            let replaced =
                insert_at(store, &mut node.children_mut()[position], key, level, value)?;
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
        Slot::Spilled(ptr, _) => Ok(Val(Held::Single(store.ok_or(Error::Detached)?.load::<V>(*ptr)?))),
    }
}

fn verify<T: Data>(encoder: &Encoder<'_>, mark: (usize, usize), store: &Store) -> Result<()> {
    let mut decoder = Decoder::new(
        &encoder.bytes()[mark.0..],
        &encoder.links()[mark.1..],
        Some(store),
    );
    T::decode(&mut decoder).map(drop)
}

fn flush<K: Data, V: Data, S: Summary<K, V>>(
    link: &Link<K, V, S>,
    sink: &mut dyn Sink,
) -> Result<Link<K, V, S>> {
    let Link::Mem(node) = link else {
        return Ok(link.clone());
    };
    let store = sink.store();
    let caching = store.is_some();
    let width = node.keys.len();
    let mut encoder = Encoder::with_capacity(&mut *sink, 64 + 24 * width, width + 1);
    encoder.u8(node.level);
    encoder.varint(width as u64);
    for (index, key) in node.keys.iter().enumerate() {
        let mark = encoder.mark();
        key.encode(&mut encoder)?;
        if let (true, Some(store)) = (node.is_fresh(index), &store) {
            verify::<K>(&encoder, mark, store)?;
        }
    }
    let mut spills: Vec<(usize, Slot<V, S>)> = Vec::new();
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
        let body = encoder.mark();
        value.encode(&mut encoder)?;
        if let (true, Some(store)) = (node.is_fresh(index), &store) {
            verify::<V>(&encoder, body, store)?;
        }
        if encoder.since(mark) > SPILL_BYTES {
            encoder.rewind(mark);
            let ptr = encoder.child(|inner| value.encode(inner))?;
            let summary = S::of(key, value);
            encoder.u8(1);
            encoder.link(ptr);
            summary.encode(&mut encoder)?;
            if caching {
                spills.push((index, Slot::Spilled(ptr, summary)));
            }
        }
    }
    let branching = caching && !node.children.is_empty();
    let mut stored: Vec<Link<K, V, S>> = Vec::with_capacity(if branching { width + 1 } else { 0 });
    let mut total = None;
    for position in 0..=width {
        let flushed = flush(node.child(position), encoder.sink())?;
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
            Link::Mem(_) => return Err(Error::Detached),
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
    let body_bytes = object.body.len();
    let cached = match &store {
        None => None,
        Some(store) if unsettled => {
            let mut decoder = Decoder::new(&object.body, &object.links, Some(store));
            Some(Node::<K, V, S>::decode(&mut decoder, Ptr { hash, offset: 0 })?)
        }
        Some(_) => Some(Node {
            level: node.level,
            keys: node.keys.clone(),
            cells: node.cells.settled(spills),
            children: stored,
            count: node.count,
            origin: None,
        }),
    };
    let ptr = match node.origin {
        Some(origin) if origin.hash == hash => origin,
        _ => sink.put(object, hash)?,
    };
    if let Some(mut decoded) = cached {
        decoded.origin = Some(ptr);
        let weight = heap_weight(decoded.fixed_bytes(), body_bytes);
        sink.remember(ptr, Arc::new(decoded), weight);
    }
    Ok(Link::stored(ptr, node.count, total))
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
    fn close(mut self, last: Link<K, V, S>) -> Link<K, V, S> {
        self.children.push(last);
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
        *link = Link::Mem(load(store, link)?.ok_or(Error::Detached)?);
    }
    let top = edits.as_slice()[..count]
        .iter()
        .filter(|edit| edit.value.is_some())
        .map(|edit| edit.level)
        .max();
    if let (Link::Mem(current), Some(level)) = (&*link, top)
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
                    node.cells.slots().insert(position, Slot::Fresh(edit.value.ok_or(Error::Detached)?));
                } else {
                    let key = &pending[before].key;
                    let piece = node.children_mut()[position].take();
                    let (mut low, high) = split(store, piece, &|probe: &K| probe.cmp(key))?;
                    absorb(store, &mut low, edits, before)?;
                    let edit = edits.next().ok_or(Error::Detached)?;
                    node.keys.insert(position, edit.key);
                    node.cells.slots().insert(position, Slot::Fresh(edit.value.ok_or(Error::Detached)?));
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
            Some(top) if top.level == level => {
                top.children.push(carry);
                top.keys.push(key);
                top.slots.push(Slot::Fresh(value));
            }
            _ => stack.push(Open {
                level,
                keys: vec![key],
                slots: vec![Slot::Fresh(value)],
                children: vec![carry],
            }),
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
    let mut unique: Vec<(K, T)> = Vec::with_capacity(items.len());
    for item in items {
        match unique.last_mut() {
            Some(last) if last.0 == item.0 => *last = item,
            _ => unique.push(item),
        }
    }
    unique
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
    let reached = reach(store, link, depth)?;
    let node = match &reached {
        Reach::Absent => return Ok(total),
        Reach::Borrowed(node) => &***node,
        Reach::Owned(node) => &**node,
    };
    let low = start.map_or(0, |below| node.keys.partition_point(below));
    let high = end.map_or(node.keys.len(), |within| node.keys.partition_point(within));
    if low > high {
        return Ok(total);
    }
    if low == high {
        return gather(store, node.child(low), depth + 1, start, end, total);
    }
    let mut total = gather(store, node.child(low), depth + 1, start, None, total)?;
    for index in low..high {
        total = fold(total, &node.entry_summary(index));
        let last = if index + 1 == high { end } else { None };
        total = gather(store, node.child(index + 1), depth + 1, None, last, total)?;
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
    let reached = reach(store, link, depth)?;
    let node = match &reached {
        Reach::Absent => return Ok(ControlFlow::Continue(())),
        Reach::Borrowed(node) => &***node,
        Reach::Owned(node) => &**node,
    };
    let low = start.map_or(0, |below| node.keys.partition_point(below));
    let high = end.map_or(node.keys.len(), |within| node.keys.partition_point(within));
    if low > high {
        return Ok(ControlFlow::Continue(()));
    }
    let leaf = node.children.is_empty();
    if !leaf {
        let last = if low == high { end } else { None };
        if let ControlFlow::Break(out) = scan(store, node.child(low), depth + 1, start, last, visit)? {
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
        matches!(self.root, Link::Mem(_))
    }

    pub fn clear(&mut self) {
        self.root = Link::Empty;
    }

    fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    fn walk<R>(&self, mut visit: impl FnMut(&Node<K, V, S>) -> Visit<R>) -> Result<Option<R>> {
        let store = self.store();
        let mut link = &self.root;
        let mut depth = 0;
        let mut current = loop {
            let node = match reach(store, link, depth)? {
                Reach::Absent => return Ok(None),
                Reach::Borrowed(node) => node,
                Reach::Owned(node) => break Some(node),
            };
            depth += 1;
            match visit(node) {
                Visit::Done(result) => return Ok(Some(result)),
                Visit::Descend(position) => link = node.child(position),
            }
        };
        while let Some(node) = current {
            depth += 1;
            match visit(&node) {
                Visit::Done(result) => return Ok(Some(result)),
                Visit::Descend(position) => {
                    current = resident(store, node.child(position), depth)?;
                }
            }
        }
        Ok(None)
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

    pub fn update<Q>(&mut self, key: &Q, f: impl FnOnce(&mut V)) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        match self.get_mut(key) {
            Some(value) => {
                f(value);
                true
            }
            None => false,
        }
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
        fatal(found).flatten().map(|(key, value)| (key, fatal(value)))
    }

    pub fn range_by(
        &self,
        below: impl Fn(&K) -> bool,
        within: impl Fn(&K) -> bool,
    ) -> Iter<K, V, S> {
        let build = || -> Result<Iter<K, V, S>> {
            let store = self.store();
            let mut front = Vec::with_capacity(PATH_FRAMES);
            let mut back = Vec::with_capacity(PATH_FRAMES);
            let mut remaining = 0;
            let mut depth = 0;
            let mut current = resident(store, &self.root, depth)?;
            while let Some(node) = current.take() {
                let low = node.keys.partition_point(|key| below(key));
                let high = node.keys.partition_point(|key| within(key));
                depth += 1;
                if low == high {
                    current = resident(store, node.child(low), depth)?;
                }
                back.push(Frame {
                    node: node.clone(),
                    position: 2 * high as isize - 1,
                });
                front.push(Frame {
                    node: node.clone(),
                    position: 2 * low as isize + 1,
                });
                if low < high {
                    let inner = (low + 1..high).map(|position| node.child(position).count());
                    let skipped = edge(store, node.child(low), depth, &below, 1, &mut front)?;
                    let kept = edge(store, node.child(high), depth, &within, -1, &mut back)?;
                    remaining = (high - low) as u64 + inner.sum::<u64>() + kept
                        + node.child(low).count().saturating_sub(skipped);
                }
            }
            Ok(Iter {
                store: self.store.clone(),
                front,
                back,
                remaining,
            })
        };
        fatal(build())
    }

    pub fn iter(&self) -> Iter<K, V, S> {
        self.range_by(|_| false, |_| true)
    }

    pub fn cursor(&self) -> Cursor<K, V, S> {
        Cursor {
            store: self.store.clone(),
            root: self.root.with_memo(),
            path: Vec::with_capacity(PATH_FRAMES),
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
        let mut link = &self.root;
        let mut depth = 0;
        let mut pinned = None;
        let mut current = loop {
            let node = match reach(store, link, depth)? {
                Reach::Absent => break None,
                Reach::Borrowed(node) => node,
                Reach::Owned(node) => break Some(node),
            };
            depth += 1;
            match node.search(key) {
                Ok(index) => return entry(node, index).map(Some),
                Err(position) => {
                    if position > 0 {
                        pinned = Some((node, position - 1));
                    }
                    link = node.child(position);
                }
            }
        };
        let mut owned = None;
        while let Some(node) = current {
            depth += 1;
            match node.search(key) {
                Ok(index) => return entry(&node, index).map(Some),
                Err(position) => {
                    current = resident(store, node.child(position), depth)?;
                    if position > 0 {
                        owned = Some((node, position - 1));
                    }
                }
            }
        }
        match (owned, pinned) {
            (Some((node, index)), _) => entry(&node, index).map(Some),
            (None, Some((node, index))) => entry(node, index).map(Some),
            (None, None) => Ok(None),
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

    pub fn try_for_each<B>(
        &self,
        mut f: impl FnMut(&K, &V) -> ControlFlow<B>,
    ) -> ControlFlow<B> {
        fatal(scan(self.store(), &self.root, 0, None, None, &mut f))
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
        fatal(scan(self.store(), &self.root, 0, Some(&below), Some(&within), &mut f))
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

    pub fn summary_by(
        &self,
        below: impl Fn(&K) -> bool,
        within: impl Fn(&K) -> bool,
    ) -> Option<S> {
        fatal(gather(self.store(), &self.root, 0, Some(&below), Some(&within), None))
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
        self.batch(other.iter().map(|(key, value)| (key, Some(value.into_arc()))).collect());
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
        self.batch(edits.into_iter().map(|(key, value)| (key, value.map(Arc::new))).collect());
    }

    fn batch(&mut self, items: Vec<(K, Option<Arc<V>>)>) {
        let items = sorted_unique(items);
        let count = items.len();
        let mut edits: vec::IntoIter<Edit<K, V>> = items
            .into_iter()
            .map(|(key, value)| Edit {
                level: fatal(level_of(&key)),
                key,
                value,
            })
            .collect::<Vec<_>>()
            .into_iter();
        let store = self.store.as_ref();
        fatal(absorb(store, &mut self.root, &mut edits, count));
    }

    pub fn diff(&self, newer: &Map<K, V, S>) -> Diff<K, V, S>
    where
        V: PartialEq,
    {
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
        resolve: impl FnMut(Conflict<K, V>) -> Option<V>,
    ) -> usize
    where
        V: PartialEq,
    {
        three_way(
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
        )
    }

    pub fn root_hash(&self) -> Option<Hash> {
        match &self.root {
            Link::Empty => None,
            Link::Stored { ptr, .. } => Some(ptr.hash),
            Link::Mem(_) => {
                let mut sink = HashSink;
                match flush(&self.root, &mut sink) {
                    Ok(Link::Stored { ptr, .. }) => Some(ptr.hash),
                    Ok(_) => None,
                    Err(error) => panic!("ferro: {error}"),
                }
            }
        }
    }

    fn rebuilt(&self) -> Map<K, V, S> {
        self.iter().map(|(key, value)| (key, (*value).clone())).collect()
    }
}

impl<K: Data + Ord, V: Data, S: Summary<K, V>> Data for Map<K, V, S> {
    fn encode(&self, w: &mut Encoder<'_>) -> Result<()> {
        let target = w.store_id();
        let foreign =
            target != 0 && self.store.as_ref().is_some_and(|store| store.id() != target);
        if foreign || matches!(self.root, Link::Mem(_)) {
            w.mark_unsettled();
        }
        let stored = if foreign {
            flush(&self.rebuilt().root, w.sink())?
        } else {
            flush(&self.root, w.sink())?
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
            Link::Mem(_) => return Err(Error::Detached),
        }
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> Result<Self> {
        let count = r.varint()?;
        let root = if count == 0 {
            Link::Empty
        } else {
            let summary = S::decode(r)?;
            Link::stored(r.link()?, count, summary)
        };
        Ok(Map {
            root,
            store: r.store(),
        })
    }

    fn missing() -> Option<Self> {
        Some(Map::default())
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

impl<K: Data + Ord, V: Data, S: Summary<K, V>> Extend<(K, V)> for Map<K, V, S> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, items: I) {
        if !self.is_empty() {
            self.apply(items.into_iter().map(|(key, value)| (key, Some(value))));
            return;
        }
        let items = sorted_unique(items.into_iter().collect());
        self.root = build_sorted(items.into_iter().map(|(key, value)| Edit {
            level: fatal(level_of(&key)),
            key,
            value: Some(Arc::new(value)),
        }));
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

struct Frame<K, V, S> {
    node: Arc<Node<K, V, S>>,
    position: isize,
}

#[inline]
fn step<K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    path: &mut Vec<Frame<K, V, S>>,
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
        let child = frame.node.child((position / 2) as usize);
        if let Some(node) = fatal(resident(store, child, depth)) {
            path.push(Frame { node, position: 0 });
        }
    }
}

fn edge<K: Data, V: Data, S: Data>(
    store: Option<&Store>,
    link: &Link<K, V, S>,
    mut depth: usize,
    before: &impl Fn(&K) -> bool,
    offset: isize,
    path: &mut Vec<Frame<K, V, S>>,
) -> Result<u64> {
    let mut rank = 0u64;
    let mut current = resident(store, link, depth)?;
    while let Some(node) = current {
        let position = node.keys.partition_point(|key| before(key));
        rank += position as u64;
        rank += node.children.iter().take(position).map(Link::count).sum::<u64>();
        depth += 1;
        current = resident(store, node.child(position), depth)?;
        path.push(Frame {
            node,
            position: 2 * position as isize + offset,
        });
    }
    Ok(rank)
}

pub struct Cursor<K, V, S = ()> {
    store: Option<Store>,
    root: Link<K, V, S>,
    path: Vec<Frame<K, V, S>>,
    index: Option<usize>,
}

impl<K: Data + Ord, V: Data, S: Data> Cursor<K, V, S> {
    pub fn rewind(&mut self) {
        self.path.clear();
        if let Some(node) = fatal(resident(self.store.as_ref(), &self.root, 0)) {
            self.path.push(Frame { node, position: 0 });
        }
        self.index = step(self.store.as_ref(), &mut self.path);
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
            && let Some(node) = fatal(resident(self.store.as_ref(), &self.root, 0))
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
        let store = self.store.as_ref();
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
                    match fatal(resident(store, frame.node.child(position), depth)) {
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
        self.index = step(self.store.as_ref(), &mut self.path);
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
        self.index = step(self.store.as_ref(), &mut self.path);
        self.key()
    }

    pub fn key(&self) -> Option<&K> {
        let index = self.index?;
        self.path.last().map(|frame| &frame.node.keys[index])
    }

    pub fn value(&self) -> Option<Val<V>> {
        let index = self.index?;
        self.path.last().map(|frame| fatal(frame.node.value(self.store.as_ref(), index)))
    }

    pub fn read<R>(&self, f: impl FnOnce(&K, &V) -> R) -> Option<R> {
        let index = self.index?;
        let node = &self.path.last()?.node;
        Some(fatal(node.read(self.store.as_ref(), index, |value| f(&node.keys[index], value))))
    }
}

pub struct Iter<K, V, S = ()> {
    store: Option<Store>,
    front: Vec<Frame<K, V, S>>,
    back: Vec<Frame<K, V, S>>,
    remaining: u64,
}

impl<K: Data + Ord, V: Data, S: Data> Iter<K, V, S> {
    pub(crate) fn into_keys(self) -> Keys<K, V, S> {
        Keys(self)
    }

    #[inline]
    fn advance(&mut self) -> Option<usize> {
        if self.remaining == 0 {
            return None;
        }
        let index = step(self.store.as_ref(), &mut self.front)?;
        self.remaining -= 1;
        Some(index)
    }

    fn drain<B>(
        &mut self,
        init: B,
        mut visit: impl FnMut(B, &Node<K, V, S>, Range<usize>) -> B,
    ) -> B {
        let mut total = init;
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
                total = visit(total, &frame.node, start..stop);
                self.remaining -= (stop - start) as u64;
                frame.position = 2 * stop as isize + 1;
                continue;
            }
            let position = frame.position;
            frame.position += 1;
            if position % 2 == 1 {
                self.remaining -= 1;
                let index = (position / 2) as usize;
                total = visit(total, &frame.node, index..index + 1);
                continue;
            }
            let child = frame.node.child((position / 2) as usize);
            if let Some(node) = fatal(resident(self.store.as_ref(), child, depth)) {
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
            if let Some(node) = fatal(resident(self.store.as_ref(), child, depth)) {
                let position = 2 * node.keys.len() as isize;
                self.back.push(Frame { node, position });
            }
        }
    }

    fn value_of(&self, frame: Option<&Frame<K, V, S>>, index: usize) -> Option<Val<V>> {
        frame.map(|frame| fatal(frame.node.value(self.store.as_ref(), index)))
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
        let store = self.store.clone();
        self.drain(init, |mut total, node, run| {
            if let Cells::Column(column) = &node.cells
                && by_copy::<V>()
            {
                for (key, value) in node.keys[run.clone()].iter().zip(&column[run]) {
                    total = f(total, (key.clone(), Val(Held::Copied(value.clone()))));
                }
                return total;
            }
            for index in run {
                let value = fatal(node.value(store.as_ref(), index));
                total = f(total, (node.keys[index].clone(), value));
            }
            total
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
        self.0.front.last().map(|frame| frame.node.keys[index].clone())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    fn fold<B, F: FnMut(B, K) -> B>(mut self, init: B, mut f: F) -> B {
        self.0.drain(init, |total, node, run| node.keys[run].iter().cloned().fold(total, &mut f))
    }
}

impl<K: Data + Ord, V: Data, S: Data> DoubleEndedIterator for Keys<K, V, S> {
    fn next_back(&mut self) -> Option<K> {
        let index = self.0.retreat()?;
        self.0.back.last().map(|frame| frame.node.keys[index].clone())
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
        let store = self.0.store.clone();
        self.0.drain(init, |mut total, node, run| {
            for index in run {
                total = f(total, fatal(node.value(store.as_ref(), index)));
            }
            total
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

pub(crate) fn three_way<T, K, V, S>(
    target: &mut T,
    base: &Map<K, V, S>,
    theirs: &Map<K, V, S>,
    read: impl Fn(&T, &K) -> Option<Val<V>>,
    mut write: impl FnMut(&mut T, K, Option<V>),
    mut resolve: impl FnMut(Conflict<K, V>) -> Option<V>,
) -> usize
where
    K: Data + Ord,
    V: Data + PartialEq,
    S: Summary<K, V>,
{
    let mut written = 0;
    for change in base.diff(theirs) {
        let key = change.key().clone();
        let before = change.before().cloned();
        let after = change.after().cloned();
        let ours = read(target, &key);
        if ours == after {
            continue;
        }
        let value = if ours == before {
            after.map(|value| (*value).clone())
        } else {
            resolve(Conflict {
                key: key.clone(),
                base: before,
                ours,
                theirs: after,
            })
        };
        write(target, key, value);
        written += 1;
    }
    written
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

fn expand<K: Data + Ord, V: Data, S: Data>(
    store: Option<&Store>,
    steps: &mut Vec<Step<K, V, S>>,
) {
    let Some(Step::Sub(link)) = steps.pop() else {
        return;
    };
    let Some(node) = fatal(load(store, &link)) else {
        return;
    };
    for index in (0..=node.keys.len()).rev() {
        steps.push(Step::Sub(node.child(index).clone()));
        if index > 0 {
            steps.push(Step::Entry(node.keys[index - 1].clone(), node.slot(index - 1)));
        }
    }
}

impl<K: Data + Ord, V: Data + PartialEq, S: Data> Diff<K, V, S> {
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

impl<K: Data + Ord, V: Data + PartialEq, S: Data> Iterator for Diff<K, V, S> {
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
                        if let (Slot::Spilled(x, _), Slot::Spilled(y, _)) = (&a, &b)
                            && x.hash == y.hash
                        {
                            continue;
                        }
                        let before = fatal(read_slot(self.old_store.as_ref(), &a));
                        let after = fatal(read_slot(self.new_store.as_ref(), &b));
                        if before != after {
                            return Some(Change::Changed(key, before, after));
                        }
                    }
                }
            }
        }
    }
}
