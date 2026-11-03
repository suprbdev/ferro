use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use chat::minihttp::{Client, SseEvent, SseStream};
use chat::{Config, Shared, build, open_db, serve};
use ferro::Durability;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct Server {
    addr: SocketAddr,
    app: Option<Shared>,
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
            app: Some(app),
            stop: Some(stop),
            task: Some(task),
        }
    }

    async fn client(&self) -> Client {
        Client::connect(self.addr).await.expect("connect")
    }

    async fn events(&self, path: &str, last: Option<u64>) -> SseStream {
        let id = last.map(|version| version.to_string());
        let headers: Vec<(&str, &str)> = match &id {
            Some(id) => vec![("last-event-id", id.as_str())],
            None => Vec::new(),
        };
        let stream = SseStream::open(self.addr, path, &headers)
            .await
            .expect("sse");
        assert_eq!(stream.status, 200, "{path}");
        stream
    }

    fn feeds(&self) -> usize {
        self.app.as_ref().map_or(0, |app| app.feeds.live())
    }

    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("server stops")
                .expect("server task");
        }
        self.app.take();
    }
}

fn buffered() -> Config {
    Config {
        durability: Durability::Buffered,
        ..Config::default()
    }
}

fn body(response: &chat::minihttp::Response) -> String {
    String::from_utf8_lossy(&response.body).to_string()
}

async fn make_user(client: &mut Client, name: &str) -> u64 {
    let response = client
        .json("POST", "/users", &json!({"name": name}))
        .await
        .unwrap();
    assert_eq!(response.status, 201, "{}", body(&response));
    response.json()["id"].as_u64().unwrap()
}

async fn make_room(client: &mut Client, owner: u64, name: &str) -> u64 {
    let response = client
        .act(owner, "POST", "/rooms", Some(&json!({"name": name})))
        .await
        .unwrap();
    assert_eq!(response.status, 201, "{}", body(&response));
    response.json()["id"].as_u64().unwrap()
}

async fn join(client: &mut Client, user: u64, room: u64) {
    let response = client
        .act(user, "POST", &format!("/rooms/{room}/join"), None)
        .await
        .unwrap();
    assert_eq!(response.status, 200, "{}", body(&response));
}

async fn post(client: &mut Client, user: u64, room: u64, text: &str) -> (u64, u64) {
    let response = client
        .act(
            user,
            "POST",
            &format!("/rooms/{room}/messages"),
            Some(&json!({"text": text})),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 201, "{}", body(&response));
    let reply = response.json();
    (
        reply["seq"].as_u64().unwrap(),
        reply["version"].as_u64().unwrap(),
    )
}

async fn get_json(client: &mut Client, path: &str) -> Value {
    let response = client.get(path).await.unwrap();
    assert_eq!(response.status, 200, "{path}: {}", body(&response));
    response.json()
}

async fn unread_of(client: &mut Client, user: u64, room: u64) -> u64 {
    let reply = get_json(client, &format!("/users/{user}/unread")).await;
    reply["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["room"].as_u64() == Some(room))
        .and_then(|entry| entry["unread"].as_u64())
        .unwrap()
}

async fn next_event(stream: &mut SseStream) -> SseEvent {
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("event in time")
        .expect("read event")
        .expect("stream open")
}

async fn changes_until(
    stream: &mut SseStream,
    mut done: impl FnMut(&[Value]) -> bool,
) -> (Vec<Value>, Option<u64>) {
    let mut seen = Vec::new();
    let mut last = None;
    while !done(&seen) {
        let event = next_event(stream).await;
        if event.event != "changes" {
            continue;
        }
        last = event.id.as_deref().and_then(|id| id.parse().ok());
        let data: Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(data["more"], json!(false));
        seen.extend(data["changes"].as_array().unwrap().iter().cloned());
    }
    (seen, last)
}

fn of_type<'a>(changes: &'a [Value], kind: &str) -> Vec<&'a Value> {
    changes
        .iter()
        .filter(|change| change["type"] == kind)
        .collect()
}

fn message_texts(changes: &[Value]) -> Vec<String> {
    of_type(changes, "message")
        .into_iter()
        .map(|change| {
            change["message"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_pages_newest_first_before_a_cursor() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("chat.ferro"), buffered()).await;
    let mut client = server.client().await;
    let ann = make_user(&mut client, "ann").await;
    let room = make_room(&mut client, ann, "general").await;
    for n in 1..=25 {
        post(&mut client, ann, room, &format!("message {n}")).await;
    }
    let first = get_json(&mut client, &format!("/rooms/{room}/messages?limit=10")).await;
    let seqs: Vec<u64> = first["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, (16..=25).rev().collect::<Vec<_>>());
    assert_eq!(first["next"], json!(16));
    let second = get_json(
        &mut client,
        &format!("/rooms/{room}/messages?limit=10&before=16"),
    )
    .await;
    assert_eq!(second["messages"][0]["seq"], json!(15));
    assert_eq!(second["messages"][9]["seq"], json!(6));
    assert_eq!(second["next"], json!(6));
    let last = get_json(
        &mut client,
        &format!("/rooms/{room}/messages?limit=10&before=6"),
    )
    .await;
    assert_eq!(last["messages"].as_array().unwrap().len(), 5);
    assert_eq!(last["messages"][4]["text"], json!("message 1"));
    assert_eq!(last["next"], Value::Null);
    let one = get_json(&mut client, &format!("/rooms/{room}/messages/7")).await;
    assert_eq!(one["text"], json!("message 7"));
    let view = get_json(&mut client, &format!("/rooms/{room}")).await;
    assert_eq!(view["last_seq"], json!(25));
    assert_eq!(view["messages"], json!(25));
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_author_edits_or_deletes_and_members_react() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("chat.ferro"), buffered()).await;
    let mut client = server.client().await;
    let ann = make_user(&mut client, "ann").await;
    let bob = make_user(&mut client, "bob").await;
    let eve = make_user(&mut client, "eve").await;
    let room = make_room(&mut client, ann, "general").await;
    join(&mut client, bob, room).await;
    let (seq, _) = post(&mut client, ann, room, "hello there").await;
    let path = format!("/rooms/{room}/messages/{seq}");
    let refused = client
        .act(bob, "PATCH", &path, Some(&json!({"text": "hijacked"})))
        .await
        .unwrap();
    assert_eq!(refused.status, 403);
    let refused = client.act(bob, "DELETE", &path, None).await.unwrap();
    assert_eq!(refused.status, 403);
    let anonymous = client
        .json(
            "POST",
            &format!("/rooms/{room}/messages"),
            &json!({"text": "hi"}),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status, 401);
    let outsider = client
        .act(
            eve,
            "POST",
            &format!("/rooms/{room}/messages"),
            Some(&json!({"text": "hi"})),
        )
        .await
        .unwrap();
    assert_eq!(outsider.status, 403);
    let edited = client
        .act(
            ann,
            "PATCH",
            &path,
            Some(&json!({"text": "hello everyone"})),
        )
        .await
        .unwrap();
    assert_eq!(edited.status, 200, "{}", body(&edited));
    let message = get_json(&mut client, &path).await;
    assert_eq!(message["text"], json!("hello everyone"));
    assert!(message["edited_ms"].as_u64().is_some());
    let thumbs = format!("{path}/reactions/%F0%9F%91%8D");
    assert_eq!(
        client.act(bob, "PUT", &thumbs, None).await.unwrap().status,
        200
    );
    assert_eq!(
        client.act(ann, "PUT", &thumbs, None).await.unwrap().status,
        200
    );
    let again = client.act(ann, "PUT", &thumbs, None).await.unwrap();
    assert_eq!(again.json()["changed"], json!(false));
    assert_eq!(
        client.act(eve, "PUT", &thumbs, None).await.unwrap().status,
        403
    );
    let message = get_json(&mut client, &path).await;
    assert_eq!(message["reactions"], json!({"👍": [ann, bob]}));
    assert_eq!(
        client
            .act(bob, "DELETE", &thumbs, None)
            .await
            .unwrap()
            .status,
        200
    );
    let message = get_json(&mut client, &path).await;
    assert_eq!(message["reactions"], json!({"👍": [ann]}));
    let deleted = client.act(ann, "DELETE", &path, None).await.unwrap();
    assert_eq!(deleted.status, 200);
    let message = get_json(&mut client, &path).await;
    assert_eq!(message["deleted"], json!(true));
    assert!(message.get("text").is_none());
    let late = client
        .act(ann, "PATCH", &path, Some(&json!({"text": "back"})))
        .await
        .unwrap();
    assert_eq!(late.status, 404);
    let found = get_json(&mut client, &format!("/rooms/{room}/search?q=everyone")).await;
    assert_eq!(found["messages"], json!([]));
    let taken = client
        .json("POST", "/users", &json!({"name": "ann"}))
        .await
        .unwrap();
    assert_eq!(taken.status, 409);
    let taken = client
        .act(bob, "POST", "/rooms", Some(&json!({"name": "general"})))
        .await
        .unwrap();
    assert_eq!(taken.status, 409);
    assert_eq!(client.get("/rooms/99/messages").await.unwrap().status, 404);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unread_counts_follow_posts_receipts_and_deletes() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("chat.ferro"), buffered()).await;
    let mut client = server.client().await;
    let ann = make_user(&mut client, "ann").await;
    let bob = make_user(&mut client, "bob").await;
    let room = make_room(&mut client, ann, "general").await;
    let side = make_room(&mut client, ann, "side").await;
    post(&mut client, ann, room, "before bob joined").await;
    join(&mut client, bob, room).await;
    join(&mut client, bob, side).await;
    assert_eq!(unread_of(&mut client, bob, room).await, 0);
    let mut seqs = Vec::new();
    for n in 1..=5 {
        seqs.push(post(&mut client, ann, room, &format!("news {n}")).await.0);
    }
    post(&mut client, ann, side, "side note").await;
    assert_eq!(unread_of(&mut client, bob, room).await, 5);
    assert_eq!(unread_of(&mut client, bob, side).await, 1);
    assert_eq!(unread_of(&mut client, ann, room).await, 0);
    let total = get_json(&mut client, &format!("/users/{bob}/unread")).await;
    assert_eq!(total["total"], json!(6));
    let read = client
        .act(
            bob,
            "POST",
            &format!("/rooms/{room}/read?seq={}", seqs[2]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(read.json()["unread"], json!(2));
    let back = client
        .act(
            bob,
            "POST",
            &format!("/rooms/{room}/read?seq={}", seqs[0]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(back.json()["read"], json!(seqs[2]));
    assert_eq!(unread_of(&mut client, bob, room).await, 2);
    client
        .act(
            ann,
            "DELETE",
            &format!("/rooms/{room}/messages/{}", seqs[4]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(unread_of(&mut client, bob, room).await, 1);
    let (own, _) = post(&mut client, bob, room, "bob says hi").await;
    assert_eq!(unread_of(&mut client, bob, room).await, 0);
    post(&mut client, ann, room, "welcome bob").await;
    assert_eq!(unread_of(&mut client, bob, room).await, 1);
    let all = client
        .act(bob, "POST", &format!("/rooms/{room}/read?seq=1000"), None)
        .await
        .unwrap();
    assert_eq!(all.json()["read"], json!(own + 1));
    assert_eq!(unread_of(&mut client, bob, room).await, 0);
    let view = get_json(&mut client, &format!("/rooms/{room}")).await;
    assert_eq!(view["messages"], json!(7));
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_matches_every_word_within_one_room() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("chat.ferro"), buffered()).await;
    let mut client = server.client().await;
    let ann = make_user(&mut client, "ann").await;
    let room = make_room(&mut client, ann, "general").await;
    let other = make_room(&mut client, ann, "other").await;
    let texts = [
        "Deploy the release today",
        "lunch at noon?",
        "the release notes need review",
        "Release, deploy, celebrate!",
        "deploy failed again",
    ];
    let mut seqs = Vec::new();
    for text in texts {
        seqs.push(post(&mut client, ann, room, text).await.0);
    }
    post(&mut client, ann, other, "deploy the release elsewhere").await;
    let search = |query: &str| format!("/rooms/{room}/search?q={query}");
    let both = get_json(&mut client, &search("release+DEPLOY")).await;
    let found: Vec<u64> = both["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(found, vec![seqs[3], seqs[0]]);
    let paged = get_json(&mut client, &format!("{}&limit=1", search("deploy"))).await;
    assert_eq!(paged["messages"][0]["seq"], json!(seqs[4]));
    assert_eq!(paged["next"], json!(seqs[4]));
    let rest = get_json(
        &mut client,
        &format!("{}&before={}", search("deploy"), seqs[4]),
    )
    .await;
    assert_eq!(rest["messages"].as_array().unwrap().len(), 2);
    client
        .act(
            ann,
            "PATCH",
            &format!("/rooms/{room}/messages/{}", seqs[1]),
            Some(&json!({"text": "lunch after the deploy"})),
        )
        .await
        .unwrap();
    let lunch = get_json(&mut client, &search("deploy+lunch")).await;
    assert_eq!(lunch["messages"][0]["seq"], json!(seqs[1]));
    let noon = get_json(&mut client, &search("noon")).await;
    assert_eq!(noon["messages"], json!([]));
    let bad = client.get(&search("a")).await.unwrap();
    assert_eq!(bad.status, 422);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_clients_each_receive_their_rooms_over_sse() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("chat.ferro"), buffered()).await;
    let mut client = server.client().await;
    let mut users = Vec::new();
    for n in 0..8 {
        users.push(make_user(&mut client, &format!("user{n}")).await);
    }
    let even = make_room(&mut client, users[0], "even").await;
    let odd = make_room(&mut client, users[1], "odd").await;
    let all = make_room(&mut client, users[0], "all").await;
    for (index, &user) in users.iter().enumerate() {
        let room = if index % 2 == 0 { even } else { odd };
        join(&mut client, user, room).await;
        join(&mut client, user, all).await;
    }
    let mut readers = Vec::new();
    for &user in &users {
        let mut stream = server.events(&format!("/users/{user}/events"), None).await;
        let hello = next_event(&mut stream).await;
        assert_eq!(hello.event, "hello");
        assert!(hello.id.is_some());
        readers.push(stream);
    }
    let mut room_stream = server.events(&format!("/rooms/{all}/events"), None).await;
    assert_eq!(next_event(&mut room_stream).await.event, "hello");
    assert_eq!(server.feeds(), users.len() + 1);
    let per_user = 12;
    let mut posters = Vec::new();
    for (index, &user) in users.iter().enumerate() {
        let addr = server.addr;
        let home = if index % 2 == 0 { even } else { odd };
        posters.push(tokio::spawn(async move {
            let mut client = Client::connect(addr).await.unwrap();
            for n in 0..per_user {
                let room = if n % 2 == 0 { home } else { all };
                post(&mut client, user, room, &format!("from {user} number {n}")).await;
            }
        }));
    }
    for poster in posters {
        poster.await.unwrap();
    }
    let expect_all = users.len() * per_user / 2;
    let expect_home = 4 * per_user / 2;
    let mut tasks = Vec::new();
    for (index, mut stream) in readers.into_iter().enumerate() {
        let wanted = expect_all + expect_home;
        let (home, away) = if index % 2 == 0 {
            (even, odd)
        } else {
            (odd, even)
        };
        tasks.push(tokio::spawn(async move {
            let (changes, _) =
                changes_until(&mut stream, |seen| of_type(seen, "message").len() >= wanted).await;
            let mut by_room: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
            for change in of_type(&changes, "message") {
                let room = change["message"]["room"].as_u64().unwrap();
                let seq = change["message"]["seq"].as_u64().unwrap();
                assert!(
                    by_room.entry(room).or_default().insert(seq),
                    "duplicate {room}/{seq}"
                );
            }
            assert_eq!(by_room.get(&all).map_or(0, BTreeSet::len), expect_all);
            assert_eq!(by_room.get(&home).map_or(0, BTreeSet::len), expect_home);
            assert!(!by_room.contains_key(&away));
            changes
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let (changes, _) = changes_until(&mut room_stream, |seen| {
        of_type(seen, "message").len() >= expect_all
    })
    .await;
    assert_eq!(of_type(&changes, "message").len(), expect_all);
    assert!(
        changes
            .iter()
            .all(|change| change["type"] != "message" || change["message"]["room"] == json!(all))
    );
    let mut waited = 0;
    while server.feeds() > 1 && waited < 500 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        waited += 1;
    }
    assert_eq!(server.feeds(), 1);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edits_deletes_reactions_and_receipts_reach_subscribers() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("chat.ferro"), buffered()).await;
    let mut client = server.client().await;
    let ann = make_user(&mut client, "ann").await;
    let bob = make_user(&mut client, "bob").await;
    let room = make_room(&mut client, ann, "general").await;
    join(&mut client, bob, room).await;
    let mut feed = server.events(&format!("/users/{bob}/events"), None).await;
    assert_eq!(next_event(&mut feed).await.event, "hello");
    let (seq, _) = post(&mut client, ann, room, "first draft").await;
    let (changes, _) = changes_until(&mut feed, |seen| !of_type(seen, "unread").is_empty()).await;
    assert_eq!(message_texts(&changes), vec!["first draft"]);
    assert_eq!(of_type(&changes, "unread")[0]["count"], json!(1));
    let path = format!("/rooms/{room}/messages/{seq}");
    client
        .act(ann, "PATCH", &path, Some(&json!({"text": "final text"})))
        .await
        .unwrap();
    let (changes, _) = changes_until(&mut feed, |seen| !of_type(seen, "edited").is_empty()).await;
    assert_eq!(of_type(&changes, "edited")[0]["text"], json!("final text"));
    client
        .act(ann, "PUT", &format!("{path}/reactions/tada"), None)
        .await
        .unwrap();
    let (changes, _) =
        changes_until(&mut feed, |seen| !of_type(seen, "reactions").is_empty()).await;
    assert_eq!(
        of_type(&changes, "reactions")[0]["reactions"],
        json!({"tada": [ann]})
    );
    client
        .act(bob, "POST", &format!("/rooms/{room}/read"), None)
        .await
        .unwrap();
    let (changes, _) = changes_until(&mut feed, |seen| !of_type(seen, "read").is_empty()).await;
    assert_eq!(of_type(&changes, "read")[0]["user"], json!(bob));
    assert_eq!(of_type(&changes, "unread")[0]["count"], json!(0));
    client.act(ann, "DELETE", &path, None).await.unwrap();
    let (changes, _) = changes_until(&mut feed, |seen| !of_type(seen, "deleted").is_empty()).await;
    assert_eq!(of_type(&changes, "deleted")[0]["seq"], json!(seq));
    let mut room_feed = server.events(&format!("/rooms/{room}/events"), None).await;
    assert_eq!(next_event(&mut room_feed).await.event, "hello");
    post(&mut client, bob, room, "bob was here").await;
    let (changes, _) =
        changes_until(&mut room_feed, |seen| !of_type(seen, "read").is_empty()).await;
    assert_eq!(message_texts(&changes), vec!["bob was here"]);
    assert!(of_type(&changes, "unread").is_empty());
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_user_feed_follows_joins_and_leaves() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("chat.ferro"), buffered()).await;
    let mut client = server.client().await;
    let ann = make_user(&mut client, "ann").await;
    let bob = make_user(&mut client, "bob").await;
    let room = make_room(&mut client, ann, "general").await;
    let mut feed = server.events(&format!("/users/{bob}/events"), None).await;
    let hello: Value = serde_json::from_str(&next_event(&mut feed).await.data).unwrap();
    assert_eq!(hello["rooms"], json!([]));
    post(&mut client, ann, room, "nobody hears this").await;
    join(&mut client, bob, room).await;
    let (changes, _) =
        changes_until(&mut feed, |seen| !of_type(seen, "joined_room").is_empty()).await;
    assert!(message_texts(&changes).is_empty());
    assert_eq!(
        of_type(&changes, "joined_room")[0]["room"]["id"],
        json!(room)
    );
    post(&mut client, ann, room, "welcome").await;
    let (changes, _) = changes_until(&mut feed, |seen| !of_type(seen, "message").is_empty()).await;
    assert_eq!(message_texts(&changes), vec!["welcome"]);
    client
        .act(bob, "POST", &format!("/rooms/{room}/leave"), None)
        .await
        .unwrap();
    let (changes, _) =
        changes_until(&mut feed, |seen| !of_type(seen, "left_room").is_empty()).await;
    assert_eq!(of_type(&changes, "left_room")[0]["room"], json!(room));
    post(&mut client, ann, room, "after bob left").await;
    let other = make_room(&mut client, bob, "bobs").await;
    let (changes, _) =
        changes_until(&mut feed, |seen| !of_type(seen, "joined_room").is_empty()).await;
    assert!(message_texts(&changes).is_empty());
    assert_eq!(
        of_type(&changes, "joined_room")[0]["room"]["id"],
        json!(other)
    );
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnecting_with_last_event_id_delivers_what_was_missed() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("chat.ferro"), buffered()).await;
    let mut client = server.client().await;
    let ann = make_user(&mut client, "ann").await;
    let bob = make_user(&mut client, "bob").await;
    let room = make_room(&mut client, ann, "general").await;
    join(&mut client, bob, room).await;
    let mut feed = server.events(&format!("/users/{bob}/events"), None).await;
    let hello = next_event(&mut feed).await;
    let mut last: u64 = hello.id.unwrap().parse().unwrap();
    let (kept, _) = post(&mut client, ann, room, "seen live").await;
    let (changes, id) = changes_until(&mut feed, |seen| !of_type(seen, "message").is_empty()).await;
    assert_eq!(message_texts(&changes), vec!["seen live"]);
    last = id.unwrap_or(last);
    drop(feed);
    let mut missed = Vec::new();
    for n in 1..=5 {
        missed.push(post(&mut client, ann, room, &format!("missed {n}")).await.0);
    }
    client
        .act(
            ann,
            "PATCH",
            &format!("/rooms/{room}/messages/{kept}"),
            Some(&json!({"text": "seen live, edited"})),
        )
        .await
        .unwrap();
    client
        .act(
            ann,
            "DELETE",
            &format!("/rooms/{room}/messages/{}", missed[1]),
            None,
        )
        .await
        .unwrap();
    let mut feed = server
        .events(&format!("/users/{bob}/events"), Some(last))
        .await;
    let hello = next_event(&mut feed).await;
    assert_eq!(hello.event, "hello");
    assert!(hello.id.is_none());
    let catch_up = next_event(&mut feed).await;
    assert_eq!(catch_up.event, "changes");
    let data: Value = serde_json::from_str(&catch_up.data).unwrap();
    assert_eq!(data["from"], json!(last));
    let changes = data["changes"].as_array().unwrap().clone();
    let texts = message_texts(&changes);
    assert_eq!(texts, vec!["missed 1", "missed 3", "missed 4", "missed 5"]);
    assert_eq!(
        of_type(&changes, "edited")[0]["text"],
        json!("seen live, edited")
    );
    assert!(of_type(&changes, "deleted").is_empty());
    assert_eq!(of_type(&changes, "unread")[0]["count"], json!(5));
    let resumed: u64 = catch_up.id.unwrap().parse().unwrap();
    post(&mut client, ann, room, "live again").await;
    let (changes, _) = changes_until(&mut feed, |seen| !of_type(seen, "message").is_empty()).await;
    assert_eq!(message_texts(&changes), vec!["live again"]);
    drop(feed);
    let mut same = server
        .events(&format!("/users/{bob}/events"), Some(resumed))
        .await;
    assert_eq!(next_event(&mut same).await.event, "hello");
    let (changes, _) = changes_until(&mut same, |seen| !of_type(seen, "message").is_empty()).await;
    assert_eq!(message_texts(&changes), vec!["live again"]);
    let mut ahead = server
        .events(&format!("/users/{bob}/events?since=1000000"), None)
        .await;
    let reset = next_event(&mut ahead).await;
    assert_eq!(reset.event, "reset");
    let mut room_feed = server
        .events(&format!("/rooms/{room}/events"), Some(last))
        .await;
    assert_eq!(next_event(&mut room_feed).await.event, "hello");
    let catch_up = next_event(&mut room_feed).await;
    let data: Value = serde_json::from_str(&catch_up.data).unwrap();
    let changes = data["changes"].as_array().unwrap();
    assert_eq!(message_texts(changes).len(), 5);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pruned_version_resets_the_feed() {
    let dir = TempDir::new().unwrap();
    let config = Config {
        keep_versions: Some(3),
        ..buffered()
    };
    let server = Server::start(&dir.path().join("chat.ferro"), config).await;
    let mut client = server.client().await;
    let ann = make_user(&mut client, "ann").await;
    let room = make_room(&mut client, ann, "general").await;
    let (_, early) = post(&mut client, ann, room, "early").await;
    for n in 0..10 {
        post(&mut client, ann, room, &format!("later {n}")).await;
    }
    let mut feed = server
        .events(&format!("/users/{ann}/events"), Some(early))
        .await;
    let reset = next_event(&mut feed).await;
    assert_eq!(reset.event, "reset");
    let data: Value = serde_json::from_str(&reset.data).unwrap();
    assert_eq!(data["rooms"][0]["last_seq"], json!(11));
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_and_feeds_survive_a_restart() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("chat.ferro");
    let server = Server::start(&path, Config::default()).await;
    let mut client = server.client().await;
    let ann = make_user(&mut client, "ann").await;
    let bob = make_user(&mut client, "bob").await;
    let room = make_room(&mut client, ann, "general").await;
    join(&mut client, bob, room).await;
    let mut feed = server.events(&format!("/users/{bob}/events"), None).await;
    let hello = next_event(&mut feed).await;
    let mut last: u64 = hello.id.unwrap().parse().unwrap();
    for n in 1..=6 {
        post(&mut client, ann, room, &format!("before restart {n}")).await;
    }
    let (_, id) = changes_until(&mut feed, |seen| of_type(seen, "message").len() >= 6).await;
    last = id.unwrap_or(last);
    client
        .act(bob, "POST", &format!("/rooms/{room}/read?seq=2"), None)
        .await
        .unwrap();
    let before = get_json(&mut client, &format!("/rooms/{room}/messages")).await;
    drop(client);
    server.shutdown().await;
    drop(feed);
    let server = Server::start(&path, Config::default()).await;
    let mut client = server.client().await;
    let after = get_json(&mut client, &format!("/rooms/{room}/messages")).await;
    assert_eq!(before["messages"], after["messages"]);
    assert_eq!(after["messages"].as_array().unwrap().len(), 6);
    assert_eq!(unread_of(&mut client, bob, room).await, 4);
    post(&mut client, ann, room, "after restart").await;
    let mut feed = server
        .events(&format!("/users/{bob}/events"), Some(last))
        .await;
    assert_eq!(next_event(&mut feed).await.event, "hello");
    let catch_up = next_event(&mut feed).await;
    let data: Value = serde_json::from_str(&catch_up.data).unwrap();
    let changes = data["changes"].as_array().unwrap();
    assert_eq!(message_texts(changes), vec!["after restart"]);
    let receipt = of_type(changes, "read")
        .into_iter()
        .find(|change| change["user"] == json!(bob))
        .unwrap();
    assert_eq!(receipt["seq"], json!(2));
    assert_eq!(of_type(changes, "unread")[0]["count"], json!(5));
    let found = get_json(&mut client, &format!("/rooms/{room}/search?q=restart")).await;
    assert_eq!(found["messages"].as_array().unwrap().len(), 7);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeded_rooms_count_unread_like_a_scan() {
    let dir = TempDir::new().unwrap();
    let server = Server::start(&dir.path().join("chat.ferro"), buffered()).await;
    let mut client = server.client().await;
    let plan = json!({
        "users": (0..20).map(|n| format!("seed{n}")).collect::<Vec<_>>(),
        "rooms": [
            {"name": "first", "members": (0..10).collect::<Vec<_>>()},
            {"name": "second", "members": (5..20).collect::<Vec<_>>()},
        ],
        "messages_per_room": 40,
    });
    let seeded = client.json("POST", "/admin/seed", &plan).await.unwrap();
    assert_eq!(seeded.status, 201, "{}", body(&seeded));
    let seeded = seeded.json();
    let first_user = seeded["first_user"].as_u64().unwrap();
    let room = seeded["first_room"].as_u64().unwrap();
    let view = get_json(&mut client, &format!("/rooms/{room}")).await;
    assert_eq!(view["messages"], json!(40));
    assert_eq!(view["members"], json!(10));
    assert_eq!(unread_of(&mut client, first_user, room).await, 0);
    for seq in [3, 17, 40] {
        let message = get_json(&mut client, &format!("/rooms/{room}/messages/{seq}")).await;
        let author = message["author"].as_u64().unwrap();
        let deleted = client
            .act(
                author,
                "DELETE",
                &format!("/rooms/{room}/messages/{seq}"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(deleted.status, 200, "{}", body(&deleted));
    }
    let late = make_user(&mut client, "late").await;
    join(&mut client, late, room).await;
    for n in 0..5 {
        post(&mut client, first_user, room, &format!("fresh {n}")).await;
    }
    assert_eq!(unread_of(&mut client, late, room).await, 5);
    client
        .act(late, "POST", &format!("/rooms/{room}/read?seq=42"), None)
        .await
        .unwrap();
    let mut live = Vec::new();
    let mut before = u64::MAX;
    loop {
        let page = get_json(
            &mut client,
            &format!("/rooms/{room}/messages?limit=7&before={before}"),
        )
        .await;
        for message in page["messages"].as_array().unwrap() {
            if message["deleted"] == json!(false) {
                live.push(message["seq"].as_u64().unwrap());
            }
        }
        match page["next"].as_u64() {
            Some(next) => before = next,
            None => break,
        }
    }
    assert_eq!(live.len(), 42);
    for member in [first_user, first_user + 4, late] {
        let read = get_json(&mut client, &format!("/users/{member}/unread")).await["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["room"] == json!(room))
            .and_then(|entry| entry["read"].as_u64())
            .unwrap();
        let scanned = live.iter().filter(|&&seq| seq > read).count() as u64;
        assert_eq!(
            unread_of(&mut client, member, room).await,
            scanned,
            "member {member}"
        );
    }
    assert_eq!(unread_of(&mut client, late, room).await, 3);
    let view = get_json(&mut client, &format!("/rooms/{room}")).await;
    assert_eq!(view["messages"], json!(42));
    let found = get_json(
        &mut client,
        &format!("/rooms/{room}/search?q=number&limit=500"),
    )
    .await;
    assert_eq!(found["messages"].as_array().unwrap().len(), 37);
    server.shutdown().await;
}
