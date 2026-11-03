use crate::error::{Error, Result};
use crate::log::{Entry, Merged, Node};
use crate::store::Store;

const ONE: u8 = 1;
const TWO: u8 = 2;
const BOTH: u8 = ONE | TWO;
const STALE: u8 = 4;

pub(crate) struct Start {
    pub(crate) store: Store,
    pub(crate) node: Node,
    pub(crate) low: u64,
    pub(crate) after: Option<u64>,
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
    found: Vec<(Store, Node)>,
    cut: [Vec<Mark>; 2],
    painted: [Vec<Mark>; 2],
}

pub(crate) fn best(ours: Vec<Start>, theirs: Vec<Start>) -> Result<Option<(Store, Node)>> {
    let mut search = Search {
        items: Vec::new(),
        found: Vec::new(),
        cut: [Vec::new(), Vec::new()],
        painted: [Vec::new(), Vec::new()],
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
                bare: false,
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
    fn run(mut self) -> Result<Option<(Store, Node)>> {
        loop {
            let live = |color: u8| {
                self.items.iter().any(|item| item.live() && item.colors & color != 0)
            };
            let (one, two) = (live(ONE), live(TWO));
            if !one && !two {
                break;
            }
            let inexact = self.items.iter().any(|item| item.live() && !item.exact());
            let cut = self.cut.iter().any(|cuts| !cuts.is_empty());
            if (!one || !two) && !inexact && !cut {
                break;
            }
            let top = (0..self.items.len())
                .max_by_key(|at| self.items[*at].level())
                .expect("a live item is queued");
            let item = self.items.swap_remove(top);
            match item.merges {
                None => self.advance(item)?,
                Some(above) => self.expand(item, above)?,
            }
        }
        for (side, other) in [(0, 1), (1, 0)] {
            let hidden = |cut: &Mark| {
                self.painted[other]
                    .iter()
                    .any(|seen| seen.time <= cut.time && seen.generation < cut.generation)
            };
            if self.cut[side].iter().any(hidden) {
                return Err(Error::NoCommonAncestor);
            }
        }
        if !self.cut[0].is_empty() && !self.cut[1].is_empty() {
            return Err(Error::NoCommonAncestor);
        }
        let key = |(_, node): &(Store, Node)| {
            (node.entry.generation(), node.time(), node.ptr.hash)
        };
        Ok(self.found.into_iter().max_by_key(key))
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

    fn cut(&mut self, colors: u8, at: &Node) {
        if let Some(side) = index(colors) {
            self.cut[side].push(Mark::of(at));
        }
    }

    fn painted(&mut self, colors: u8, bottom: &Node) {
        if let Some(side) = index(colors) {
            self.painted[side].push(Mark::of(bottom));
        }
    }

    fn advance(&mut self, mut item: Item) -> Result<()> {
        if item.colors & BOTH == BOTH && item.live() {
            self.found.push((item.store.clone(), item.node.clone()));
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
                if item.node.entry.prev.is_some() {
                    self.cut(item.colors, &item.node);
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
            _ => self.cut(item.colors, &merge),
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
            return Err(Error::Corrupt(format!("merge link below its floor {}", merged.floor)));
        }
        let mut low = merged.floor;
        let after = merged.after(item.after);
        let bare = after.is_some_and(|after| node.time() < after);
        if bare {
            low = node.version();
        } else if let Some(after) = after {
            let kept = |entry: &Entry| entry.commit.time_ms >= after;
            low = node.lowest_kept(&item.store, merged.floor, kept)?;
        }
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
    let (mut high_a, mut high_b) = (a.node.down_to(&a.store, top)?, b.node.down_to(&b.store, top)?);
    if high_a.ptr.hash == high_b.ptr.hash {
        return Ok(Some(top));
    }
    let same = |x: &Node, y: &Node| x.ptr.hash == y.ptr.hash;
    if !same(&high_a.down_to(&a.store, floor)?, &high_b.down_to(&b.store, floor)?) {
        return Ok(None);
    }
    let (mut low, mut high) = (floor, top);
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        let (x, y) = (high_a.down_to(&a.store, middle)?, high_b.down_to(&b.store, middle)?);
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
