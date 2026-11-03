use std::path::{Path, PathBuf};

use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::TempDir;

struct Devices {
    dir: TempDir,
}

impl Devices {
    fn new() -> Devices {
        Devices {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, device: &str) -> PathBuf {
        self.dir.path().join(device)
    }

    fn run(&self, device: &str, args: &[&str]) -> String {
        let out = cargo_bin_cmd!("notes")
            .arg("--device")
            .arg(self.path(device))
            .args(args)
            .assert()
            .success()
            .get_output()
            .clone();
        String::from_utf8(out.stdout).unwrap()
    }

    fn fail(&self, device: &str, args: &[&str]) -> String {
        let out = cargo_bin_cmd!("notes")
            .arg("--device")
            .arg(self.path(device))
            .args(args)
            .assert()
            .failure()
            .get_output()
            .clone();
        String::from_utf8(out.stderr).unwrap()
    }

    fn sync(&self, device: &str, other: &str) -> String {
        let other = self.path(other);
        self.run(device, &["sync", path_str(&other)])
    }
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn add_list_edit_tag_and_delete_on_one_device() {
    let devices = Devices::new();
    assert_eq!(
        devices.run("laptop", &["add", "buy milk", "--tag", "home"]),
        "laptop-1\n"
    );
    assert_eq!(
        devices.run("laptop", &["add", "call the bank"]),
        "laptop-2\n"
    );
    devices.run("laptop", &["edit", "laptop-1", "buy oat milk"]);
    devices.run("laptop", &["tag", "laptop-2", "errands"]);
    assert_eq!(
        devices.run("laptop", &["list"]),
        "laptop-1  buy oat milk #home\nlaptop-2  call the bank #errands\n"
    );
    assert_eq!(
        devices.run("laptop", &["list", "--tag", "home"]),
        "laptop-1  buy oat milk #home\n"
    );
    devices.run("laptop", &["untag", "laptop-1", "home"]);
    devices.run("laptop", &["delete", "laptop-2"]);
    assert_eq!(devices.run("laptop", &["list"]), "laptop-1  buy oat milk\n");
    let shown = devices.run("laptop", &["show", "laptop-2"]);
    assert!(shown.contains("deleted on laptop"), "{shown}");
    assert!(
        devices
            .fail("laptop", &["edit", "laptop-2", "x"])
            .contains("is deleted")
    );
    assert!(
        devices
            .fail("laptop", &["edit", "laptop-9", "x"])
            .contains("no note laptop-9")
    );
    let log = devices.run("laptop", &["log", "--limit", "2"]);
    assert_eq!(log, "v6 delete laptop-2\nv5 untag laptop-1\n");
}

#[test]
fn sync_exchanges_changes_both_ways_and_resolves_a_concurrent_edit() {
    let devices = Devices::new();
    devices.run("laptop", &["add", "shopping list", "--tag", "home"]);
    devices.run("laptop", &["add", "pay rent"]);
    let first = devices.sync("phone", "laptop");
    assert!(first.contains("laptop into phone: fast-forward"), "{first}");
    devices.run("laptop", &["edit", "laptop-1", "eggs and bread"]);
    devices.run("phone", &["edit", "laptop-1", "eggs, bread and jam"]);
    devices.run("phone", &["edit", "laptop-1", "eggs, bread, jam and tea"]);
    devices.run("phone", &["tag", "laptop-2", "money"]);
    devices.run("phone", &["add", "from the phone"]);
    let synced = devices.sync("laptop", "phone");
    assert!(synced.contains("phone into laptop: merged"), "{synced}");
    assert!(synced.contains("bytes in all"), "{synced}");
    let laptop = devices.run("laptop", &["list"]);
    assert_eq!(laptop, devices.run("phone", &["list"]));
    assert_eq!(
        laptop,
        "laptop-1  eggs, bread, jam and tea #home\nlaptop-2  pay rent #money\nphone-6  from the phone\n"
    );
    let again = devices.sync("phone", "laptop");
    assert!(again.contains(" 0 objects"), "{again}");
    assert!(
        !again.contains("merged") && !again.contains("fast-forward"),
        "{again}"
    );
}

#[test]
fn sync_refuses_a_directory_without_notes() {
    let devices = Devices::new();
    devices.run("laptop", &["add", "one"]);
    let error = devices.fail("laptop", &["sync", path_str(&devices.path("nowhere"))]);
    assert!(error.contains("no notes in"), "{error}");
}
