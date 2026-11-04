use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::ops::Bound;
use std::sync::atomic::Ordering;

use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use ferro::{Snapshot, Val};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::{ApiError, Shared, blocking};
use crate::events;
use crate::model::{
    Cart, Customer, Line, Order, OrderStatus, Product, Shop, cart_view, now_ms, orders_tag,
    products_tag, row_hash,
};

pub struct Snap {
    pub snapshot: Snapshot<Shop>,
    pub pinned: bool,
}

impl FromRequestParts<Shared> for Snap {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, Infallible> {
        Ok(match parts.extensions.get::<Snapshot<Shop>>() {
            Some(snapshot) => Snap {
                snapshot: snapshot.clone(),
                pinned: true,
            },
            None => Snap {
                snapshot: app.read_snapshot(),
                pinned: false,
            },
        })
    }
}

struct Reply {
    tag: String,
    version: u64,
    pinned: bool,
    body: Option<Value>,
}

impl Reply {
    fn new(snap: &Snap, tag: String, inm: &Option<String>, body: impl FnOnce() -> Value) -> Reply {
        let fresh = !matches_tag(inm, &tag);
        Reply {
            version: snap.snapshot.version(),
            pinned: snap.pinned,
            body: fresh.then(body),
            tag,
        }
    }

    fn finish(self, app: &Shared) -> Response {
        let mut response = match self.body {
            Some(body) => Json(body).into_response(),
            None => {
                app.counters.not_modified.fetch_add(1, Ordering::Relaxed);
                StatusCode::NOT_MODIFIED.into_response()
            }
        };
        let headers = response.headers_mut();
        if let Ok(value) = HeaderValue::from_str(&format!("W/\"{}\"", self.tag)) {
            headers.insert(ETAG, value);
        }
        headers.insert("x-version", HeaderValue::from(self.version));
        let cache = if self.pinned {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
        response
    }
}

fn matches_tag(inm: &Option<String>, tag: &str) -> bool {
    let Some(inm) = inm else {
        return false;
    };
    inm.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.trim_start_matches("W/").trim_matches('"') == tag
    })
}

fn if_none_match(headers: &HeaderMap) -> Option<String> {
    headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

type Params = Path<HashMap<String, String>>;

fn id_of(params: &HashMap<String, String>) -> Result<u64, ApiError> {
    params
        .get("id")
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| ApiError::Bad("bad id".into()))
}

fn product_view(product: &Product, stock: u32) -> Value {
    json!({
        "id": product.id,
        "name": product.name,
        "category": product.category,
        "price_cents": product.price_cents,
        "active": product.active,
        "stock": stock,
    })
}

fn stock_of(shop: &Shop, id: u64) -> u32 {
    shop.stock.get(&id).map(|stock| *stock).unwrap_or(0)
}

#[derive(Deserialize, Default)]
struct ListQuery {
    category: Option<String>,
    min_price: Option<u64>,
    max_price: Option<u64>,
    q: Option<String>,
    in_stock: Option<bool>,
    active: Option<bool>,
    after: Option<u64>,
    limit: Option<usize>,
}

async fn list_products(
    State(app): State<Shared>,
    snap: Snap,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let inm = if_none_match(&headers);
    let reply = app
        .bridge
        .read(move || {
            let shop = &*snap.snapshot;
            let tag = products_tag(shop);
            Reply::new(&snap, tag, &inm, || products_page(shop, &query))
        })
        .await?;
    Ok(reply.finish(&app))
}

fn products_page(shop: &Shop, query: &ListQuery) -> Value {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let after = query.after.unwrap_or(0);
    let candidates: Box<dyn Iterator<Item = Val<Product>>> = match &query.category {
        Some(category) => Box::new(shop.products.by_category_after(category.as_str(), &after)),
        None => Box::new(
            shop.products
                .range((Bound::Excluded(after), Bound::Unbounded))
                .map(|(_, product)| product),
        ),
    };
    let needle = query.q.as_ref().map(|text| text.to_lowercase());
    let mut items = Vec::new();
    let mut more = false;
    for product in candidates {
        if query.min_price.is_some_and(|min| product.price_cents < min)
            || query.max_price.is_some_and(|max| product.price_cents > max)
            || query.active.is_some_and(|active| product.active != active)
            || needle
                .as_ref()
                .is_some_and(|needle| !product.name.to_lowercase().contains(needle))
        {
            continue;
        }
        let stock = stock_of(shop, product.id);
        if query.in_stock.is_some_and(|wanted| (stock > 0) != wanted) {
            continue;
        }
        if items.len() == limit {
            more = true;
            break;
        }
        items.push(product_view(&product, stock));
    }
    let next = if more {
        items.last().and_then(|item| item["id"].as_u64())
    } else {
        None
    };
    json!({"items": items, "next_cursor": next})
}

async fn get_product(
    State(app): State<Shared>,
    snap: Snap,
    Path(params): Params,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = id_of(&params)?;
    let inm = if_none_match(&headers);
    let reply = app
        .bridge
        .read(move || -> Result<Reply, ApiError> {
            let shop = &*snap.snapshot;
            let product = shop
                .products
                .get(&id)
                .ok_or_else(|| ApiError::NotFound(format!("product {id}")))?;
            let stock = stock_of(shop, id);
            let tag = format!("{}-{stock}", row_hash(&*product));
            Ok(Reply::new(&snap, tag, &inm, || {
                product_view(&product, stock)
            }))
        })
        .await??;
    Ok(reply.finish(&app))
}

async fn get_order(
    State(app): State<Shared>,
    snap: Snap,
    Path(params): Params,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = id_of(&params)?;
    let inm = if_none_match(&headers);
    let reply = app
        .bridge
        .read(move || -> Result<Reply, ApiError> {
            let order = snap
                .snapshot
                .orders
                .get(&id)
                .ok_or_else(|| ApiError::NotFound(format!("order {id}")))?;
            Ok(Reply::new(&snap, row_hash(&*order), &inm, || {
                serde_json::to_value(&*order).unwrap_or(Value::Null)
            }))
        })
        .await??;
    Ok(reply.finish(&app))
}

#[derive(Deserialize, Default)]
struct OrdersQuery {
    before: Option<u64>,
    limit: Option<usize>,
    status: Option<OrderStatusParam>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum OrderStatusParam {
    Placed,
    Shipped,
    Cancelled,
}

impl From<OrderStatusParam> for OrderStatus {
    fn from(param: OrderStatusParam) -> Self {
        match param {
            OrderStatusParam::Placed => OrderStatus::Placed,
            OrderStatusParam::Shipped => OrderStatus::Shipped,
            OrderStatusParam::Cancelled => OrderStatus::Cancelled,
        }
    }
}

async fn customer_orders(
    State(app): State<Shared>,
    snap: Snap,
    Path(params): Params,
    Query(query): Query<OrdersQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = id_of(&params)?;
    let inm = if_none_match(&headers);
    let reply = app
        .bridge
        .read(move || -> Result<Reply, ApiError> {
            let shop = &*snap.snapshot;
            if !shop.customers.contains(&id) {
                return Err(ApiError::NotFound(format!("customer {id}")));
            }
            Ok(Reply::new(&snap, orders_tag(shop), &inm, || {
                let limit = query.limit.unwrap_or(50).clamp(1, 200);
                let before = query.before.unwrap_or(u64::MAX);
                let status: Option<OrderStatus> = query.status.map(Into::into);
                let rows = match status {
                    Some(status) => shop.orders.by_customer_status(&(id, status)),
                    None => shop.orders.by_customer(&id),
                };
                let mut orders: Vec<Val<Order>> = rows
                    .rev()
                    .filter(|order| order.id < before)
                    .take(limit + 1)
                    .collect();
                let next = if orders.len() > limit {
                    orders.truncate(limit);
                    orders.last().map(|order| order.id)
                } else {
                    None
                };
                json!({"items": orders.iter().map(|order| &**order).collect::<Vec<_>>(), "next_cursor": next})
            }))
        })
        .await??;
    Ok(reply.finish(&app))
}

async fn get_customer(
    State(app): State<Shared>,
    snap: Snap,
    Path(params): Params,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = id_of(&params)?;
    let inm = if_none_match(&headers);
    let reply = app
        .bridge
        .read(move || -> Result<Reply, ApiError> {
            let customer = snap
                .snapshot
                .customers
                .get(&id)
                .ok_or_else(|| ApiError::NotFound(format!("customer {id}")))?;
            Ok(Reply::new(&snap, row_hash(&*customer), &inm, || {
                serde_json::to_value(&*customer).unwrap_or(Value::Null)
            }))
        })
        .await??;
    Ok(reply.finish(&app))
}

async fn get_cart(
    State(app): State<Shared>,
    snap: Snap,
    Path(params): Params,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = id_of(&params)?;
    let inm = if_none_match(&headers);
    let reply = app
        .bridge
        .read(move || -> Result<Reply, ApiError> {
            let cart = snap
                .snapshot
                .carts
                .get(&id)
                .ok_or_else(|| ApiError::NotFound(format!("cart {id}")))?;
            Ok(Reply::new(&snap, row_hash(&*cart), &inm, || {
                cart_view(&cart)
            }))
        })
        .await??;
    Ok(reply.finish(&app))
}

async fn order_history(
    State(app): State<Shared>,
    snap: Snap,
    Path(params): Params,
) -> Result<Response, ApiError> {
    let id = id_of(&params)?;
    let db = app.db.clone();
    let horizon = snap.snapshot.version();
    if !snap.snapshot.orders.contains(&id) {
        return Err(ApiError::NotFound(format!("order {id}")));
    }
    let mut versions: Vec<Value> = blocking(move || {
        db.history_of_range(..=horizon, |shop| shop.orders.get(&id))
            .take_while(|(_, order)| order.is_some())
            .map(|(commit, order)| {
                json!({
                    "version": commit.version,
                    "time_ms": commit.time_ms,
                    "message": commit.message,
                    "order": order.as_deref(),
                })
            })
            .collect()
    })
    .await?;
    versions.reverse();
    Ok(Json(json!({"id": id, "history": versions})).into_response())
}

async fn health(State(app): State<Shared>, snap: Snap) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": snap.snapshot.version(),
        "replica": app.replica,
        "bridge": format!("reads={:?} writes={:?}", app.bridge.reads, app.bridge.writes),
    }))
}

async fn stats(State(app): State<Shared>, snap: Snap) -> Json<Value> {
    let shop = &*snap.snapshot;
    let counters = &app.counters;
    let space = app.db.space().unwrap_or_default();
    Json(json!({
        "version": snap.snapshot.version(),
        "products": shop.products.len(),
        "customers": shop.customers.len(),
        "carts": shop.carts.len(),
        "orders": shop.orders.len(),
        "next_order": shop.next_order,
        "orders_ok": counters.orders_ok.load(Ordering::Relaxed),
        "orders_conflict": counters.orders_conflict.load(Ordering::Relaxed),
        "writes": counters.writes.load(Ordering::Relaxed),
        "not_modified": counters.not_modified.load(Ordering::Relaxed),
        "events_sent": counters.events_sent.load(Ordering::Relaxed),
        "events_lagged": counters.events_lagged.load(Ordering::Relaxed),
        "feed_subscribers": app.feed.receiver_count(),
        "db_bytes": app.db.size_on_disk(),
        "db_live_bytes": space.live_bytes,
        "db_retired_bytes": space.retired_bytes,
        "reclaim_cycles": space.cycles,
        "reclaim_longest_pause_ms": space.longest_pause.as_secs_f64() * 1000.0,
    }))
}

async fn invariants(State(app): State<Shared>, snap: Snap) -> Result<Json<Value>, ApiError> {
    let report = app
        .bridge
        .read(move || {
            let shop = &*snap.snapshot;
            let mut consumed: BTreeMap<u64, u64> = BTreeMap::new();
            let mut placed = 0u64;
            for order in shop.orders.iter() {
                if order.status == OrderStatus::Cancelled {
                    continue;
                }
                placed += 1;
                for line in &order.lines {
                    *consumed.entry(line.product_id).or_default() += u64::from(line.qty);
                }
            }
            let total_stock: u64 = shop.stock.values().map(|stock| u64::from(*stock)).sum();
            let consumed_total: u64 = consumed.values().sum();
            json!({
                "version": snap.snapshot.version(),
                "live_orders": placed,
                "consumed_units": consumed_total,
                "stock_units": total_stock,
                "stock_total_len": shop.stock.len(),
            })
        })
        .await?;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct NewProduct {
    name: String,
    category: String,
    price_cents: u64,
    #[serde(default)]
    stock: u32,
}

async fn create_product(
    State(app): State<Shared>,
    Json(new): Json<NewProduct>,
) -> Result<Response, ApiError> {
    app.writable()?;
    if new.name.trim().is_empty() || new.category.trim().is_empty() {
        return Err(ApiError::Bad("name and category are required".into()));
    }
    let done = app
        .commit(move |shop| -> Result<_, ApiError> {
            shop.next_product += 1;
            let id = shop.next_product;
            shop.products.insert(Product {
                id,
                name: new.name,
                category: new.category,
                price_cents: new.price_cents,
                active: true,
            });
            shop.stock.insert(id, new.stock);
            Ok((format!("add product {id}"), id))
        })
        .await?;
    app.counters.writes.fetch_add(1, Ordering::Relaxed);
    let view = json!({"id": done.value, "version": done.version});
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

#[derive(Deserialize, Default)]
struct PatchProduct {
    name: Option<String>,
    category: Option<String>,
    price_cents: Option<u64>,
    active: Option<bool>,
    stock: Option<u32>,
    stock_delta: Option<i64>,
}

async fn patch_product(
    State(app): State<Shared>,
    Path(params): Params,
    Json(patch): Json<PatchProduct>,
) -> Result<Response, ApiError> {
    app.writable()?;
    let id = id_of(&params)?;
    let done = app
        .commit(move |shop| -> Result<_, ApiError> {
            if !shop.products.contains(&id) {
                return Err(ApiError::NotFound(format!("product {id}")));
            }
            if let Some(stock) = patch.stock {
                shop.stock.insert(id, stock);
            }
            if let Some(delta) = patch.stock_delta {
                let current = i64::from(stock_of(shop, id));
                let next = current + delta;
                if !(0..=i64::from(u32::MAX)).contains(&next) {
                    return Err(ApiError::Conflict(
                        json!({"error": "stock would leave range", "stock": current}),
                    ));
                }
                shop.stock.insert(id, next as u32);
            }
            shop.products.update(&id, |product| {
                if let Some(name) = patch.name {
                    product.name = name;
                }
                if let Some(category) = patch.category {
                    product.category = category;
                }
                if let Some(price) = patch.price_cents {
                    product.price_cents = price;
                }
                if let Some(active) = patch.active {
                    product.active = active;
                }
            });
            let view = product_view(&shop.products.get(&id).expect("exists"), stock_of(shop, id));
            Ok((format!("patch product {id}"), view))
        })
        .await?;
    app.counters.writes.fetch_add(1, Ordering::Relaxed);
    Ok(Json(json!({"product": done.value, "version": done.version})).into_response())
}

#[derive(Deserialize)]
struct NewCustomer {
    name: String,
    email: String,
}

async fn create_customer(
    State(app): State<Shared>,
    Json(new): Json<NewCustomer>,
) -> Result<Response, ApiError> {
    app.writable()?;
    let done = app
        .commit(move |shop| -> Result<_, ApiError> {
            if shop.customers.by_email(new.email.as_str()).next().is_some() {
                return Err(ApiError::Conflict(json!({"error": "email taken"})));
            }
            shop.next_customer += 1;
            let id = shop.next_customer;
            shop.customers.insert(Customer {
                id,
                name: new.name,
                email: new.email,
            });
            Ok((format!("add customer {id}"), id))
        })
        .await?;
    app.counters.writes.fetch_add(1, Ordering::Relaxed);
    Ok((
        StatusCode::CREATED,
        Json(json!({"id": done.value, "version": done.version})),
    )
        .into_response())
}

#[derive(Deserialize)]
struct AddItem {
    product_id: u64,
    qty: u32,
}

async fn add_cart_item(
    State(app): State<Shared>,
    Path(params): Params,
    Json(item): Json<AddItem>,
) -> Result<Response, ApiError> {
    app.writable()?;
    let id = id_of(&params)?;
    if item.qty == 0 || item.qty > 1000 {
        return Err(ApiError::Bad("qty must be 1..=1000".into()));
    }
    let done = app
        .commit(move |shop| -> Result<_, ApiError> {
            if !shop.products.contains(&item.product_id) {
                return Err(ApiError::NotFound(format!("product {}", item.product_id)));
            }
            let cart = shop.carts.get_or_insert_with(id, Cart::default);
            let line = cart.items.get_or_insert_with(item.product_id, || 0);
            *line = (*line + item.qty).min(1000);
            cart.updated_ms = now_ms();
            Ok((format!("cart {id} add"), cart_view(cart)))
        })
        .await?;
    app.counters.writes.fetch_add(1, Ordering::Relaxed);
    Ok((
        StatusCode::CREATED,
        Json(json!({"cart": done.value, "version": done.version})),
    )
        .into_response())
}

#[derive(Deserialize)]
struct NewOrder {
    customer_id: u64,
    items: Option<Vec<AddItem>>,
    cart_id: Option<u64>,
}

fn place_order(shop: &mut Shop, request: NewOrder) -> Result<(String, Order), ApiError> {
    if !shop.customers.contains(&request.customer_id) {
        return Err(ApiError::NotFound(format!(
            "customer {}",
            request.customer_id
        )));
    }
    let mut wanted: BTreeMap<u64, u32> = BTreeMap::new();
    match (&request.items, request.cart_id) {
        (Some(items), _) => {
            for item in items {
                if item.qty == 0 {
                    return Err(ApiError::Bad("qty must be positive".into()));
                }
                let slot = wanted.entry(item.product_id).or_default();
                *slot = slot.saturating_add(item.qty);
            }
        }
        (None, Some(cart_id)) => {
            let cart = shop
                .carts
                .get(&cart_id)
                .ok_or_else(|| ApiError::NotFound(format!("cart {cart_id}")))?;
            wanted.extend(cart.items.iter().map(|(product, qty)| (product, *qty)));
        }
        (None, None) => return Err(ApiError::Bad("items or cart_id required".into())),
    }
    if wanted.is_empty() || wanted.len() > 50 {
        return Err(ApiError::Bad("order needs 1..=50 distinct products".into()));
    }
    let mut lines = Vec::with_capacity(wanted.len());
    let mut short = Vec::new();
    let mut total = 0u64;
    for (&product_id, &qty) in &wanted {
        let product = shop
            .products
            .get(&product_id)
            .ok_or_else(|| ApiError::NotFound(format!("product {product_id}")))?;
        if !product.active {
            return Err(ApiError::Bad(format!(
                "product {product_id} is not for sale"
            )));
        }
        let available = stock_of(shop, product_id);
        if available < qty {
            short.push(json!({"product_id": product_id, "requested": qty, "available": available}));
        } else {
            total += product.price_cents * u64::from(qty);
            lines.push(Line {
                product_id,
                qty,
                price_cents: product.price_cents,
            });
        }
    }
    if !short.is_empty() {
        return Err(ApiError::Conflict(
            json!({"error": "insufficient stock", "short": short}),
        ));
    }
    for line in &lines {
        if let Some(stock) = shop.stock.get_mut(&line.product_id) {
            *stock -= line.qty;
        }
    }
    if request.items.is_none()
        && let Some(cart_id) = request.cart_id
    {
        shop.carts.remove(&cart_id);
    }
    shop.next_order += 1;
    let order = Order {
        id: shop.next_order,
        customer: request.customer_id,
        status: OrderStatus::Placed,
        lines,
        total_cents: total,
        created_ms: now_ms(),
    };
    shop.orders.insert(order.clone());
    Ok((format!("order {}", order.id), order))
}

async fn create_order(
    State(app): State<Shared>,
    Json(request): Json<NewOrder>,
) -> Result<Response, ApiError> {
    app.writable()?;
    let outcome = app.commit(move |shop| place_order(shop, request)).await;
    match outcome {
        Ok(done) => {
            app.counters.orders_ok.fetch_add(1, Ordering::Relaxed);
            app.counters.writes.fetch_add(1, Ordering::Relaxed);
            let mut body = serde_json::to_value(&done.value).unwrap_or(Value::Null);
            body["version"] = json!(done.version);
            Ok((StatusCode::CREATED, Json(body)).into_response())
        }
        Err(error) => {
            if matches!(error, ApiError::Conflict(_)) {
                app.counters.orders_conflict.fetch_add(1, Ordering::Relaxed);
            }
            Err(error)
        }
    }
}

#[derive(Deserialize)]
struct PatchOrder {
    status: OrderStatusParam,
}

async fn patch_order(
    State(app): State<Shared>,
    Path(params): Path<HashMap<String, String>>,
    Json(patch): Json<PatchOrder>,
) -> Result<Response, ApiError> {
    app.writable()?;
    let id = id_of(&params)?;
    let target: OrderStatus = patch.status.into();
    let done = app
        .commit(move |shop| -> Result<_, ApiError> {
            let order = shop
                .orders
                .get(&id)
                .ok_or_else(|| ApiError::NotFound(format!("order {id}")))?;
            if order.status != OrderStatus::Placed || target == OrderStatus::Placed {
                return Err(ApiError::Conflict(
                    json!({"error": "invalid transition", "status": order.status}),
                ));
            }
            if target == OrderStatus::Cancelled {
                for line in &order.lines {
                    let stock = shop.stock.get_or_insert_with(line.product_id, || 0);
                    *stock += line.qty;
                }
            }
            shop.orders.update(&id, |order| order.status = target);
            Ok((format!("order {id} {target:?}"), id))
        })
        .await?;
    app.counters.writes.fetch_add(1, Ordering::Relaxed);
    Ok(Json(json!({"id": done.value, "version": done.version})).into_response())
}

#[derive(Deserialize)]
struct Seed {
    products: u64,
    customers: u64,
    stock: u32,
    #[serde(default)]
    scarce_every: u64,
    #[serde(default)]
    scarce_stock: u32,
}

async fn seed(State(app): State<Shared>, Json(seed): Json<Seed>) -> Result<Response, ApiError> {
    app.writable()?;
    let done = app
        .commit(move |shop| -> Result<_, ApiError> {
            const CATEGORIES: [&str; 8] = [
                "tools", "garden", "kitchen", "toys", "books", "audio", "outdoor", "office",
            ];
            let mut scarce = Vec::new();
            for _ in 0..seed.products {
                shop.next_product += 1;
                let id = shop.next_product;
                shop.products.insert(Product {
                    id,
                    name: format!("Product {id}"),
                    category: CATEGORIES[(id % 8) as usize].to_string(),
                    price_cents: 199 + (id * 37) % 9800,
                    active: true,
                });
                let low = seed.scarce_every > 0 && id % seed.scarce_every == 0;
                if low {
                    scarce.push(id);
                }
                shop.stock
                    .insert(id, if low { seed.scarce_stock } else { seed.stock });
            }
            let mut first_customer = 0;
            for _ in 0..seed.customers {
                shop.next_customer += 1;
                let id = shop.next_customer;
                first_customer = if first_customer == 0 {
                    id
                } else {
                    first_customer
                };
                shop.customers.insert(Customer {
                    id,
                    name: format!("Customer {id}"),
                    email: format!("customer{id}@example.test"),
                });
            }
            Ok((
                "seed".to_string(),
                json!({"products": seed.products, "customers": seed.customers, "scarce": scarce}),
            ))
        })
        .await?;
    Ok(Json(json!({"seeded": done.value, "version": done.version})).into_response())
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<u64>,
}

async fn events_stream(
    State(app): State<Shared>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let last_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    events::open(app, query.since.or(last_id)).await
}

async fn pin_version(
    State(app): State<Shared>,
    Path(params): Path<HashMap<String, String>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(version) = params
        .get("version")
        .and_then(|text| text.parse::<u64>().ok())
    else {
        return ApiError::Bad("bad version".into()).into_response();
    };
    let db = app.db.clone();
    match app.bridge.read(move || db.at(version)).await {
        Ok(Ok(snapshot)) => {
            request.extensions_mut().insert(snapshot);
            next.run(request).await
        }
        Ok(Err(error)) => ApiError::from(error).into_response(),
        Err(error) => error.into_response(),
    }
}

fn read_routes() -> Router<Shared> {
    Router::new()
        .route("/products", get(list_products))
        .route("/products/{id}", get(get_product))
        .route("/orders/{id}", get(get_order))
        .route("/orders/{id}/history", get(order_history))
        .route("/customers/{id}", get(get_customer))
        .route("/customers/{id}/orders", get(customer_orders))
        .route("/carts/{id}", get(get_cart))
        .route("/health", get(health))
        .route("/admin/stats", get(stats))
        .route("/admin/invariants", get(invariants))
}

pub fn router(app: Shared) -> Router {
    let pinned = read_routes().layer(middleware::from_fn_with_state(app.clone(), pin_version));
    Router::new()
        .merge(read_routes())
        .route("/products", post(create_product))
        .route("/products/{id}", patch(patch_product))
        .route("/customers", post(create_customer))
        .route("/carts/{id}/items", post(add_cart_item))
        .route("/orders", post(create_order))
        .route("/orders/{id}", patch(patch_order))
        .route("/admin/seed", post(seed))
        .route("/events", get(events_stream))
        .nest("/admin/snapshot/{version}", pinned)
        .with_state(app)
}
