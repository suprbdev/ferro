mod common;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::fast;
use ferro::{Data, Db, Map, Record, Set, Snapshot, Table};

static PANICS: AtomicUsize = AtomicUsize::new(0);
static FIRST: Mutex<Option<String>> = Mutex::new(None);

fn count_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PANICS.fetch_add(1, Ordering::SeqCst);
        {
            let mut first = FIRST.lock().unwrap();
            if first.is_none() {
                *first = Some(format!("{} on {:?}", info, std::thread::current().name()));
            }
        }
        previous(info);
    }));
}

fn assert_no_panic(case: &str) {
    let first = FIRST.lock().unwrap().clone();
    assert_eq!(PANICS.load(Ordering::SeqCst), 0, "{case}: {first:?}");
}

mod peer {
    use ferro::{Data, Map, Record, Set, Table};

    #[derive(Data, Record, Clone, Debug, PartialEq)]
    pub struct Sub {
        #[key]
        pub k: u64,
        pub tags: Set<u64>,
    }

    #[derive(Data, Record, Clone, Debug, PartialEq)]
    pub struct Row {
        #[key]
        pub id: u64,
        #[index]
        pub n: u32,
        pub sub: Table<Sub>,
    }

    #[derive(Data, Clone, Default)]
    pub struct Root {
        pub a_pad: Map<u64, u64>,
        pub rows: Table<Row>,
        pub z_pad: Map<u64, u64>,
    }
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Sub {
    #[key]
    k: u64,
    #[index(each)]
    tags: Set<u32>,
}

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u64,
    #[index]
    n: u32,
    sub: Table<Sub>,
}

#[derive(Data, Clone, Default)]
struct Mine {
    a_pad: Map<u64, u64>,
    rows: Table<Row>,
    z_pad: Map<u64, u64>,
}

fn unmerged<T>(_: &mut T, _: &Snapshot<T>, _: &Snapshot<T>) -> (String, ()) {
    ("merge".to_string(), ())
}

fn peer_with(dir: &std::path::Path, tag: u64) -> Db<peer::Root> {
    let peer: Db<peer::Root> = fast().open(dir.join("peer.ferro")).unwrap();
    peer.update(|root| {
        for id in [1, 2] {
            root.rows.insert(peer::Row {
                id,
                n: id as u32 + 4,
                sub: Table::new(),
            });
        }
    })
    .unwrap();
    peer.update(|root| {
        for i in 0..40_000 {
            root.a_pad.insert(i, i);
            root.z_pad.insert(i, i);
        }
        let mut tags: Set<u64> = Set::new();
        tags.insert(tag);
        let mut sub: Table<peer::Sub> = Table::new();
        sub.insert(peer::Sub { k: 1, tags });
        root.rows.insert(peer::Row { id: 1, n: 5, sub });
    })
    .unwrap();
    peer
}

#[test]
fn checking_a_table_beside_the_walk_panics_on_no_thread() {
    count_panics();
    for run in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let peer = peer_with(dir.path(), 1 << 40);
        let mine: Db<Mine> = fast().open(dir.path().join("mine.ferro")).unwrap();
        let bundle = peer.bundle(&mine.have().unwrap()).unwrap();
        match mine.receive("peer", &bundle, unmerged) {
            Err(ferro::Error::Bundle(message)) => {
                assert!(message.contains("does not decode"), "run {run}: {message}")
            }
            other => panic!("run {run}: a set of u64 past u32::MAX landed as Set<u32>: {other:?}"),
        }
        assert_eq!(mine.version(), 0);
        assert_no_panic(&format!("a value the receiver cannot read, run {run}"));
    }
    let dir = tempfile::tempdir().unwrap();
    let peer = peer_with(dir.path(), 7);
    let mine: Db<Mine> = fast().open(dir.path().join("mine.ferro")).unwrap();
    let bundle = peer.bundle(&mine.have().unwrap()).unwrap();
    mine.receive("peer", &bundle, unmerged).unwrap();
    let read = mine.read();
    let row = read.rows.get(&1).unwrap();
    assert_eq!(row.sub.by_tags(&7).count(), 1);
    mine.verify_typed().unwrap();
    assert_no_panic("an honest pull");
}
