use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ferro::{Change, Closer, Db, Snapshot, Val, Watcher};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::error::ApiError;
use crate::model::{Chat, Room, User, message_view, reactions_view, room_view};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Room(u64),
    User(u64),
}

#[derive(Default)]
struct Marks {
    user: Option<Val<User>>,
    rooms: Vec<(u64, Option<Val<Room>>)>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Scope {
    fn viewer(self) -> Option<u64> {
        match self {
            Scope::User(user) => Some(user),
            Scope::Room(_) => None,
        }
    }

    fn rooms(self, chat: &Chat) -> BTreeSet<u64> {
        match self {
            Scope::Room(room) => BTreeSet::from([room]),
            Scope::User(user) => chat
                .users
                .get(&user)
                .map(|row| row.rooms.clone())
                .unwrap_or_default(),
        }
    }

    fn exists(self, chat: &Chat) -> bool {
        match self {
            Scope::Room(room) => chat.rooms.contains(&room),
            Scope::User(user) => chat.users.contains(&user),
        }
    }

    fn marks(self, chat: &Chat) -> Marks {
        let user = self.viewer().and_then(|user| chat.users.get(&user));
        let rooms = self
            .rooms(chat)
            .into_iter()
            .map(|room| (room, chat.rooms.get(&room)))
            .collect();
        Marks { user, rooms }
    }

    fn moved(self, marks: &Marks, chat: &Chat) -> bool {
        if let Some(user) = self.viewer()
            && chat.users.get(&user) != marks.user
        {
            return true;
        }
        marks
            .rooms
            .iter()
            .any(|(room, seen)| chat.rooms.get(room) != *seen)
    }
}

fn room_entry(room: &Room, viewer: Option<u64>) -> Value {
    let mut view = room_view(room);
    if let Some(user) = viewer {
        view["unread"] = json!(room.unread_for(user));
        view["read"] = json!(room.members.get(&user).map(|member| member.read));
    }
    view
}

fn room_changes(
    room: u64,
    was: &Room,
    now: &Room,
    viewer: Option<u64>,
    limit: usize,
    out: &mut Vec<Value>,
) -> bool {
    for change in was.messages.diff(&now.messages) {
        if out.len() >= limit {
            return true;
        }
        match change {
            Change::Added(seq, message) => {
                if !message.deleted {
                    out.push(
                        json!({"type": "message", "message": message_view(room, seq, &message)}),
                    );
                }
            }
            Change::Removed(seq, _) => {
                out.push(json!({"type": "deleted", "room": room, "seq": seq}));
            }
            Change::Changed(seq, before, after) => {
                if after.deleted {
                    if !before.deleted {
                        out.push(json!({"type": "deleted", "room": room, "seq": seq}));
                    }
                    continue;
                }
                if after.text != before.text || after.edited_ms != before.edited_ms {
                    out.push(json!({
                        "type": "edited",
                        "room": room,
                        "seq": seq,
                        "text": after.text,
                        "edited_ms": after.edited_ms,
                    }));
                }
                if after.reactions != before.reactions {
                    out.push(json!({
                        "type": "reactions",
                        "room": room,
                        "seq": seq,
                        "reactions": reactions_view(&after.reactions),
                    }));
                }
            }
        }
    }
    for change in was.members.diff(&now.members) {
        if out.len() >= limit {
            return true;
        }
        match change {
            Change::Added(user, _) => {
                out.push(json!({"type": "joined", "room": room, "user": user}))
            }
            Change::Removed(user, _) => {
                out.push(json!({"type": "left", "room": room, "user": user}))
            }
            Change::Changed(user, before, after) => {
                if after.read != before.read {
                    out.push(
                        json!({"type": "read", "room": room, "user": user, "seq": after.read}),
                    );
                }
            }
        }
    }
    if let Some(user) = viewer {
        let count = now.unread_for(user);
        if count != was.unread_for(user)
            && let Some(count) = count
        {
            out.push(
                json!({"type": "unread", "room": room, "count": count, "last_seq": now.last_seq}),
            );
        }
    }
    false
}

pub fn changes(scope: Scope, old: &Chat, new: &Chat, limit: usize) -> (Vec<Value>, bool) {
    let viewer = scope.viewer();
    let before = scope.rooms(old);
    let after = scope.rooms(new);
    let mut out = Vec::new();
    for &room in before.union(&after) {
        if out.len() >= limit {
            return (out, true);
        }
        if !after.contains(&room) {
            out.push(json!({"type": "left_room", "room": room}));
            continue;
        }
        let Some(now) = new.rooms.get(&room) else {
            continue;
        };
        let was = old.rooms.get(&room);
        let Some(was) = was.filter(|_| before.contains(&room)) else {
            out.push(json!({"type": "joined_room", "room": room_entry(&now, viewer)}));
            continue;
        };
        if *was == *now {
            continue;
        }
        if room_changes(room, &was, &now, viewer, limit, &mut out) {
            return (out, true);
        }
    }
    (out, false)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub event: &'static str,
    pub id: Option<u64>,
    pub data: String,
}

fn state(scope: Scope, chat: &Snapshot<Chat>) -> Value {
    match scope {
        Scope::Room(room) => json!({
            "version": chat.version(),
            "room": chat.rooms.get(&room).map(|room| room_entry(&room, None)),
        }),
        Scope::User(user) => {
            let rooms: Vec<Value> = scope
                .rooms(chat)
                .into_iter()
                .filter_map(|room| chat.rooms.get(&room))
                .map(|room| room_entry(&room, Some(user)))
                .collect();
            json!({"version": chat.version(), "user": user, "rooms": rooms})
        }
    }
}

fn change_frame(from: u64, version: u64, changes: Vec<Value>, more: bool) -> Frame {
    Frame {
        event: "changes",
        id: Some(version),
        data: json!({"from": from, "version": version, "more": more, "changes": changes})
            .to_string(),
    }
}

enum Start {
    Fresh,
    Resume(u64, Option<Snapshot<Chat>>),
    Reset(String),
}

pub struct Feeds {
    next: AtomicU64,
    live: Mutex<HashMap<u64, Closer<Chat>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    pub frames: AtomicU64,
    pub limit: usize,
    pub buffer: usize,
}

impl Feeds {
    pub fn new(limit: usize, buffer: usize) -> Self {
        Feeds {
            next: AtomicU64::new(1),
            live: Mutex::new(HashMap::new()),
            threads: Mutex::new(Vec::new()),
            frames: AtomicU64::new(0),
            limit: limit.max(1),
            buffer: buffer.max(1),
        }
    }

    pub fn live(&self) -> usize {
        lock(&self.live).len()
    }

    pub fn close_all(&self) {
        let closers: Vec<Closer<Chat>> =
            lock(&self.live).drain().map(|(_, closer)| closer).collect();
        for closer in closers {
            closer.close();
        }
        let mut threads = std::mem::take(&mut *lock(&self.threads));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !threads.is_empty() && Instant::now() < deadline {
            let (done, running) = threads.into_iter().partition(JoinHandle::is_finished);
            threads = running;
            for thread in done {
                let _ = thread.join();
            }
            if !threads.is_empty() {
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

pub struct Subscription {
    pub frames: mpsc::Receiver<Frame>,
    guard: Guard,
}

impl Subscription {
    pub fn id(&self) -> u64 {
        self.guard.id
    }
}

struct Guard {
    id: u64,
    closer: Closer<Chat>,
    feeds: Arc<Feeds>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.closer.close();
        lock(&self.feeds.live).remove(&self.id);
    }
}

pub fn subscribe(
    feeds: &Arc<Feeds>,
    db: &Db<Chat>,
    scope: Scope,
    since: Option<u64>,
) -> Result<Subscription, ApiError> {
    if !scope.exists(&db.read()) {
        return Err(ApiError::NotFound(match scope {
            Scope::Room(room) => format!("no room {room}"),
            Scope::User(user) => format!("no user {user}"),
        }));
    }
    let marks = Arc::new(Mutex::new(Marks::default()));
    let watcher = db.watch_when({
        let marks = marks.clone();
        move |snapshot| scope.moved(&lock(&marks), snapshot)
    });
    let current = {
        let mut held = lock(&marks);
        let current = db.read();
        *held = scope.marks(&current);
        current
    };
    let start = match since {
        None => Start::Fresh,
        Some(version) if version == current.version() => Start::Resume(version, None),
        Some(version) if version > current.version() => Start::Reset(format!(
            "version {version} is ahead of {}",
            current.version()
        )),
        Some(version) => match db.at(version) {
            Ok(old) => Start::Resume(version, Some(old)),
            Err(ferro::Error::NoVersion(_)) => {
                Start::Reset(format!("version {version} is no longer retained"))
            }
            Err(error) => return Err(error.into()),
        },
    };
    let (sender, frames) = mpsc::channel(feeds.buffer);
    let id = feeds.next.fetch_add(1, Ordering::Relaxed);
    let closer = watcher.closer();
    lock(&feeds.live).insert(id, closer.clone());
    let worker = Worker {
        scope,
        watcher,
        marks,
        seen: current,
        sender,
        feeds: feeds.clone(),
    };
    let spawned = thread::Builder::new()
        .name(format!("chat-feed-{id}"))
        .stack_size(256 << 10)
        .spawn(move || worker.run(start));
    let guard = Guard {
        id,
        closer,
        feeds: feeds.clone(),
    };
    let handle = spawned.map_err(|error| ApiError::Internal(format!("spawn feed: {error}")))?;
    let mut threads = lock(&feeds.threads);
    threads.retain(|thread| !thread.is_finished());
    threads.push(handle);
    drop(threads);
    Ok(Subscription { frames, guard })
}

struct Worker {
    scope: Scope,
    watcher: Watcher<Chat>,
    marks: Arc<Mutex<Marks>>,
    seen: Snapshot<Chat>,
    sender: mpsc::Sender<Frame>,
    feeds: Arc<Feeds>,
}

impl Worker {
    fn send(&self, frame: Frame) -> bool {
        self.feeds.frames.fetch_add(1, Ordering::Relaxed);
        self.sender.blocking_send(frame).is_ok()
    }

    fn opening(&self, start: Start) -> Vec<Frame> {
        let current = &self.seen;
        match start {
            Start::Fresh => vec![Frame {
                event: "hello",
                id: Some(current.version()),
                data: state(self.scope, current).to_string(),
            }],
            Start::Resume(from, old) => {
                let mut hello = state(self.scope, current);
                hello["resumed_from"] = json!(from);
                let mut frames = vec![Frame {
                    event: "hello",
                    id: None,
                    data: hello.to_string(),
                }];
                if let Some(old) = old {
                    let (list, more) = changes(self.scope, &old, current, self.feeds.limit);
                    if !list.is_empty() || more {
                        frames.push(change_frame(from, current.version(), list, more));
                    }
                }
                frames
            }
            Start::Reset(reason) => {
                let mut reset = state(self.scope, current);
                reset["reason"] = json!(reason);
                vec![Frame {
                    event: "reset",
                    id: Some(current.version()),
                    data: reset.to_string(),
                }]
            }
        }
    }

    fn run(mut self, start: Start) {
        for frame in self.opening(start) {
            if !self.send(frame) {
                return;
            }
        }
        while let Ok(snapshot) = self.watcher.recv() {
            if snapshot.version() <= self.seen.version() {
                continue;
            }
            let (list, more) = changes(self.scope, &self.seen, &snapshot, self.feeds.limit);
            *lock(&self.marks) = self.scope.marks(&snapshot);
            let from = self.seen.version();
            let version = snapshot.version();
            self.seen = snapshot;
            if (!list.is_empty() || more) && !self.send(change_frame(from, version, list, more)) {
                return;
            }
        }
    }
}
