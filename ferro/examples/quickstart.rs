use ferro::{Change, Data, Db, Map, Record, Table};

#[derive(Data, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Pending,
    Shipped,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Order {
    #[key]
    id: u64,
    #[index]
    customer: String,
    #[index]
    status: Status,
    total_cents: u64,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Shop {
    stock: Map<String, u32>,
    orders: Table<Order>,
    next_order: u64,
}

fn main() -> ferro::Result<()> {
    let dir = tempfile::tempdir()?;
    let db = Db::<Shop>::open(dir.path().join("shop.ferro"))?;

    db.commit("receive stock", |shop| {
        shop.stock.insert("anvil".to_string(), 5);
        shop.stock.insert("rocket".to_string(), 2);
    })?;

    let placed = db.commit_with(|shop| {
        *shop.stock.get_mut("anvil").unwrap() -= 1;
        shop.next_order += 1;
        let id = shop.next_order;
        shop.orders.insert(Order {
            id,
            customer: "wile".to_string(),
            status: Status::Pending,
            total_cents: 4999,
        });
        (format!("order #{id}"), id)
    })?;

    let before_shipping = db.read();

    db.commit("ship", |shop| {
        shop.orders.get_mut(&placed.value).unwrap().status = Status::Shipped;
    })?;

    let shop = db.read();
    println!("anvils left: {}", shop.stock.get("anvil").unwrap());
    println!(
        "pending now: {}",
        shop.orders.by_status(&Status::Pending).len()
    );
    println!(
        "pending then: {}",
        before_shipping.orders.by_status(&Status::Pending).len()
    );

    let big: Vec<_> = shop
        .orders
        .by_customer("wile")
        .filter(|order| order.total_cents > 1000)
        .collect();
    println!("wile's big orders: {big:?}");

    for change in before_shipping.orders.diff(&shop.orders) {
        if let Change::Changed(id, old, new) = change {
            println!("order {id}: {:?} -> {:?}", old.status, new.status);
        }
    }

    for commit in db.log().values() {
        println!("v{} {}", commit.version, commit.message);
    }

    db.restore(&db.at(1)?)?;
    println!("after restore: {} orders", db.read().orders.len());
    Ok(())
}
