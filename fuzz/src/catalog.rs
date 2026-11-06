use std::fmt::Debug;
use std::ops::Bound;

use ferro::map::Summary;
use ferro::{Bytes, Data, List, Map, Record, Set, Table};

use crate::guarded;
use crate::input::Input;
use crate::types::{Gen, Heft, Inner, Probe, Total};

#[derive(Data, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Free,
    Paid,
    Gold,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
pub struct ItemV1 {
    #[key]
    pub id: u32,
    #[index]
    pub shelf: u8,
    #[index]
    pub price: i64,
    #[index(each)]
    pub tags: Vec<String>,
    #[unique]
    pub code: Option<String>,
    pub tier: Tier,
    pub label: String,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(shelf, rank, cover(rank, price))]
#[unique(shelf, label, cover(price))]
#[index(vendor, sku)]
pub struct Item {
    #[key]
    pub id: u32,
    #[index(cover(tier, price, label))]
    pub shelf: u8,
    #[index(cover(price))]
    pub price: i64,
    #[index(cover(price, tier))]
    pub coupon: Option<String>,
    #[index(each, cover(tier))]
    pub tags: Vec<String>,
    #[unique(cover(shelf, tier))]
    pub code: Option<String>,
    #[index]
    #[ferro(default)]
    pub rank: u8,
    pub tier: Tier,
    pub label: String,
    #[ferro(default)]
    pub vendor: u8,
    #[ferro(default)]
    pub sku: u16,
}

fn seven() -> u8 {
    7
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
#[index(vendor, sku)]
#[unique(shelf, label, cover(tier))]
pub struct ItemV3 {
    #[key]
    pub id: u32,
    #[index(cover(label))]
    pub shelf: u8,
    #[index]
    #[ferro(rename = "price")]
    pub cost: i64,
    #[index]
    pub tags: Vec<String>,
    #[index(cover(tier))]
    pub code: Option<String>,
    #[ferro(default = "seven")]
    pub rank: u8,
    pub tier: Tier,
    #[unique(cover(tier))]
    pub label: String,
    #[ferro(default)]
    pub vendor: u8,
    #[ferro(default)]
    pub sku: u16,
    #[index(each)]
    pub marks: Vec<u16>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
pub struct CatalogV1 {
    pub items: Table<ItemV1>,
    pub notes: Map<u32, String>,
    pub stamp: u64,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    pub items: Table<Item>,
    pub notes: Map<u32, String>,
    pub stamp: u64,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
pub struct CatalogV3 {
    pub items: Table<ItemV3>,
    pub notes: Map<u32, String>,
    pub stamp: u64,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
pub struct Person {
    #[key]
    pub id: u64,
    #[unique]
    pub email: String,
    #[index]
    pub city: String,
    pub tags: Vec<String>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
pub struct World {
    pub names: Map<u64, String>,
    pub nested: Map<u64, Map<u64, u64>, Inner>,
    pub tags: Set<String>,
    pub events: List<u64>,
    pub people: Table<Person>,
    pub items: Table<Item>,
    pub blobs: Map<u64, Bytes, Heft>,
    pub totals: Map<u64, u64, Total>,
    pub note: Option<Box<(u64, String)>>,
}

pub type Pairs = Map<u32, u32>;
pub type Ints = Map<u64, u64>;

pub fn word(seed: u64, len: usize) -> String {
    (0..len)
        .map(|at| {
            let mixed = seed
                .wrapping_add(1)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add((at as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9));
            char::from(b'a' + ((mixed >> 33) % 26) as u8)
        })
        .collect()
}

fn tier(seed: u64) -> Tier {
    match seed % 3 {
        0 => Tier::Free,
        1 => Tier::Paid,
        _ => Tier::Gold,
    }
}

pub fn item(id: u32, round: u64) -> Item {
    let seed = u64::from(id).wrapping_mul(7).wrapping_add(round);
    Item {
        id,
        shelf: (seed % 5) as u8,
        price: (seed % 9) as i64 * 100 - 200,
        coupon: (seed % 3 == 0).then(|| word(seed % 4, 3)),
        tags: (0..seed % 4).map(|at| word((seed + at) % 6, 2)).collect(),
        code: (seed % 2 == 0).then(|| format!("c{id}")),
        rank: (seed % 4) as u8,
        tier: tier(seed),
        label: format!("item {id} {}", word(seed, 4)),
        vendor: (id % 3) as u8,
        sku: id as u16,
    }
}

pub fn item_v1(id: u32, round: u64) -> ItemV1 {
    let full = item(id, round);
    ItemV1 {
        id,
        shelf: full.shelf,
        price: full.price,
        tags: full.tags,
        code: full.code,
        tier: full.tier,
        label: full.label,
    }
}

pub fn item_v3(id: u32, round: u64) -> ItemV3 {
    let full = item(id, round);
    ItemV3 {
        id,
        shelf: full.shelf,
        cost: full.price,
        tags: full.tags,
        code: full.code,
        rank: id as u8,
        tier: full.tier,
        label: full.label,
        vendor: full.vendor,
        sku: full.sku,
        marks: (0..round % 3)
            .map(|at| (id as u16).wrapping_add(at as u16))
            .collect(),
    }
}

pub fn person(id: u64, round: u64) -> Person {
    Person {
        id,
        email: format!("user{id}@{round}.example"),
        city: format!("city{}", (id + round) % 7),
        tags: (0..id % 3).map(|tag| format!("t{tag}")).collect(),
    }
}

pub fn fill(world: &mut World, round: u64) {
    for key in (0..300).step_by(1 + round as usize * 7) {
        world
            .names
            .insert(key, word(key + round, 20 + (key % 50) as usize));
        world.totals.insert(key, key * (round + 1));
    }
    for outer in (0..30).step_by(1 + round as usize * 3) {
        let inner = world.nested.get_or_insert_with(outer, Map::new);
        for key in 0..25 {
            inner.insert(key, outer * 1000 + key + round);
        }
    }
    for tag in (0..80).step_by(1 + round as usize * 5) {
        world.tags.insert(format!("tag{tag}-{round}"));
    }
    for event in 0..60 {
        world.events.push(event * 3 + round);
    }
    for id in (0..80).step_by(1 + round as usize * 4) {
        world.people.insert(person(id, round));
    }
    for id in (0..60).step_by(1 + round as usize * 2) {
        world.items.insert(item(id, round));
    }
    for key in (0..12).step_by(1 + round as usize) {
        let seed = if key % 3 == 0 {
            round
        } else {
            key * 31 + round
        };
        world
            .blobs
            .insert(key, Bytes::from(word(seed, 700).into_bytes()));
    }
    world.note = Some(Box::new((round, format!("round {round}"))));
}

pub fn folds<K, V, S>(map: &Map<K, V, S>)
where
    K: Data + Ord,
    V: Data,
    S: Summary<K, V> + PartialEq + Debug,
{
    let mut folded: Option<S> = None;
    for (key, value) in map.iter() {
        let entry = S::of(&key, &value);
        folded = Some(match folded {
            Some(total) => total.combine(&entry),
            None => entry,
        });
    }
    assert_eq!(
        map.summary(),
        folded,
        "a map's summary is not the fold of its entries"
    );
    if let Some((middle, _)) = map.nth(map.len() / 2) {
        let joined = match (
            map.range_summary::<K, _>(..middle.clone()),
            map.range_summary::<K, _>(middle..),
        ) {
            (Some(below), Some(above)) => Some(below.combine(&above)),
            (below, above) => below.or(above),
        };
        assert_eq!(joined, folded, "a map's halves do not fold to its summary");
    }
}

pub fn whole<K, V, S>(map: &Map<K, V, S>) -> usize
where
    K: Data + Ord,
    V: Data,
    S: Summary<K, V>,
{
    let mut seen = 0;
    for (key, _) in map.iter() {
        assert!(
            map.get(&key).is_some(),
            "an iterated key is not found by get"
        );
        seen += 1;
    }
    assert_eq!(map.len(), seen, "a map's length differs from its iteration");
    assert_eq!(
        map.iter().rev().count(),
        seen,
        "a map iterates differently backwards"
    );
    seen
}

fn unique_whole<I: DoubleEndedIterator + ExactSizeIterator>(
    keys: impl Fn() -> I,
    len: usize,
) -> usize {
    assert_eq!(
        keys().len(),
        len,
        "a unique range's length differs from its index"
    );
    assert_eq!(
        keys().count(),
        len,
        "a unique range's length differs from its walk"
    );
    assert_eq!(
        keys().rev().count(),
        len,
        "a unique range walks differently backwards"
    );
    len
}

pub fn items_whole(items: &Table<Item>) -> usize {
    let mut seen = whole(items.rows());
    seen += unique_whole(
        || items.by_code_range::<str, _>(..).keys(),
        items.unique_code().len(),
    );
    seen += unique_whole(
        || items.by_shelf_label_range::<(u8, String), _>(..).keys(),
        items.unique_shelf_label().len(),
    );
    for row in items.iter() {
        seen += items.by_shelf(&row.shelf).count();
        seen += items.by_shelf(&row.shelf).covered().count();
        seen += items.by_price_range(row.price..).take(4).count();
        seen += items.by_coupon_some("abc").count();
        seen += items.by_tags_counts().len();
        seen += items.by_shelf_rank_prefix(&row.shelf).covered().count();
        seen += items.by_shelf_label_prefix(&row.shelf).count();
        seen += items
            .by_shelf_label_prefix(&row.shelf)
            .covered()
            .rev()
            .count();
        seen += usize::from(
            items
                .covered_by_shelf_label(&(row.shelf, row.label.clone()))
                .is_some(),
        );
        if let Some(code) = &row.code {
            seen += usize::from(items.key_by_code(code).is_some());
            seen += usize::from(items.covered_by_code(code).is_some());
            seen += items
                .by_code_range::<str, _>((Bound::Included(code.as_str()), Bound::Unbounded))
                .take(3)
                .count();
        }
    }
    seen
}

pub fn world_whole(world: &World) -> usize {
    let mut seen = whole(&world.names);
    seen += whole(&world.nested);
    folds(&world.nested);
    for (_, inner) in world.nested.iter() {
        seen += whole(&inner);
    }
    assert_eq!(world.tags.len(), world.tags.iter().count());
    for tag in world.tags.iter() {
        assert!(world.tags.contains(&tag), "an iterated tag is not found");
    }
    assert_eq!(world.events.len(), world.events.iter().count());
    for index in 0..world.events.len() {
        assert!(world.events.get(index).is_some(), "an event is missing");
    }
    seen += whole(world.people.rows());
    for found in world.people.iter() {
        let _ = world.people.by_email(&found.email);
        seen += world.people.by_city(&found.city).count();
    }
    seen += world.people.by_city_counts().len();
    seen += unique_whole(
        || world.people.by_email_range::<str, _>(..).keys(),
        world.people.unique_email().len(),
    );
    seen += items_whole(&world.items);
    seen += whole(&world.blobs);
    for (_, blob) in world.blobs.iter() {
        seen += blob.len();
    }
    folds(&world.blobs);
    seen += whole(&world.totals);
    folds(&world.totals);
    seen + world.note.as_ref().map_or(0, |note| note.1.len())
}

fn budgeted(budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    true
}

impl Probe for Item {}
impl Probe for ItemV1 {}
impl Probe for ItemV3 {}
impl Probe for Person {}

impl Probe for CatalogV1 {
    fn probe(&self, budget: &mut usize) {
        self.notes.probe(budget);
        self.items.rows().probe(budget);
        let items = &self.items;
        guarded(|| items.by_shelf_counts());
        guarded(|| items.by_tags("ab").take(8).count());
        guarded(|| items.unique_code().len());
        guarded(|| items.by_code_range::<str, _>(..).keys().rev().count());
        guarded(|| {
            for row in items.iter() {
                if !budgeted(budget) {
                    break;
                }
                let _ = items.by_shelf(&row.shelf).take(4).count();
                let _ = items.by_price_range(..row.price).next_back();
                if let Some(code) = &row.code {
                    let _ = items.by_code(code);
                    let _ = items
                        .by_code_range::<str, _>((Bound::Excluded(code.as_str()), Bound::Unbounded))
                        .take(2)
                        .count();
                }
            }
        });
    }

    fn measure(&self) -> u64 {
        (self.items.len() as u64).wrapping_add(self.stamp)
    }

    fn poke(&mut self, seed: u64) {
        self.stamp = self.stamp.wrapping_add(1);
        let id = (seed % 97) as u32;
        match seed % 3 {
            0 => {
                let _ = self.items.try_insert(item_v1(id, seed));
            }
            1 => {
                let _ = self.items.try_update(&id, |row| row.price += 1);
            }
            _ => {
                self.items.remove(&id);
            }
        }
        self.notes.insert(id, word(seed, 5));
    }
}

impl Probe for Catalog {
    fn probe(&self, budget: &mut usize) {
        self.notes.probe(budget);
        self.items.rows().probe(budget);
        let items = &self.items;
        guarded(|| items.by_shelf_counts());
        guarded(|| items.by_tags("ab").covered().take(8).count());
        guarded(|| items.by_coupon_some("abc").covered().count());
        guarded(|| items.by_coupon(&None::<String>).take(4).count());
        guarded(|| items.unique_code().len());
        guarded(|| items.unique_shelf_label().len());
        guarded(|| items.by_rank_range(1..3).keys().count());
        guarded(|| items.by_code_range::<str, _>(..).covered().take(8).count());
        guarded(|| {
            items
                .by_shelf_label_range::<(u8, String), _>(..)
                .rev()
                .take(8)
                .count()
        });
        guarded(|| {
            for row in items.iter() {
                if !budgeted(budget) {
                    break;
                }
                let _ = items.by_shelf(&row.shelf).covered().rev().take(4).count();
                let _ = items.by_shelf_after(&row.shelf, &row.id).take(2).count();
                let _ = items.by_price_range(..row.price).covered().next_back();
                let _ = items.by_shelf_rank(&(row.shelf, row.rank)).count();
                let _ = items.by_shelf_rank_prefix(&row.shelf).covered().count();
                let _ = items.covered_by_shelf_label(&(row.shelf, row.label.clone()));
                let _ = items.by_vendor_sku_prefix(&row.vendor).count();
                let _ = items.by_shelf_label_prefix(&row.shelf).covered().count();
                let _ = items.by_shelf_label_prefix(&row.shelf).next_back();
                for tag in &row.tags {
                    let _ = items.by_tags_after(tag, &row.id).covered().count();
                }
                if let Some(code) = &row.code {
                    let _ = items.covered_by_code(code);
                    let _ = items.by_code(code);
                    let _ = items
                        .by_code_range::<str, _>((Bound::Unbounded, Bound::Included(code.as_str())))
                        .rev()
                        .take(2)
                        .count();
                }
            }
        });
    }

    fn measure(&self) -> u64 {
        (self.items.len() as u64).wrapping_add(self.stamp)
    }

    fn poke(&mut self, seed: u64) {
        self.stamp = self.stamp.wrapping_add(1);
        let id = (seed % 97) as u32;
        match seed % 4 {
            0 => {
                let _ = self.items.try_insert(item(id, seed));
            }
            1 => {
                let _ = self.items.try_update(&id, |row| {
                    row.price += 1;
                    row.tags.push(word(seed, 2));
                });
            }
            2 => {
                let _ = self.items.try_update(&id, |row| {
                    row.code = None;
                    row.tier = Tier::Gold;
                });
            }
            _ => {
                self.items.remove(&id);
            }
        }
        self.notes.insert(id, word(seed, 5));
    }
}

impl Probe for CatalogV3 {
    fn probe(&self, budget: &mut usize) {
        self.notes.probe(budget);
        self.items.rows().probe(budget);
        let items = &self.items;
        guarded(|| items.by_shelf_counts());
        guarded(|| items.by_marks_counts());
        guarded(|| items.unique_label().len());
        guarded(|| items.unique_shelf_label().len());
        guarded(|| {
            items
                .by_label_range::<str, _>(..)
                .covered()
                .rev()
                .take(8)
                .count()
        });
        guarded(|| {
            for row in items.iter() {
                if !budgeted(budget) {
                    break;
                }
                let _ = items.by_shelf(&row.shelf).covered().take(4).count();
                let _ = items
                    .by_label_range::<str, _>((
                        Bound::Included(row.label.as_str()),
                        Bound::Unbounded,
                    ))
                    .take(2)
                    .count();
                let _ = items.by_shelf_label_prefix(&row.shelf).keys().count();
                let _ = items.by_cost_range(row.cost..).take(2).count();
                let _ = items.by_tags(&row.tags).count();
                let _ = items.by_vendor_sku_prefix(&row.vendor).take(3).count();
                let _ = items.covered_by_label(&row.label);
                let _ = items.covered_by_shelf_label(&(row.shelf, row.label.clone()));
                let _ = items.by_code(&row.code).covered().count();
            }
        });
    }

    fn measure(&self) -> u64 {
        (self.items.len() as u64).wrapping_add(self.stamp)
    }

    fn poke(&mut self, seed: u64) {
        self.stamp = self.stamp.wrapping_add(1);
        let id = (seed % 97) as u32;
        match seed % 3 {
            0 => {
                let _ = self.items.try_insert(item_v3(id, seed));
            }
            1 => {
                let _ = self
                    .items
                    .try_update(&id, |row| row.marks.push(seed as u16));
            }
            _ => {
                self.items.remove(&id);
            }
        }
        self.notes.insert(id, word(seed, 5));
    }
}

impl Probe for World {
    fn probe(&self, budget: &mut usize) {
        self.names.probe(budget);
        self.nested.probe(budget);
        self.blobs.probe(budget);
        self.totals.probe(budget);
        self.people.rows().probe(budget);
        self.items.rows().probe(budget);
        guarded(|| self.tags.iter().take(16).count());
        guarded(|| self.tags.contains("tag0-0"));
        guarded(|| self.events.iter().take(16).count());
        guarded(|| self.events.get(self.events.len() / 2));
        guarded(|| self.people.by_city_counts());
        guarded(|| self.people.by_city("city1").take(8).count());
        guarded(|| self.people.by_email("user1@0.example"));
        guarded(|| self.people.by_email_range::<str, _>(..).take(8).count());
        guarded(|| self.items.by_code_range::<str, _>(..).rev().take(8).count());
        guarded(|| self.items.by_shelf(&1).covered().take(8).count());
        guarded(|| self.items.unique_shelf_label().len());
        guarded(|| self.items.by_tags_counts().len());
        guarded(|| self.items.by_shelf_rank_prefix(&2).covered().count());
        guarded(|| self.items.by_price_range(..0).covered().next_back());
    }

    fn measure(&self) -> u64 {
        (self.names.len() as u64).wrapping_add(self.items.len() as u64)
    }

    fn poke(&mut self, seed: u64) {
        self.names.insert(seed % 50, word(seed, 6));
        self.totals.insert(seed % 50, seed);
        self.tags.insert(format!("tag{}", seed % 9));
        self.events.push(seed);
        self.people.insert(person(seed % 40, seed));
        let _ = self.items.try_insert(item((seed % 70) as u32, seed));
        self.nested.poke(seed);
        if seed.is_multiple_of(5) {
            self.blobs.remove(&(seed % 12));
        }
    }
}

impl Gen for Catalog {
    fn generate(input: &mut Input<'_>) -> Self {
        let mut catalog = Catalog::default();
        for _ in 0..input.byte() % 6 {
            let id = u32::from(input.byte());
            let _ = catalog.items.try_insert(item(id, input.num()));
        }
        catalog.stamp = input.num();
        catalog
    }
}
