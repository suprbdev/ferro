use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("database is locked by another writer")]
    Locked,
    #[error("no database at {0}")]
    NoDatabase(String),
    #[error("not a ferro database: {0}")]
    NotFerro(String),
    #[error("corrupt database: {0}")]
    Corrupt(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("missing field `{0}` and no default available")]
    MissingField(&'static str),
    #[error("no such version: {0}")]
    NoVersion(u64),
    #[error("no such branch: {0}")]
    NoBranch(String),
    #[error("branches share no retained version")]
    NoCommonAncestor,
    #[error("branch already exists: {0}")]
    BranchExists(String),
    #[error("cannot drop branch {0}: it is main or this handle's own branch")]
    BranchInUse(String),
    #[error("collection has stored nodes but no store attached")]
    Detached,
    #[error("database is read-only")]
    ReadOnly,
    #[error("database file was replaced by compaction; reopen it")]
    Replaced,
    #[error("a previous commit failed; reopen the database to recover")]
    Poisoned,
    #[error("object at offset {0} belongs to a pruned version and was reclaimed")]
    Pruned(u64),
    #[error("reclamation was interrupted by shutdown")]
    Interrupted,
    #[error("object of {0} bytes exceeds the 4 GiB limit")]
    TooLarge(u64),
}

pub type Result<T> = std::result::Result<T, Error>;
