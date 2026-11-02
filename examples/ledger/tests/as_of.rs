use ferro::{Db, Durability, Options};
use ledger::audit::audit;
use ledger::model::PostingKey;
use ledger::ops::NewEntry;
use ledger::{Kind, Ledger};

const FIRST_DAY: i32 = 19_000;
const DAYS: u64 = 400;
const NOW: u64 = 1_700_000_000_000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }
}

fn scan(ledger: &Ledger, account: u32, from: i32, to: i32) -> i128 {
    let low: PostingKey = (account, from, 0, 0);
    let high: PostingKey = (account, to, u64::MAX, u16::MAX);
    ledger
        .postings
        .range::<PostingKey, _>(low..=high)
        .map(|(_, amount)| i128::from(*amount))
        .sum()
}

fn names() -> Vec<String> {
    (0..8).map(|index| format!("assets:a{index}")).collect()
}

fn entry(rng: &mut Rng, names: &[String]) -> NewEntry {
    let from = rng.below(names.len() as u64) as usize;
    let to = (from + 1 + rng.below(names.len() as u64 - 1) as usize) % names.len();
    let date = FIRST_DAY + rng.below(DAYS) as i32;
    let amount = 1 + rng.below(1_000_000) as i64;
    NewEntry::transfer(date, "move", &names[from], &names[to], amount)
}

fn check(ledger: &Ledger, rng: &mut Rng) {
    for account in ledger.accounts.iter() {
        for _ in 0..12 {
            let from = FIRST_DAY - 5 + rng.below(DAYS + 10) as i32;
            let to = FIRST_DAY - 5 + rng.below(DAYS + 10) as i32;
            let wanted = if from > to { 0 } else { scan(ledger, account.id, from, to) };
            assert_eq!(ledger.sum_postings(account.id, from, to), wanted);
            let closing = scan(ledger, account.id, i32::MIN, to);
            assert_eq!(ledger.balance_at(&account, Some(to)), closing);
            let opening = scan(ledger, account.id, i32::MIN, from - 1);
            let statement = ledger.statement(account.id, from, to);
            assert_eq!(statement.opening, opening);
            assert_eq!(statement.closing, opening + wanted);
        }
        assert_eq!(ledger.balance_at(&account, None), i128::from(account.balance));
        assert_eq!(
            ledger.sum_postings(account.id, i32::MIN, i32::MAX),
            i128::from(account.balance)
        );
    }
}

#[test]
fn as_of_balances_match_a_scan_with_backdated_entries_and_reversals() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("l.ferro");
    let open = || {
        Options::new()
            .durability(Durability::Buffered)
            .cache_bytes(1 << 16)
            .open::<Ledger>(&path)
            .unwrap()
    };
    let db: Db<Ledger> = open();
    let names = names();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    db.update(|ledger| {
        for name in &names {
            ledger.add_account(name, Kind::Asset, "USD").unwrap();
        }
    })
    .unwrap();
    for round in 0..60 {
        db.update(|ledger| {
            for _ in 0..40 {
                ledger.post(entry(&mut rng, &names), NOW).unwrap();
            }
            let newest = ledger.next_entry;
            ledger.reverse(newest, FIRST_DAY + rng.below(DAYS) as i32, None, NOW).unwrap();
            if round % 7 == 0 {
                check(ledger, &mut rng);
            }
        })
        .unwrap();
        if round % 13 == 0 {
            check(&db.read(), &mut rng);
        }
    }
    drop(db);
    let db = open();
    check(&db.read(), &mut rng);
    let report = audit(&db, 8).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}

#[test]
fn an_as_of_total_past_the_range_of_i64_stays_exact() {
    let dir = tempfile::tempdir().unwrap();
    let db: Db<Ledger> = Options::new()
        .durability(Durability::Buffered)
        .open(dir.path().join("w.ferro"))
        .unwrap();
    let (a, b) = ("assets:a", "assets:b");
    db.update(|ledger| {
        ledger.add_account(a, Kind::Asset, "USD").unwrap();
        ledger.add_account(b, Kind::Asset, "USD").unwrap();
        ledger.post(NewEntry::transfer(30, "up", b, a, i64::MAX), NOW).unwrap();
        ledger.post(NewEntry::transfer(50, "down", a, b, i64::MAX), NOW).unwrap();
        ledger.post(NewEntry::transfer(20, "up", b, a, i64::MAX), NOW).unwrap();
    })
    .unwrap();
    let snapshot = db.read();
    let account = snapshot.account(a).unwrap();
    assert_eq!(snapshot.balance_at(&account, Some(40)), 2 * i128::from(i64::MAX));
    assert_eq!(snapshot.balance_at(&account, Some(60)), i128::from(i64::MAX));
}
