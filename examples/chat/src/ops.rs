use std::collections::BTreeSet;

use ferro::{Map, Set, Val};
use serde::Deserialize;

use crate::error::ApiError;
use crate::model::{Chat, Member, Message, Room, User, terms};

pub const MAX_NAME: usize = 64;
pub const MAX_TEXT: usize = 4000;
pub const MAX_EMOJI: usize = 16;

fn no_room() -> ApiError {
    ApiError::NotFound("no such room".into())
}

fn no_message() -> ApiError {
    ApiError::NotFound("no such message".into())
}

fn not_member() -> ApiError {
    ApiError::Forbidden("not a member of this room".into())
}

fn check_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(ApiError::Bad(format!(
            "name must be 1 to {MAX_NAME} characters"
        )));
    }
    Ok(name.to_string())
}

fn check_text(text: &str) -> Result<String, ApiError> {
    if text.trim().is_empty() || text.chars().count() > MAX_TEXT {
        return Err(ApiError::Bad(format!(
            "text must be 1 to {MAX_TEXT} characters"
        )));
    }
    Ok(text.to_string())
}

fn check_emoji(emoji: &str) -> Result<(), ApiError> {
    if emoji.is_empty() || emoji.chars().count() > MAX_EMOJI || emoji.contains(char::is_whitespace)
    {
        return Err(ApiError::Bad(format!(
            "a reaction is 1 to {MAX_EMOJI} characters with no spaces"
        )));
    }
    Ok(())
}

pub fn check_user(chat: &Chat, user: u64) -> Result<(), ApiError> {
    if chat.users.contains(&user) {
        Ok(())
    } else {
        Err(ApiError::NotFound(format!("no user {user}")))
    }
}

pub fn room_of(chat: &Chat, room: u64) -> Result<Val<Room>, ApiError> {
    chat.rooms.get(&room).ok_or_else(no_room)
}

fn member_of(room: &Room, user: u64) -> Result<Member, ApiError> {
    room.members
        .get(&user)
        .map(|member| *member)
        .ok_or_else(not_member)
}

fn live_message(room: &Room, seq: u64) -> Result<Message, ApiError> {
    let message = room.messages.get(&seq).ok_or_else(no_message)?;
    if message.deleted {
        return Err(no_message());
    }
    Ok((*message).clone())
}

fn edit_room(chat: &mut Chat, room: u64, f: impl FnOnce(&mut Room)) -> Result<(), ApiError> {
    chat.rooms.update(&room, f).ok_or_else(no_room)
}

pub fn create_user(chat: &mut Chat, name: &str, now: u64) -> Result<u64, ApiError> {
    let name = check_name(name)?;
    let id = chat.next_user + 1;
    let user = User {
        id,
        name,
        created_ms: now,
        rooms: BTreeSet::new(),
    };
    chat.users
        .try_insert(user)
        .map_err(|_| ApiError::Conflict("user name taken".into()))?;
    chat.next_user = id;
    Ok(id)
}

pub fn create_room(chat: &mut Chat, user: u64, name: &str, now: u64) -> Result<u64, ApiError> {
    check_user(chat, user)?;
    let name = check_name(name)?;
    let id = chat.next_room + 1;
    let mut members = Map::new();
    members.insert(
        user,
        Member {
            joined_ms: now,
            read: 0,
        },
    );
    let room = Room {
        id,
        name,
        created_by: user,
        created_ms: now,
        last_seq: 0,
        members,
        messages: Map::default(),
        terms: Set::new(),
    };
    chat.rooms
        .try_insert(room)
        .map_err(|_| ApiError::Conflict("room name taken".into()))?;
    chat.next_room = id;
    chat.users.update(&user, |row| {
        row.rooms.insert(id);
    });
    Ok(id)
}

pub fn join(chat: &mut Chat, room: u64, user: u64, now: u64) -> Result<bool, ApiError> {
    check_user(chat, user)?;
    let current = room_of(chat, room)?;
    if current.members.contains_key(&user) {
        return Ok(false);
    }
    let read = current.last_seq;
    edit_room(chat, room, |row| {
        row.members.insert(
            user,
            Member {
                joined_ms: now,
                read,
            },
        );
    })?;
    chat.users.update(&user, |row| {
        row.rooms.insert(room);
    });
    Ok(true)
}

pub fn leave(chat: &mut Chat, room: u64, user: u64) -> Result<bool, ApiError> {
    check_user(chat, user)?;
    if !room_of(chat, room)?.members.contains_key(&user) {
        return Ok(false);
    }
    edit_room(chat, room, |row| {
        row.members.remove(&user);
    })?;
    chat.users.update(&user, |row| {
        row.rooms.remove(&room);
    });
    Ok(true)
}

pub fn post(chat: &mut Chat, room: u64, user: u64, text: &str, now: u64) -> Result<u64, ApiError> {
    let text = check_text(text)?;
    let current = room_of(chat, room)?;
    let member = member_of(&current, user)?;
    let seq = current.last_seq + 1;
    edit_room(chat, room, |row| {
        row.last_seq = seq;
        row.terms
            .extend(terms(&text).into_iter().map(|term| (term, seq)));
        row.messages.insert(
            seq,
            Message {
                author: user,
                text,
                sent_ms: now,
                ..Message::default()
            },
        );
        row.members.insert(
            user,
            Member {
                read: seq,
                ..member
            },
        );
    })?;
    Ok(seq)
}

pub fn edit(
    chat: &mut Chat,
    room: u64,
    seq: u64,
    user: u64,
    text: &str,
    now: u64,
) -> Result<bool, ApiError> {
    let text = check_text(text)?;
    let current = room_of(chat, room)?;
    let message = live_message(&current, seq)?;
    if message.author != user {
        return Err(ApiError::Forbidden(
            "only the author can edit a message".into(),
        ));
    }
    if message.text == text {
        return Ok(false);
    }
    let before = terms(&message.text);
    let after = terms(&text);
    edit_room(chat, room, |row| {
        for gone in before.difference(&after) {
            row.terms.remove(&(gone.clone(), seq));
        }
        row.terms
            .extend(after.difference(&before).map(|term| (term.clone(), seq)));
        row.messages.insert(
            seq,
            Message {
                text,
                edited_ms: Some(now),
                ..message
            },
        );
    })?;
    Ok(true)
}

pub fn delete(chat: &mut Chat, room: u64, seq: u64, user: u64) -> Result<bool, ApiError> {
    let current = room_of(chat, room)?;
    let message = current.messages.get(&seq).ok_or_else(no_message)?;
    if message.author != user {
        return Err(ApiError::Forbidden(
            "only the author can delete a message".into(),
        ));
    }
    if message.deleted {
        return Ok(false);
    }
    let gone = terms(&message.text);
    let author = message.author;
    let sent_ms = message.sent_ms;
    let edited_ms = message.edited_ms;
    edit_room(chat, room, |row| {
        for term in gone {
            row.terms.remove(&(term, seq));
        }
        row.messages.insert(
            seq,
            Message {
                author,
                sent_ms,
                edited_ms,
                deleted: true,
                ..Message::default()
            },
        );
    })?;
    Ok(true)
}

pub fn react(
    chat: &mut Chat,
    room: u64,
    seq: u64,
    user: u64,
    emoji: &str,
    on: bool,
) -> Result<bool, ApiError> {
    check_emoji(emoji)?;
    let current = room_of(chat, room)?;
    member_of(&current, user)?;
    let mut message = live_message(&current, seq)?;
    let present = message
        .reactions
        .get(emoji)
        .is_some_and(|users| users.contains(&user));
    if present == on {
        return Ok(false);
    }
    if on {
        message
            .reactions
            .entry(emoji.to_string())
            .or_default()
            .insert(user);
    } else if let Some(users) = message.reactions.get_mut(emoji) {
        users.remove(&user);
        if users.is_empty() {
            message.reactions.remove(emoji);
        }
    }
    edit_room(chat, room, |row| {
        row.messages.insert(seq, message);
    })?;
    Ok(true)
}

pub fn mark_read(
    chat: &mut Chat,
    room: u64,
    user: u64,
    upto: Option<u64>,
) -> Result<u64, ApiError> {
    let current = room_of(chat, room)?;
    let member = member_of(&current, user)?;
    let target = upto.unwrap_or(current.last_seq).min(current.last_seq);
    if target <= member.read {
        return Ok(member.read);
    }
    edit_room(chat, room, |row| {
        row.members.insert(
            user,
            Member {
                read: target,
                ..member
            },
        );
    })?;
    Ok(target)
}

#[derive(Debug, Clone, Deserialize)]
pub struct SeedRoom {
    pub name: String,
    pub members: Vec<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SeedPlan {
    pub users: Vec<String>,
    pub rooms: Vec<SeedRoom>,
    #[serde(default)]
    pub messages_per_room: u64,
}

pub struct Seeded {
    pub first_user: u64,
    pub first_room: u64,
}

const WORDS: [&str; 16] = [
    "hello", "deploy", "lunch", "review", "ferro", "merge", "coffee", "ticket", "release", "build",
    "standup", "design", "bug", "thanks", "meeting", "later",
];

pub fn seed(chat: &mut Chat, plan: &SeedPlan, now: u64) -> Result<Seeded, ApiError> {
    let first_user = chat.next_user + 1;
    let first_room = chat.next_room + 1;
    let user_id = |index: usize| first_user + index as u64;
    let mut rooms_of: Vec<BTreeSet<u64>> = vec![BTreeSet::new(); plan.users.len()];
    for (offset, room) in plan.rooms.iter().enumerate() {
        let id = first_room + offset as u64;
        for &member in &room.members {
            let slot = rooms_of
                .get_mut(member)
                .ok_or_else(|| ApiError::Bad(format!("no seed user {member}")))?;
            slot.insert(id);
        }
    }
    for (index, name) in plan.users.iter().enumerate() {
        let user = User {
            id: user_id(index),
            name: check_name(name)?,
            created_ms: now,
            rooms: std::mem::take(&mut rooms_of[index]),
        };
        chat.users
            .try_insert(user)
            .map_err(|_| ApiError::Conflict(format!("user name {name} taken")))?;
    }
    chat.next_user += plan.users.len() as u64;
    for (offset, seed) in plan.rooms.iter().enumerate() {
        let id = first_room + offset as u64;
        let last_seq = if seed.members.is_empty() {
            0
        } else {
            plan.messages_per_room
        };
        let members: Map<u64, Member> = seed
            .members
            .iter()
            .map(|&member| {
                (
                    user_id(member),
                    Member {
                        joined_ms: now,
                        read: last_seq,
                    },
                )
            })
            .collect();
        let mut messages: Map<u64, Message, _> = Map::default();
        let mut index: Set<(String, u64)> = Set::new();
        let mut written = Vec::new();
        let mut postings = Vec::new();
        for seq in 1..=last_seq {
            let author = user_id(seed.members[(seq as usize * 7 + offset) % seed.members.len()]);
            let text = format!(
                "{} {} {} number {seq}",
                WORDS[(seq as usize + offset) % WORDS.len()],
                WORDS[(seq as usize * 3 + offset) % WORDS.len()],
                WORDS[(seq as usize * 5 + 1) % WORDS.len()],
            );
            postings.extend(terms(&text).into_iter().map(|term| (term, seq)));
            written.push((
                seq,
                Message {
                    author,
                    text,
                    sent_ms: now,
                    ..Message::default()
                },
            ));
        }
        messages.extend(written);
        index.extend(postings);
        let room = Room {
            id,
            name: check_name(&seed.name)?,
            created_by: seed.members.first().map_or(0, |&member| user_id(member)),
            created_ms: now,
            last_seq,
            members,
            messages,
            terms: index,
        };
        chat.rooms
            .try_insert(room)
            .map_err(|_| ApiError::Conflict(format!("room name {} taken", seed.name)))?;
    }
    chat.next_room += plan.rooms.len() as u64;
    Ok(Seeded {
        first_user,
        first_room,
    })
}
