mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use ferro::db::Hook;
use ferro::{Db, Durability, Error, Map, Options, Reclaim};

type Root = Map<u64, u64>;

#[derive(Default)]
struct Gates {
    state: Mutex<HashMap<String, (bool, bool)>>,
    moved: Condvar,
}

impl Gates {
    fn arm(&self, name: &str) {
        self.state
            .lock()
            .unwrap()
            .insert(name.to_string(), (false, false));
    }

    fn hold(&self, name: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.get_mut(name) else {
            return false;
        };
        if entry.0 {
            return false;
        }
        entry.0 = true;
        self.moved.notify_all();
        while !state[name].1 {
            state = self.moved.wait(state).unwrap();
        }
        true
    }

    fn reached(&self, name: &str) {
        let mut state = self.state.lock().unwrap();
        while !state[name].0 {
            let (next, timeout) = self
                .moved
                .wait_timeout(state, Duration::from_secs(10))
                .unwrap();
            state = next;
            assert!(!timeout.timed_out(), "{name} never reached its step");
        }
    }

    fn release(&self, name: &str) {
        self.state.lock().unwrap().get_mut(name).unwrap().1 = true;
        self.moved.notify_all();
    }
}

fn spawn(name: &str, db: &Db<Root>, key: u64) -> mpsc::Receiver<ferro::Result<u64>> {
    let (tx, rx) = mpsc::channel();
    let db = db.clone();
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let done = db
                .commit_with(|r| {
                    r.insert(key, 1);
                    ("", ())
                })
                .map(|c| c.version);
            let _ = tx.send(done);
        })
        .unwrap();
    rx
}

fn panicking_hook(step_name: &'static str, thread_name: &'static str) -> (Hook, Arc<AtomicBool>) {
    let armed = Arc::new(AtomicBool::new(false));
    let hook = {
        let armed = armed.clone();
        Hook(Arc::new(move |step| {
            if step == step_name
                && thread::current().name() == Some(thread_name)
                && armed.swap(false, Ordering::SeqCst)
            {
                panic!("ferro: probe panic at {step}");
            }
        }))
    };
    (hook, armed)
}

fn publishing_panic(coalesce: bool, step: &'static str) {
    common::quiet_ferro_panics();
    let (hook, armed) = panicking_hook(step, "leader");
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Root> = Options::new()
        .durability(Durability::Buffered)
        .coalesce(coalesce)
        .reclaim(Reclaim { every_bytes: 0 })
        .hook(hook)
        .open(dir.path().join("p.ferro"))
        .unwrap();
    main.update(|r| {
        r.insert(0, 0);
    })
    .unwrap();
    armed.store(true, Ordering::SeqCst);
    let leader = spawn("leader", &main, 1);
    let _ = leader.recv_timeout(Duration::from_secs(10));
    let later = spawn("later", &main, 2);
    match later.recv_timeout(Duration::from_secs(5)) {
        Ok(_) => {}
        Err(_) => panic!(
            "coalesce {coalesce} {step}: a later writer never returned after a panic at {step}"
        ),
    }
}

#[test]
fn a_panic_while_publishing_strands_no_later_writer() {
    publishing_panic(false, "publishing");
}

#[test]
fn a_panic_while_publishing_strands_no_later_coalesced_writer() {
    publishing_panic(true, "publishing");
}

#[test]
fn a_panic_once_published_strands_no_later_coalesced_writer() {
    publishing_panic(true, "published");
}

#[test]
fn a_panic_while_arriving_strands_no_later_coalesced_writer() {
    common::quiet_ferro_panics();
    let (hook, armed) = panicking_hook("arriving", "arriver");
    let dir = tempfile::tempdir().unwrap();
    let main: Db<Root> = Options::new()
        .durability(Durability::Buffered)
        .coalesce(true)
        .reclaim(Reclaim { every_bytes: 0 })
        .hook(hook)
        .open(dir.path().join("a.ferro"))
        .unwrap();
    main.update(|r| {
        r.insert(0, 0);
    })
    .unwrap();
    armed.store(true, Ordering::SeqCst);
    let arriver = spawn("arriver", &main, 1);
    let _ = arriver.recv_timeout(Duration::from_secs(10));
    let later = spawn("later", &main, 2);
    match later.recv_timeout(Duration::from_secs(5)) {
        Ok(_) => {}
        Err(_) => panic!("a later writer never returned after a panic at arriving"),
    }
}

#[test]
fn a_member_whose_batch_a_panicking_leader_failed_is_not_acknowledged() {
    common::quiet_ferro_panics();
    for durability in [Durability::Buffered, Durability::Fsync] {
        let gates = Arc::new(Gates::default());
        let hook = {
            let gates = gates.clone();
            Hook(Arc::new(move |step| {
                let me = thread::current().name().unwrap_or("").to_string();
                match (step, me.as_str()) {
                    ("leading", "L") => {
                        gates.hold("L");
                    }
                    ("drafted", "TA") => {
                        gates.hold("TA-drafted");
                    }
                    ("landed", "TA") => {
                        gates.hold("TA-landed");
                    }
                    ("leading", "N") => {
                        if gates.hold("N") {
                            panic!("ferro: probe leader panic");
                        }
                    }
                    _ => {}
                }
            }))
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.ferro");
        let main: Db<Root> = Options::new()
            .durability(durability)
            .coalesce(true)
            .reclaim(Reclaim { every_bytes: 0 })
            .hook(hook)
            .open(&path)
            .unwrap();
        main.update(|r| {
            r.insert(0, 0);
        })
        .unwrap();
        let a = main.fork("a").unwrap();
        let b = main.fork("b").unwrap();
        let c = main.fork("c").unwrap();
        let z = main.fork("z").unwrap();

        gates.arm("L");
        let l = spawn("L", &z, 1);
        gates.reached("L");
        let ta = spawn("TA", &a, 2);
        thread::sleep(Duration::from_millis(100));
        let tb = spawn("TB", &b, 3);
        thread::sleep(Duration::from_millis(100));
        gates.arm("TA-drafted");
        gates.release("L");
        l.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        gates.reached("TA-drafted");
        gates.arm("N");
        let n = spawn("N", &c, 4);
        gates.reached("N");
        gates.arm("TA-landed");
        gates.release("TA-drafted");
        gates.reached("TA-landed");
        gates.release("N");
        let _ = n.recv_timeout(Duration::from_secs(10));
        let tb_done = tb.recv_timeout(Duration::from_secs(10)).unwrap();
        gates.release("TA-landed");
        let ta_done = ta.recv_timeout(Duration::from_secs(10)).unwrap();
        let after = main.update(|_| ());
        assert!(
            matches!(ta_done, Err(Error::Poisoned)),
            "{durability:?}: TA {ta_done:?} after its batch's leader panicked (TB {tb_done:?})"
        );
        assert!(matches!(after, Err(Error::Poisoned)));
    }
}
