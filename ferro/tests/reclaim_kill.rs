#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::{cases, text};
use ferro::db::Hook;
use ferro::{Db, Keep, Map, Options, Reclaim};
use tempfile::TempDir;

type Root = Map<u64, String>;
type Model = BTreeMap<u64, String>;

const MANUAL: Reclaim = Reclaim { every_bytes: 0 };

fn dir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn dump(root: &Root) -> Model {
    root.iter().map(|(k, v)| (k, (*v).clone())).collect()
}

fn child_writer(path: &str, stage: &'static str) {
    let db: Db<Root> = Options::new()
        .reclaim(MANUAL)
        .hook(Hook(Arc::new(move |name| {
            if name == stage {
                std::process::abort();
            }
        })))
        .open(path)
        .unwrap();
    for round in 0..4u64 {
        for i in 0..40u64 {
            let key = i % 12;
            let value = format!("{round}-{i}-{}", text(i, 30));
            db.update(|root| {
                root.insert(key, value.clone());
            })
            .unwrap();
            println!("ACK {} {key} {value}", db.version());
        }
        if round == 2 {
            db.compact(Keep::Last(3)).unwrap();
        }
    }
}

#[test]
fn process_child() {
    let Ok(path) = std::env::var("FERRO_RECLAIM_PATH") else {
        return;
    };
    let stage = std::env::var("FERRO_RECLAIM_STAGE").unwrap();
    child_writer(&path, Box::leak(stage.into_boxed_str()));
}

#[test]
fn killing_the_process_at_every_stage_keeps_every_acknowledged_commit() {
    for stage in ["created", "copied", "synced", "swapped", "retired"] {
        for attempt in 0..cases(3) {
            let dir = dir();
            let path = dir.path().join("killed.ferro");
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "process_child", "--nocapture", "--test-threads=1"])
                .env("FERRO_RECLAIM_PATH", &path)
                .env("FERRO_RECLAIM_STAGE", stage)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut acked = BTreeMap::new();
            for line in BufReader::new(child.stdout.take().unwrap()).lines() {
                let line = line.unwrap();
                let Some((_, rest)) = line.split_once("ACK ") else {
                    continue;
                };
                let mut fields = rest.splitn(3, ' ');
                let version: u64 = fields.next().unwrap().parse().unwrap();
                let key: u64 = fields.next().unwrap().parse().unwrap();
                acked.insert(version, (key, fields.next().unwrap().to_string()));
            }
            assert!(!child.wait().unwrap().success(), "{stage}: the hook never fired");
            assert!(acked.len() >= 120, "{stage} {attempt}: {}", acked.len());
            let last = *acked.keys().max().unwrap();
            let db: Db<Root> = Options::new().reclaim(MANUAL).open(&path).unwrap();
            assert!(db.version() >= last, "{stage}: {} below {last}", db.version());
            let want: Model = acked.values().cloned().collect::<BTreeMap<_, _>>();
            let root = db.at(last).unwrap();
            assert_eq!(dump(&root), want, "{stage}");
            db.verify().unwrap();
            db.update(|root| {
                root.insert(999, "after".into());
            })
            .unwrap();
            db.compact(Keep::Last(1)).unwrap();
            assert_eq!(dump(&db.read()).len(), want.len() + 1);
            db.verify().unwrap();
        }
    }
}
