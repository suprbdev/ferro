use ferro::{Commit, Conflict, Data, Db, Error, Map, Record, Table};

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Task {
    #[key]
    id: u64,
    #[index]
    owner: Option<String>,
    #[index]
    state: String,
    #[index(each)]
    tags: Vec<String>,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Board {
    tasks: Table<Task>,
}

fn task(id: u64, owner: Option<&str>, state: &str, tags: &[&str]) -> Task {
    Task {
        id,
        owner: owner.map(str::to_string),
        state: state.to_string(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
    }
}

fn open(dir: &tempfile::TempDir) -> Db<Board> {
    Db::open(dir.path().join("b.ferro")).unwrap()
}

#[test]
fn commit_with_takes_the_message_from_the_closure_and_reports_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let done = db
        .commit_with(|board| {
            let id = board.tasks.len() as u64 + 1;
            board.tasks.insert(task(id, None, "open", &[]));
            (format!("add #{id}"), id)
        })
        .unwrap();
    assert_eq!(done.value, 1);
    assert_eq!(done.version, 1);
    assert!(done.changed);
    assert_eq!(db.log().get(&1).unwrap().message, "add #1");
    let same = db.commit_with(|_| ("nothing", ())).unwrap();
    assert!(!same.changed);
    assert_eq!(same.version, 1);
    assert_eq!(db.log().len(), 1);
}

#[test]
fn a_multi_line_message_is_one_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.ferro");
    {
        let db = Db::<Board>::open(&path).unwrap();
        db.commit("title\n\nbody line\n", |b| {
            b.tasks.insert(task(1, None, "open", &[]))
        })
        .unwrap();
        db.commit("", |b| b.tasks.insert(task(2, None, "open", &[])))
            .unwrap();
        assert_eq!(db.read().commit().messages().count(), 0);
    }
    let db = Db::<Board>::open(&path).unwrap();
    let log = db.log();
    let first = log.get(&1).unwrap();
    assert_eq!(first.message, "title\n\nbody line\n");
    assert_eq!(
        first.messages().collect::<Vec<_>>(),
        vec!["title\n\nbody line\n"]
    );
    assert_eq!(log.get(&2).unwrap().messages().count(), 0);
}

#[test]
fn restore_records_its_source_and_reports_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.update(|board| board.tasks.insert(task(1, None, "open", &[])))
        .unwrap();
    db.update(|board| board.tasks.insert(task(2, None, "open", &[])))
        .unwrap();
    let done = db.restore(&db.at(1).unwrap()).unwrap();
    assert!(done.changed);
    assert_eq!(done.version, 3);
    let log = db.log();
    let commit: std::sync::Arc<Commit> = log.get(&3).unwrap();
    assert_eq!(commit.restored, Some(1));
    assert!(!db.restore(&db.at(1).unwrap()).unwrap().changed);
}

#[test]
fn index_counts_and_option_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.update(|board| {
        board
            .tasks
            .insert(task(1, Some("ann"), "open", &["a", "b"]));
        board.tasks.insert(task(2, Some("ann"), "done", &["b"]));
        board.tasks.insert(task(3, Some("bob"), "open", &[]));
        board.tasks.insert(task(4, None, "open", &["c"]));
    })
    .unwrap();
    let snapshot = db.read();
    assert_eq!(
        snapshot.tasks.by_state_counts(),
        vec![("done".to_string(), 1), ("open".to_string(), 3)]
    );
    assert_eq!(
        snapshot.tasks.by_tags_counts(),
        vec![
            ("a".to_string(), 1),
            ("b".to_string(), 2),
            ("c".to_string(), 1)
        ]
    );
    assert_eq!(
        snapshot.tasks.by_owner_counts(),
        vec![
            (None, 1),
            (Some("ann".to_string()), 2),
            (Some("bob".to_string()), 1)
        ]
    );
    let ids: Vec<u64> = snapshot.tasks.by_owner_some("ann").map(|t| t.id).collect();
    assert_eq!(ids, vec![1, 2]);
    assert_eq!(snapshot.tasks.by_owner_some("nobody").len(), 0);
    assert_eq!(snapshot.tasks.by_owner(&None).len(), 1);
}

#[test]
fn index_iterators_share_one_type() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.update(|board| board.tasks.insert(task(1, Some("ann"), "open", &["a"])))
        .unwrap();
    let snapshot = db.read();
    let mut sources = vec![
        snapshot.tasks.by_state("open"),
        snapshot.tasks.by_tags("a"),
        snapshot.tasks.by_owner_some("ann"),
    ];
    sources.sort_by_key(|source| source.len());
    assert_eq!(sources.remove(0).next().unwrap().id, 1);
}

#[test]
fn history_of_reports_only_versions_where_the_projection_changed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.commit("create", |b| b.tasks.insert(task(1, None, "open", &[])))
        .unwrap();
    db.commit("unrelated", |b| b.tasks.insert(task(2, None, "open", &[])))
        .unwrap();
    db.commit("close", |b| {
        b.tasks.get_mut(&1).unwrap().state = "done".to_string();
    })
    .unwrap();
    db.commit("gone", |b| b.tasks.remove(&1)).unwrap();
    let history: Vec<_> = db.history_of(|b| b.tasks.get(&1)).collect();
    let summary: Vec<(u64, &str, Option<String>)> = history
        .iter()
        .map(|(commit, row)| {
            (
                commit.version,
                commit.message.as_str(),
                row.as_ref().map(|row| row.state.clone()),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (4, "gone", None),
            (3, "close", Some("done".to_string())),
            (1, "create", Some("open".to_string())),
        ]
    );
}

#[test]
fn common_ancestor_is_the_fork_point() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.update(|b| b.tasks.insert(task(1, None, "open", &[])))
        .unwrap();
    db.update(|b| b.tasks.insert(task(2, None, "open", &[])))
        .unwrap();
    let other = db.fork("other").unwrap();
    assert_eq!(db.common_ancestor(&other).unwrap().version(), 2);
    other
        .update(|b| b.tasks.insert(task(3, None, "open", &[])))
        .unwrap();
    db.update(|b| b.tasks.insert(task(4, None, "open", &[])))
        .unwrap();
    db.update(|b| b.tasks.insert(task(5, None, "open", &[])))
        .unwrap();
    assert_eq!(db.common_ancestor(&other).unwrap().version(), 2);
    assert_eq!(other.common_ancestor(&db).unwrap().version(), 2);
}

#[test]
fn common_ancestor_from_genesis_and_after_pruning() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.ferro");
    {
        let db = Db::<Map<u64, u64>>::open(&path).unwrap();
        let other = db.fork("other").unwrap();
        db.update(|m| m.insert(1, 1)).unwrap();
        other.update(|m| m.insert(1, 2)).unwrap();
        assert_eq!(db.common_ancestor(&other).unwrap().version(), 0);
        for i in 0..10 {
            db.update(|m| m.insert(i, i)).unwrap();
            other.update(|m| m.insert(i, i + 1)).unwrap();
        }
    }
    ferro::admin::compact(&path, ferro::admin::Keep::Last(2)).unwrap();
    let db = Db::<Map<u64, u64>>::open(&path).unwrap();
    let other = db.branch("other").unwrap();
    assert!(matches!(
        db.common_ancestor(&other).err(),
        Some(Error::NoCommonAncestor)
    ));
}

#[test]
fn three_way_merge_applies_disjoint_changes_and_resolves_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let main = open(&dir);
    main.update(|b| {
        b.tasks.insert(task(1, None, "open", &[]));
        b.tasks.insert(task(2, None, "open", &[]));
        b.tasks.insert(task(3, None, "open", &[]));
        b.tasks.insert(task(4, None, "open", &[]));
    })
    .unwrap();
    let side = main.fork("side").unwrap();
    side.update(|b| {
        b.tasks.update(&1, |t| t.state = "side".to_string());
        b.tasks.remove(&2);
        b.tasks.insert(task(5, Some("sid"), "open", &["new"]));
        b.tasks.update(&3, |t| t.state = "both".to_string());
        b.tasks.update(&4, |t| t.state = "theirs".to_string());
    })
    .unwrap();
    main.update(|b| {
        b.tasks.update(&3, |t| t.state = "both".to_string());
        b.tasks.update(&4, |t| t.state = "ours".to_string());
    })
    .unwrap();
    let base = main.common_ancestor(&side).unwrap();
    let theirs = side.read();
    let mut conflicts: Vec<Conflict<u64, Task>> = Vec::new();
    let written = main
        .commit("merge side", |b| {
            b.tasks.merge(&base.tasks, &theirs.tasks, |conflict| {
                let keep = conflict.ours.as_deref().cloned();
                conflicts.push(conflict);
                keep
            })
        })
        .unwrap();
    assert_eq!(written, 4);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].key, 4);
    assert_eq!(conflicts[0].theirs.as_ref().unwrap().state, "theirs");
    let merged = main.read();
    assert_eq!(merged.tasks.get(&1).unwrap().state, "side");
    assert!(merged.tasks.get(&2).is_none());
    assert_eq!(merged.tasks.get(&3).unwrap().state, "both");
    assert_eq!(merged.tasks.get(&4).unwrap().state, "ours");
    assert_eq!(merged.tasks.by_owner_some("sid").len(), 1);
    assert_eq!(merged.tasks.by_tags("new").len(), 1);
    assert_eq!(merged.tasks.by_state("open").len(), 1);
}

#[test]
fn map_merge_matches_table_semantics_and_read_only_error_is_clear() {
    let dir = tempfile::tempdir().unwrap();
    let mut ours: Map<u64, u64> = [(1, 1), (2, 2)].into_iter().collect();
    let base = ours.clone();
    let mut theirs = base.clone();
    theirs.insert(3, 3);
    theirs.remove(&2);
    ours.insert(1, 9);
    assert_eq!(
        ours.merge(&base, &theirs, |_| panic!("no conflict expected")),
        2
    );
    assert_eq!(ours.len(), 2);
    assert_eq!(*ours.get(&1).unwrap(), 9);
    assert_eq!(*ours.get(&3).unwrap(), 3);
    let missing = ferro::Options::new()
        .read_only()
        .open::<Map<u64, u64>>(dir.path().join("nope.ferro"));
    assert!(matches!(missing.err(), Some(Error::NoDatabase(_))));
}

#[test]
fn merge_with_remembers_what_it_already_took() {
    let dir = tempfile::tempdir().unwrap();
    let main = open(&dir);
    main.update(|b| b.tasks.insert(task(1, None, "open", &[])))
        .unwrap();
    let side = main.fork("side").unwrap();
    side.update(|b| b.tasks.update(&1, |t| t.state = "side".to_string()))
        .unwrap();
    let take = |main: &Db<Board>, side: &Db<Board>| {
        main.merge_with(side, |b, base, theirs| {
            let written = b
                .tasks
                .merge(&base.tasks, &theirs.tasks, |c| c.ours.as_deref().cloned());
            ("merge", written)
        })
        .unwrap()
    };
    assert_eq!(take(&main, &side).value, 1);
    main.update(|b| b.tasks.update(&1, |t| t.state = "main".to_string()))
        .unwrap();
    let again = take(&main, &side);
    assert_eq!(again.value, 0);
    assert!(!again.changed);
    assert_eq!(main.read().tasks.get(&1).unwrap().state, "main");
    side.update(|b| b.tasks.insert(task(2, None, "open", &[])))
        .unwrap();
    assert_eq!(take(&main, &side).value, 1);
    assert_eq!(main.read().tasks.len(), 2);
    assert_eq!(main.read().tasks.get(&1).unwrap().state, "main");
}

#[test]
fn merge_keeping_only_ours_still_records_progress_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("k.ferro");
    {
        let main = Db::<Board>::open(&path).unwrap();
        main.update(|b| b.tasks.insert(task(1, None, "open", &[])))
            .unwrap();
        let side = main.fork("side").unwrap();
        side.update(|b| b.tasks.update(&1, |t| t.state = "theirs".to_string()))
            .unwrap();
        main.update(|b| b.tasks.update(&1, |t| t.state = "ours".to_string()))
            .unwrap();
        let done = main
            .merge_with(&side, |b, base, theirs| {
                let mut seen = 0;
                b.tasks.merge(&base.tasks, &theirs.tasks, |c| {
                    seen += 1;
                    c.ours.as_deref().cloned()
                });
                ("merge", seen)
            })
            .unwrap();
        assert_eq!(done.value, 1);
        assert!(done.changed);
    }
    let main = Db::<Board>::open(&path).unwrap();
    let side = main.branch("side").unwrap();
    assert_eq!(
        main.common_ancestor(&side).unwrap().version(),
        side.version()
    );
    let done = main
        .merge_with(&side, |b, base, theirs| {
            let mut seen = 0;
            b.tasks.merge(&base.tasks, &theirs.tasks, |c| {
                seen += 1;
                c.ours.as_deref().cloned()
            });
            ("merge", seen)
        })
        .unwrap();
    assert_eq!(done.value, 0);
    assert!(!done.changed);
}

#[test]
fn merging_back_uses_the_reverse_record() {
    let dir = tempfile::tempdir().unwrap();
    let main = open(&dir);
    main.update(|b| b.tasks.insert(task(1, None, "open", &[])))
        .unwrap();
    let side = main.fork("side").unwrap();
    main.update(|b| b.tasks.insert(task(2, None, "open", &[])))
        .unwrap();
    side.merge_with(&main, |b, base, theirs| {
        let written = b
            .tasks
            .merge(&base.tasks, &theirs.tasks, |c| c.ours.as_deref().cloned());
        ("sync", written)
    })
    .unwrap();
    side.update(|b| b.tasks.insert(task(3, None, "open", &[])))
        .unwrap();
    assert_eq!(
        main.common_ancestor(&side).unwrap().version(),
        main.version()
    );
    let written = main
        .merge_with(&side, |b, base, theirs| {
            let n = b
                .tasks
                .merge(&base.tasks, &theirs.tasks, |c| c.ours.as_deref().cloned());
            ("back", n)
        })
        .unwrap();
    assert_eq!(written.value, 1);
    assert_eq!(main.read().tasks.len(), 3);
}

#[test]
fn changing_a_key_onto_an_existing_row_unindexes_the_displaced_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.update(|b| {
        b.tasks.insert(task(1, None, "one", &["x"]));
        b.tasks.insert(task(2, None, "two", &["y"]));
    })
    .unwrap();
    db.update(|b| b.tasks.get_mut(&1).unwrap().id = 2).unwrap();
    let snapshot = db.read();
    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks.get(&2).unwrap().state, "one");
    assert_eq!(snapshot.tasks.by_state("one").len(), 1);
    assert_eq!(snapshot.tasks.by_state("two").len(), 0);
    assert_eq!(snapshot.tasks.by_tags("x").len(), 1);
    assert_eq!(snapshot.tasks.by_tags("y").len(), 0);
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Lopsided(u64);

impl ferro::codec::Data for Lopsided {
    fn encode(&self, w: &mut ferro::codec::Encoder<'_>) -> ferro::Result<()> {
        w.varint(self.0);
        Ok(())
    }

    fn decode(r: &mut ferro::codec::Decoder<'_>) -> ferro::Result<Self> {
        let value = r.varint()?;
        if value == 7 {
            return Err(Error::Decode("seven is unreadable".into()));
        }
        Ok(Lopsided(value))
    }
}

#[test]
fn a_root_that_cannot_be_read_back_is_refused_before_it_becomes_durable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("l.ferro");
    {
        let db = Db::<Lopsided>::open(&path).unwrap();
        db.update(|root| root.0 = 1).unwrap();
        assert!(db.update(|root| root.0 = 7).is_err());
        assert_eq!(db.read().0, 1);
    }
    let db = Db::<Lopsided>::open(&path).unwrap();
    assert_eq!(db.read().0, 1);
    assert_eq!(db.version(), 1);
}
