use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ferro::{Change, Closer, Db, Snapshot, Watcher};
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::model::{Chat, Room, message_view, reactions_view, room_view};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Room(u64),
    User(u64),
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

    fn moved(self, seen: &Chat, now: &Chat) -> bool {
        match self {
            Scope::Room(room) => seen.rooms.get(&room) != now.rooms.get(&room),
            Scope::User(user) => {
                let row = seen.users.get(&user);
                row != now.users.get(&user)
                    || row.is_some_and(|row| {
                        row.rooms
                            .iter()
                            .any(|room| seen.rooms.get(room) != now.rooms.get(room))
                    })
            }
        }
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
    pub frames: AtomicU64,
    pub limit: usize,
}

impl Feeds {
    pub fn new(limit: usize) -> Self {
        Feeds {
            next: AtomicU64::new(1),
            live: Mutex::new(HashMap::new()),
            frames: AtomicU64::new(0),
            limit: limit.max(1),
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
    }
}

pub struct Subscription {
    id: u64,
    scope: Scope,
    watcher: Watcher<Chat>,
    opening: VecDeque<Frame>,
    feeds: Arc<Feeds>,
}

impl Subscription {
    pub fn id(&self) -> u64 {
        self.id
    }

    fn sent(&self, frame: Frame) -> Frame {
        self.feeds.frames.fetch_add(1, Ordering::Relaxed);
        frame
    }

    pub async fn next(&mut self) -> Option<Frame> {
        if let Some(frame) = self.opening.pop_front() {
            return Some(self.sent(frame));
        }
        loop {
            let seen = self.watcher.seen()?;
            let snapshot = self.watcher.changed().await.ok()?;
            if snapshot.version() <= seen.version() {
                continue;
            }
            let (list, more) = changes(self.scope, &seen, &snapshot, self.feeds.limit);
            if !list.is_empty() || more {
                let frame = change_frame(seen.version(), snapshot.version(), list, more);
                return Some(self.sent(frame));
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        lock(&self.feeds.live).remove(&self.id);
    }
}

fn opening(scope: Scope, current: &Snapshot<Chat>, start: Start, limit: usize) -> Vec<Frame> {
    match start {
        Start::Fresh => vec![Frame {
            event: "hello",
            id: Some(current.version()),
            data: state(scope, current).to_string(),
        }],
        Start::Resume(from, old) => {
            let mut hello = state(scope, current);
            hello["resumed_from"] = json!(from);
            let mut frames = vec![Frame {
                event: "hello",
                id: None,
                data: hello.to_string(),
            }];
            if let Some(old) = old {
                let (list, more) = changes(scope, &old, current, limit);
                if !list.is_empty() || more {
                    frames.push(change_frame(from, current.version(), list, more));
                }
            }
            frames
        }
        Start::Reset(reason) => {
            let mut reset = state(scope, current);
            reset["reason"] = json!(reason);
            vec![Frame {
                event: "reset",
                id: Some(current.version()),
                data: reset.to_string(),
            }]
        }
    }
}

pub fn subscribe(
    feeds: &Arc<Feeds>,
    db: &Db<Chat>,
    scope: Scope,
    since: Option<u64>,
) -> Result<Subscription, ApiError> {
    let watcher = db.watch_changes(move |seen, now| scope.moved(seen, now));
    let current = watcher
        .seen()
        .ok_or_else(|| ApiError::Internal("the database is closing".into()))?;
    if !scope.exists(&current) {
        return Err(ApiError::NotFound(match scope {
            Scope::Room(room) => format!("no room {room}"),
            Scope::User(user) => format!("no user {user}"),
        }));
    }
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
    let opening = opening(scope, &current, start, feeds.limit).into();
    let id = feeds.next.fetch_add(1, Ordering::Relaxed);
    lock(&feeds.live).insert(id, watcher.closer());
    Ok(Subscription {
        id,
        scope,
        watcher,
        opening,
        feeds: feeds.clone(),
    })
}
