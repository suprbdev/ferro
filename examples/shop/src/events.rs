use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use ferro::{Change, Snapshot};
use futures_util::stream::{Stream, unfold};
use serde_json::{Value, json};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::watch;

use crate::app::{ApiError, Shared, blocking};
use crate::model::{Shop, cart_view};

const MAX_CHANGES: usize = 200;

pub struct Frame {
    pub from: u64,
    pub version: u64,
    pub json: String,
}

fn describe<K, V>(
    table: &'static str,
    changes: impl Iterator<Item = Change<K, V>>,
    key: impl Fn(&K) -> u64,
    show: impl Fn(&V) -> Value,
    out: &mut Vec<Value>,
    more: &mut bool,
) {
    for (taken, change) in changes.enumerate() {
        if taken >= MAX_CHANGES {
            *more = true;
            return;
        }
        let id = key(change.key());
        let entry = match &change {
            Change::Added(_, after) => json!({"table": table, "op": "added", "id": id, "after": show(after)}),
            Change::Removed(_, before) => json!({"table": table, "op": "removed", "id": id, "before": show(before)}),
            Change::Changed(_, before, after) => json!({"table": table, "op": "changed", "id": id, "before": show(before), "after": show(after)}),
        };
        out.push(entry);
    }
}

pub fn diff_frame(old: &Snapshot<Shop>, new: &Snapshot<Shop>) -> Frame {
    let mut changes = Vec::new();
    let mut more = false;
    describe(
        "products",
        old.products.diff(&new.products),
        |key| *key,
        |row| serde_json::to_value(row).unwrap_or(Value::Null),
        &mut changes,
        &mut more,
    );
    describe(
        "stock",
        old.stock.diff(&new.stock),
        |key| *key,
        |stock| json!(*stock),
        &mut changes,
        &mut more,
    );
    describe(
        "customers",
        old.customers.diff(&new.customers),
        |key| *key,
        |row| serde_json::to_value(row).unwrap_or(Value::Null),
        &mut changes,
        &mut more,
    );
    describe(
        "carts",
        old.carts.diff(&new.carts),
        |key| *key,
        cart_view,
        &mut changes,
        &mut more,
    );
    describe(
        "orders",
        old.orders.diff(&new.orders),
        |key| *key,
        |row| serde_json::to_value(row).unwrap_or(Value::Null),
        &mut changes,
        &mut more,
    );
    let body = json!({
        "from": old.version(),
        "version": new.version(),
        "message": new.commit().message,
        "time_ms": new.commit().time_ms,
        "more": more,
        "changes": changes,
    });
    Frame {
        from: old.version(),
        version: new.version(),
        json: body.to_string(),
    }
}

pub fn spawn_broadcaster(db: &ferro::Db<Shop>, feed: broadcast::Sender<Arc<Frame>>) {
    let watcher = db.watch();
    let mut previous = db.read();
    std::thread::Builder::new()
        .name("shop-broadcaster".into())
        .spawn(move || {
            for snapshot in watcher {
                if feed.receiver_count() > 0 {
                    let frame = diff_frame(&previous, &snapshot);
                    let _ = feed.send(Arc::new(frame));
                }
                previous = snapshot;
            }
        })
        .expect("spawn broadcaster");
}

struct Feed {
    app: Shared,
    receiver: broadcast::Receiver<Arc<Frame>>,
    closing: watch::Receiver<bool>,
    last: u64,
    queued: VecDeque<Event>,
}

fn sse_event(frame: &Frame) -> Event {
    Event::default()
        .id(frame.version.to_string())
        .event("change")
        .data(frame.json.clone())
}

async fn catch_up(app: &Shared, last: u64) -> Result<Option<Frame>, ApiError> {
    let db = app.db.clone();
    blocking(move || -> Result<Option<Frame>, ferro::Error> {
        let current = db.read();
        if current.version() <= last {
            return Ok(None);
        }
        let old = db.at(last)?;
        Ok(Some(diff_frame(&old, &current)))
    })
    .await?
    .map_err(ApiError::from)
}

fn stream(feed: Feed) -> impl Stream<Item = Result<Event, std::convert::Infallible>> {
    unfold(feed, |mut feed| async move {
        if let Some(event) = feed.queued.pop_front() {
            return Some((Ok(event), feed));
        }
        loop {
            let received = tokio::select! {
                received = feed.receiver.recv() => received,
                _ = feed.closing.changed() => return None,
            };
            match received {
                Ok(frame) => {
                    if frame.version <= feed.last {
                        continue;
                    }
                    if frame.from > feed.last {
                        match catch_up(&feed.app, feed.last).await {
                            Ok(Some(frame)) => {
                                feed.last = frame.version;
                                feed.app.counters.events_sent.fetch_add(1, Ordering::Relaxed);
                                return Some((Ok(sse_event(&frame)), feed));
                            }
                            _ => continue,
                        }
                    }
                    feed.last = frame.version;
                    feed.app.counters.events_sent.fetch_add(1, Ordering::Relaxed);
                    return Some((Ok(sse_event(&frame)), feed));
                }
                Err(RecvError::Lagged(_)) => {
                    feed.app.counters.events_lagged.fetch_add(1, Ordering::Relaxed);
                    match catch_up(&feed.app, feed.last).await {
                        Ok(Some(frame)) => {
                            feed.last = frame.version;
                            return Some((Ok(sse_event(&frame)), feed));
                        }
                        _ => continue,
                    }
                }
                Err(RecvError::Closed) => return None,
            }
        }
    })
}

pub async fn open(
    app: Shared,
    since: Option<u64>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let receiver = app.feed.subscribe();
    let current = app.db.version();
    let start = since.map(|since| since.min(current)).unwrap_or(current);
    let hello = Event::default()
        .event("hello")
        .data(json!({"version": current, "resume_from": start}).to_string());
    let mut queued = VecDeque::from([hello]);
    let mut last = start;
    if start < current
        && let Ok(Some(frame)) = catch_up(&app, start).await
    {
        last = frame.version;
        queued.push_back(sse_event(&frame));
    }
    let closing = app.closing.subscribe();
    let feed = Feed {
        app,
        receiver,
        closing,
        last,
        queued,
    };
    Sse::new(stream(feed)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
