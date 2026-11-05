use std::path::Path;
use std::sync::{Arc, Mutex};

use ferro::db::Hook;
use ferro::{Bundle, Db, Durability, Keep, Options, Outcome};
use notes::{Device, Note, Notes, NotesError, merge};
use tempfile::TempDir;

fn device(root: &Path, name: &str) -> Device {
    let options = Options::new().durability(Durability::Buffered);
    Device::open_with(root.join(name), options).unwrap()
}

fn devices(names: &[&str]) -> (TempDir, Vec<Device>) {
    let dir = tempfile::tempdir().unwrap();
    let all = names.iter().map(|name| device(dir.path(), name)).collect();
    (dir, all)
}

fn visible(device: &Device) -> Vec<(String, String, Vec<String>)> {
    device
        .list(None)
        .into_iter()
        .map(|note| (note.id, note.text, note.tags))
        .collect()
}

fn assert_converged(all: &[Device]) {
    let first: Vec<Note> = all[0].everything();
    for device in &all[1..] {
        assert_eq!(
            device.everything(),
            first,
            "{} differs from {}",
            device.name(),
            all[0].name()
        );
    }
    for device in all {
        device.db().verify().unwrap();
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }
}

#[test]
fn notes_written_offline_on_two_devices_meet_after_a_sync() {
    let (_dir, all) = devices(&["laptop", "phone"]);
    let (laptop, phone) = (&all[0], &all[1]);
    let groceries = laptop.add("buy milk", &["home".to_string()]).unwrap();
    let call = phone.add("call the bank", &[]).unwrap();
    assert_eq!(groceries, "laptop-1");
    assert_eq!(call, "phone-1");
    let synced = laptop.sync(phone).unwrap();
    assert!(
        synced
            .steps
            .iter()
            .any(|step| step.pulled.outcome == Outcome::Merged)
    );
    assert_eq!(visible(laptop), visible(phone));
    assert_eq!(visible(laptop).len(), 2);
    assert_eq!(laptop.list(Some("home")).len(), 1);
    assert_eq!(phone.list(Some("home"))[0].id, groceries);
    assert_converged(&all);
}

#[test]
fn a_device_with_nothing_fast_forwards_and_shares_the_history() {
    let (_dir, all) = devices(&["laptop", "tablet"]);
    let (laptop, tablet) = (&all[0], &all[1]);
    for text in ["one", "two", "three"] {
        laptop.add(text, &[]).unwrap();
    }
    let synced = tablet.sync(laptop).unwrap();
    assert_eq!(synced.steps[0].pulled.outcome, Outcome::FastForward);
    assert_eq!(tablet.db().head_hash(), laptop.db().head_hash());
    assert_eq!(tablet.db().version(), 3);
    assert_converged(&all);
}

#[test]
fn concurrent_edits_to_one_note_keep_the_later_text_and_every_tag() {
    let (_dir, all) = devices(&["laptop", "phone"]);
    let (laptop, phone) = (&all[0], &all[1]);
    let id = laptop.add("draft", &["work".to_string()]).unwrap();
    laptop.sync(phone).unwrap();
    laptop.edit(&id, "laptop text").unwrap();
    phone.edit(&id, "phone text").unwrap();
    phone.edit(&id, "phone text, again").unwrap();
    phone.tag(&id, "urgent").unwrap();
    laptop.tag(&id, "review").unwrap();
    laptop.untag(&id, "work").unwrap();
    laptop.sync(phone).unwrap();
    let note = laptop.get(&id).unwrap();
    assert_eq!(note.text, "phone text, again");
    assert_eq!(note.tags, vec!["review".to_string(), "urgent".to_string()]);
    assert_converged(&all);
}

#[test]
fn the_later_of_a_delete_and_an_edit_wins() {
    let (_dir, all) = devices(&["laptop", "phone"]);
    let (laptop, phone) = (&all[0], &all[1]);
    let kept = laptop.add("kept", &[]).unwrap();
    let gone = laptop.add("gone", &[]).unwrap();
    laptop.sync(phone).unwrap();
    laptop.delete(&kept).unwrap();
    phone.edit(&kept, "edited after the delete").unwrap();
    phone.edit(&kept, "and again").unwrap();
    phone.edit(&gone, "edited before the delete").unwrap();
    laptop.edit(&kept, "unused").unwrap_err();
    laptop.add("pad", &[]).unwrap();
    laptop.add("pad", &[]).unwrap();
    laptop.delete(&gone).unwrap();
    phone.sync(laptop).unwrap();
    assert_eq!(phone.get(&kept).unwrap().text, "and again");
    assert!(!phone.get(&kept).unwrap().deleted);
    assert!(phone.get(&gone).unwrap().deleted);
    assert!(matches!(
        phone.edit(&gone, "x"),
        Err(NotesError::Deleted(_))
    ));
    assert_converged(&all);
}

#[test]
fn a_repeated_sync_sends_no_objects_and_writes_nothing() {
    let (_dir, all) = devices(&["laptop", "phone"]);
    let (laptop, phone) = (&all[0], &all[1]);
    laptop.add("one", &[]).unwrap();
    phone.add("two", &[]).unwrap();
    laptop.sync(phone).unwrap();
    let sizes = (laptop.db().size_on_disk(), phone.db().size_on_disk());
    for _ in 0..3 {
        let again = phone.sync(laptop).unwrap();
        assert_eq!(again.objects(), 0, "{:?}", again.steps);
        assert!(
            again
                .steps
                .iter()
                .all(|step| step.pulled.outcome == Outcome::UpToDate)
        );
        assert_eq!(
            (laptop.db().size_on_disk(), phone.db().size_on_disk()),
            sizes
        );
    }
}

#[test]
fn devices_with_the_same_name_refuse_to_sync() {
    let dir = tempfile::tempdir().unwrap();
    let first = device(&dir.path().join("a"), "phone");
    let second = device(&dir.path().join("b"), "phone");
    assert!(matches!(first.sync(&second), Err(NotesError::BadDevice(_))));
}

fn act(device: &Device, rng: &mut Rng) {
    let notes = device.list(None);
    match rng.below(6) {
        0 | 1 if !notes.is_empty() => {
            let id = &notes[rng.below(notes.len())].id;
            device
                .edit(
                    id,
                    &format!("{} wrote {}", device.name(), rng.next() % 1000),
                )
                .unwrap();
        }
        2 if !notes.is_empty() => {
            let id = &notes[rng.below(notes.len())].id;
            device.tag(id, &format!("t{}", rng.below(4))).unwrap();
        }
        3 if !notes.is_empty() => {
            let note = &notes[rng.below(notes.len())];
            if let Some(tag) = note.tags.first() {
                device.untag(&note.id, tag).unwrap();
            }
        }
        4 if notes.len() > 3 => {
            let id = &notes[rng.below(notes.len())].id;
            device.delete(id).unwrap();
        }
        _ => {
            let tags = vec![format!("t{}", rng.below(4))];
            device
                .add(
                    &format!("{} note {}", device.name(), rng.next() % 1000),
                    &tags,
                )
                .unwrap();
        }
    }
}

fn gossip(all: &[Device]) {
    for _ in 0..2 {
        for (index, device) in all.iter().enumerate() {
            device.sync(&all[(index + 1) % all.len()]).unwrap();
        }
    }
}

#[test]
fn three_devices_converge_whichever_order_they_sync_in() {
    let orders: [&[(usize, usize)]; 4] = [
        &[(0, 1), (1, 2), (2, 0)],
        &[(2, 0), (1, 0), (2, 1)],
        &[(0, 2), (0, 1)],
        &[(1, 2), (0, 2), (1, 0), (2, 1)],
    ];
    for (round, order) in orders.iter().enumerate() {
        let (_dir, all) = devices(&["laptop", "phone", "tablet"]);
        let mut rng = Rng(round as u64 * 7919 + 13);
        for device in &all {
            device.add("seed", &[]).unwrap();
        }
        for _ in 0..6 {
            for device in &all {
                for _ in 0..1 + rng.below(3) {
                    act(device, &mut rng);
                }
            }
            for (from, to) in order.iter() {
                all[*to].sync(&all[*from]).unwrap();
            }
        }
        gossip(&all);
        assert_converged(&all);
        let sizes: Vec<u64> = all
            .iter()
            .map(|device| device.db().size_on_disk())
            .collect();
        gossip(&all);
        let after: Vec<u64> = all
            .iter()
            .map(|device| device.db().size_on_disk())
            .collect();
        assert_eq!(sizes, after, "order {round}");
    }
}

#[test]
fn many_devices_editing_the_same_notes_converge_under_random_syncs() {
    for seed in 1..=4u64 {
        let names = ["a1", "b2", "c3", "d4"];
        let (_dir, all) = devices(&names);
        let mut rng = Rng(seed * 104_729);
        all[0].add("shared one", &[]).unwrap();
        all[0].add("shared two", &[]).unwrap();
        for index in 1..all.len() {
            all[index].sync(&all[0]).unwrap();
        }
        for _ in 0..60 {
            let device = rng.below(all.len());
            if rng.below(3) == 0 {
                let other = (device + 1 + rng.below(all.len() - 1)) % all.len();
                all[device].sync(&all[other]).unwrap();
            } else {
                act(&all[device], &mut rng);
            }
        }
        gossip(&all);
        assert_converged(&all);
    }
}

#[test]
fn a_sync_after_small_edits_on_a_large_notebook_moves_little() {
    let (_dir, all) = devices(&["laptop", "phone"]);
    let (laptop, phone) = (&all[0], &all[1]);
    let batch: Vec<_> = (0..20_000)
        .map(|n| {
            (
                format!("note number {n} with some words in it"),
                vec![format!("t{}", n % 7)],
            )
        })
        .collect();
    laptop.add_many(batch).unwrap();
    let cloned = phone.sync(laptop).unwrap();
    let ids: Vec<String> = laptop
        .list(None)
        .into_iter()
        .step_by(4999)
        .map(|note| note.id)
        .collect();
    for id in &ids {
        laptop.edit(id, "changed on the laptop").unwrap();
    }
    phone.tag(&ids[0], "phone").unwrap();
    let synced = phone.sync(laptop).unwrap();
    let file = laptop.db().size_on_disk();
    println!(
        "file {file} bytes, clone {} bytes, sync after {} edits {} bytes in {} objects",
        cloned.bytes(),
        ids.len() + 1,
        synced.bytes(),
        synced.objects()
    );
    assert!(
        synced.bytes() * 50 < file,
        "{} bytes against a {file} byte file",
        synced.bytes()
    );
    assert_eq!(
        phone.get(&ids[0]).unwrap().tags,
        vec!["phone".to_string(), "t0".to_string()]
    );
    assert_converged(&all);
}

#[test]
fn a_pull_whose_have_went_stale_tries_again_and_the_devices_converge() {
    type Armed = Option<(Db<Notes>, Bundle)>;
    let dir = tempfile::tempdir().unwrap();
    let slot: Arc<Mutex<Armed>> = Arc::new(Mutex::new(None));
    let refused: Arc<Mutex<Option<ferro::Error>>> = Arc::new(Mutex::new(None));
    let bundled = Arc::new(Mutex::new(0));
    let (armed, seen, attempts) = (slot.clone(), refused.clone(), bundled.clone());
    let hook = Hook(Arc::new(move |step| {
        if step != "bundled" {
            return;
        }
        *attempts.lock().unwrap() += 1;
        let held = armed.lock().unwrap().take();
        if let Some((db, bundle)) = held {
            db.compact(Keep::Last(3)).unwrap();
            *seen.lock().unwrap() = db.receive("laptop", &bundle, merge).err();
        }
    }));
    let laptop = device(dir.path(), "laptop");
    let options = Options::new().durability(Durability::Buffered).hook(hook);
    let phone = Device::open_with(dir.path().join("phone"), options).unwrap();
    for text in ["one", "two", "three"] {
        laptop.add(text, &[]).unwrap();
    }
    phone.sync(&laptop).unwrap();
    phone.db().drop_branch("laptop").unwrap();
    phone.add("four", &[]).unwrap();
    phone.add("five", &[]).unwrap();
    laptop.add("six", &[]).unwrap();
    let stale = laptop.db().bundle(&phone.db().have().unwrap()).unwrap();
    *slot.lock().unwrap() = Some((phone.db().clone(), stale));
    let before = *bundled.lock().unwrap();
    let synced = phone.sync(&laptop).unwrap();
    assert!(slot.lock().unwrap().is_none(), "the hook never ran");
    let into_phone = synced.steps.iter().filter(|step| step.into == "phone");
    assert_eq!(*bundled.lock().unwrap() - before, into_phone.count() + 1);
    assert!(
        matches!(*refused.lock().unwrap(), Some(ferro::Error::StaleHave(_))),
        "the first pull's have did not go stale: {:?}",
        refused.lock().unwrap()
    );
    assert_eq!(synced.steps[0].from, "laptop");
    assert_eq!(synced.steps[0].pulled.outcome, Outcome::Merged);
    assert_eq!(
        phone.db().log().first().map(|(version, _)| version),
        Some(3)
    );
    assert_eq!(visible(&phone).len(), 6);
    assert_converged(&[laptop, phone]);
}
