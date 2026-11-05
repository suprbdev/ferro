//! The error type that every fallible call in ferro returns.
//!
//! Reads from collections do not return [`Result`], so that they read like the standard library's:
//! an I/O failure or a damaged file makes them panic, and their `try_` forms, such as
//! [`Map::try_get`](crate::Map::try_get), return the [`Error`] instead. Writes, history, branches,
//! sync and the [`admin`](crate::admin) functions return it directly.
//! [Errors and checks on local reads](https://github.com/archlemon/ferro/blob/main/docs/internals/errors.md)
//! describes where each kind comes from.

use std::io;

/// Everything that can go wrong in a ferro call.
///
/// The variants that carry a `String` describe what was found, for a person to read; match on the
/// variant, not on the text.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A call to the operating system failed.
    ///
    /// Opening with [`Options::shared`](crate::Options::shared) where the locks it needs are not
    /// available, anywhere but 64-bit Linux, fails with an [`io::ErrorKind::Unsupported`] error.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// Another writer has the file open.
    ///
    /// A second writer, in this process or another, gets it from
    /// [`Options::open`](crate::Options::open) once
    /// [`Options::lock_wait`](crate::Options::lock_wait) has passed. Writers opened with
    /// [`Options::shared`](crate::Options::shared) get it while a writer without the option has
    /// the file open, and that writer gets it while they do. A read-only open never does.
    #[error("database is locked by another writer")]
    Locked,
    /// A read-only open found no file at the path, which the error holds.
    ///
    /// A writable open creates the file instead.
    #[error("no database at {0}")]
    NoDatabase(String),
    /// The file at the path, which the error holds, is not a ferro database.
    ///
    /// A writable open returns it for a file with other contents and creates a database only in an
    /// empty file. A read-only open also returns it for an empty file.
    #[error("not a ferro database: {0}")]
    NotFerro(String),
    /// The file is damaged, or holds something no file ferro wrote can hold.
    ///
    /// An object whose bytes do not match its hash, a commit log whose versions do not descend, or
    /// a file whose superblocks are both invalid ends up here, and so do the type checks of
    /// [`Db::verify_typed`](crate::Db::verify_typed). The message says what was found and where.
    #[error("corrupt database: {0}")]
    Corrupt(String),
    /// Stored bytes do not decode as the type that reads them.
    ///
    /// The checks on each map node read from the file, of its counts and levels, fail with this
    /// too. In a file that ferro wrote it means damage, or a change to a type that broke its stored
    /// form, as described in
    /// [Changing your types](https://github.com/archlemon/ferro/blob/main/docs/guide/schema-evolution.md).
    #[error("decode error: {0}")]
    Decode(String),
    /// A stored struct lacks the named field, and the field has no default.
    ///
    /// A field added to a type after data was written needs one, such as `#[ferro(default)]`.
    #[error("missing field `{0}` and no default available")]
    MissingField(&'static str),
    /// The branch does not retain the version the error holds.
    ///
    /// The version was never committed on the branch, or pruned by
    /// [`Options::history`](crate::Options::history) or [`Db::compact`](crate::Db::compact).
    /// [`Db::as_of`](crate::Db::as_of) returns it, holding the version below the oldest retained
    /// one, for a time before the oldest retained commit.
    #[error("no such version: {0}")]
    NoVersion(u64),
    /// No branch has the name the error holds, or it was dropped.
    ///
    /// Writes through a handle on a branch that [`Db::drop_branch`](crate::Db::drop_branch)
    /// removed return it too.
    #[error("no such branch: {0}")]
    NoBranch(String),
    /// A merge base could lie in pruned history, so the merge refuses to guess it.
    ///
    /// Merging against an older base than the true one would bring back what one side deleted, so
    /// [`Db::merge_with`](crate::Db::merge_with),
    /// [`Db::common_ancestor`](crate::Db::common_ancestor) and [`Db::pull`](crate::Db::pull) return
    /// this instead. Two branches that share no commit and have pruned nothing merge against
    /// version 0 and do not return it.
    #[error("branches share no retained version")]
    NoCommonAncestor,
    /// [`Db::fork`](crate::Db::fork) was given the name of a branch that exists, or `main`.
    #[error("branch already exists: {0}")]
    BranchExists(String),
    /// The named branch is `main` or the branch of the handle the call was made on.
    ///
    /// [`Db::drop_branch`](crate::Db::drop_branch) refuses to drop those, and
    /// [`Db::pull`](crate::Db::pull) and [`Db::receive`](crate::Db::receive) refuse to land a
    /// remote branch over them.
    #[error("cannot drop branch {0}: it is main or this handle's own branch")]
    BranchInUse(String),
    /// A collection whose nodes are in a file was read without that file.
    ///
    /// A [`Map`](crate::Map) decoded from bytes with no store attached holds links it cannot
    /// follow.
    #[error("collection has stored nodes but no store attached")]
    Detached,
    /// The handle was opened with [`Options::read_only`](crate::Options::read_only), and the call
    /// writes.
    #[error("database is read-only")]
    ReadOnly,
    /// A reclamation cycle replaced the file this handle was reading.
    ///
    /// [`Db::refresh`](crate::Db::refresh) opens the new file when it meets this, so it does not
    /// return it.
    #[error("database file was replaced by compaction; reopen it")]
    Replaced,
    /// A background reclamation cycle stopped because the database was closing, or because another
    /// process was running one.
    ///
    /// It is not recorded as [`Space::last_error`](crate::Space::last_error).
    #[error("reclamation was interrupted by shutdown")]
    Interrupted,
    /// A [`Ptr`](crate::Ptr) being written does not name an object in the current file.
    ///
    /// A commit checks each `Ptr` stored in the value it writes, so one held in memory across a
    /// reclamation cycle, which names an object in the old file, fails with this. The error holds
    /// the pointer's offset.
    #[error("pointer {0} does not name an object in this generation of the file")]
    StalePointer(u64),
    /// A write to the file or a sync failed earlier, so nothing more can be acknowledged.
    ///
    /// Every commit queued behind the failure gets this, and so does every later one. Reopening the
    /// database recovers to the last commit that reached the disk. A failed commit may still be
    /// there after the reopen: an error means it was not acknowledged, not that it is absent.
    #[error("a previous commit failed; reopen the database to recover")]
    Poisoned,
    /// An object, such as one value or one node, would encode to more than 4 GiB. The error holds
    /// its size in bytes.
    #[error("object of {0} bytes exceeds the 4 GiB limit")]
    TooLarge(u64),
    /// With [`Options::coalesce`](crate::Options::coalesce), the group this closure joined could
    /// not be written.
    ///
    /// Every closure in the group gets this, holding the group's failure, and all their changes are
    /// gone, those of closures that did nothing wrong included.
    #[error("the coalesced group this closure joined was not written: {0}")]
    GroupFailed(String),
    /// The call would wait on a writer lock that this thread holds, or that is held by a write
    /// waiting on this thread.
    ///
    /// A write from inside a commit closure, a merge closure, a watcher test run there, or a `Drop`
    /// that runs under a writer lock returns this instead of deadlocking.
    /// [Writes from inside a closure](https://github.com/archlemon/ferro/blob/main/docs/guide/concurrency.md#writing-from-inside-a-closure)
    /// lists what is allowed.
    #[error(
        "this write would deadlock on a writer lock held by this thread or by a write waiting on it"
    )]
    Reentrant,
    /// A sync [`Have`](crate::Have) or [`Bundle`](crate::Bundle) is malformed, damaged or forged.
    ///
    /// [`Db::receive`](crate::Db::receive) checks every object before any of it reaches the file,
    /// so a rejected bundle changes nothing.
    #[error("sync bundle rejected: {0}")]
    Bundle(String),
    /// A bundle builds on a commit this file does not hold, so it cannot land.
    ///
    /// The bundle was built for another file, or this file pruned or dropped that commit after it
    /// sent its [`Have`](crate::Have). Nothing lands; build a new bundle from a fresh have.
    #[error("sync bundle is stale: {0}; build a new one from a fresh have")]
    StaleHave(String),
}

/// A [`std::result::Result`] whose error is ferro's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
