use ferro::codec::{Decoder, Encoder};
use ferro::map::Summary;
use ferro::{Bytes, Data, Map, Record, Table};

use crate::catalog::word;
use crate::guarded;
use crate::input::Input;

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Pending,
    Shipped,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
pub struct Order {
    #[key]
    pub id: u64,
    #[index]
    pub customer: String,
    #[index]
    pub status: Status,
    pub total_cents: u64,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
pub struct Shop {
    pub stock: Map<String, u32>,
    pub orders: Table<Order>,
    pub next_order: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Total(pub u64);

impl Data for Total {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Total(u64::decode(r)?))
    }
}

impl Summary<u64, u64> for Total {
    fn of(_key: &u64, value: &u64) -> Self {
        Total(*value)
    }

    fn combine(&self, other: &Self) -> Self {
        Total(self.0.wrapping_add(other.0))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Heft(pub u64);

impl Data for Heft {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Heft(u64::decode(r)?))
    }
}

impl Summary<u64, Bytes> for Heft {
    fn of(key: &u64, value: &Bytes) -> Self {
        Heft(key.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ value.len() as u64)
    }

    fn combine(&self, other: &Self) -> Self {
        Heft(self.0.wrapping_add(other.0))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inner(pub u64);

impl Data for Inner {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Inner(u64::decode(r)?))
    }
}

impl Summary<u64, Map<u64, u64>> for Inner {
    fn of(key: &u64, value: &Map<u64, u64>) -> Self {
        let inner = value
            .iter()
            .fold(*key, |sum, (_, item)| sum.wrapping_add(*item));
        Inner(inner.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    fn combine(&self, other: &Self) -> Self {
        Inner(self.0.wrapping_add(other.0))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Opaque;

impl Data for Opaque {
    fn encode(&self, _w: &mut Encoder<'_>) -> ferro::Result<()> {
        Ok(())
    }

    fn decode(_r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Opaque)
    }
}

impl Probe for Opaque {}

pub type Root = Map<u64, String>;
pub type Counters = Map<u8, i64>;
pub type Docs = Map<String, Bytes>;
pub type Nested = Map<String, Map<u64, String>>;
pub type Summed = Map<u64, u64, Total>;

pub trait Probe: Data {
    fn probe(&self, _budget: &mut usize) {}

    fn measure(&self) -> u64 {
        0
    }

    fn poke(&mut self, _seed: u64) {}
}

impl Probe for u8 {}
impl Probe for u32 {}
impl Probe for u64 {}
impl Probe for i64 {}
impl Probe for String {}
impl Probe for Bytes {}
impl Probe for Order {}

impl<K, V, S> Probe for Map<K, V, S>
where
    K: Data + Ord + Clone,
    V: Probe + Clone,
    S: Summary<K, V>,
{
    fn poke(&mut self, seed: u64) {
        let (Some((first, low)), Some((last, high))) = (self.first(), self.last()) else {
            return;
        };
        let (low, high) = ((*low).clone(), (*high).clone());
        match seed % 4 {
            0 => {
                self.remove(&first);
            }
            1 => {
                self.insert(first, high);
            }
            2 => {
                self.insert(last, low);
            }
            _ => {
                if let Some(value) = self.get_mut(&first) {
                    value.poke(seed / 4);
                }
            }
        }
    }

    fn probe(&self, budget: &mut usize) {
        let _ = self.len();
        guarded(|| self.summary());
        let mut first = None;
        guarded(|| {
            for (key, value) in self.iter() {
                if *budget == 0 {
                    break;
                }
                *budget -= 1;
                value.probe(budget);
                if first.is_none() {
                    first = Some(key);
                }
            }
        });
        if let Some(key) = &first {
            guarded(|| self.try_get(key));
            guarded(|| self.range(key..).next_back());
            guarded(|| self.floor(key));
            guarded(|| self.range_summary(key..));
            guarded(|| {
                let mut cursor = self.cursor();
                cursor.seek(key);
                cursor.advance();
                cursor.seek_back(key);
                cursor.value()
            });
        }
        guarded(|| self.first());
        guarded(|| self.last());
        guarded(|| self.nth(self.len() / 2));
        guarded(|| self.iter().rev().take(3).count());
    }

    fn measure(&self) -> u64 {
        self.len() as u64
    }
}

impl Probe for Shop {
    fn probe(&self, budget: &mut usize) {
        self.stock.probe(budget);
        self.orders.rows().probe(budget);
        let _ = self.orders.len();
        guarded(|| self.orders.by_status(&Status::Pending).take(8).count());
        guarded(|| self.orders.by_customer("wile").take(8).count());
        guarded(|| self.orders.first());
        guarded(|| self.orders.get(&1));
    }

    fn measure(&self) -> u64 {
        (self.stock.len() as u64)
            .wrapping_add(self.orders.len() as u64)
            .wrapping_add(self.next_order)
    }

    fn poke(&mut self, seed: u64) {
        self.next_order = self.next_order.wrapping_add(1);
        self.stock.poke(seed);
        match seed % 3 {
            0 => {
                self.orders.insert(Order {
                    id: self.next_order,
                    customer: word(seed, 4),
                    status: Status::Pending,
                    total_cents: seed,
                });
            }
            1 => {
                if let Some((id, _)) = self.orders.rows().first() {
                    self.orders
                        .update(&id, |order| order.status = Status::Shipped);
                }
            }
            _ => {
                if let Some((id, _)) = self.orders.rows().last() {
                    self.orders.remove(&id);
                }
            }
        }
    }
}

pub trait Gen: Sized {
    fn generate(input: &mut Input<'_>) -> Self;
}

impl Gen for () {
    fn generate(_input: &mut Input<'_>) -> Self {}
}

impl Gen for u8 {
    fn generate(input: &mut Input<'_>) -> Self {
        input.byte()
    }
}

impl Gen for u32 {
    fn generate(input: &mut Input<'_>) -> Self {
        input.num() as u32
    }
}

impl Gen for u64 {
    fn generate(input: &mut Input<'_>) -> Self {
        input.num()
    }
}

impl Gen for i64 {
    fn generate(input: &mut Input<'_>) -> Self {
        input.num() as i64
    }
}

impl Gen for String {
    fn generate(input: &mut Input<'_>) -> Self {
        input.text()
    }
}

impl Gen for Total {
    fn generate(input: &mut Input<'_>) -> Self {
        Total(input.num())
    }
}

impl Gen for Bytes {
    fn generate(input: &mut Input<'_>) -> Self {
        let len = match input.byte() % 4 {
            0 => 600 + usize::from(input.byte()) * 8,
            _ => usize::from(input.byte()),
        };
        let fill = input.byte();
        Bytes::from(vec![fill; len])
    }
}

impl<K: Gen + Data + Ord, V: Gen + Data> Gen for Map<K, V> {
    fn generate(input: &mut Input<'_>) -> Self {
        let mut map = Map::new();
        for _ in 0..input.byte() % 4 {
            map.insert(K::generate(input), V::generate(input));
        }
        map
    }
}

impl Gen for Shop {
    fn generate(input: &mut Input<'_>) -> Self {
        let mut shop = Shop {
            stock: Map::generate(input),
            ..Shop::default()
        };
        for _ in 0..input.byte() % 4 {
            shop.next_order += 1;
            shop.orders.insert(Order {
                id: input.num(),
                customer: input.text(),
                status: if input.byte().is_multiple_of(2) {
                    Status::Pending
                } else {
                    Status::Shipped
                },
                total_cents: input.num(),
            });
        }
        shop
    }
}
