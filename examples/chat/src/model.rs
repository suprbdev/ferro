use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use ferro::{Data, Map, Record, Set, Summary, Table};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct User {
    #[key]
    pub id: u64,
    #[unique]
    pub name: String,
    pub created_ms: u64,
    pub rooms: BTreeSet<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Data)]
pub struct Member {
    pub joined_ms: u64,
    pub read: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
pub struct Message {
    pub author: u64,
    pub text: String,
    pub sent_ms: u64,
    pub edited_ms: Option<u64>,
    pub deleted: bool,
    pub reactions: BTreeMap<String, BTreeSet<u64>>,
}

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

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

pub fn terms(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| (2..=32).contains(&word.chars().count()))
        .map(str::to_lowercase)
        .collect()
}

pub fn reactions_view(reactions: &BTreeMap<String, BTreeSet<u64>>) -> Value {
    Value::Object(
        reactions
            .iter()
            .map(|(emoji, users)| (emoji.clone(), json!(users)))
            .collect(),
    )
}

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

pub fn user_view(user: &User) -> Value {
    json!({
        "id": user.id,
        "name": user.name,
        "created_ms": user.created_ms,
        "rooms": user.rooms,
    })
}
