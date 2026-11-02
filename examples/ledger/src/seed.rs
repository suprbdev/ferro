use ferro::Db;

use crate::error::Result;
use crate::model::{Kind, Ledger};
use crate::money::{days_from_civil, now_ms};
use crate::ops::{NewEntry, NewLine};

const USD: [(&str, Kind); 31] = [
    ("assets:cash", Kind::Asset),
    ("assets:bank-1", Kind::Asset),
    ("assets:bank-2", Kind::Asset),
    ("assets:bank-3", Kind::Asset),
    ("assets:bank-4", Kind::Asset),
    ("assets:receivable", Kind::Asset),
    ("liabilities:payable", Kind::Liability),
    ("liabilities:card", Kind::Liability),
    ("liabilities:tax", Kind::Liability),
    ("liabilities:loan", Kind::Liability),
    ("equity:capital", Kind::Equity),
    ("equity:retained", Kind::Equity),
    ("income:sales-1", Kind::Income),
    ("income:sales-2", Kind::Income),
    ("income:sales-3", Kind::Income),
    ("income:sales-4", Kind::Income),
    ("income:sales-5", Kind::Income),
    ("income:interest", Kind::Income),
    ("expense:rent", Kind::Expense),
    ("expense:payroll", Kind::Expense),
    ("expense:software", Kind::Expense),
    ("expense:travel", Kind::Expense),
    ("expense:marketing", Kind::Expense),
    ("expense:supplies", Kind::Expense),
    ("expense:utilities", Kind::Expense),
    ("expense:fees", Kind::Expense),
    ("expense:insurance", Kind::Expense),
    ("expense:meals", Kind::Expense),
    ("expense:legal", Kind::Expense),
    ("expense:repairs", Kind::Expense),
    ("expense:training", Kind::Expense),
];

const EUR: [(&str, Kind); 6] = [
    ("assets:bank-eur", Kind::Asset),
    ("assets:cash-eur", Kind::Asset),
    ("liabilities:vat-eur", Kind::Liability),
    ("equity:capital-eur", Kind::Equity),
    ("income:sales-eur", Kind::Income),
    ("expense:ops-eur", Kind::Expense),
];

const BANKS: [&str; 4] = [
    "assets:bank-1",
    "assets:bank-2",
    "assets:bank-3",
    "assets:bank-4",
];
const SALES: [&str; 5] = [
    "income:sales-1",
    "income:sales-2",
    "income:sales-3",
    "income:sales-4",
    "income:sales-5",
];
const EXPENSES: [&str; 12] = [
    "expense:rent",
    "expense:software",
    "expense:travel",
    "expense:marketing",
    "expense:supplies",
    "expense:utilities",
    "expense:fees",
    "expense:insurance",
    "expense:meals",
    "expense:legal",
    "expense:repairs",
    "expense:training",
];
const MEMOS: [&str; 8] = [
    "invoice",
    "order",
    "subscription",
    "refund adjustment",
    "monthly charge",
    "contractor",
    "hardware",
    "service call",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }

    fn amount(&mut self) -> i64 {
        100 + (self.next() % 250_000) as i64
    }
}

fn make_entry(rng: &mut Rng, date: i32) -> NewEntry {
    let memo = format!("{} {}", rng.pick(&MEMOS), rng.next() % 100_000);
    let amount = rng.amount();
    let lines = match rng.below(100) {
        0..35 => {
            let tax = amount / 10;
            let asset = match rng.below(10) {
                0 => "assets:cash",
                1..4 => "assets:receivable",
                _ => rng.pick(&BANKS),
            };
            vec![
                NewLine::new(asset, amount + tax),
                NewLine::new(rng.pick(&SALES), -amount),
                NewLine::new("liabilities:tax", -tax),
            ]
        }
        35..65 => {
            let source = match rng.below(10) {
                0..3 => "liabilities:card",
                3..5 => "liabilities:payable",
                _ => rng.pick(&BANKS),
            };
            vec![
                NewLine::new(rng.pick(&EXPENSES), amount),
                NewLine::new(source, -amount),
            ]
        }
        65..80 => {
            let from = rng.pick(&BANKS);
            let mut to = rng.pick(&BANKS);
            if to == from {
                to = "assets:cash";
            }
            vec![NewLine::new(to, amount), NewLine::new(from, -amount)]
        }
        80..85 => {
            let tax = amount / 4;
            let net = amount - tax;
            let half = net / 2;
            vec![
                NewLine::new("expense:payroll", amount),
                NewLine::new(rng.pick(&BANKS), -half),
                NewLine::new(rng.pick(&BANKS), -(net - half)),
                NewLine::new("liabilities:tax", -tax),
            ]
        }
        85..90 => vec![
            NewLine::new(rng.pick(&BANKS), amount),
            NewLine::new("assets:receivable", -amount),
        ],
        90..95 => vec![
            NewLine::new("assets:bank-eur", amount),
            NewLine::new("income:sales-eur", -amount),
        ],
        _ => vec![
            NewLine::new("expense:ops-eur", amount),
            NewLine::new("assets:bank-eur", -amount),
        ],
    };
    NewEntry::new(date, memo, lines)
}

fn setup(ledger: &mut Ledger, date: i32, now: u64) -> Result<()> {
    if ledger.accounts.key_by_name("assets:cash").is_some() {
        return Ok(());
    }
    for (name, kind) in USD {
        ledger.add_account(name, kind, "USD")?;
    }
    for (name, kind) in EUR {
        ledger.add_account(name, kind, "EUR")?;
    }
    for bank in BANKS.iter().chain(["assets:cash"].iter()) {
        ledger.post(
            NewEntry::transfer(date, "opening balance", "equity:capital", bank, 500_000_000),
            now,
        )?;
    }
    ledger.post(
        NewEntry::transfer(
            date,
            "opening balance",
            "equity:capital-eur",
            "assets:bank-eur",
            500_000_000,
        ),
        now,
    )?;
    Ok(())
}

pub fn run(db: &Db<Ledger>, n: u64, batch: u64) -> Result<()> {
    let now = now_ms();
    let start = days_from_civil(2023, 1, 1) as i32;
    let first_day = db
        .read()
        .locked_through
        .map_or(start, |locked| start.max(locked + 1));
    db.try_commit_with::<_, _, crate::LedgerError>(|ledger| {
        setup(ledger, first_day, now)?;
        Ok(("seed chart of accounts".to_string(), ()))
    })?;
    let span = 1095u64;
    let mut done = 0u64;
    while done < n {
        let size = batch.max(1).min(n - done);
        let from = done;
        db.try_commit_with::<_, _, crate::LedgerError>(|ledger| {
            let mut rng = Rng((from + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            for index in from..from + size {
                let date = first_day + (index * span / n.max(1)) as i32;
                ledger.post(make_entry(&mut rng, date), now)?;
            }
            Ok((format!("seed entries {}..{}", from + 1, from + size), ()))
        })?;
        done += size;
    }
    Ok(())
}
