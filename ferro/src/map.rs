use std::borrow::Borrow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

use crate::codec::{Data, Decoder, Encoder, Hash, HashSink, Ptr, Sink};
use crate::error::{Error, Result};
use crate::store::Store;
use xxhash_rust::xxh3::xxh3_64;

const SPILL_BYTES: usize = 512;
const LEVEL_BITS: u32 = 3;

pub(crate) fn fatal<T>(result: Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("ferro: {error}"),
    }
}

thread_local! {
    static KEY_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
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

enum Slot<V> {
    Inline(Arc<V>),
    Fresh(Arc<V>),
    Spilled(Ptr),
}

impl<V> Clone for Slot<V> {
    fn clone(&self) -> Self {
        match self {
            Slot::Inline(value) => Slot::Inline(value.clone()),
            Slot::Fresh(value) => Slot::Fresh(value.clone()),
            Slot::Spilled(ptr) => Slot::Spilled(*ptr),
        }
    }
}

enum Link<K, V> {
    Empty,
    Stored { ptr: Ptr, count: u64 },
    Mem(Arc<Node<K, V>>),
}

impl<K, V> Clone for Link<K, V> {
    fn clone(&self) -> Self {
        match self {
            Link::Empty => Link::Empty,
            Link::Stored { ptr, count } => Link::Stored {
                ptr: *ptr,
                count: *count,
            },
            Link::Mem(node) => Link::Mem(node.clone()),
        }
    }
}

impl<K, V> Link<K, V> {
    fn count(&self) -> u64 {
        match self {
            Link::Empty => 0,
            Link::Stored { count, .. } => *count,
            Link::Mem(node) => node.count,
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

struct Node<K, V> {
    level: u8,
    keys: Vec<K>,
    slots: Vec<Slot<V>>,
    children: Vec<Link<K, V>>,
    count: u64,
    origin: Option<Ptr>,
}

impl<K: Clone, V> Clone for Node<K, V> {
    fn clone(&self) -> Self {
        Node {
            level: self.level,
            keys: self.keys.clone(),
            slots: self.slots.clone(),
            children: self.children.clone(),
            count: self.count,
            origin: self.origin,
        }
    }
}

impl<K, V> Node<K, V> {
    fn search<Q>(&self, key: &Q) -> std::result::Result<usize, usize>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.keys.binary_search_by(|probe| probe.borrow().cmp(key))
    }

    fn recount(&mut self) {
        self.count =
            self.keys.len() as u64 + self.children.iter().map(Link::count).sum::<u64>();
    }
}

impl<K: Data, V: Data> Node<K, V> {
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
        let mut slots = Vec::with_capacity(keys.len());
        for _ in 0..n {
            slots.push(match decoder.u8()? {
                0 => Slot::Inline(Arc::new(V::decode(decoder)?)),
                1 => Slot::Spilled(decoder.link()?),
                other => return Err(Error::Decode(format!("invalid slot tag {other}"))),
            });
        }
        let mut children = Vec::with_capacity(keys.len() + 1);
        for _ in 0..=n {
            let count = decoder.varint()?;
            children.push(if count == 0 {
                Link::Empty
            } else {
                Link::Stored {
                    ptr: decoder.link()?,
                    count,
                }
            });
        }
        let mut node = Node {
            level,
            keys,
            slots,
            children,
            count: 0,
            origin: Some(origin),
        };
        node.recount();
        Ok(node)
    }
}

fn load<K: Data, V: Data>(
    store: Option<&Store>,
    link: &Link<K, V>,
) -> Result<Option<Arc<Node<K, V>>>> {
    match link {
        Link::Empty => Ok(None),
        Link::Mem(node) => Ok(Some(node.clone())),
        Link::Stored { ptr, .. } => {
            let store = store.ok_or(Error::Detached)?;
            let ptr = *ptr;
            Ok(Some(store.load_as(ptr, |decoder| Node::decode(decoder, ptr))?))
        }
    }
}

fn node_mut<'a, K: Data, V: Data>(
    store: Option<&Store>,
    link: &'a mut Link<K, V>,
) -> Result<&'a mut Node<K, V>> {
    if let Link::Stored { .. } = link {
        let node = load(store, link)?.ok_or(Error::Detached)?;
        *link = Link::Mem(node);
    }
    match link {
        Link::Mem(node) => Ok(Arc::make_mut(node)),
        _ => Err(Error::Detached),
    }
}

fn link_from<K, V>(
    level: u8,
    keys: Vec<K>,
    slots: Vec<Slot<V>>,
    mut children: Vec<Link<K, V>>,
) -> Link<K, V> {
    if keys.is_empty() {
        return children.pop().unwrap_or(Link::Empty);
    }
    let mut node = Node {
        level,
        keys,
        slots,
        children,
        count: 0,
        origin: None,
    };
    node.recount();
    Link::Mem(Arc::new(node))
}

fn split<K: Data + Ord, V: Data>(
    store: Option<&Store>,
    link: Link<K, V>,
    key: &K,
) -> Result<(Link<K, V>, Link<K, V>)> {
    let Some(node) = load(store, &link)? else {
        return Ok((Link::Empty, Link::Empty));
    };
    drop(link);
    let Node {
        level,
        mut keys,
        mut slots,
        mut children,
        ..
    } = Arc::unwrap_or_clone(node);
    let position = keys
        .binary_search(key)
        .err()
        .ok_or_else(|| Error::Corrupt("key found at a level it does not hash to".into()))?;
    let right_keys = keys.split_off(position);
    let right_slots = slots.split_off(position);
    let mut right_children = children.split_off(position + 1);
    let middle = children.pop().ok_or(Error::Detached)?;
    let (left_part, right_part) = split(store, middle, key)?;
    children.push(left_part);
    right_children.insert(0, right_part);
    Ok((
        link_from(level, keys, slots, children),
        link_from(level, right_keys, right_slots, right_children),
    ))
}

fn merge<K: Data + Ord, V: Data>(
    store: Option<&Store>,
    a: Link<K, V>,
    b: Link<K, V>,
) -> Result<Link<K, V>> {
    let (Some(x), Some(y)) = (load(store, &a)?, load(store, &b)?) else {
        return Ok(if a.count() == 0 { b } else { a });
    };
    match x.level.cmp(&y.level) {
        Ordering::Greater => {
            drop(a);
            let mut node = Arc::unwrap_or_clone(x);
            let last = node.children.pop().ok_or(Error::Detached)?;
            node.children.push(merge(store, last, b)?);
            node.recount();
            Ok(Link::Mem(Arc::new(node)))
        }
        Ordering::Less => {
            drop(b);
            let mut node = Arc::unwrap_or_clone(y);
            let first = node.children[0].take();
            node.children[0] = merge(store, a, first)?;
            node.recount();
            Ok(Link::Mem(Arc::new(node)))
        }
        Ordering::Equal => {
            drop(a);
            drop(b);
            let mut left = Arc::unwrap_or_clone(x);
            let mut right = Arc::unwrap_or_clone(y);
            let inner_left = left.children.pop().ok_or(Error::Detached)?;
            let inner_right = right.children[0].take();
            left.children.push(merge(store, inner_left, inner_right)?);
            left.children.extend(right.children.drain(1..));
            left.keys.append(&mut right.keys);
            left.slots.append(&mut right.slots);
            left.origin = None;
            left.recount();
            Ok(Link::Mem(Arc::new(left)))
        }
    }
}

fn insert_at<K: Data + Ord, V: Data>(
    store: Option<&Store>,
    link: &mut Link<K, V>,
    key: K,
    level: u8,
    value: Arc<V>,
) -> Result<Option<Slot<V>>> {
    let Some(current) = load(store, link)? else {
        *link = link_from(
            level,
            vec![key],
            vec![Slot::Fresh(value)],
            vec![Link::Empty, Link::Empty],
        );
        return Ok(None);
    };
    if level > current.level {
        drop(current);
        let (left, right) = split(store, link.take(), &key)?;
        *link = link_from(level, vec![key], vec![Slot::Fresh(value)], vec![left, right]);
        return Ok(None);
    }
    drop(current);
    let node = node_mut(store, link)?;
    match node.search(&key) {
        Ok(index) => Ok(Some(std::mem::replace(
            &mut node.slots[index],
            Slot::Fresh(value),
        ))),
        Err(position) if level == node.level => {
            let (left, right) = split(store, node.children[position].take(), &key)?;
            node.keys.insert(position, key);
            node.slots.insert(position, Slot::Fresh(value));
            node.children[position] = left;
            node.children.insert(position + 1, right);
            node.count += 1;
            Ok(None)
        }
        Err(position) => {
            let replaced = insert_at(store, &mut node.children[position], key, level, value)?;
            if replaced.is_none() {
                node.count += 1;
            }
            Ok(replaced)
        }
    }
}

fn remove_at<K, V, Q>(
    store: Option<&Store>,
    link: &mut Link<K, V>,
    key: &Q,
) -> Result<Option<Slot<V>>>
where
    K: Data + Ord + Borrow<Q>,
    V: Data,
    Q: Ord + ?Sized,
{
    if matches!(link, Link::Empty) {
        return Ok(None);
    }
    let node = node_mut(store, link)?;
    match node.search(key) {
        Ok(index) => {
            node.keys.remove(index);
            let slot = node.slots.remove(index);
            let right = node.children.remove(index + 1);
            let left = node.children[index].take();
            node.children[index] = merge(store, left, right)?;
            node.count -= 1;
            if node.keys.is_empty() {
                let only = node.children[0].take();
                *link = only;
            }
            Ok(Some(slot))
        }
        Err(position) => {
            let removed = remove_at(store, &mut node.children[position], key)?;
            if removed.is_some() {
                node.count -= 1;
            }
            Ok(removed)
        }
    }
}

fn find_mut<'a, K, V, Q>(
    store: Option<&Store>,
    link: &'a mut Link<K, V>,
    key: &Q,
) -> Result<Option<&'a mut V>>
where
    K: Data + Ord + Borrow<Q>,
    V: Data,
    Q: Ord + ?Sized,
{
    if matches!(link, Link::Empty) {
        return Ok(None);
    }
    let node = node_mut(store, link)?;
    match node.search(key) {
        Ok(index) => {
            let slot = &mut node.slots[index];
            let value = match &*slot {
                Slot::Inline(value) => Some(value.clone()),
                Slot::Fresh(_) => None,
                Slot::Spilled(ptr) => Some(store.ok_or(Error::Detached)?.load::<V>(*ptr)?),
            };
            if let Some(value) = value {
                *slot = Slot::Fresh(value);
            }
            match slot {
                Slot::Fresh(value) => Ok(Some(Arc::make_mut(value))),
                _ => Err(Error::Detached),
            }
        }
        Err(position) => find_mut(store, &mut node.children[position], key),
    }
}

fn read_slot<V: Data>(store: Option<&Store>, slot: &Slot<V>) -> Result<Arc<V>> {
    match slot {
        Slot::Inline(value) | Slot::Fresh(value) => Ok(value.clone()),
        Slot::Spilled(ptr) => store.ok_or(Error::Detached)?.load::<V>(*ptr),
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

fn flush<K: Data, V: Data>(link: &Link<K, V>, sink: &mut dyn Sink) -> Result<Link<K, V>> {
    let Link::Mem(node) = link else {
        return Ok(link.clone());
    };
    let store = sink.store();
    let caching = store.is_some();
    let width = node.keys.len();
    let mut encoder = Encoder::with_capacity(&mut *sink, 64 + 24 * width, width + 1);
    encoder.u8(node.level);
    encoder.varint(width as u64);
    for (key, slot) in node.keys.iter().zip(&node.slots) {
        let mark = encoder.mark();
        key.encode(&mut encoder)?;
        if let (Slot::Fresh(_), Some(store)) = (slot, &store) {
            verify::<K>(&encoder, mark, store)?;
        }
    }
    let mut settled: Vec<Slot<V>> = Vec::with_capacity(if caching { width } else { 0 });
    for slot in &node.slots {
        match slot {
            Slot::Spilled(ptr) => {
                encoder.u8(1);
                encoder.link(*ptr);
                if caching {
                    settled.push(Slot::Spilled(*ptr));
                }
            }
            Slot::Inline(value) | Slot::Fresh(value) => {
                let mark = encoder.mark();
                encoder.u8(0);
                let body = encoder.mark();
                value.encode(&mut encoder)?;
                if let (Slot::Fresh(_), Some(store)) = (slot, &store) {
                    verify::<V>(&encoder, body, store)?;
                }
                if encoder.since(mark) > SPILL_BYTES {
                    encoder.rewind(mark);
                    let ptr = encoder.child(|inner| value.encode(inner))?;
                    encoder.u8(1);
                    encoder.link(ptr);
                    if caching {
                        settled.push(Slot::Spilled(ptr));
                    }
                } else if caching {
                    settled.push(Slot::Inline(value.clone()));
                }
            }
        }
    }
    let mut stored: Vec<Link<K, V>> = Vec::with_capacity(if caching { width + 1 } else { 0 });
    for child in &node.children {
        let flushed = flush(child, encoder.sink())?;
        match &flushed {
            Link::Empty => encoder.varint(0),
            Link::Stored { ptr, count } => {
                encoder.varint(*count);
                encoder.link(*ptr);
            }
            Link::Mem(_) => return Err(Error::Detached),
        }
        if caching {
            stored.push(flushed);
        }
    }
    let unsettled = encoder.is_unsettled();
    let object = encoder.into_object();
    let hash = object.hash();
    let weight = (object.body.len() + object.links.len() * 40 + 128) as u32;
    let cached = match &store {
        None => None,
        Some(store) if unsettled => {
            let mut decoder = Decoder::new(&object.body, &object.links, Some(store));
            Some(Node::<K, V>::decode(&mut decoder, Ptr { hash, offset: 0 })?)
        }
        Some(_) => Some(Node {
            level: node.level,
            keys: node.keys.clone(),
            slots: settled,
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
        sink.remember(ptr, Arc::new(decoded), weight);
    }
    Ok(Link::Stored {
        ptr,
        count: node.count,
    })
}

struct Open<K, V> {
    level: u8,
    keys: Vec<K>,
    slots: Vec<Slot<V>>,
    children: Vec<Link<K, V>>,
}

impl<K, V> Open<K, V> {
    fn close(mut self, last: Link<K, V>) -> Link<K, V> {
        self.children.push(last);
        link_from(self.level, self.keys, self.slots, self.children)
    }
}

fn build_sorted<K: Data + Ord, V: Data>(items: Vec<(K, V)>) -> Result<Link<K, V>> {
    let mut stack: Vec<Open<K, V>> = Vec::new();
    for (key, value) in items {
        let level = level_of(&key)?;
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
                top.slots.push(Slot::Fresh(Arc::new(value)));
            }
            _ => stack.push(Open {
                level,
                keys: vec![key],
                slots: vec![Slot::Fresh(Arc::new(value))],
                children: vec![carry],
            }),
        }
    }
    let mut carry = Link::Empty;
    while let Some(top) = stack.pop() {
        carry = top.close(carry);
    }
    Ok(carry)
}

pub struct Map<K, V> {
    root: Link<K, V>,
    store: Option<Store>,
}

impl<K, V> Clone for Map<K, V> {
    fn clone(&self) -> Self {
        Map {
            root: self.root.clone(),
            store: self.store.clone(),
        }
    }
}

impl<K, V> Default for Map<K, V> {
    fn default() -> Self {
        Map {
            root: Link::Empty,
            store: None,
        }
    }
}

impl<K: Data + Ord, V: Data> Map<K, V> {
    pub fn new() -> Self {
        Map::default()
    }

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

    pub fn try_get<Q>(&self, key: &Q) -> Result<Option<Arc<V>>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut current = load(self.store(), &self.root)?;
        while let Some(node) = current {
            match node.search(key) {
                Ok(index) => return read_slot(self.store(), &node.slots[index]).map(Some),
                Err(position) => current = load(self.store(), &node.children[position])?,
            }
        }
        Ok(None)
    }

    pub fn get<Q>(&self, key: &Q) -> Option<Arc<V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        fatal(self.try_get(key))
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let walk = || -> Result<bool> {
            let mut current = load(self.store(), &self.root)?;
            while let Some(node) = current {
                match node.search(key) {
                    Ok(_) => return Ok(true),
                    Err(position) => current = load(self.store(), &node.children[position])?,
                }
            }
            Ok(false)
        };
        fatal(walk())
    }

    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if !self.contains_key(key) {
            return None;
        }
        let store = self.store.clone();
        fatal(find_mut(store.as_ref(), &mut self.root, key))
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

    pub fn insert(&mut self, key: K, value: V) -> Option<Arc<V>> {
        let store = self.store.clone();
        let level = fatal(level_of(&key));
        let replaced = fatal(insert_at(
            store.as_ref(),
            &mut self.root,
            key,
            level,
            Arc::new(value),
        ));
        replaced.map(|slot| fatal(read_slot(store.as_ref(), &slot)))
    }

    pub fn remove<Q>(&mut self, key: &Q) -> Option<Arc<V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if !self.contains_key(key) {
            return None;
        }
        let store = self.store.clone();
        let removed = fatal(remove_at(store.as_ref(), &mut self.root, key));
        removed.map(|slot| fatal(read_slot(store.as_ref(), &slot)))
    }

    pub fn first(&self) -> Option<(K, Arc<V>)> {
        self.iter().next()
    }

    pub fn last(&self) -> Option<(K, Arc<V>)> {
        self.iter().next_back()
    }

    pub fn pop_first(&mut self) -> Option<(K, Arc<V>)> {
        let (key, value) = self.first()?;
        self.remove(&key);
        Some((key, value))
    }

    pub fn pop_last(&mut self) -> Option<(K, Arc<V>)> {
        let (key, value) = self.last()?;
        self.remove(&key);
        Some((key, value))
    }

    pub fn nth(&self, index: usize) -> Option<(K, Arc<V>)> {
        let walk = || -> Result<Option<(K, Arc<V>)>> {
            let mut remaining = index as u64;
            let mut current = load(self.store(), &self.root)?;
            while let Some(node) = current {
                let mut next = None;
                for (position, child) in node.children.iter().enumerate() {
                    let size = child.count();
                    if remaining < size {
                        next = Some(position);
                        break;
                    }
                    remaining -= size;
                    if position < node.keys.len() {
                        if remaining == 0 {
                            let value = read_slot(self.store(), &node.slots[position])?;
                            return Ok(Some((node.keys[position].clone(), value)));
                        }
                        remaining -= 1;
                    }
                }
                let Some(position) = next else {
                    return Ok(None);
                };
                current = load(self.store(), &node.children[position])?;
            }
            Ok(None)
        };
        fatal(walk())
    }

    fn rank_by(&self, before: &impl Fn(&K) -> bool) -> Result<u64> {
        let mut total = 0u64;
        let mut current = load(self.store(), &self.root)?;
        while let Some(node) = current {
            let position = node.keys.partition_point(|key| before(key));
            total += position as u64;
            total += node.children[..position]
                .iter()
                .map(Link::count)
                .sum::<u64>();
            current = load(self.store(), &node.children[position])?;
        }
        Ok(total)
    }

    fn front_stack(&self, before: &impl Fn(&K) -> bool) -> Result<Vec<Frame<K, V>>> {
        let mut stack = Vec::new();
        let mut current = load(self.store(), &self.root)?;
        while let Some(node) = current {
            let position = node.keys.partition_point(|key| before(key));
            current = load(self.store(), &node.children[position])?;
            stack.push(Frame {
                node,
                position: 2 * position as isize + 1,
            });
        }
        Ok(stack)
    }

    fn back_stack(&self, before: &impl Fn(&K) -> bool) -> Result<Vec<Frame<K, V>>> {
        let mut stack = Vec::new();
        let mut current = load(self.store(), &self.root)?;
        while let Some(node) = current {
            let position = node.keys.partition_point(|key| before(key));
            current = load(self.store(), &node.children[position])?;
            stack.push(Frame {
                node,
                position: 2 * position as isize - 1,
            });
        }
        Ok(stack)
    }

    pub fn range_by(
        &self,
        below: impl Fn(&K) -> bool,
        within: impl Fn(&K) -> bool,
    ) -> Iter<K, V> {
        let build = || -> Result<Iter<K, V>> {
            let low = self.rank_by(&below)?;
            let high = self.rank_by(&within)?;
            Ok(Iter {
                store: self.store.clone(),
                front: self.front_stack(&below)?,
                back: self.back_stack(&within)?,
                remaining: high.saturating_sub(low),
            })
        };
        fatal(build())
    }

    pub fn iter(&self) -> Iter<K, V> {
        self.range_by(|_| false, |_| true)
    }

    pub fn keys(&self) -> Keys<K, V> {
        Keys(self.iter())
    }

    pub fn values(&self) -> Values<K, V> {
        Values(self.iter())
    }

    pub fn range<Q, R>(&self, range: R) -> Iter<K, V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        let start = range.start_bound();
        let end = range.end_bound();
        self.range_by(
            |key| match start {
                Bound::Included(bound) => key.borrow() < bound,
                Bound::Excluded(bound) => key.borrow() <= bound,
                Bound::Unbounded => false,
            },
            |key| match end {
                Bound::Included(bound) => key.borrow() <= bound,
                Bound::Excluded(bound) => key.borrow() < bound,
                Bound::Unbounded => true,
            },
        )
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let doomed: Vec<K> = self
            .iter()
            .filter(|(key, value)| !keep(key, value))
            .map(|(key, _)| key)
            .collect();
        for key in doomed {
            self.remove(&key);
        }
    }

    pub fn diff(&self, newer: &Map<K, V>) -> Diff<K, V>
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
        base: &Map<K, V>,
        theirs: &Map<K, V>,
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

    fn rebuilt(&self) -> Map<K, V> {
        let mut copy = Map::new();
        for (key, value) in self.iter() {
            copy.insert(key, (*value).clone());
        }
        copy
    }
}

impl<K: Data + Ord, V: Data> Data for Map<K, V> {
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
            Link::Stored { ptr, count } => {
                w.varint(count);
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
            Link::Stored {
                ptr: r.link()?,
                count,
            }
        };
        Ok(Map {
            root,
            store: r.store(),
        })
    }

    fn missing() -> Option<Self> {
        Some(Map::new())
    }
}

impl<K: Data + Ord, V: Data> PartialEq for Map<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.root_hash() == other.root_hash()
    }
}

impl<K: Data + Ord, V: Data> Eq for Map<K, V> {}

impl<K: Data + Ord + fmt::Debug, V: Data + fmt::Debug> fmt::Debug for Map<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Data + Ord, V: Data> Extend<(K, V)> for Map<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, items: I) {
        let mut items: Vec<(K, V)> = items.into_iter().collect();
        if !items.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            items.sort_by(|a, b| a.0.cmp(&b.0));
            let mut unique: Vec<(K, V)> = Vec::with_capacity(items.len());
            for item in items {
                match unique.last_mut() {
                    Some(last) if last.0 == item.0 => *last = item,
                    _ => unique.push(item),
                }
            }
            items = unique;
        }
        if self.is_empty() {
            self.root = fatal(build_sorted(items));
            return;
        }
        for (key, value) in items {
            self.insert(key, value);
        }
    }
}

impl<K: Data + Ord, V: Data> FromIterator<(K, V)> for Map<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(items: I) -> Self {
        let mut map = Map::new();
        map.extend(items);
        map
    }
}

impl<K: Data + Ord, V: Data> IntoIterator for &Map<K, V> {
    type Item = (K, Arc<V>);
    type IntoIter = Iter<K, V>;

    fn into_iter(self) -> Iter<K, V> {
        self.iter()
    }
}

struct Frame<K, V> {
    node: Arc<Node<K, V>>,
    position: isize,
}

pub struct Iter<K, V> {
    store: Option<Store>,
    front: Vec<Frame<K, V>>,
    back: Vec<Frame<K, V>>,
    remaining: u64,
}

impl<K: Data + Ord, V: Data> Iter<K, V> {
    fn advance(&mut self) -> Option<(Arc<Node<K, V>>, usize)> {
        if self.remaining == 0 {
            return None;
        }
        loop {
            let frame = self.front.last_mut()?;
            let limit = 2 * frame.node.keys.len() as isize;
            if frame.position > limit {
                self.front.pop();
                continue;
            }
            let position = frame.position;
            frame.position += 1;
            if position % 2 == 1 {
                self.remaining -= 1;
                return Some((frame.node.clone(), (position / 2) as usize));
            }
            let child = &frame.node.children[(position / 2) as usize];
            if let Some(node) = fatal(load(self.store.as_ref(), child)) {
                self.front.push(Frame { node, position: 0 });
            }
        }
    }

    fn retreat(&mut self) -> Option<(Arc<Node<K, V>>, usize)> {
        if self.remaining == 0 {
            return None;
        }
        loop {
            let frame = self.back.last_mut()?;
            if frame.position < 0 {
                self.back.pop();
                continue;
            }
            let position = frame.position;
            frame.position -= 1;
            if position % 2 == 1 {
                self.remaining -= 1;
                return Some((frame.node.clone(), (position / 2) as usize));
            }
            let child = &frame.node.children[(position / 2) as usize];
            if let Some(node) = fatal(load(self.store.as_ref(), child)) {
                let position = 2 * node.keys.len() as isize;
                self.back.push(Frame { node, position });
            }
        }
    }

    fn pair(&self, node: &Node<K, V>, index: usize) -> (K, Arc<V>) {
        let value = fatal(read_slot(self.store.as_ref(), &node.slots[index]));
        (node.keys[index].clone(), value)
    }
}

impl<K: Data + Ord, V: Data> Iterator for Iter<K, V> {
    type Item = (K, Arc<V>);

    fn next(&mut self) -> Option<Self::Item> {
        let (node, index) = self.advance()?;
        Some(self.pair(&node, index))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining as usize, Some(self.remaining as usize))
    }
}

impl<K: Data + Ord, V: Data> DoubleEndedIterator for Iter<K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let (node, index) = self.retreat()?;
        Some(self.pair(&node, index))
    }
}

impl<K: Data + Ord, V: Data> ExactSizeIterator for Iter<K, V> {}

pub struct Keys<K, V>(Iter<K, V>);

impl<K: Data + Ord, V: Data> Iterator for Keys<K, V> {
    type Item = K;

    fn next(&mut self) -> Option<K> {
        let (node, index) = self.0.advance()?;
        Some(node.keys[index].clone())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K: Data + Ord, V: Data> DoubleEndedIterator for Keys<K, V> {
    fn next_back(&mut self) -> Option<K> {
        let (node, index) = self.0.retreat()?;
        Some(node.keys[index].clone())
    }
}

impl<K: Data + Ord, V: Data> ExactSizeIterator for Keys<K, V> {}

pub struct Values<K, V>(Iter<K, V>);

impl<K: Data + Ord, V: Data> Iterator for Values<K, V> {
    type Item = Arc<V>;

    fn next(&mut self) -> Option<Arc<V>> {
        self.0.next().map(|(_, value)| value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K: Data + Ord, V: Data> DoubleEndedIterator for Values<K, V> {
    fn next_back(&mut self) -> Option<Arc<V>> {
        self.0.next_back().map(|(_, value)| value)
    }
}

impl<K: Data + Ord, V: Data> ExactSizeIterator for Values<K, V> {}

#[derive(Clone, Debug, PartialEq)]
pub enum Change<K, V> {
    Added(K, Arc<V>),
    Removed(K, Arc<V>),
    Changed(K, Arc<V>, Arc<V>),
}

impl<K, V> Change<K, V> {
    pub fn key(&self) -> &K {
        match self {
            Change::Added(key, _) | Change::Removed(key, _) | Change::Changed(key, _, _) => key,
        }
    }

    pub fn before(&self) -> Option<&Arc<V>> {
        match self {
            Change::Added(..) => None,
            Change::Removed(_, value) | Change::Changed(_, value, _) => Some(value),
        }
    }

    pub fn after(&self) -> Option<&Arc<V>> {
        match self {
            Change::Removed(..) => None,
            Change::Added(_, value) | Change::Changed(_, _, value) => Some(value),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Conflict<K, V> {
    pub key: K,
    pub base: Option<Arc<V>>,
    pub ours: Option<Arc<V>>,
    pub theirs: Option<Arc<V>>,
}

pub(crate) fn three_way<S, K, V>(
    target: &mut S,
    base: &Map<K, V>,
    theirs: &Map<K, V>,
    read: impl Fn(&S, &K) -> Option<Arc<V>>,
    mut write: impl FnMut(&mut S, K, Option<V>),
    mut resolve: impl FnMut(Conflict<K, V>) -> Option<V>,
) -> usize
where
    K: Data + Ord,
    V: Data + PartialEq,
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

enum Step<K, V> {
    Sub(Link<K, V>),
    Entry(K, Slot<V>),
}

pub struct Diff<K, V> {
    old_store: Option<Store>,
    new_store: Option<Store>,
    old: Vec<Step<K, V>>,
    new: Vec<Step<K, V>>,
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

fn expand<K: Data + Ord, V: Data>(store: Option<&Store>, steps: &mut Vec<Step<K, V>>) {
    let Some(Step::Sub(link)) = steps.pop() else {
        return;
    };
    let Some(node) = fatal(load(store, &link)) else {
        return;
    };
    for index in (0..=node.keys.len()).rev() {
        steps.push(Step::Sub(node.children[index].clone()));
        if index > 0 {
            steps.push(Step::Entry(
                node.keys[index - 1].clone(),
                node.slots[index - 1].clone(),
            ));
        }
    }
}

impl<K: Data + Ord, V: Data + PartialEq> Diff<K, V> {
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

impl<K: Data + Ord, V: Data + PartialEq> Iterator for Diff<K, V> {
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
                        if let (Slot::Spilled(x), Slot::Spilled(y)) = (&a, &b)
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
