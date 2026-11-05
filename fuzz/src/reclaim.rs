use std::fs;
use std::path::Path;

use ferro::admin::{self, Keep};
use ferro::{Durability, Options};

use crate::{Opaque, file_bytes, fresh, scratch};

pub const AGAIN: u8 = 8;

type Outcome = Result<(u64, u64, u64, Vec<u8>), String>;

fn keep(mode: u8) -> Keep {
    match (mode >> 1) & 3 {
        0 => Keep::All,
        1 => Keep::Last(1),
        2 => Keep::Last(3),
        _ => Keep::Since(0),
    }
}

fn compact(path: &Path, threads: usize, keep: Keep) -> Outcome {
    let named = path.display().to_string();
    let text = |error: ferro::Error| error.to_string().replace(&named, "<file>");
    let db = Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20)
        .copy_threads(threads)
        .open::<Opaque>(path)
        .map_err(|error| format!("open: {}", text(error)))?;
    let sound = db.verify().is_ok() && db.verify_typed().is_ok();
    let report = db.compact(keep).map_err(text)?;
    drop(db);
    let bytes = fs::read(path).unwrap();
    assert_eq!(
        report.after_bytes,
        bytes.len() as u64,
        "a compacted file's size differs from its report"
    );
    if sound && let Err(error) = admin::verify(path) {
        panic!("a file that verified fails verify once compacted on {threads} threads: {error}");
    }
    Ok((
        report.objects,
        report.commits_dropped,
        report.after_bytes,
        bytes,
    ))
}

fn same(path: &Path, threads: [usize; 2], keep: Keep, round: &str) -> Outcome {
    let other = scratch().join("other.ferro");
    fs::copy(path, &other).unwrap();
    let one = compact(path, threads[0], keep);
    let many = compact(&other, threads[1], keep);
    match (&one, &many) {
        (Ok(one), Ok(many)) => {
            assert_eq!(
                (one.0, one.1, one.2),
                (many.0, many.1, many.2),
                "{round}: the copy on {threads:?} threads reports differently"
            );
            assert!(
                one.3 == many.3,
                "{round}: the copy on {threads:?} threads writes different files"
            );
        }
        (Err(one), Err(many)) => assert_eq!(
            one, many,
            "{round}: the copy on {threads:?} threads fails differently"
        ),
        _ => panic!(
            "{round}: the copy on {threads:?} threads gives {:?} and {:?}",
            one.as_ref().map(|done| done.0),
            many.as_ref().map(|done| done.0)
        ),
    }
    one
}

pub fn run(data: &[u8]) {
    let Some((mode, bytes)) = file_bytes(data) else {
        return;
    };
    let path = fresh("reclaim.ferro");
    fs::write(&path, &bytes).unwrap();
    let keep = keep(mode);
    if same(&path, [1, 4], keep, "first").is_ok() && mode & AGAIN != 0 {
        let _ = same(&path, [4, 1], Keep::Last(1), "again");
    }
}
