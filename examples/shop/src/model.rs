use std::time::{SystemTime, UNIX_EPOCH};

use ferro::codec::{Encoder, HashSink};
use ferro::{Data, Map, Record, Table, TableData};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize, Data, Record)]
pub struct Product {
    #[key]
    pub id: u64,
    pub name: String,
    #[index]
    pub category: String,
    pub price_cents: u64,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Data, Record)]
pub struct Customer {
    #[key]
    pub id: u64,
    pub name: String,
    #[index]
    pub email: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Data)]
#[serde(rename_all = "lowercase")]
pub enum OrderStatus {
    Placed,
    Shipped,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Data)]
pub struct Line {
    pub product_id: u64,
    pub qty: u32,
    pub price_cents: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Data, Record)]
pub struct Order {
    #[key]
    pub id: u64,
    #[index]
    pub customer: u64,
    #[index]
    pub status: OrderStatus,
    pub lines: Vec<Line>,
    pub total_cents: u64,
    pub created_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
pub struct Cart {
    pub items: Map<u64, u32>,
    pub updated_ms: u64,
}

#[derive(Clone, Default, Data)]
pub struct Shop {
    pub products: Table<Product>,
    pub stock: Map<u64, u32>,
    pub customers: Table<Customer>,
    pub carts: Map<u64, Cart>,
    pub orders: Table<Order>,
    pub next_product: u64,
    pub next_customer: u64,
    pub next_order: u64,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

pub fn hex(hash: &[u8]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn row_hash<D: Data>(row: &D) -> String {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    row.encode(&mut encoder).expect("hash encode");
    hex(&encoder.finish().expect("hash finish").hash)
}

pub fn root_tag<K: Data + Ord, V: Data>(map: &Map<K, V>) -> String {
    map.root_hash().map(|hash| hex(&hash)).unwrap_or_default()
}

pub fn products_tag(shop: &Shop) -> String {
    format!(
        "{}-{}",
        root_tag(shop.products.rows()),
        root_tag(&shop.stock)
    )
}

pub fn orders_tag(shop: &Shop) -> String {
    root_tag(shop.orders.rows())
}

pub fn cart_view(cart: &Cart) -> serde_json::Value {
    let items: Vec<_> = cart
        .items
        .iter()
        .map(|(product_id, qty)| serde_json::json!({"product_id": product_id, "qty": *qty}))
        .collect();
    serde_json::json!({"items": items, "updated_ms": cart.updated_ms})
}
