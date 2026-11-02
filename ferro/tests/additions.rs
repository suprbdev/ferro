mod common;

use std::sync::mpsc::TryRecvError;

use common::{fast, text};
use ferro::admin::{self, Keep};
use ferro::codec::{Encoder, HashSink};
use ferro::{Data, Db, Error, List, Map, Options, Record, Table, hash_of};

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Item {
    #[key]
    id: u64,
    #[index]
    shelf: String,
    #[index]
    owner: Option<u32>,
    #[index(each)]
    tags: Vec<String>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Store {
    items: Table<Item>,
    notes: Map<u64, String>,
}

fn item(id: u64) -> Item {
    Item {
        id,
        shelf: format!("shelf-{}", id % 3),
        owner: (!id.is_multiple_of(4)).then_some((id % 2) as u32),
        tags: vec![format!("t{}", id % 2), "all".to_string()],
    }
}

fn filled(path: &std::path::Path) -> Db<Store> {
    let db = fast().open::<Store>(path).unwrap();
    db.update(|store| {
        for id in 1..=300 {
            store.items.insert(item(id));
        }
    })
    .unwrap();
    db
}

fn ids(rows: impl Iterator<Item = ferro::Val<Item>>) -> Vec<u64> {
    rows.map(|row| row.id).collect()
}

#[test]
fn head_hash_covers_the_whole_chain_and_stands_for_it_later() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h.ferro");
    let db = fast().open::<Map<u64, u64>>(&path).unwrap();
    assert_eq!(db.head_hash(), None);
    db.commit("one", |root| {
        root.insert(1, 1);
    })
    .unwrap();
    let first = db.head_hash().unwrap();
    db.commit("two", |root| {
        root.insert(1, 2);
    })
    .unwrap();
    let second = db.head_hash().unwrap();
    assert_ne!(first, second);
    assert_eq!(db.log().hash_at(&1), Some(first));
    assert_eq!(db.log().hash_at(&2), Some(second));
    assert_eq!(db.log().hash_at(&3), None);
    assert_eq!(db.log().hash_at(&0), None);

    let fork = db.fork("side").unwrap();
    assert_eq!(fork.head_hash(), Some(second));
    fork.commit("three", |root| {
        root.insert(1, 3);
    })
    .unwrap();
    assert_ne!(fork.head_hash(), Some(second));
    assert_eq!(fork.log().hash_at(&2), Some(second));
    drop(fork);
    drop(db);

    admin::compact(&path, Keep::All).unwrap();
    let db = fast().open::<Map<u64, u64>>(&path).unwrap();
    assert_eq!(db.head_hash(), Some(second));
    assert_eq!(db.log().hash_at(&1), Some(first));
    drop(db);

    admin::compact(&path, Keep::Last(1)).unwrap();
    let db = fast().open::<Map<u64, u64>>(&path).unwrap();
    assert_eq!(db.log().hash_at(&1), None);
    assert_ne!(db.head_hash(), Some(second));
}

#[test]
fn head_hash_differs_for_the_same_values_reached_differently() {
    let dir = tempfile::tempdir().unwrap();
    let build = |name: &str, message: &str| {
        let db = fast().open::<Map<u64, u64>>(dir.path().join(name)).unwrap();
        db.commit("same", |root| {
            root.insert(1, 1);
        })
        .unwrap();
        db.commit(message, |root| {
            root.insert(2, 2);
        })
        .unwrap();
        db
    };
    let (a, b) = (build("a.ferro", "left"), build("b.ferro", "right"));
    assert_eq!(a.read().root_hash(), b.read().root_hash());
    assert_ne!(a.head_hash(), b.head_hash());
}

#[test]
fn table_root_hash_follows_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Store>(dir.path().join("t.ferro")).unwrap();
    assert_eq!(db.read().items.root_hash(), None);
    db.update(|store| {
        store.items.insert(item(1));
    })
    .unwrap();
    let before = db.read();
    assert_eq!(before.items.root_hash(), before.items.rows().root_hash());
    assert!(before.items.root_hash().is_some());
    db.update(|store| {
        store.notes.insert(1, "elsewhere".to_string());
    })
    .unwrap();
    assert_eq!(db.read().items.root_hash(), before.items.root_hash());
    db.update(|store| {
        store.items.insert(item(2));
    })
    .unwrap();
    assert_ne!(db.read().items.root_hash(), before.items.root_hash());
}

#[test]
fn hash_of_matches_encoding_by_hand() {
    let by_hand = |value: &Item| {
        let mut sink = HashSink;
        let mut encoder = Encoder::new(&mut sink);
        value.encode(&mut encoder).unwrap();
        encoder.finish().unwrap().hash
    };
    let row = item(5);
    assert_eq!(hash_of(&row), by_hand(&row));
    assert_ne!(hash_of(&row), hash_of(&item(6)));
    let list: List<u64> = (0..10).collect();
    let same: List<u64> = (0..10).collect();
    assert_eq!(hash_of(&list), hash_of(&same));
}

#[test]
fn value_hash_agrees_with_hash_of_for_inline_spilled_and_dirty_values() {
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Store>(dir.path().join("v.ferro")).unwrap();
    let spilled = text(3, 4000);
    db.update(|store| {
        store.notes.insert(1, "short".to_string());
        store.notes.insert(2, spilled.clone());
    })
    .unwrap();
    let snapshot = db.read();
    assert_eq!(snapshot.notes.value_hash(&1), Some(hash_of(&"short".to_string())));
    assert_eq!(snapshot.notes.value_hash(&2), Some(hash_of(&spilled)));
    assert_eq!(snapshot.notes.value_hash(&3), None);
    let mut dirty = snapshot.notes.clone();
    dirty.insert(4, "fresh".to_string());
    dirty.insert(5, spilled.clone());
    assert_eq!(dirty.value_hash(&4), Some(hash_of(&"fresh".to_string())));
    assert_eq!(dirty.value_hash(&5), Some(hash_of(&spilled)));
}

#[test]
fn a_spilled_value_hash_does_not_read_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.ferro");
    let spilled = text(4, 4000);
    let (hash, offset) = {
        let db = fast().open::<Map<u64, String>>(&path).unwrap();
        db.update(|root| {
            root.insert(1, spilled.clone());
        })
        .unwrap();
        (hash_of(&spilled), db.read().value_hash(&1).unwrap())
    };
    assert_eq!(hash, offset);
    let needle = &spilled.as_bytes()[..64];
    let mut bytes = std::fs::read(&path).unwrap();
    let at = bytes.windows(64).position(|window| window == needle).unwrap();
    bytes[at + 16] ^= 0xFF;
    std::fs::write(&path, bytes).unwrap();
    let db = Options::new().read_only().open::<Map<u64, String>>(&path).unwrap();
    let snapshot = db.read();
    assert_eq!(snapshot.value_hash(&1), Some(hash));
    assert!(snapshot.try_get(&1).is_err());
}

#[test]
fn an_index_cursor_continues_after_the_key_it_was_given() {
    let dir = tempfile::tempdir().unwrap();
    let db = filled(&dir.path().join("i.ferro"));
    let store = db.read();
    let shelf = "shelf-1";
    let whole = ids(store.items.by_shelf(shelf));
    assert_eq!(whole.len(), 100);
    assert_eq!(ids(store.items.by_shelf_after(shelf, &0)), whole);
    assert_eq!(ids(store.items.by_shelf_after(shelf, &1)), whole[1..]);
    assert_eq!(ids(store.items.by_shelf_after(shelf, &2)), whole[1..]);
    assert_eq!(ids(store.items.by_shelf_after(shelf, &whole[41])), whole[42..]);
    assert_eq!(store.items.by_shelf_after(shelf, &whole[41]).len(), 58);
    assert_eq!(ids(store.items.by_shelf_after(shelf, &whole[99])), Vec::<u64>::new());
    assert_eq!(ids(store.items.by_shelf_after(shelf, &u64::MAX)), Vec::<u64>::new());
    assert_eq!(ids(store.items.by_shelf_after("nowhere", &0)), Vec::<u64>::new());
    let reversed = ids(store.items.by_shelf_after(shelf, &whole[97]).rev());
    assert_eq!(reversed, vec![whole[99], whole[98]]);
}

#[test]
fn cursors_exist_for_each_kind_of_index() {
    let dir = tempfile::tempdir().unwrap();
    let db = filled(&dir.path().join("i.ferro"));
    let store = db.read();
    let tagged = ids(store.items.by_tags("t1"));
    assert_eq!(ids(store.items.by_tags_after("t1", &tagged[9])), tagged[10..]);
    let owned = ids(store.items.by_owner_some(&1));
    assert_eq!(ids(store.items.by_owner_some_after(&1, &owned[4])), owned[5..]);
    assert_eq!(ids(store.items.by_owner_some_after(&1, &0)), owned);
    let unowned = ids(store.items.by_owner(&None));
    assert_eq!(ids(store.items.by_owner_after(&None, &unowned[0])), unowned[1..]);
}

#[test]
fn paging_with_a_cursor_visits_every_row_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = filled(&dir.path().join("i.ferro"));
    let store = db.read();
    let mut seen = Vec::new();
    let mut after = 0;
    loop {
        let page: Vec<u64> = ids(store.items.by_shelf_after("shelf-2", &after).take(7));
        let Some(last) = page.last() else {
            break;
        };
        after = *last;
        seen.extend(page);
    }
    assert_eq!(seen, ids(store.items.by_shelf("shelf-2")));
}

#[test]
fn string_keys_take_a_borrowed_cursor() {
    #[derive(Data, Record, Clone, Debug, PartialEq)]
    struct Named {
        #[key]
        name: String,
        #[index]
        group: u32,
    }
    #[derive(Data, Clone, Debug, Default)]
    struct Names {
        rows: Table<Named>,
    }
    let dir = tempfile::tempdir().unwrap();
    let db = fast().open::<Names>(dir.path().join("n.ferro")).unwrap();
    db.update(|names| {
        for name in ["a", "b", "c"] {
            names.rows.insert(Named {
                name: name.to_string(),
                group: 1,
            });
        }
    })
    .unwrap();
    let names = db.read();
    let found: Vec<String> = names
        .rows
        .by_group_after(&1, "a")
        .map(|row| row.name.clone())
        .collect();
    assert_eq!(found, vec!["b".to_string(), "c".to_string()]);
}

#[test]
fn rows_needs_no_trait_import() {
    let dir = tempfile::tempdir().unwrap();
    let db = filled(&dir.path().join("r.ferro"));
    assert_eq!(db.read().items.rows().len(), 300);
}

#[test]
fn db_snapshot_and_watcher_format_without_a_debug_bound() {
    #[derive(Data, Clone, Default)]
    struct Opaque {
        value: u64,
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.ferro");
    let db = fast().open::<Opaque>(&path).unwrap();
    db.commit("first", |root| root.value = 1).unwrap();
    let held = format!("{db:?}");
    assert!(held.contains("main") && held.contains("version: 1"), "{held}");
    assert!(format!("{:?}", db.read()).contains("first"));
    let error = fast().open::<Opaque>(&path).unwrap_err();
    assert!(matches!(error, Error::Locked));
    let missing = db.at(9).unwrap_err();
    assert!(matches!(missing, Error::NoVersion(9)));
    let watcher = db.watch();
    assert_eq!(watcher.try_recv().unwrap_err(), TryRecvError::Empty);
    assert!(format!("{watcher:?}").contains("Watcher"));
}
