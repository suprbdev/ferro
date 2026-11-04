use std::convert::Infallible;
use std::ops::Bound;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::stream::{Stream, unfold};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Shared;
use crate::error::ApiError;
use crate::feed::{Scope, Subscription};
use crate::model::{Room, message_view, now_ms, room_view, terms, user_view};
use crate::ops::{self, SeedPlan, room_of};

pub struct Actor(pub u64);

impl<S: Send + Sync> FromRequestParts<S> for Actor {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        parts
            .headers
            .get("x-user")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok())
            .map(Actor)
            .ok_or(ApiError::Unauthorized)
    }
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

#[derive(Deserialize)]
struct TextBody {
    text: String,
}

#[derive(Deserialize, Default)]
struct Page {
    before: Option<u64>,
    after: Option<u64>,
    limit: Option<usize>,
    q: Option<String>,
    seq: Option<u64>,
    since: Option<u64>,
}

impl Page {
    fn limit(&self, default: usize) -> usize {
        self.limit.unwrap_or(default).clamp(1, 500)
    }
}

fn created(body: Value) -> Response {
    (StatusCode::CREATED, Json(body)).into_response()
}

async fn create_user(
    State(app): State<Shared>,
    Json(body): Json<NameBody>,
) -> Result<Response, ApiError> {
    let done = app
        .write(move |chat| {
            let id = ops::create_user(chat, &body.name, now_ms())?;
            Ok((format!("user {id}"), id))
        })
        .await?;
    let user = app
        .db
        .read()
        .users
        .get(&done.value)
        .map(|user| user_view(&user));
    Ok(created(
        json!({"id": done.value, "user": user, "version": done.version}),
    ))
}

async fn get_user(
    State(app): State<Shared>,
    Path(user): Path<u64>,
) -> Result<Json<Value>, ApiError> {
    let chat = app.db.read();
    let row = chat
        .users
        .get(&user)
        .ok_or_else(|| ApiError::NotFound(format!("no user {user}")))?;
    Ok(Json(user_view(&row)))
}

async fn unread(State(app): State<Shared>, Path(user): Path<u64>) -> Result<Json<Value>, ApiError> {
    let chat = app.db.read();
    let row = chat
        .users
        .get(&user)
        .ok_or_else(|| ApiError::NotFound(format!("no user {user}")))?;
    let mut total = 0;
    let mut rooms = Vec::new();
    for &id in &row.rooms {
        let Some(room) = chat.rooms.get(&id) else {
            continue;
        };
        let Some(member) = room.members.get(&user) else {
            continue;
        };
        let count = room.unread(member.read);
        total += count;
        rooms.push(json!({
            "room": id,
            "name": room.name,
            "unread": count,
            "read": member.read,
            "last_seq": room.last_seq,
        }));
    }
    Ok(Json(
        json!({"user": user, "version": chat.version(), "total": total, "rooms": rooms}),
    ))
}

async fn create_room(
    State(app): State<Shared>,
    Actor(user): Actor,
    Json(body): Json<NameBody>,
) -> Result<Response, ApiError> {
    let done = app
        .write(move |chat| {
            let id = ops::create_room(chat, user, &body.name, now_ms())?;
            Ok((format!("room {id}"), id))
        })
        .await?;
    let room = app
        .db
        .read()
        .rooms
        .get(&done.value)
        .map(|room| room_view(&room));
    Ok(created(
        json!({"id": done.value, "room": room, "version": done.version}),
    ))
}

async fn list_rooms(State(app): State<Shared>, Query(page): Query<Page>) -> Json<Value> {
    let chat = app.db.read();
    let limit = page.limit(50);
    let after = page.after.unwrap_or(0);
    let mut rows = chat.rooms.range((Bound::Excluded(after), Bound::Unbounded));
    let more = rows.len() > limit;
    let rooms: Vec<Value> = rows
        .by_ref()
        .take(limit)
        .map(|room| room_view(&room))
        .collect();
    let next = more
        .then(|| rooms.last().and_then(|room| room["id"].as_u64()))
        .flatten();
    Json(json!({"rooms": rooms, "next": next, "version": chat.version()}))
}

async fn get_room(
    State(app): State<Shared>,
    Path(room): Path<u64>,
) -> Result<Json<Value>, ApiError> {
    let chat = app.db.read();
    let row = room_of(&chat, room)?;
    Ok(Json(room_view(&row)))
}

async fn members(
    State(app): State<Shared>,
    Path(room): Path<u64>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let chat = app.db.read();
    let room = room_of(&chat, room)?;
    let limit = page.limit(200);
    let after = page.after.unwrap_or(0);
    let range = room
        .members
        .range((Bound::Excluded(after), Bound::Unbounded));
    let more = range.len() > limit;
    let members: Vec<Value> = range
        .take(limit)
        .map(|(user, member)| json!({"user": user, "read": member.read, "joined_ms": member.joined_ms}))
        .collect();
    let next = more
        .then(|| members.last().and_then(|member| member["user"].as_u64()))
        .flatten();
    Ok(Json(
        json!({"room": room.id, "members": members, "next": next}),
    ))
}

async fn join(
    State(app): State<Shared>,
    Actor(user): Actor,
    Path(room): Path<u64>,
) -> Result<Json<Value>, ApiError> {
    let done = app
        .write(move |chat| {
            let joined = ops::join(chat, room, user, now_ms())?;
            Ok((format!("join {room} {user}"), joined))
        })
        .await?;
    Ok(Json(json!({"joined": done.value, "version": done.version})))
}

async fn leave(
    State(app): State<Shared>,
    Actor(user): Actor,
    Path(room): Path<u64>,
) -> Result<Json<Value>, ApiError> {
    let done = app
        .write(move |chat| {
            let left = ops::leave(chat, room, user)?;
            Ok((format!("leave {room} {user}"), left))
        })
        .await?;
    Ok(Json(json!({"left": done.value, "version": done.version})))
}

async fn mark_read(
    State(app): State<Shared>,
    Actor(user): Actor,
    Path(room): Path<u64>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let done = app
        .write(move |chat| {
            let read = ops::mark_read(chat, room, user, page.seq)?;
            Ok((format!("read {room} {user} {read}"), read))
        })
        .await?;
    let chat = app.db.read();
    let unread = chat.rooms.get(&room).and_then(|room| room.unread_for(user));
    Ok(Json(
        json!({"read": done.value, "unread": unread, "version": done.version}),
    ))
}

async fn post_message(
    State(app): State<Shared>,
    Actor(user): Actor,
    Path(room): Path<u64>,
    Json(body): Json<TextBody>,
) -> Result<Response, ApiError> {
    let done = app
        .write(move |chat| {
            let seq = ops::post(chat, room, user, &body.text, now_ms())?;
            Ok((format!("post {room}/{seq}"), seq))
        })
        .await?;
    app.counters.posts.fetch_add(1, Ordering::Relaxed);
    Ok(created(
        json!({"room": room, "seq": done.value, "version": done.version}),
    ))
}

async fn history(
    State(app): State<Shared>,
    Path(room): Path<u64>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let chat = app.db.read();
    let row = room_of(&chat, room)?;
    let limit = page.limit(50);
    let before = page.before.unwrap_or(u64::MAX);
    let range = row.messages.range(..before);
    let more = range.len() > limit;
    let messages: Vec<Value> = range
        .rev()
        .take(limit)
        .map(|(seq, message)| message_view(room, seq, &message))
        .collect();
    let next = more
        .then(|| messages.last().and_then(|message| message["seq"].as_u64()))
        .flatten();
    Ok(Json(json!({
        "room": room,
        "version": chat.version(),
        "last_seq": row.last_seq,
        "messages": messages,
        "next": next,
    })))
}

async fn get_message(
    State(app): State<Shared>,
    Path((room, seq)): Path<(u64, u64)>,
) -> Result<Json<Value>, ApiError> {
    let chat = app.db.read();
    let row = room_of(&chat, room)?;
    let message = row
        .messages
        .get(&seq)
        .ok_or_else(|| ApiError::NotFound("no such message".into()))?;
    Ok(Json(message_view(room, seq, &message)))
}

async fn edit_message(
    State(app): State<Shared>,
    Actor(user): Actor,
    Path((room, seq)): Path<(u64, u64)>,
    Json(body): Json<TextBody>,
) -> Result<Json<Value>, ApiError> {
    let done = app
        .write(move |chat| {
            let edited = ops::edit(chat, room, seq, user, &body.text, now_ms())?;
            Ok((format!("edit {room}/{seq}"), edited))
        })
        .await?;
    Ok(Json(json!({"edited": done.value, "version": done.version})))
}

async fn delete_message(
    State(app): State<Shared>,
    Actor(user): Actor,
    Path((room, seq)): Path<(u64, u64)>,
) -> Result<Json<Value>, ApiError> {
    let done = app
        .write(move |chat| {
            let deleted = ops::delete(chat, room, seq, user)?;
            Ok((format!("delete {room}/{seq}"), deleted))
        })
        .await?;
    Ok(Json(
        json!({"deleted": done.value, "version": done.version}),
    ))
}

async fn react(
    app: Shared,
    user: u64,
    room: u64,
    seq: u64,
    emoji: String,
    on: bool,
) -> Result<Json<Value>, ApiError> {
    let done = app
        .write(move |chat| {
            let changed = ops::react(chat, room, seq, user, &emoji, on)?;
            Ok((format!("react {room}/{seq}"), changed))
        })
        .await?;
    Ok(Json(
        json!({"changed": done.value, "version": done.version}),
    ))
}

async fn add_reaction(
    State(app): State<Shared>,
    Actor(user): Actor,
    Path((room, seq, emoji)): Path<(u64, u64, String)>,
) -> Result<Json<Value>, ApiError> {
    react(app, user, room, seq, emoji, true).await
}

async fn remove_reaction(
    State(app): State<Shared>,
    Actor(user): Actor,
    Path((room, seq, emoji)): Path<(u64, u64, String)>,
) -> Result<Json<Value>, ApiError> {
    react(app, user, room, seq, emoji, false).await
}

fn search_room(
    room: &Room,
    query: &str,
    before: u64,
    limit: usize,
) -> Result<(Vec<Value>, Option<u64>), ApiError> {
    let words: Vec<String> = terms(query).into_iter().collect();
    if words.is_empty() {
        return Err(ApiError::Bad(
            "search needs a word of two or more letters".into(),
        ));
    }
    let postings = |word: &String| room.terms.range((word.clone(), 0)..(word.clone(), before));
    let Some(rarest) = words.iter().min_by_key(|word| postings(word).len()) else {
        return Ok((Vec::new(), None));
    };
    let mut hits = Vec::new();
    let mut next = None;
    for (_, seq) in postings(rarest).rev() {
        if !words
            .iter()
            .all(|word| word == rarest || room.terms.contains(&(word.clone(), seq)))
        {
            continue;
        }
        let Some(message) = room.messages.get(&seq) else {
            continue;
        };
        if hits.len() == limit {
            next = hits.last().and_then(|hit: &Value| hit["seq"].as_u64());
            break;
        }
        hits.push(message_view(room.id, seq, &message));
    }
    Ok((hits, next))
}

async fn search(
    State(app): State<Shared>,
    Path(room): Path<u64>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let chat = app.db.read();
    let row = room_of(&chat, room)?;
    let query = page.q.clone().unwrap_or_default();
    let (hits, next) = search_room(
        &row,
        &query,
        page.before.unwrap_or(u64::MAX),
        page.limit(20),
    )?;
    Ok(Json(
        json!({"room": room, "q": query, "messages": hits, "next": next}),
    ))
}

fn last_event_id(headers: &HeaderMap, page: &Page) -> Result<Option<u64>, ApiError> {
    match headers.get("last-event-id") {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .map(Some)
            .ok_or_else(|| ApiError::Bad("Last-Event-ID must be a version number".into())),
        None => Ok(page.since),
    }
}

fn sse(
    app: &Shared,
    subscription: Subscription,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + use<>> {
    let closing = app.closing.subscribe();
    let stream = unfold(
        (subscription, closing),
        |(mut subscription, mut closing)| async move {
            if *closing.borrow() {
                return None;
            }
            let frame = tokio::select! {
                frame = subscription.next() => frame?,
                _ = closing.changed() => return None,
            };
            let mut event = Event::default().event(frame.event).data(frame.data);
            if let Some(id) = frame.id {
                event = event.id(id.to_string());
            }
            Some((Ok(event), (subscription, closing)))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn user_events(
    State(app): State<Shared>,
    Path(user): Path<u64>,
    Query(page): Query<Page>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let since = last_event_id(&headers, &page)?;
    let subscription = app.subscribe(Scope::User(user), since)?;
    Ok(sse(&app, subscription).into_response())
}

async fn room_events(
    State(app): State<Shared>,
    Path(room): Path<u64>,
    Query(page): Query<Page>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let since = last_event_id(&headers, &page)?;
    let subscription = app.subscribe(Scope::Room(room), since)?;
    Ok(sse(&app, subscription).into_response())
}

async fn seed(State(app): State<Shared>, Json(plan): Json<SeedPlan>) -> Result<Response, ApiError> {
    let done = app
        .write(move |chat| {
            let seeded = ops::seed(chat, &plan, now_ms())?;
            Ok(("seed".to_string(), seeded))
        })
        .await?;
    Ok(created(json!({
        "first_user": done.value.first_user,
        "first_room": done.value.first_room,
        "version": done.version,
    })))
}

async fn health(State(app): State<Shared>) -> Json<Value> {
    let chat = app.db.read();
    Json(json!({
        "version": chat.version(),
        "users": chat.users.len(),
        "rooms": chat.rooms.len(),
        "feeds": app.feeds.live(),
        "frames": app.feeds.frames.load(Ordering::Relaxed),
        "posts": app.counters.posts.load(Ordering::Relaxed),
        "file_bytes": app.db.size_on_disk(),
    }))
}

pub fn router(app: Shared) -> Router {
    Router::new()
        .route("/users", post(create_user))
        .route("/users/{user}", get(get_user))
        .route("/users/{user}/unread", get(unread))
        .route("/users/{user}/events", get(user_events))
        .route("/rooms", post(create_room).get(list_rooms))
        .route("/rooms/{room}", get(get_room))
        .route("/rooms/{room}/members", get(members))
        .route("/rooms/{room}/join", post(join))
        .route("/rooms/{room}/leave", post(leave))
        .route("/rooms/{room}/read", post(mark_read))
        .route("/rooms/{room}/messages", post(post_message).get(history))
        .route(
            "/rooms/{room}/messages/{seq}",
            get(get_message).patch(edit_message).delete(delete_message),
        )
        .route(
            "/rooms/{room}/messages/{seq}/reactions/{emoji}",
            put(add_reaction).delete(remove_reaction),
        )
        .route("/rooms/{room}/search", get(search))
        .route("/rooms/{room}/events", get(room_events))
        .route(
            "/admin/seed",
            post(seed).layer(DefaultBodyLimit::max(256 << 20)),
        )
        .route("/health", get(health))
        .with_state(app)
}
