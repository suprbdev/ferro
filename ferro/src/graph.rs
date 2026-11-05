//! The search for the best common ancestors of two sets of commits over the merge graph.
//!
//! [`best`] paints down from both sides as git's merge-base does, but in jumps: a chain moves
//! straight to the highest place another chain could join it, found by a binary search on versions,
//! and merge entries are opened only while what they merged could still matter. A commit reached
//! from both sides is a candidate, and everything below it is stale. The search refuses to answer
//! where pruned history could change the answer.
//! [Finding the base](https://github.com/archlemon/ferro/blob/main/docs/internals/merging.md#finding-the-base)
//! describes it.

use std::cmp::Reverse;

use crate::codec::Hash;
use crate::error::{Error, Result};
use crate::log::{Entry, Merged, Node};
use crate::store::Store;

const ONE: u8 = 1;
const TWO: u8 = 2;
const BOTH: u8 = ONE | TWO;
const STALE: u8 = 4;

/// A commit to search down from, with what is retained below it.
#[derive(Clone)]
pub(crate) struct Start {
    /// The file the commit is read from.
    pub(crate) store: Store,
    /// The commit's log entry.
    pub(crate) node: Node,
    /// The oldest version retained on the chain it was reached through.
    pub(crate) low: u64,
    /// The time before which commits on that chain count as pruned, where history is kept by
    /// time.
    pub(crate) after: Option<u64>,
    /// Whether only this commit is known, and not the history below it.
    pub(crate) bare: bool,
}

#[derive(Clone)]
struct Item {
    store: Store,
    node: Node,
    colors: u8,
    low: u64,
    after: Option<u64>,
    merges: Option<u64>,
    bare: bool,
}

#[derive(Clone, Copy)]
struct Mark {
    time: u64,
    generation: u64,
}

impl Mark {
    fn of(node: &Node) -> Mark {
        Mark {
            time: node.time(),
            generation: node.entry.generation(),
        }
    }

    fn under(&self, other: &Mark) -> bool {
        self.time <= other.time && self.generation < other.generation
    }
}

struct Hidden {
    parent: Hash,
    version: Option<u64>,
    colors: u8,
    at: Mark,
    reached: bool,
}

impl Item {
    fn live(&self) -> bool {
        self.colors & STALE == 0
    }

    fn level(&self) -> (u64, bool) {
        match self.merges {
            None => (self.node.entry.generation(), false),
            Some(_) => (self.node.entry.graph.reach, true),
        }
    }

    fn exact(&self) -> bool {
        self.low <= 1 && self.after.is_none() && !self.bare && !self.node.entry.graph.bounded
    }

    fn reach(&self) -> u64 {
        self.node.entry.graph.reach
    }

    fn wider(&self, other: &Item) -> bool {
        let rank = |item: &Item| {
            let after = item.after.map_or(0, |after| after.saturating_add(1));
            (item.low, after, item.bare)
        };
        rank(self) < rank(other)
    }
}

struct Search {
    items: Vec<Item>,
    found: Vec<Start>,
    cut: [Vec<Mark>; 2],
    painted: [Vec<Mark>; 2],
    hidden: Vec<Hidden>,
}

/// The best common ancestors of the commits in `ours` and those in `theirs`: the commits both
/// sides reach through `prev` and merge links that are not below another such commit.
///
/// They come back by generation, then time, then entry hash, highest first and without
/// duplicates, so the order depends only on the set. None come back when the sides share no
/// commit and nothing is pruned.
///
/// # Errors
///
/// [`Error::NoCommonAncestor`] when pruned history could hold a better ancestor or hide one, and
/// [`Error::Corrupt`] or read errors from the logs.
pub(crate) fn best(ours: Vec<Start>, theirs: Vec<Start>) -> Result<Vec<Start>> {
    let mut search = Search {
        items: Vec::new(),
        found: Vec::new(),
        cut: [Vec::new(), Vec::new()],
        painted: [Vec::new(), Vec::new()],
        hidden: Vec::new(),
    };
    for (colors, starts) in [(ONE, ours), (TWO, theirs)] {
        for start in starts {
            search.push(Item {
                store: start.store,
                node: start.node,
                colors,
                low: start.low.max(1),
                after: start.after,
                merges: None,
                bare: start.bare,
            });
        }
    }
    search.run()
}

fn ends_early() -> Error {
    Error::Corrupt("commit log ends early".into())
}

fn index(colors: u8) -> Option<usize> {
    match colors & (BOTH | STALE) {
        ONE => Some(0),
        TWO => Some(1),
        _ => None,
    }
}

impl Search {
    fn run(mut self) -> Result<Vec<Start>> {
        loop {
            let Some(top) = (0..self.items.len()).max_by_key(|at| self.items[*at].level()) else {
                break;
            };
            let (level, _) = self.items[top].level();
            let mut unreached = self.hidden.iter().filter(|hidden| !hidden.reached);
            let open = unreached
                .clone()
                .any(|hidden| hidden.version.is_some_and(|v| v <= level));
            let live = |color: u8| {
                self.items
                    .iter()
                    .any(|item| item.live() && item.colors & color != 0)
            };
            let (one, two) = (live(ONE), live(TWO));
            if !one && !two && !open {
                break;
            }
            let inexact = self.items.iter().any(|item| item.live() && !item.exact());
            let cut = self.cut.iter().any(|cuts| !cuts.is_empty())
                || unreached.any(|hidden| index(hidden.colors).is_some());
            if (!one || !two) && !inexact && !cut && !open {
                break;
            }
            let item = self.items.swap_remove(top);
            match item.merges {
                None => self.advance(item)?,
                Some(above) => self.expand(item, above)?,
            }
        }
        let mut stale = Vec::new();
        for hidden in std::mem::take(&mut self.hidden) {
            match index(hidden.colors) {
                Some(side) if !hidden.reached => self.cut[side].push(hidden.at),
                None if !hidden.reached || hidden.version.is_none() => stale.push(Mark {
                    generation: hidden.at.generation.saturating_sub(1),
                    ..hidden.at
                }),
                _ => {}
            }
        }
        for (side, other) in [(0, 1), (1, 0)] {
            let hidden = |cut: &Mark| self.painted[other].iter().any(|seen| seen.under(cut));
            if self.cut[side].iter().any(hidden) {
                return Err(Error::NoCommonAncestor);
            }
        }
        if !self.cut[0].is_empty() && !self.cut[1].is_empty() {
            return Err(Error::NoCommonAncestor);
        }
        let found = self.found.iter().map(|start| Mark::of(&start.node));
        if stale
            .iter()
            .any(|bound| found.clone().any(|base| base.under(bound)))
        {
            return Err(Error::NoCommonAncestor);
        }
        let key = |start: &Start| {
            let node = &start.node;
            Reverse((node.entry.generation(), node.time(), node.ptr.hash))
        };
        let mut found = self.found;
        found.sort_by_key(key);
        found.dedup_by_key(|start| start.node.ptr.hash);
        Ok(found)
    }

    fn push(&mut self, item: Item) {
        if item.merges.is_some() {
            self.items.push(item);
            return;
        }
        let same = self
            .items
            .iter_mut()
            .find(|queued| queued.merges.is_none() && queued.node.ptr.hash == item.node.ptr.hash);
        match same {
            Some(queued) => {
                let colors = queued.colors | item.colors;
                if item.wider(queued) {
                    *queued = item;
                }
                queued.colors = colors;
            }
            None => self.items.push(item),
        }
    }

    fn cut(&mut self, colors: u8, at: &Node, parent: Hash, version: Option<u64>) {
        let mark = Mark::of(at);
        match (index(colors), version) {
            (Some(side), None) => self.cut[side].push(mark),
            _ => self.hidden.push(Hidden {
                parent,
                version,
                colors,
                at: mark,
                reached: false,
            }),
        }
    }

    fn painted(&mut self, colors: u8, bottom: &Node) {
        if let Some(side) = index(colors) {
            self.painted[side].push(Mark::of(bottom));
        }
    }

    fn advance(&mut self, mut item: Item) -> Result<()> {
        for hidden in &mut self.hidden {
            if hidden.parent == item.node.ptr.hash {
                hidden.reached = true;
                item.colors |= hidden.colors;
            }
        }
        if item.colors & BOTH == BOTH && item.live() {
            self.found.push(Start {
                store: item.store.clone(),
                node: item.node.clone(),
                low: item.low,
                after: item.after,
                bare: item.bare,
            });
            item.colors |= STALE;
        }
        let at = item.node.version();
        let mut target = (item.low < at).then_some(item.low);
        let mut reach = item.reach();
        for other in &self.items {
            if other.merges.is_none()
                && let Some(meet) = meet(&item, other)?
                && meet < at
            {
                target = target.max(Some(meet));
            }
            reach = reach.max(other.reach());
        }
        for hidden in &self.hidden {
            if let Some(version) = hidden.version
                && !hidden.reached
                && item.low <= version
                && version < at
                && item.node.down_to(&item.store, version)?.ptr.hash == hidden.parent
            {
                target = target.max(Some(version));
            }
        }
        if reach > 0 {
            let above = |entry: &Entry| entry.generation() > reach;
            let found = item.node.last_where(&item.store, item.low, above)?;
            if let Some(node) = found.filter(|node| node.version() < at) {
                target = target.max(Some(node.version()));
            }
        }
        let last_merge = item.node.entry.graph.last_merge;
        match target {
            Some(version) => {
                if last_merge > version {
                    self.push(Item {
                        merges: Some(version),
                        ..item.clone()
                    });
                }
                let above = item.node.down_to(&item.store, version + 1)?;
                if item.live() {
                    self.painted(item.colors, &above);
                }
                let node = Node::load(&item.store, above.entry.prev.ok_or_else(ends_early)?)?;
                self.push(Item { node, ..item });
            }
            None => {
                if last_merge >= item.low {
                    self.push(Item {
                        merges: Some(item.low - 1),
                        ..item.clone()
                    });
                }
                if item.live() {
                    self.painted(item.colors, &item.node);
                }
                if let Some(prev) = item.node.entry.prev {
                    self.cut(item.colors, &item.node, prev.hash, Some(at - 1));
                }
            }
        }
        Ok(())
    }

    fn expand(&mut self, item: Item, above: u64) -> Result<()> {
        let last = item.node.entry.graph.last_merge;
        if last == 0 || last <= above || last < item.low {
            return Ok(());
        }
        let merge = item.node.down_to(&item.store, last)?;
        let merged = merge
            .entry
            .merged
            .ok_or_else(|| Error::Corrupt(format!("commit log entry {last} is not a merge")))?;
        match self.closure(&item, merged)? {
            Some(child) if !item.bare => self.push(child),
            _ => self.cut(item.colors, &merge, merged.ptr.hash, None),
        }
        if last > item.low && last - 1 > above {
            let node = Node::load(&item.store, merge.entry.prev.ok_or_else(ends_early)?)?;
            let next = node.entry.graph.last_merge;
            if next > above && next >= item.low {
                self.push(Item { node, ..item });
            }
        }
        Ok(())
    }

    fn closure(&self, item: &Item, merged: Merged) -> Result<Option<Item>> {
        if merged.ptr.offset == 0 {
            return Ok(None);
        }
        let node = Node::load(&item.store, merged.ptr)?;
        if node.version() < merged.floor {
            return Err(Error::Corrupt(format!(
                "merge link below its floor {}",
                merged.floor
            )));
        }
        let after = merged.after(item.after);
        let bare = after.is_some_and(|after| node.time() < after);
        let kept = |entry: &Entry| after.is_none_or(|after| entry.commit.time_ms >= after);
        let low = if bare {
            node.version()
        } else {
            node.lowest_kept(&item.store, merged.floor, kept)?
        };
        Ok(Some(Item {
            store: item.store.clone(),
            node,
            colors: item.colors,
            low,
            after,
            merges: None,
            bare,
        }))
    }
}

fn meet(a: &Item, b: &Item) -> Result<Option<u64>> {
    let floor = a.low.max(b.low);
    let top = a.node.version().min(b.node.version());
    if floor > top {
        return Ok(None);
    }
    let (mut high_a, mut high_b) = (
        a.node.down_to(&a.store, top)?,
        b.node.down_to(&b.store, top)?,
    );
    if high_a.ptr.hash == high_b.ptr.hash {
        return Ok(Some(top));
    }
    let same = |x: &Node, y: &Node| x.ptr.hash == y.ptr.hash;
    let (low_a, low_b) = (
        high_a.down_to(&a.store, floor)?,
        high_b.down_to(&b.store, floor)?,
    );
    if !same(&low_a, &low_b) {
        let parent = |node: &Node| node.entry.prev.map(|prev| prev.hash);
        let joined = a.low < floor && parent(&low_a).is_some() && parent(&low_a) == parent(&low_b);
        return Ok(joined.then(|| floor - 1));
    }
    let (mut low, mut high) = (floor, top);
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        let (x, y) = (
            high_a.down_to(&a.store, middle)?,
            high_b.down_to(&b.store, middle)?,
        );
        if same(&x, &y) {
            low = middle;
        } else {
            high = middle;
            high_a = x;
            high_b = y;
        }
    }
    Ok(Some(low))
}
