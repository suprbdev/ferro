use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use booking::{Config, Hotels, Reservations, Status, check_state, walk};
use ferro::{Db, Options};

fn spawn(db: &Path, acks: &Path, coalesce: bool, seed: u64) -> Child {
    Command::new(assert_cmd::cargo::cargo_bin("booking-stress"))
        .args([
            "--threads",
            "8",
            "--secs",
            "600",
            "--no-check",
            "--think-us",
            "200",
        ])
        .args([
            "--venues",
            "2",
            "--rooms",
            "6",
            "--span",
            "40",
            "--customers",
            "60",
        ])
        .args(["--coalesce", if coalesce { "on" } else { "off" }])
        .args(["--seed", &seed.to_string()])
        .arg("--path")
        .arg(db)
        .arg("--ack-log")
        .arg(acks)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn version_of(db: &Path) -> u64 {
    if !db.exists() {
        return 0;
    }
    match Options::new().read_only().open::<Hotels>(db) {
        Ok(reader) => reader.version(),
        Err(_) => 0,
    }
}

fn wait_for(db: &Path, version: u64) {
    let start = Instant::now();
    while version_of(db) < version {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "stress made no progress"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn acks(path: &Path) -> BTreeMap<u64, Vec<char>> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut seen: BTreeMap<u64, Vec<char>> = BTreeMap::new();
    for line in text.lines() {
        let Some((kind, id)) = line.split_once(' ') else {
            continue;
        };
        if let (Some(kind), Ok(id)) = (kind.chars().next(), id.parse::<u64>()) {
            seen.entry(id).or_default().push(kind);
        }
    }
    seen
}

fn survive(db: &Path, log: &Path, round: usize) {
    let svc = Reservations::open(db, Config::default()).unwrap();
    let walked = walk(svc.db(), 25).unwrap();
    assert!(
        walked.ok(),
        "round {round}: {:#?}",
        &walked.violations[..walked.violations.len().min(10)]
    );
    assert!(
        walked.steps > 100,
        "round {round}: only {} versions",
        walked.steps
    );
    svc.db().verify().unwrap();

    let h = svc.read();
    for (id, kinds) in acks(log) {
        let status = h.bookings.get(&id).map(|b| b.status);
        let fine = if kinds.contains(&'X') {
            status == Some(Status::Cancelled)
        } else if kinds.contains(&'C') {
            matches!(status, Some(Status::Confirmed | Status::Cancelled))
        } else {
            status.is_some()
        };
        assert!(
            fine,
            "round {round}: acknowledged {kinds:?} for #{id}, found {status:?}"
        );
    }
    drop(h);

    let far = svc.now() + 3_600_000;
    let released = svc
        .write(|h: &mut Hotels, _| {
            let expired = h.expire_due(far);
            Ok((
                format!("expire {} holds after restart", expired.len()),
                expired,
            ))
        })
        .unwrap();
    let h = svc.read();
    assert_eq!(
        h.bookings.by_status(&Status::Held).len(),
        0,
        "round {round}"
    );
    assert!(
        check_state(&h).is_empty(),
        "round {round}: {:?}",
        check_state(&h)
    );
    assert!(
        released
            .iter()
            .all(|id| h.bookings.get(id).unwrap().status == Status::Expired)
    );
}

fn crash_rounds(coalesce: bool) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("stress.ferro");
    let log = dir.path().join("acks.log");
    for round in 0..3 {
        let target = version_of(&db) + 400;
        let mut child = spawn(&db, &log, coalesce, round as u64 + 11);
        wait_for(&db, target);
        child.kill().unwrap();
        child.wait().unwrap();
        survive(&db, &log, round);
    }
    let svc = Reservations::open(&db, Config::default()).unwrap();
    let reader: Db<Hotels> = Options::new().read_only().open(&db).unwrap();
    assert_eq!(reader.version(), svc.db().version());
    assert!(!acks(&log).is_empty());
}

#[test]
fn killing_the_stress_run_mid_commit_leaves_every_version_valid() {
    crash_rounds(false);
}

#[test]
fn killing_a_coalesced_stress_run_leaves_every_version_valid() {
    crash_rounds(true);
}
