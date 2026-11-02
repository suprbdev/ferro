use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use ferro::{Db, Options};
use ledger::audit::audit;
use ledger::money::now_ms;
use ledger::ops::{NewEntry, NewLine};
use ledger::{Kind, Ledger, LedgerError};

const THREADS: usize = 16;
const PER_THREAD: u64 = 120;
const ACCOUNTS: usize = 12;

fn names() -> Vec<String> {
    (0..ACCOUNTS).map(|i| format!("assets:a{i}")).collect()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn nets_to_zero(ledger: &Ledger) {
    let mut net: Vec<(String, i128)> = Vec::new();
    for account in ledger.accounts.iter() {
        match net.iter_mut().find(|(c, _)| *c == account.currency) {
            Some((_, total)) => *total += i128::from(account.balance),
            None => net.push((account.currency.clone(), i128::from(account.balance))),
        }
    }
    for (currency, total) in net {
        assert_eq!(total, 0, "stored {currency} balances do not net to zero");
    }
    let (mut debits, mut credits) = (0i128, 0i128);
    for (_, amount) in ledger.postings.iter() {
        if *amount > 0 {
            debits += i128::from(*amount);
        } else {
            credits -= i128::from(*amount);
        }
    }
    assert_eq!(debits, credits, "postings debits != credits");
}

#[test]
fn sixteen_writers_and_a_checking_reader() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("c.ferro");
    let db: Db<Ledger> = Options::new().open(&path).unwrap();
    let names = Arc::new(names());
    db.try_commit_with::<_, _, LedgerError>(|ledger| {
        for name in names.iter() {
            ledger.add_account(name, Kind::Asset, "USD")?;
        }
        ledger.add_account("equity:capital", Kind::Equity, "USD")?;
        for name in names.iter() {
            ledger.post(
                NewEntry::transfer(19_000, "fund", "equity:capital", name, 1_000_000),
                now_ms(),
            )?;
        }
        Ok(("setup".to_string(), ()))
    })
    .unwrap();
    let setup_version = db.version();

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let reader = {
        let db = db.clone();
        let stop = stop.clone();
        let reads = reads.clone();
        thread::spawn(move || {
            let mut last = 0;
            while !stop.load(Ordering::Acquire) {
                let snapshot = db.read();
                assert!(snapshot.version() >= last, "version went backwards");
                last = snapshot.version();
                nets_to_zero(&snapshot);
                reads.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    let rejected = Arc::new(AtomicU64::new(0));
    let writers: Vec<_> = (0..THREADS)
        .map(|t| {
            let db = db.clone();
            let names = names.clone();
            let rejected = rejected.clone();
            thread::spawn(move || {
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ ((t as u64 + 1) << 20));
                let mut ok = 0u64;
                for round in 0..PER_THREAD {
                    let from = (rng.next() % ACCOUNTS as u64) as usize;
                    let to = (from + 1 + (rng.next() % (ACCOUNTS as u64 - 1)) as usize) % ACCOUNTS;
                    let amount = 1 + (rng.next() % 50_000) as i64;
                    let broken = round % 10 == 9;
                    let result = db.try_commit_with::<_, _, LedgerError>(|ledger| {
                        let mut entry = NewEntry::transfer(
                            19_001 + round as i32,
                            format!("t{t} r{round}"),
                            &names[from],
                            &names[to],
                            amount,
                        );
                        if broken {
                            entry.lines.push(NewLine::new(names[to].clone(), 1));
                        }
                        let id = ledger.post(entry, now_ms())?;
                        Ok((format!("t{t} r{round}"), id))
                    });
                    match result {
                        Ok(_) => ok += 1,
                        Err(LedgerError::Unbalanced { .. }) => {
                            rejected.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(other) => panic!("unexpected error: {other}"),
                    }
                }
                ok
            })
        })
        .collect();
    let committed: u64 = writers.into_iter().map(|w| w.join().unwrap()).sum();
    stop.store(true, Ordering::Release);
    reader.join().unwrap();

    assert!(reads.load(Ordering::Relaxed) > 0);
    assert_eq!(
        committed + rejected.load(Ordering::Relaxed),
        THREADS as u64 * PER_THREAD
    );
    assert!(rejected.load(Ordering::Relaxed) > 0);
    let last = db.read();
    assert_eq!(last.version(), setup_version + committed);
    assert_eq!(last.entries.len() as u64, ACCOUNTS as u64 + committed);
    nets_to_zero(&last);
    let report = audit(&db, 0).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(report.checked.len() as u64, last.version());
    drop(last);
    drop(db);

    let reopened: Db<Ledger> = Options::new().read_only().open(&path).unwrap();
    assert_eq!(reopened.version(), setup_version + committed);
    nets_to_zero(&reopened.read());
}
