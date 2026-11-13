mod common;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read};
use std::path::Path;
use std::sync::Arc;

use common::{Rng, fast, text};
use ferro::store::{Op, Trace};
use ferro::{Db, Error, Map, Options, Outcome, Snapshot};
use parking_lot::Mutex;
use tempfile::TempDir;

type Root = Map<u64, String>;

const KEYS: u64 = 2_000;
const VALUE: usize = 1_500;
const SPILL: usize = 1 << 20;

fn refuse(_: &mut Root, _: &Snapshot<Root>, _: &Snapshot<Root>) -> (String, ()) {
    panic!("no merge expected")
}

fn fill(db: &Db<Root>, from: u64, to: u64) {
    let rounds = 4;
    let step = (to - from).div_ceil(rounds);
    for round in 0..rounds {
        let start = from + round * step;
        let end = (start + step).min(to);
        db.commit(format!("keys {start} to {end}"), |root| {
            for key in start..end {
                root.insert(key, text(key, VALUE));
            }
        })
        .unwrap();
    }
}

fn file_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap()
}

fn dump(root: &Root) -> BTreeMap<u64, String> {
    root.iter()
        .map(|(key, value)| (key, (*value).clone()))
        .collect()
}

struct Case {
    dir: TempDir,
    a: Db<Root>,
    bundle: Vec<u8>,
}

impl Case {
    fn new() -> Case {
        let dir = tempfile::tempdir().unwrap();
        let a: Db<Root> = fast().open(dir.path().join("a.ferro")).unwrap();
        let b: Db<Root> = fast().open(dir.path().join("b.ferro")).unwrap();
        fill(&a, 0, 10);
        assert_eq!(
            b.pull(&a, "a", refuse).unwrap().outcome,
            Outcome::FastForward
        );
        fill(&a, 10, KEYS);
        let bundle = a.bundle(&b.have().unwrap()).unwrap().into();
        Case { dir, a, bundle }
    }

    fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("b.ferro")
    }

    fn b(&self) -> Db<Root> {
        fast().open(self.path()).unwrap()
    }
}

#[test]
fn a_bundle_written_to_a_stream_matches_the_one_in_memory_and_lands_the_same() {
    let case = Case::new();
    let b = case.b();
    let have = b.have().unwrap();
    let path = case.dir.path().join("bundle");
    let written = case
        .a
        .bundle_to(&have, BufWriter::new(File::create(&path).unwrap()))
        .unwrap();
    assert_eq!(file_bytes(&path), case.bundle);
    assert_eq!(written, case.bundle.len() as u64);
    assert!(case.bundle.len() > 2 * SPILL, "{} bytes", case.bundle.len());
    let input = BufReader::new(File::open(&path).unwrap());
    let pulled = b.receive_from("a", input, refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(pulled.received_bytes, written);
    assert_eq!(b.head_hash(), case.a.head_hash());
    assert_eq!(dump(&b.read()), dump(&case.a.read()));
    b.verify_typed().unwrap();
    drop(b);
    let reopened = case.b();
    assert_eq!(reopened.head_hash(), case.a.head_hash());
    reopened.verify_typed().unwrap();
}

#[cfg(target_os = "linux")]
fn allocated(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).unwrap().blocks() * 512
}

#[test]
fn a_stream_refused_after_it_spilled_leaves_the_file_as_it_was() {
    let case = Case::new();
    let path = case.path();
    let before = file_bytes(&path);
    let b = case.b();
    let (head, size) = (b.head_hash(), b.log_bytes());
    #[cfg(target_os = "linux")]
    let taken = allocated(&path);
    let mut damaged = case.bundle.clone();
    let at = damaged.len() - 40;
    damaged[at] ^= 0x20;
    let cut = &case.bundle[..case.bundle.len() - 3];
    for bytes in [&damaged[..], cut] {
        match b.receive_from("a", bytes, refuse) {
            Err(Error::Bundle(_)) => {}
            other => panic!("accepted a damaged stream: {other:?}"),
        }
        assert_eq!(b.head_hash(), head);
        assert_eq!(b.log_bytes(), size);
        #[cfg(target_os = "linux")]
        assert!(
            allocated(&path) < taken + (64 << 10),
            "a refused bundle kept {} bytes of disk",
            allocated(&path) - taken
        );
    }
    b.verify().unwrap();
    drop(b);
    assert_eq!(file_bytes(&path), before);
    let b = case.b();
    let pulled = b.receive_from("a", &case.bundle[..], refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    b.verify_typed().unwrap();
}

struct Failing<R> {
    inner: R,
    left: usize,
}

impl<R: Read> Read for Failing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "the peer went away",
            ));
        }
        let take = buf.len().min(self.left);
        let got = self.inner.read(&mut buf[..take])?;
        self.left -= got;
        Ok(got)
    }
}

#[test]
fn a_stream_that_fails_part_way_returns_its_error_and_writes_nothing() {
    let case = Case::new();
    let path = case.path();
    let before = file_bytes(&path);
    let b = case.b();
    let head = b.head_hash();
    let input = Failing {
        inner: &case.bundle[..],
        left: case.bundle.len() * 3 / 4,
    };
    match b.receive_from("a", input, refuse) {
        Err(Error::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::ConnectionReset),
        other => panic!("a failed stream landed: {other:?}"),
    }
    assert_eq!(b.head_hash(), head);
    drop(b);
    assert_eq!(file_bytes(&path), before);
}

#[test]
fn a_damaged_sender_fails_the_pull_with_its_own_error() {
    let case = Case::new();
    let source = case.dir.path().join("a.ferro");
    let Case { dir, a, .. } = case;
    drop(a);
    let mut bytes = file_bytes(&source);
    let value = text(KEYS / 2, VALUE).into_bytes();
    let at = bytes
        .windows(value.len())
        .position(|window| window == value)
        .expect("the value is stored uncompressed");
    bytes[at + VALUE / 2] ^= 0x01;
    fs::write(&source, &bytes).unwrap();
    let a: Db<Root> = fast().open(&source).unwrap();
    let b: Db<Root> = fast().open(dir.path().join("fresh.ferro")).unwrap();
    match b.pull(&a, "a", refuse) {
        Err(Error::Corrupt(message)) => assert!(message.contains("hash mismatch"), "{message}"),
        other => panic!("pulled from a damaged file: {other:?}"),
    }
    assert_eq!(b.version(), 0);
    assert_eq!(b.branches(), vec!["main".to_string()]);
}

fn apply(image: &mut Vec<u8>, op: &Op) {
    match op {
        Op::Write(_, at, bytes) => {
            let end = *at as usize + bytes.len();
            if image.len() < end {
                image.resize(end, 0);
            }
            image[*at as usize..end].copy_from_slice(bytes);
        }
        Op::SetLen(_, len) => image.resize(*len as usize, 0),
        _ => {}
    }
}

#[test]
fn a_power_cut_at_any_write_of_a_spilling_receive_keeps_a_database_that_opens() {
    let case = Case::new();
    let path = case.path();
    let old = {
        let b = case.b();
        (b.head_hash(), b.branch("a").unwrap().head_hash())
    };
    let base = file_bytes(&path);
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let b: Db<Root> = Options::new().trace(trace.clone()).open(&path).unwrap();
    let pulled = b.receive_from("a", &case.bundle[..], refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    b.trace_op(Op::Acked(1));
    let new = (b.head_hash(), dump(&b.read()));
    drop(b);
    let ops = trace.lock().clone();
    let writes = ops.iter().filter(|op| matches!(op, Op::Write(..))).count();
    assert!(writes > 4, "{writes} writes");
    let acked = ops
        .iter()
        .position(|op| matches!(op, Op::Acked(_)))
        .unwrap();
    let target = case.dir.path().join("cut.ferro");
    let mut rng = Rng::new(17);
    let mut states = BTreeMap::new();
    for cut in 0..=ops.len() {
        for loss in 0..4 {
            let mut image = base.clone();
            let mut pending = Vec::new();
            for op in &ops[..cut] {
                match op {
                    Op::Write(..) | Op::SetLen(..) => pending.push(op),
                    Op::Sync(_) => {
                        for done in pending.drain(..) {
                            apply(&mut image, done);
                        }
                    }
                    _ => {}
                }
            }
            for op in pending {
                let keep = match loss {
                    0 => false,
                    1 => true,
                    _ => rng.chance(50),
                };
                if keep {
                    apply(&mut image, op);
                }
            }
            fs::write(&target, &image).unwrap();
            let db = fast()
                .open::<Root>(&target)
                .unwrap_or_else(|error| panic!("cut {cut} loss {loss}: {error}"));
            db.verify()
                .unwrap_or_else(|error| panic!("cut {cut} loss {loss}: {error}"));
            let remote = db.branch("a").unwrap().head_hash();
            let state = if db.head_hash() == old.0 {
                assert!(
                    cut < acked,
                    "cut {cut} loss {loss} lost an acknowledged pull"
                );
                if remote == old.1 {
                    "previous"
                } else {
                    assert_eq!(remote, new.0, "cut {cut} loss {loss}");
                    "landed"
                }
            } else {
                assert_eq!(db.head_hash(), new.0, "cut {cut} loss {loss}");
                assert_eq!(dump(&db.read()), new.1, "cut {cut} loss {loss}");
                "forwarded"
            };
            *states.entry(state).or_insert(0) += 1;
            let size = fs::metadata(&target).unwrap().len();
            assert_eq!(
                size,
                db.log_bytes(),
                "cut {cut} loss {loss} kept a partial bundle"
            );
        }
    }
    assert_eq!(states.len(), 3, "{states:?}");
}

#[test]
fn a_pull_large_enough_to_pipe_lands_whole_and_resends_nothing() {
    let case = Case::new();
    let b = case.b();
    let pulled = b.pull(&case.a, "a", refuse).unwrap();
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(pulled.received_bytes, case.bundle.len() as u64);
    assert!(pulled.objects > 1_000, "{pulled:?}");
    assert_eq!(b.head_hash(), case.a.head_hash());
    b.verify_typed().unwrap();
    let again = b.pull(&case.a, "a", refuse).unwrap();
    assert_eq!(again.outcome, Outcome::UpToDate);
    assert_eq!(again.objects, 0);
}
