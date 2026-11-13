use std::fmt::Debug;

use ferro::table::{Explain, IndexIter, Matches};
use ferro::{
    Bundle, Bytes, Change, Closer, Commit, Committed, Compacted, Conflict, Data, Db, Durability,
    Error, Following, Have, Keep, List, Log, Map, Options, Outcome, Reclaim, Record, Set, Snapshot,
    Space, Synced, Table, Taken, Val, Verified, Watcher,
};

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u32,
    #[index]
    group: u8,
}

#[derive(Data, Clone, Debug, Default)]
struct Root {
    names: Map<u32, String>,
    tags: Set<String>,
    queue: List<u64>,
    rows: Table<Row>,
}

fn shared<T: Send + Sync + Debug>() {}

fn sent<T: Send + Debug>() {}

fn error<E: std::error::Error + Send + Sync + 'static>() {}

#[test]
fn handles_and_values_are_send_sync_and_debug() {
    shared::<Db<Root>>();
    shared::<Snapshot<Root>>();
    shared::<Watcher<Root>>();
    shared::<Closer<Root>>();
    shared::<Log>();
    shared::<Commit>();
    shared::<Options>();
    shared::<Root>();
    shared::<Map<u32, String>>();
    shared::<Set<String>>();
    shared::<List<u64>>();
    shared::<Table<Row>>();
    shared::<Val<String>>();
    shared::<Bytes>();
    shared::<Have>();
    shared::<Bundle>();
}

#[test]
fn reports_and_settings_are_send_sync_and_debug() {
    shared::<Committed<u32>>();
    shared::<Synced<u32>>();
    shared::<Outcome>();
    shared::<Space>();
    shared::<Compacted>();
    shared::<Verified>();
    shared::<Following>();
    shared::<Keep>();
    shared::<Reclaim>();
    shared::<Durability>();
    shared::<Change<u32, String>>();
    shared::<Conflict<u32, String>>();
    shared::<Taken<u32>>();
    shared::<Explain>();
}

#[test]
fn iterators_are_send_and_debug() {
    sent::<ferro::map::Iter<u32, String>>();
    sent::<ferro::map::Keys<u32, String>>();
    sent::<ferro::map::Values<u32, String>>();
    sent::<ferro::map::Diff<u32, String>>();
    sent::<ferro::map::Cursor<u32, String>>();
    sent::<ferro::log::Iter>();
    sent::<IndexIter<Row>>();
    sent::<Matches<Row>>();
}

#[test]
fn errors_box_as_send_sync() {
    error::<Error>();
    error::<Taken<u32>>();
}
