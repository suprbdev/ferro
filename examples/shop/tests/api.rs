use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use ferro::Durability;
use serde_json::{Value, json};
use shop::minihttp::{Client, SseStream};
use shop::{Config, ReadMode, WriteMode, build, open_db, serve};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct Server {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Server {
    async fn start(path: &Path, config: Config) -> Server {
        let db = open_db(path, &config).expect("open");
        let app = build(db, &config);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (stop, wait) = oneshot::channel::<()>();
        let task = tokio::spawn({
            let app = app.clone();
            async move {
                serve(listener, app, async {
                    let _ = wait.await;
                })
                .await
                .expect("serve");
            }
        });
        Server {
            addr,
            stop: Some(stop),
            task: Some(task),
        }
    }

    async fn client(&self) -> Client {
        Client::connect(self.addr).await.expect("connect")
    }

    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
    }
}

fn buffered() -> Config {
    Config {
        durability: Durability::Buffered,
        ..Config::default()
    }
}

async fn make_customer(client: &mut Client, name: &str) -> u64 {
    let response = client
        .json(
            "POST",
            "/customers",
            &json!({"name": name, "email": format!("{name}@example.test")}),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 201, "{}", String::from_utf8_lossy(&response.body));
    response.json()["id"].as_u64().unwrap()
}

async fn make_product(client: &mut Client, name: &str, category: &str, price: u64, stock: u32) -> u64 {
    let response = client
        .json(
            "POST",
            "/products",
            &json!({"name": name, "category": category, "price_cents": price, "stock": stock}),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 201);
    response.json()["id"].as_u64().unwrap()
}

async fn stock_of(client: &mut Client, id: u64) -> u64 {
    let response = client.get(&format!("/products/{id}")).await.unwrap();
    assert_eq!(response.status, 200);
    response.json()["stock"].as_u64().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_reports_version() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("shop.ferro"), buffered()).await;
    let mut client = server.client().await;
    let response = client.get("/health").await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["status"], "ok");
    assert_eq!(response.json()["version"], 0);
    make_product(&mut client, "a", "tools", 100, 1).await;
    let response = client.get("/health").await.unwrap();
    assert_eq!(response.json()["version"], 1);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn product_crud_filters_and_cursor_pagination() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("shop.ferro"), buffered()).await;
    let mut client = server.client().await;
    let response = client
        .json("POST", "/admin/seed", &json!({"products": 40, "customers": 2, "stock": 5, "scarce_every": 10, "scarce_stock": 0}))
        .await
        .unwrap();
    assert_eq!(response.status, 200);

    let mut seen = Vec::new();
    let mut cursor: Option<u64> = None;
    let mut pages = 0;
    loop {
        let path = match cursor {
            Some(after) => format!("/products?limit=7&after={after}"),
            None => "/products?limit=7".to_string(),
        };
        let page = client.get(&path).await.unwrap().json();
        pages += 1;
        for item in page["items"].as_array().unwrap() {
            seen.push(item["id"].as_u64().unwrap());
        }
        match page["next_cursor"].as_u64() {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 6);
    assert_eq!(seen, (1..=40).collect::<Vec<u64>>());

    let page = client.get("/products?category=tools&limit=100").await.unwrap().json();
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    assert!(items.iter().all(|item| item["category"] == "tools"));
    let ids: Vec<u64> = items.iter().map(|item| item["id"].as_u64().unwrap()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);

    let page = client
        .get(&format!("/products?category=tools&limit=2&after={}", ids[1]))
        .await
        .unwrap()
        .json();
    let after: Vec<u64> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_u64().unwrap())
        .collect();
    assert_eq!(after, vec![ids[2], ids[3]]);
    assert_eq!(page["next_cursor"], ids[3]);

    let page = client.get("/products?in_stock=false&limit=100").await.unwrap().json();
    assert_eq!(page["items"].as_array().unwrap().len(), 4);
    let page = client.get("/products?min_price=5000&max_price=6000&limit=100").await.unwrap().json();
    for item in page["items"].as_array().unwrap() {
        let price = item["price_cents"].as_u64().unwrap();
        assert!((5000..=6000).contains(&price));
    }
    let page = client.get("/products?q=product%2033").await.unwrap().json();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);

    let id = make_product(&mut client, "Lamp", "office", 2500, 3).await;
    assert_eq!(id, 41);
    let product = client.get("/products/41").await.unwrap();
    assert_eq!(product.json()["name"], "Lamp");
    assert_eq!(product.json()["stock"], 3);

    let patched = client
        .json("PATCH", "/products/41", &json!({"price_cents": 1999, "name": "Desk lamp", "stock_delta": -1, "active": false}))
        .await
        .unwrap();
    assert_eq!(patched.status, 200);
    let product = client.get("/products/41").await.unwrap().json();
    assert_eq!(product["price_cents"], 1999);
    assert_eq!(product["name"], "Desk lamp");
    assert_eq!(product["stock"], 2);
    assert_eq!(product["active"], false);

    let response = client
        .json("PATCH", "/products/41", &json!({"stock_delta": -5}))
        .await
        .unwrap();
    assert_eq!(response.status, 409);
    assert_eq!(stock_of(&mut client, 41).await, 2);

    assert_eq!(client.get("/products/9999").await.unwrap().status, 404);
    assert_eq!(client.json("PATCH", "/products/9999", &json!({"name": "x"})).await.unwrap().status, 404);
    assert_eq!(client.get("/products/abc").await.unwrap().status, 422);
    let bad = client.json("POST", "/products", &json!({"name": " ", "category": "x", "price_cents": 1})).await.unwrap();
    assert_eq!(bad.status, 422);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn carts_and_orders_from_items_and_cart() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("shop.ferro"), buffered()).await;
    let mut client = server.client().await;
    let customer = make_customer(&mut client, "wile").await;
    let anvil = make_product(&mut client, "Anvil", "tools", 5000, 10).await;
    let rope = make_product(&mut client, "Rope", "tools", 700, 4).await;

    let response = client
        .json("POST", &format!("/carts/{customer}/items"), &json!({"product_id": anvil, "qty": 2}))
        .await
        .unwrap();
    assert_eq!(response.status, 201);
    client
        .json("POST", &format!("/carts/{customer}/items"), &json!({"product_id": anvil, "qty": 1}))
        .await
        .unwrap();
    client
        .json("POST", &format!("/carts/{customer}/items"), &json!({"product_id": rope, "qty": 4}))
        .await
        .unwrap();
    let cart = client.get(&format!("/carts/{customer}")).await.unwrap().json();
    assert_eq!(cart["items"], json!([{"product_id": anvil, "qty": 3}, {"product_id": rope, "qty": 4}]));
    let missing = client
        .json("POST", &format!("/carts/{customer}/items"), &json!({"product_id": 999, "qty": 1}))
        .await
        .unwrap();
    assert_eq!(missing.status, 404);
    let zero = client
        .json("POST", &format!("/carts/{customer}/items"), &json!({"product_id": anvil, "qty": 0}))
        .await
        .unwrap();
    assert_eq!(zero.status, 422);

    let order = client
        .json("POST", "/orders", &json!({"customer_id": customer, "cart_id": customer}))
        .await
        .unwrap();
    assert_eq!(order.status, 201, "{}", String::from_utf8_lossy(&order.body));
    let order = order.json();
    assert_eq!(order["total_cents"], 3 * 5000 + 4 * 700);
    assert_eq!(order["status"], "placed");
    assert_eq!(stock_of(&mut client, anvil).await, 7);
    assert_eq!(stock_of(&mut client, rope).await, 0);
    assert_eq!(client.get(&format!("/carts/{customer}")).await.unwrap().status, 404);

    let id = order["id"].as_u64().unwrap();
    let fetched = client.get(&format!("/orders/{id}")).await.unwrap().json();
    assert_eq!(fetched["lines"].as_array().unwrap().len(), 2);

    let second = client
        .json("POST", "/orders", &json!({"customer_id": customer, "items": [{"product_id": anvil, "qty": 1}, {"product_id": anvil, "qty": 1}]}))
        .await
        .unwrap();
    assert_eq!(second.status, 201);
    assert_eq!(second.json()["lines"][0]["qty"], 2);

    let list = client.get(&format!("/customers/{customer}/orders")).await.unwrap().json();
    let ids: Vec<u64> = list["items"].as_array().unwrap().iter().map(|o| o["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, vec![2, 1]);
    let list = client.get(&format!("/customers/{customer}/orders?limit=1")).await.unwrap().json();
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["next_cursor"], 2);
    let list = client.get(&format!("/customers/{customer}/orders?before=2")).await.unwrap().json();
    assert_eq!(list["items"][0]["id"], 1);
    assert_eq!(client.get("/customers/77/orders").await.unwrap().status, 404);
    assert_eq!(client.get("/orders/77").await.unwrap().status, 404);

    let nobody = client
        .json("POST", "/orders", &json!({"customer_id": 77, "items": [{"product_id": anvil, "qty": 1}]}))
        .await
        .unwrap();
    assert_eq!(nobody.status, 404);
    let empty = client.json("POST", "/orders", &json!({"customer_id": customer, "items": []})).await.unwrap();
    assert_eq!(empty.status, 422);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn short_stock_refuses_whole_order_with_409() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("shop.ferro"), buffered()).await;
    let mut client = server.client().await;
    let customer = make_customer(&mut client, "road").await;
    let plenty = make_product(&mut client, "Plenty", "toys", 100, 100).await;
    let scarce = make_product(&mut client, "Scarce", "toys", 100, 2).await;
    let version = client.get("/health").await.unwrap().json()["version"].as_u64().unwrap();

    let response = client
        .json("POST", "/orders", &json!({"customer_id": customer, "items": [{"product_id": plenty, "qty": 10}, {"product_id": scarce, "qty": 3}]}))
        .await
        .unwrap();
    assert_eq!(response.status, 409);
    let body = response.json();
    assert_eq!(body["short"], json!([{"product_id": scarce, "requested": 3, "available": 2}]));
    assert_eq!(stock_of(&mut client, plenty).await, 100);
    assert_eq!(stock_of(&mut client, scarce).await, 2);
    assert_eq!(client.get("/health").await.unwrap().json()["version"], version);
    let stats = client.get("/admin/stats").await.unwrap().json();
    assert_eq!(stats["orders"], 0);
    assert_eq!(stats["orders_conflict"], 1);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_orders_for_last_unit_have_one_winner() {
    let dir = TempDir::new().unwrap();
    let config = Config {
        durability: Durability::Fsync,
        ..Config::default()
    };
    let server = Server::start(&dir.path().join("shop.ferro"), config).await;
    let mut client = server.client().await;
    let customer = make_customer(&mut client, "racer").await;
    let last = make_product(&mut client, "Last", "toys", 100, 1).await;
    let mut tasks = Vec::new();
    for _ in 0..48 {
        let addr = server.addr;
        tasks.push(tokio::spawn(async move {
            let mut client = Client::connect(addr).await.unwrap();
            client
                .json("POST", "/orders", &json!({"customer_id": customer, "items": [{"product_id": last, "qty": 1}]}))
                .await
                .unwrap()
                .status
        }));
    }
    let mut won = 0;
    let mut refused = 0;
    for task in tasks {
        match task.await.unwrap() {
            201 => won += 1,
            409 => refused += 1,
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!((won, refused), (1, 47));
    assert_eq!(stock_of(&mut client, last).await, 0);
    let stats = client.get("/admin/stats").await.unwrap().json();
    assert_eq!(stats["orders"], 1);
    assert_eq!(stats["orders_ok"], 1);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn many_orders_never_oversell_and_conserve_units() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("shop.ferro"), buffered()).await;
    let mut client = server.client().await;
    let customer = make_customer(&mut client, "mob").await;
    let a = make_product(&mut client, "A", "toys", 100, 40).await;
    let b = make_product(&mut client, "B", "toys", 100, 25).await;
    let mut tasks = Vec::new();
    for worker in 0..24u32 {
        let addr = server.addr;
        tasks.push(tokio::spawn(async move {
            let mut client = Client::connect(addr).await.unwrap();
            let mut won = 0u64;
            for round in 0..10u32 {
                let qty = 1 + (worker + round) % 3;
                let items = if round % 2 == 0 {
                    json!([{"product_id": a, "qty": qty}])
                } else {
                    json!([{"product_id": a, "qty": 1}, {"product_id": b, "qty": qty}])
                };
                let response = client
                    .json("POST", "/orders", &json!({"customer_id": customer, "items": items}))
                    .await
                    .unwrap();
                match response.status {
                    201 => won += 1,
                    409 => {}
                    other => panic!("unexpected {other}"),
                }
            }
            won
        }));
    }
    let mut won = 0;
    for task in tasks {
        won += task.await.unwrap();
    }
    let report = client.get("/admin/invariants").await.unwrap().json();
    let stats = client.get("/admin/stats").await.unwrap().json();
    assert_eq!(stats["orders"].as_u64().unwrap(), won);
    assert_eq!(report["live_orders"].as_u64().unwrap(), won);
    assert_eq!(report["consumed_units"].as_u64().unwrap() + report["stock_units"].as_u64().unwrap(), 65);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn order_history_lists_each_version_of_the_order() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("shop.ferro"), buffered()).await;
    let mut client = server.client().await;
    let customer = make_customer(&mut client, "hist").await;
    let item = make_product(&mut client, "Item", "books", 100, 5).await;
    let order = client
        .json("POST", "/orders", &json!({"customer_id": customer, "items": [{"product_id": item, "qty": 2}]}))
        .await
        .unwrap()
        .json();
    let id = order["id"].as_u64().unwrap();
    make_product(&mut client, "Noise", "books", 100, 5).await;
    let shipped = client.json("PATCH", &format!("/orders/{id}"), &json!({"status": "shipped"})).await.unwrap();
    assert_eq!(shipped.status, 200);
    make_product(&mut client, "Noise2", "books", 100, 5).await;

    let history = client.get(&format!("/orders/{id}/history")).await.unwrap();
    assert_eq!(history.status, 200);
    let entries = history.json()["history"].as_array().unwrap().clone();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["order"]["status"], "placed");
    assert_eq!(entries[1]["order"]["status"], "shipped");
    assert!(entries[0]["version"].as_u64().unwrap() < entries[1]["version"].as_u64().unwrap());
    assert_eq!(entries[0]["message"], format!("order {id}"));

    let again = client.json("PATCH", &format!("/orders/{id}"), &json!({"status": "cancelled"})).await.unwrap();
    assert_eq!(again.status, 409);
    assert_eq!(client.get("/orders/999/history").await.unwrap().status, 404);

    let second = client
        .json("POST", "/orders", &json!({"customer_id": customer, "items": [{"product_id": item, "qty": 1}]}))
        .await
        .unwrap()
        .json();
    let second_id = second["id"].as_u64().unwrap();
    assert_eq!(stock_of(&mut client, item).await, 2);
    client.json("PATCH", &format!("/orders/{second_id}"), &json!({"status": "cancelled"})).await.unwrap();
    assert_eq!(stock_of(&mut client, item).await, 3);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn time_travel_serves_old_versions_read_only() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("shop.ferro"), buffered()).await;
    let mut client = server.client().await;
    let customer = make_customer(&mut client, "tt").await;
    let item = make_product(&mut client, "Item", "books", 1000, 5).await;
    let before = client.get("/health").await.unwrap().json()["version"].as_u64().unwrap();
    client.json("PATCH", &format!("/products/{item}"), &json!({"price_cents": 2000})).await.unwrap();
    let order = client
        .json("POST", "/orders", &json!({"customer_id": customer, "items": [{"product_id": item, "qty": 2}]}))
        .await
        .unwrap()
        .json();
    let order_id = order["id"].as_u64().unwrap();

    let old = client.get(&format!("/admin/snapshot/{before}/products/{item}")).await.unwrap();
    assert_eq!(old.status, 200);
    assert_eq!(old.json()["price_cents"], 1000);
    assert_eq!(old.json()["stock"], 5);
    assert_eq!(old.header("x-version"), Some(before.to_string().as_str()));
    assert!(old.header("cache-control").unwrap().contains("immutable"));
    let now = client.get(&format!("/products/{item}")).await.unwrap().json();
    assert_eq!(now["price_cents"], 2000);
    assert_eq!(now["stock"], 3);

    assert_eq!(client.get(&format!("/admin/snapshot/{before}/orders/{order_id}")).await.unwrap().status, 404);
    let list = client.get(&format!("/admin/snapshot/{before}/products")).await.unwrap().json();
    assert_eq!(list["items"][0]["price_cents"], 1000);
    let orders = client.get(&format!("/admin/snapshot/{before}/customers/{customer}/orders")).await.unwrap().json();
    assert_eq!(orders["items"].as_array().unwrap().len(), 0);
    let health = client.get(&format!("/admin/snapshot/{before}/health")).await.unwrap().json();
    assert_eq!(health["version"], before);
    let history = client.get(&format!("/admin/snapshot/{before}/orders/{order_id}/history")).await.unwrap();
    assert_eq!(history.status, 404);

    assert_eq!(client.get("/admin/snapshot/99999/products").await.unwrap().status, 404);
    assert_eq!(client.get("/admin/snapshot/xyz/products").await.unwrap().status, 422);
    let write = client.json("POST", &format!("/admin/snapshot/{before}/products"), &json!({"name": "x", "category": "y", "price_cents": 1})).await.unwrap();
    assert!(write.status == 404 || write.status == 405);
    let genesis = client.get("/admin/snapshot/0/products").await.unwrap().json();
    assert_eq!(genesis["items"].as_array().unwrap().len(), 0);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn etag_gives_304_and_survives_unrelated_writes() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("shop.ferro"), buffered()).await;
    let mut client = server.client().await;
    let customer = make_customer(&mut client, "etag").await;
    let item = make_product(&mut client, "Item", "books", 1000, 50).await;
    let other = make_product(&mut client, "Other", "books", 1000, 50).await;

    let list = client.get("/products").await.unwrap();
    let list_tag = list.header("etag").unwrap().to_string();
    let one = client.get(&format!("/products/{item}")).await.unwrap();
    let one_tag = one.header("etag").unwrap().to_string();

    let again = client.send("GET", "/products", &[("if-none-match", &list_tag)], None).await.unwrap();
    assert_eq!(again.status, 304);
    assert!(again.body.is_empty());
    assert_eq!(again.header("etag"), Some(list_tag.as_str()));

    let item_path = format!("/products/{item}");
    client
        .json("POST", &format!("/carts/{customer}/items"), &json!({"product_id": item, "qty": 1}))
        .await
        .unwrap();
    client.json("PATCH", &format!("/products/{other}"), &json!({"name": "Renamed"})).await.unwrap();
    let still = client.send("GET", &item_path, &[("if-none-match", &one_tag)], None).await.unwrap();
    assert_eq!(still.status, 304);
    let stale_list = client.send("GET", "/products", &[("if-none-match", &list_tag)], None).await.unwrap();
    assert_eq!(stale_list.status, 200);

    client.json("PATCH", &item_path, &json!({"price_cents": 1})).await.unwrap();
    let changed = client.send("GET", &item_path, &[("if-none-match", &one_tag)], None).await.unwrap();
    assert_eq!(changed.status, 200);
    assert_ne!(changed.header("etag").unwrap(), one_tag);

    let multi = client
        .send("GET", &item_path, &[("if-none-match", &format!("W/\"nope\", {}", changed.header("etag").unwrap()))], None)
        .await
        .unwrap();
    assert_eq!(multi.status, 304);

    let order = client
        .json("POST", "/orders", &json!({"customer_id": customer, "items": [{"product_id": item, "qty": 1}]}))
        .await
        .unwrap();
    let order_id = order.json()["id"].as_u64().unwrap();
    let order_path = format!("/orders/{order_id}");
    let first = client.get(&order_path).await.unwrap();
    let tag = first.header("etag").unwrap().to_string();
    client.json("PATCH", &format!("/products/{other}"), &json!({"name": "Again"})).await.unwrap();
    let second = client.send("GET", &order_path, &[("if-none-match", &tag)], None).await.unwrap();
    assert_eq!(second.status, 304);
    client.json("PATCH", &order_path, &json!({"status": "shipped"})).await.unwrap();
    let third = client.send("GET", &order_path, &[("if-none-match", &tag)], None).await.unwrap();
    assert_eq!(third.status, 200);

    let stats = client.get("/admin/stats").await.unwrap().json();
    assert!(stats["not_modified"].as_u64().unwrap() >= 4);
    server.shutdown().await;
}

async fn next_change(stream: &mut SseStream) -> Value {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("event in time")
            .unwrap()
            .expect("stream open");
        if event.event == "change" {
            return serde_json::from_str(&event.data).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_stream_sends_only_what_changed() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("shop.ferro"), buffered()).await;
    let mut client = server.client().await;
    let customer = make_customer(&mut client, "feed").await;
    for index in 0..30 {
        make_product(&mut client, &format!("P{index}"), "toys", 100, 10).await;
    }
    let mut stream = SseStream::open(server.addr, "/events", &[]).await.unwrap();
    assert_eq!(stream.status, 200);
    let hello = stream.next().await.unwrap().unwrap();
    assert_eq!(hello.event, "hello");
    let start = serde_json::from_str::<Value>(&hello.data).unwrap()["version"].as_u64().unwrap();

    client
        .json("POST", "/orders", &json!({"customer_id": customer, "items": [{"product_id": 7, "qty": 3}]}))
        .await
        .unwrap();
    let change = next_change(&mut stream).await;
    assert_eq!(change["version"], start + 1);
    assert_eq!(change["from"], start);
    let changes = change["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 2);
    let stock = changes.iter().find(|c| c["table"] == "stock").unwrap();
    assert_eq!(stock["id"], 7);
    assert_eq!(stock["before"], 10);
    assert_eq!(stock["after"], 7);
    let order = changes.iter().find(|c| c["table"] == "orders").unwrap();
    assert_eq!(order["op"], "added");
    assert_eq!(order["after"]["customer"], customer);

    client.json("PATCH", "/products/3", &json!({"name": "Renamed"})).await.unwrap();
    let change = next_change(&mut stream).await;
    let changes = change["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["table"], "products");
    assert_eq!(changes[0]["op"], "changed");
    assert_eq!(changes[0]["before"]["name"], "P2");
    assert_eq!(changes[0]["after"]["name"], "Renamed");

    let noop = client.json("PATCH", "/products/3", &json!({"name": "Renamed"})).await.unwrap();
    assert_eq!(noop.status, 200);
    client.json("PATCH", "/products/4", &json!({"active": false})).await.unwrap();
    let change = next_change(&mut stream).await;
    assert_eq!(change["changes"][0]["id"], 4);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_stream_resumes_from_version_with_coalesced_diff() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("shop.ferro"), buffered()).await;
    let mut client = server.client().await;
    make_product(&mut client, "A", "toys", 100, 10).await;
    let version = client.get("/health").await.unwrap().json()["version"].as_u64().unwrap();
    client.json("PATCH", "/products/1", &json!({"stock": 9})).await.unwrap();
    client.json("PATCH", "/products/1", &json!({"stock": 8})).await.unwrap();
    make_product(&mut client, "B", "toys", 100, 1).await;

    let mut stream = SseStream::open(server.addr, &format!("/events?since={version}"), &[]).await.unwrap();
    let hello = stream.next().await.unwrap().unwrap();
    assert_eq!(hello.event, "hello");
    let change = next_change(&mut stream).await;
    assert_eq!(change["from"], version);
    assert_eq!(change["version"], version + 3);
    let stock = change["changes"].as_array().unwrap().iter().find(|c| c["table"] == "stock" && c["id"] == 1).unwrap().clone();
    assert_eq!(stock["before"], 10);
    assert_eq!(stock["after"], 8);

    make_product(&mut client, "C", "toys", 100, 1).await;
    let change = next_change(&mut stream).await;
    assert_eq!(change["from"], version + 3);

    let mut header_stream = SseStream::open(server.addr, "/events", &[("last-event-id", &version.to_string())]).await.unwrap();
    header_stream.next().await.unwrap().unwrap();
    let change = next_change(&mut header_stream).await;
    assert_eq!(change["from"], version);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_event_consumer_does_not_stall_writers_and_catches_up() {
    let dir = TempDir::new().unwrap();
    let config = Config {
        durability: Durability::Buffered,
        feed_capacity: 8,
        ..Config::default()
    };
    let server = Server::start(&dir.path().join("shop.ferro"), config).await;
    let mut client = server.client().await;
    make_product(&mut client, "A", "toys", 100, 100000).await;
    let mut slow = SseStream::open(server.addr, "/events", &[]).await.unwrap();
    let hello = slow.next().await.unwrap().unwrap();
    let start = serde_json::from_str::<Value>(&hello.data).unwrap()["version"].as_u64().unwrap();

    let began = std::time::Instant::now();
    let bulk = "x".repeat(16 * 1024);
    for round in 0..400 {
        let response = client
            .json("PATCH", "/products/1", &json!({"stock_delta": -1, "name": format!("{round}{bulk}")}))
            .await
            .unwrap();
        assert_eq!(response.status, 200);
    }
    assert!(began.elapsed() < Duration::from_secs(10));

    let mut last = start;
    let mut final_stock = None;
    let mut frames = 0;
    while last < start + 400 {
        let change = next_change(&mut slow).await;
        assert_eq!(change["from"].as_u64().unwrap(), last);
        last = change["version"].as_u64().unwrap();
        frames += 1;
        for entry in change["changes"].as_array().unwrap() {
            if entry["table"] == "stock" {
                final_stock = entry["after"].as_u64();
            }
        }
    }
    assert_eq!(final_stock, Some(100000 - 400));
    assert!(frames < 400);
    let stats = client.get("/admin/stats").await.unwrap().json();
    assert!(stats["events_lagged"].as_u64().unwrap() >= 1);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_reclaims_under_load_and_the_replica_keeps_following() {
    let dir = TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("shop.ferro");
    let config = Config {
        durability: Durability::Buffered,
        keep_versions: Some(20),
        reclaim_bytes: 256 << 10,
        ..Config::default()
    };
    let primary = Server::start(&path, config).await;
    let mut writer = primary.client().await;
    let item = make_product(&mut writer, "Item", "books", 100, 100_000).await;
    let replica = Server::start(
        &path,
        Config {
            replica: true,
            refresh_ms: 2,
            ..Config::default()
        },
    )
    .await;
    let mut reader = replica.client().await;
    let bulk = "x".repeat(8 * 1024);
    let mut sold = 0;
    let mut cycles = 0;
    for round in 0..3000 {
        let response = writer
            .json(
                "PATCH",
                &format!("/products/{item}"),
                &json!({"stock_delta": -1, "name": format!("{round}{bulk}")}),
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        sold += 1;
        if round % 100 == 99 {
            cycles = writer.get("/admin/stats").await.unwrap().json()["reclaim_cycles"]
                .as_u64()
                .unwrap();
            if cycles >= 2 {
                break;
            }
        }
    }
    assert!(cycles >= 2, "only {cycles} cycles");
    let stats = writer.get("/admin/stats").await.unwrap().json();
    assert!(stats["db_bytes"].as_u64().unwrap() < sold * 8 * 1024 / 2, "{stats}");
    assert_eq!(stock_of(&mut writer, item).await, 100_000 - sold);
    let mut seen = 0;
    for _ in 0..200 {
        seen = stock_of(&mut reader, item).await;
        if seen == 100_000 - sold {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(seen, 100_000 - sold);
    replica.shutdown().await;
    primary.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replica_serves_reads_follows_primary_and_refuses_writes() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("shop.ferro");
    let primary = Server::start(&path, Config::default()).await;
    let mut writer = primary.client().await;
    let item = make_product(&mut writer, "Item", "books", 100, 9).await;
    let replica = Server::start(
        &path,
        Config {
            replica: true,
            refresh_ms: 2,
            ..Config::default()
        },
    )
    .await;
    let mut reader = replica.client().await;
    assert_eq!(stock_of(&mut reader, item).await, 9);

    let customer = make_customer(&mut writer, "rep").await;
    writer
        .json("POST", "/orders", &json!({"customer_id": customer, "items": [{"product_id": item, "qty": 4}]}))
        .await
        .unwrap();
    let mut seen = 0;
    for _ in 0..200 {
        seen = stock_of(&mut reader, item).await;
        if seen == 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(seen, 5);

    let refused = reader.json("POST", "/products", &json!({"name": "x", "category": "y", "price_cents": 1})).await.unwrap();
    assert_eq!(refused.status, 403);
    let health = reader.get("/health").await.unwrap().json();
    assert_eq!(health["replica"], true);

    let mut stream = SseStream::open(replica.addr, "/events", &[]).await.unwrap();
    stream.next().await.unwrap().unwrap();
    writer.json("PATCH", &format!("/products/{item}"), &json!({"name": "Live"})).await.unwrap();
    let change = next_change(&mut stream).await;
    assert_eq!(change["changes"][0]["after"]["name"], "Live");

    replica.shutdown().await;
    primary.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_keeps_data_and_history() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("shop.ferro");
    let server = Server::start(&path, Config::default()).await;
    let mut client = server.client().await;
    let customer = make_customer(&mut client, "restart").await;
    let item = make_product(&mut client, "Item", "books", 100, 9).await;
    client
        .json("POST", "/orders", &json!({"customer_id": customer, "items": [{"product_id": item, "qty": 4}]}))
        .await
        .unwrap();
    let version = client.get("/health").await.unwrap().json()["version"].as_u64().unwrap();
    drop(client);
    server.shutdown().await;

    let mut reopened = None;
    for _ in 0..100 {
        match open_db(&path, &Config::default()) {
            Ok(db) => {
                reopened = Some(db);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let db = reopened.expect("lock released after shutdown");
    assert_eq!(db.version(), version);
    assert_eq!(db.read().stock.get(&item).map(|s| *s), Some(5));
    let config = Config {
        reads: ReadMode::Blocking,
        writes: WriteMode::Blocking,
        ..Config::default()
    };
    let app = build(db, &config);
    assert_eq!(app.db.at(version - 1).unwrap().orders.len(), 0);
}
