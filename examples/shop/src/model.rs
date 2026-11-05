use std::time::{SystemTime, UNIX_EPOCH};

use ferro::{Data, Hash, Map, Record, Table, hash_of};
use serde::Serialize;

/// A product for sale, indexed by category, with its price and whether it is still sold.
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

/// A customer, indexed by email.
#[derive(Clone, Debug, PartialEq, Serialize, Data, Record)]
pub struct Customer {
    #[key]
    pub id: u64,
    pub name: String,
    #[index]
    pub email: String,
}

/// Where an [`Order`] is: placed, shipped or cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Data)]
#[serde(rename_all = "lowercase")]
pub enum OrderStatus {
    Placed,
    Shipped,
    Cancelled,
}

/// One line of an order: a product, a quantity, and the price it was bought at.
#[derive(Clone, Debug, PartialEq, Serialize, Data)]
pub struct Line {
    pub product_id: u64,
    pub qty: u32,
    pub price_cents: u64,
}

/// An order, indexed by customer, by status, and by customer and status together.
#[derive(Clone, Debug, PartialEq, Serialize, Data, Record)]
#[index(customer, status)]
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

/// A customer's cart: a quantity for each product id, and when it last changed.
#[derive(Clone, Debug, Default, PartialEq, Data)]
pub struct Cart {
    pub items: Map<u64, u32>,
    pub updated_ms: u64,
}

/// The database root: products and their stock, customers and their carts, orders, and the next id
/// of each.
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

/// The current time in Unix milliseconds, or 0 if the clock is before 1970.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Lowercase hex for a hash.
pub fn hex(hash: &[u8]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The hex content hash of one row, used as its `ETag`.
pub fn row_hash<D: Data>(row: &D) -> String {
    hex(&hash_of(row))
}

/// The hex of a collection's root hash, or an empty string for an empty collection.
pub fn root_tag(hash: Option<Hash>) -> String {
    hash.map(|hash| hex(&hash)).unwrap_or_default()
}

/// The `ETag` of the product listing, which changes when a product or its stock does.
pub fn products_tag(shop: &Shop) -> String {
    format!(
        "{}-{}",
        root_tag(shop.products.root_hash()),
        root_tag(shop.stock.root_hash())
    )
}

/// A tag that changes whenever any order does.
pub fn orders_tag(shop: &Shop) -> String {
    root_tag(shop.orders.root_hash())
}

/// The JSON for a cart.
pub fn cart_view(cart: &Cart) -> serde_json::Value {
    let items: Vec<_> = cart
        .items
        .iter()
        .map(|(product_id, qty)| serde_json::json!({"product_id": product_id, "qty": *qty}))
        .collect();
    serde_json::json!({"items": items, "updated_ms": cart.updated_ms})
}
