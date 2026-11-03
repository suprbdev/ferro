use std::path::Path;

use assert_cmd::Command;
use booking::date::{format_night, night_of, now_ms};
use predicates::prelude::*;

fn booking(db: &Path) -> Command {
    let mut command = Command::cargo_bin("booking").unwrap();
    command.arg("--db").arg(db);
    command
}

#[test]
fn a_guest_holds_pays_and_cancels_from_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cli.ferro");
    let night = format_night(night_of(now_ms()) + 12);

    booking(&db).arg("seed").assert().success().stdout("seeded 3 venues, 12 rooms, 5 customers\n");
    booking(&db)
        .args(["avail", "Harbour House", "--from", &night, "--nights", "2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("room 101  120.00 120.00  total free 240.00"));
    booking(&db)
        .args(["hold", "guest1@example.com", "Harbour House/101", "--from", &night, "--nights", "2"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("held #1 for 240.00 until"));
    booking(&db)
        .args(["hold", "2", "1", "--from", &night])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is taken by booking #1"));
    booking(&db)
        .args(["confirm", "1", "--expect", "239"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not the 23900 the customer agreed to"));
    booking(&db).args(["confirm", "1", "--expect", "240"]).assert().success().stdout("confirmed #1, paid 240.00\n");
    booking(&db).args(["room", "rate", "1", "150"]).assert().success();
    booking(&db)
        .args(["receipt", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("quoted 120.00  rate then 120.00  rate now 150.00"));
    booking(&db).args(["cancel", "1"]).assert().success().stdout("cancelled #1, refunded 240.00\n");
    booking(&db)
        .args(["audit", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("held").and(predicate::str::contains("confirmed")).and(predicate::str::contains("cancelled")));
    booking(&db)
        .args(["report", "--from", &night, "--nights", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("charged 240.00  refunded 240.00"));
    booking(&db).arg("check").assert().success().stdout(predicate::str::contains("0 violations"));
}

#[test]
fn a_what_if_closure_is_previewed_then_applied() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cli.ferro");
    let night = format_night(night_of(now_ms()) + 3);
    booking(&db).arg("seed").assert().success();
    booking(&db).args(["hold", "1", "Alpine Lodge/101", "--from", &night]).assert().success();
    booking(&db).args(["confirm", "1"]).assert().success();

    booking(&db)
        .args(["whatif", "close", "storm", "--venue", "Alpine Lodge"])
        .assert()
        .success()
        .stdout(predicate::str::contains("#1 confirmed -> cancelled, refund 130.00"));
    booking(&db).args(["whatif", "list"]).assert().success().stdout("storm  close venue 2\n");
    booking(&db).args(["hold", "2", "Alpine Lodge/102", "--from", &night]).assert().success();
    booking(&db)
        .args(["whatif", "show", "storm"])
        .assert()
        .success()
        .stdout(predicate::str::contains("main has since taken #2"));
    booking(&db)
        .args(["whatif", "apply", "storm"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cancelled 2, refunded 13000"));
    booking(&db).args(["whatif", "list"]).assert().success().stdout("");
    booking(&db)
        .args(["hold", "3", "Alpine Lodge/103", "--from", &night])
        .assert()
        .failure()
        .stderr(predicate::str::contains("venue 2 is closed"));
    booking(&db).args(["check", "--every", "1"]).assert().success();
}
