use std::time::{SystemTime, UNIX_EPOCH};

use ferro::{Data, Map, Record, Set, Table};

use crate::adj::Adj;
pub use crate::adj::UserId;

pub type PostId = u64;

#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct User {
    #[key]
    pub id: UserId,
    #[unique]
    pub name: String,
    pub created_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Data, Record)]
pub struct Post {
    #[key]
    pub id: PostId,
    #[index]
    pub author: UserId,
    pub at_ms: u64,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Data)]
pub struct Degrees {
    pub followers: u32,
    pub following: u32,
}

#[derive(Clone, Default, Data)]
pub struct Graph {
    pub users: Table<User>,
    pub out: Adj,
    pub inn: Adj,
    pub blocks: Set<(UserId, UserId)>,
    pub blocked_by: Set<(UserId, UserId)>,
    pub degrees: Map<UserId, Degrees>,
    pub posts: Table<Post>,
    pub likes: Set<(PostId, UserId)>,
    pub inbox: Set<(UserId, PostId)>,
    pub next_user: UserId,
    pub next_post: PostId,
    pub edges: u64,
}

impl Graph {
    pub fn user_count(&self) -> usize {
        self.next_user as usize
    }

    pub fn is_blocked(&self, a: UserId, b: UserId) -> bool {
        self.blocks.contains(&(a, b)) || self.blocks.contains(&(b, a))
    }

    pub fn blocked_either_way(&self, user: UserId) -> Vec<UserId> {
        let mut out: Vec<UserId> = self
            .blocks
            .range((user, 0)..=(user, UserId::MAX))
            .map(|(_, other)| other)
            .chain(
                self.blocked_by
                    .range((user, 0)..=(user, UserId::MAX))
                    .map(|(_, other)| other),
            )
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub fn like_count(&self, post: PostId) -> usize {
        self.likes.range((post, 0)..=(post, UserId::MAX)).len()
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
