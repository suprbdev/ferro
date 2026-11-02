mod common;

use std::collections::BTreeMap;
use std::fs;
use std::process::{Command, Stdio};

use common::{fast, text, uncached};
use ferro::{Error, Map};

type Root = Map<u64, String>;

fn child_body(path: &str) {
    let db = fast().open::<Root>(path).unwrap();
    let mut model = BTreeMap::new();
    let mut failure = None;
    for key in 0..200_000u64 {
        let value = text(key, 700);
        match db.update(|root| {
            root.insert(key, value.clone());
        }) {
            Ok(()) => {
                model.insert(key, value);
            }
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    assert!(
        matches!(failure, Some(Error::Io(_))),
        "write limit never hit: {failure:?}"
    );
    let version = db.version();
    assert!(version > 0);
    assert!(matches!(
        db.update(|root| {
            root.insert(u64::MAX, "x".into());
        }),
        Err(Error::Poisoned)
    ));
    assert!(matches!(db.fork("other"), Err(Error::Poisoned)));
    assert_eq!(db.version(), version);
    let snapshot = db.read();
    assert_eq!(snapshot.len(), model.len());
    for (key, value) in &model {
        assert_eq!(snapshot.get(key).as_deref(), Some(value));
    }
    db.verify().unwrap();
    let report = format!("{version} {}", model.len());
    fs::write(format!("{path}.report"), report).unwrap();
}

#[test]
fn write_limit_child() {
    if let Ok(path) = std::env::var("FERRO_POISON_PATH") {
        child_body(&path);
    }
}

#[test]
fn failed_write_poisons_until_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("poison.ferro");
    let exe = std::env::current_exe().unwrap();
    let status = Command::new("sh")
        .arg("-c")
        .arg("trap '' XFSZ; ulimit -f 512; exec \"$0\" --exact write_limit_child --test-threads=1")
        .arg(&exe)
        .env("FERRO_POISON_PATH", &path)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let report = fs::read_to_string(format!("{}.report", path.display())).unwrap();
    let (version, length) = report.split_once(' ').unwrap();
    let version: u64 = version.parse().unwrap();
    let length: usize = length.parse().unwrap();
    let dirty = fs::metadata(&path).unwrap().len();
    let db = fast().open::<Root>(&path).unwrap();
    assert_eq!(db.version(), version);
    assert_eq!(db.read().len(), length);
    db.verify().unwrap();
    assert!(db.size_on_disk() <= dirty);
    for key in 0..length as u64 {
        assert_eq!(*db.read().get(&key).unwrap(), text(key, 700));
    }
    db.update(|root| {
        root.insert(u64::MAX, "after".into());
    })
    .unwrap();
    assert_eq!(db.version(), version + 1);
    drop(db);
    let db = uncached().open::<Root>(&path).unwrap();
    assert_eq!(db.version(), version + 1);
    db.verify().unwrap();
}
