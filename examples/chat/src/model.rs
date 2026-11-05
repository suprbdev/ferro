use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use ferro::{Data, Map, Record, Set, Summary, Table};
use serde_json::{Value, json};

/// A user, unique by name, with the rooms they belong to.
#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct User {
    #[key]
    pub id: u64,
    #[unique]
    pub name: String,
    pub created_ms: u64,
    pub rooms: BTreeSet<u64>,
}

/// A user's membership of a room: when they joined and the last message they have read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Data)]
pub struct Member {
    pub joined_ms: u64,
    pub read: u64,
}

/// A message in a room, with its edit time, its reactions, and whether its author deleted it.
#[derive(Clone, Debug, Default, PartialEq, Data)]
pub struct Message {
    pub author: u64,
    pub text: String,
    pub sent_ms: u64,
    pub edited_ms: Option<u64>,
    pub deleted: bool,
    pub reactions: BTreeMap<String, BTreeSet<u64>>,
}

/// The summary of a room's messages: how many of them are not deleted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Data)]
#[ferro(transparent)]
pub struct Live(pub u64);

impl Summary<u64, Message> for Live {
    fn of(_seq: &u64, message: &Message) -> Self {
        Live(u64::from(!message.deleted))
    }

    fn combine(&self, other: &Self) -> Self {
        Live(self.0 + other.0)
    }
}

/// A room, unique by name, holding its members, its messages by sequence number, and the
/// `(term, seq)` pairs that index its messages for search.
#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct Room {
    #[key]
    pub id: u64,
    #[unique]
    pub name: String,
    pub created_by: u64,
    pub created_ms: u64,
    pub last_seq: u64,
    pub members: Map<u64, Member>,
    pub messages: Map<u64, Message, Live>,
    pub terms: Set<(String, u64)>,
}

/// The database root: every user and room, and the next id of each.
#[derive(Clone, Default, Data)]
pub struct Chat {
    pub users: Table<User>,
    pub rooms: Table<Room>,
    pub next_user: u64,
    pub next_room: u64,
}

impl Room {
    pub fn unread(&self, read: u64) -> u64 {
        self.messages
            .range_summary(read.saturating_add(1)..)
            .map_or(0, |live| live.0)
    }

    pub fn live(&self) -> u64 {
        self.messages.summary().map_or(0, |live| live.0)
    }

    pub fn unread_for(&self, user: u64) -> Option<u64> {
        self.members
            .get(&user)
            .map(|member| self.unread(member.read))
    }
}

/// The current time in Unix milliseconds, or 0 if the clock is before 1970.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// The lowercased words of 2 to 32 characters in `text`, as the search index keeps them.
pub fn terms(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| (2..=32).contains(&word.chars().count()))
        .map(str::to_lowercase)
        .collect()
}

/// The JSON for a message's reactions: each emoji with the users who chose it.
pub fn reactions_view(reactions: &BTreeMap<String, BTreeSet<u64>>) -> Value {
    Value::Object(
        reactions
            .iter()
            .map(|(emoji, users)| (emoji.clone(), json!(users)))
            .collect(),
    )
}

/// The JSON for one message; a deleted message keeps only its place, author and time.
pub fn message_view(room: u64, seq: u64, message: &Message) -> Value {
    if message.deleted {
        return json!({
            "room": room,
            "seq": seq,
            "author": message.author,
            "sent_ms": message.sent_ms,
            "deleted": true,
        });
    }
    json!({
        "room": room,
        "seq": seq,
        "author": message.author,
        "text": message.text,
        "sent_ms": message.sent_ms,
        "edited_ms": message.edited_ms,
        "deleted": false,
        "reactions": reactions_view(&message.reactions),
    })
}

/// The JSON for a room, with its member count and the number of messages not deleted.
pub fn room_view(room: &Room) -> Value {
    json!({
        "id": room.id,
        "name": room.name,
        "created_by": room.created_by,
        "created_ms": room.created_ms,
        "last_seq": room.last_seq,
        "members": room.members.len(),
        "messages": room.live(),
    })
}

/// The JSON for a user and the rooms they belong to.
pub fn user_view(user: &User) -> Value {
    json!({
        "id": user.id,
        "name": user.name,
        "created_ms": user.created_ms,
        "rooms": user.rooms,
    })
}
