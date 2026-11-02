use std::collections::HashMap;

use ferro::{Change, Db};

use crate::error::Result;
use crate::model::{Ledger, PostingKey};
use crate::money::format_amount;

pub struct VersionCheck {
    pub version: u64,
    pub totals: Vec<(String, i128, i128)>,
    pub stored_net: Vec<(String, i128)>,
}

pub struct AuditReport {
    pub version: u64,
    pub entries: u64,
    pub postings: u64,
    pub accounts: usize,
    pub checked: Vec<VersionCheck>,
    pub problems: Vec<String>,
}

fn add(totals: &mut Vec<(String, i128, i128)>, currency: &str, amount: i128) {
    let slot = match totals.iter().position(|(name, _, _)| name == currency) {
        Some(slot) => slot,
        None => {
            totals.push((currency.to_string(), 0, 0));
            totals.len() - 1
        }
    };
    if amount > 0 {
        totals[slot].1 += amount;
    } else {
        totals[slot].2 -= amount;
    }
}

fn sample_versions(head: u64, samples: usize) -> Vec<u64> {
    if head == 0 {
        return Vec::new();
    }
    if samples == 0 || samples as u64 >= head {
        return (1..=head).collect();
    }
    let mut versions: Vec<u64> = (1..=samples as u64)
        .map(|i| (i * head / samples as u64).max(1))
        .collect();
    versions.dedup();
    versions
}

pub fn audit(db: &Db<Ledger>, samples: usize) -> Result<AuditReport> {
    let head = db.read();
    let ledger: &Ledger = &head;
    let mut problems: Vec<String> = Vec::new();
    let currencies: HashMap<u32, String> = ledger
        .accounts
        .iter()
        .map(|account| (account.id, account.currency.clone()))
        .collect();
    let mut recomputed: HashMap<u32, i128> = HashMap::new();
    let mut expected: Vec<(PostingKey, i64)> = Vec::new();
    let mut entries = 0u64;
    let mut last_id = 0u64;
    for entry in ledger.entries.iter() {
        entries += 1;
        last_id = entry.id;
        let mut sums: Vec<(String, i128, i128)> = Vec::new();
        for (index, line) in entry.lines.iter().enumerate() {
            let Some(currency) = currencies.get(&line.account) else {
                problems.push(format!(
                    "entry #{} uses unknown account {}",
                    entry.id, line.account
                ));
                continue;
            };
            add(&mut sums, currency, i128::from(line.amount));
            *recomputed.entry(line.account).or_default() += i128::from(line.amount);
            expected.push((
                (line.account, entry.date, entry.id, index as u16),
                line.amount,
            ));
        }
        for (currency, debits, credits) in &sums {
            if debits != credits {
                problems.push(format!(
                    "entry #{} unbalanced in {currency}: debits {} credits {}",
                    entry.id,
                    format_amount(*debits, currency),
                    format_amount(*credits, currency)
                ));
            }
        }
        if let Some(original) = entry.reverses {
            match ledger.reversed.get(&original) {
                Some(by) if *by == entry.id => {}
                other => problems.push(format!(
                    "entry #{} claims to reverse #{original} but reversal map says {:?}",
                    entry.id,
                    other.map(|by| *by)
                )),
            }
        }
    }
    expected.sort_unstable();
    let mut stored = ledger.postings.iter();
    let mut postings = 0u64;
    let mut reported = 0;
    let mut mismatch = |text: String, problems: &mut Vec<String>| {
        reported += 1;
        if reported <= 20 {
            problems.push(text);
        }
    };
    let mut wanted = expected.iter();
    let mut left = wanted.next();
    let mut right = stored.next();
    while left.is_some() || right.is_some() {
        match (left, &right) {
            (Some((key, amount)), Some((stored_key, stored_amount))) => {
                if key == stored_key {
                    if amount != &**stored_amount {
                        mismatch(
                            format!(
                                "posting {key:?} journal {amount} stored {}",
                                **stored_amount
                            ),
                            &mut problems,
                        );
                    }
                    left = wanted.next();
                    right = stored.next();
                    postings += 1;
                } else if key < stored_key {
                    mismatch(
                        format!("posting {key:?} in journal but not indexed"),
                        &mut problems,
                    );
                    left = wanted.next();
                } else {
                    mismatch(
                        format!("posting {stored_key:?} indexed but not in journal"),
                        &mut problems,
                    );
                    right = stored.next();
                    postings += 1;
                }
            }
            (Some((key, _)), None) => {
                mismatch(
                    format!("posting {key:?} in journal but not indexed"),
                    &mut problems,
                );
                left = wanted.next();
            }
            (None, Some((stored_key, _))) => {
                mismatch(
                    format!("posting {stored_key:?} indexed but not in journal"),
                    &mut problems,
                );
                right = stored.next();
                postings += 1;
            }
            (None, None) => {}
        }
    }
    if reported > 20 {
        problems.push(format!("... {} more posting mismatches", reported - 20));
    }
    for account in ledger.accounts.iter() {
        let sum = recomputed.get(&account.id).copied().unwrap_or(0);
        if sum != i128::from(account.balance) {
            problems.push(format!(
                "account {} stored balance {} but journal recomputes {}",
                account.name,
                format_amount(i128::from(account.balance), &account.currency),
                format_amount(sum, &account.currency)
            ));
        }
        if ledger.names.get(account.name.as_str()).map(|id| *id) != Some(account.id) {
            problems.push(format!("name index disagrees for account {}", account.name));
        }
    }
    if ledger.names.len() != ledger.accounts.len() {
        problems.push("name index and account table differ in size".to_string());
    }
    if ledger.next_entry != last_id || ledger.next_entry != entries {
        problems.push(format!(
            "entry counter {} but {entries} entries, last id {last_id}",
            ledger.next_entry
        ));
    }
    for (original, reversal) in ledger.reversed.iter() {
        match ledger.entries.get(&*reversal) {
            Some(row) if row.reverses == Some(original) => {}
            _ => problems.push(format!(
                "reversal map #{original} -> #{reversal} is dangling"
            )),
        }
    }
    let mut checked = Vec::new();
    let mut totals: Vec<(String, i128, i128)> = Vec::new();
    let mut previous = db.at(0)?;
    for version in sample_versions(head.version(), samples) {
        let snapshot = db.at(version)?;
        for change in previous.postings.diff(&snapshot.postings) {
            match change {
                Change::Added(key, amount) => {
                    if let Some(currency) = currencies.get(&key.0) {
                        add(&mut totals, currency, i128::from(*amount));
                    }
                }
                Change::Removed(key, amount) => {
                    if let Some(currency) = currencies.get(&key.0) {
                        add(&mut totals, currency, -i128::from(*amount));
                    }
                }
                Change::Changed(key, before, after) => {
                    if let Some(currency) = currencies.get(&key.0) {
                        add(&mut totals, currency, -i128::from(*before));
                        add(&mut totals, currency, i128::from(*after));
                    }
                }
            }
        }
        let mut net: Vec<(String, i128)> = Vec::new();
        for account in snapshot.accounts.iter() {
            match net.iter_mut().find(|(name, _)| *name == account.currency) {
                Some((_, total)) => *total += i128::from(account.balance),
                None => net.push((account.currency.clone(), i128::from(account.balance))),
            }
        }
        net.sort();
        for (currency, debits, credits) in &totals {
            if debits != credits {
                problems.push(format!(
                    "v{version}: {currency} debits {} != credits {}",
                    format_amount(*debits, currency),
                    format_amount(*credits, currency)
                ));
            }
        }
        for (currency, total) in &net {
            if *total != 0 {
                problems.push(format!(
                    "v{version}: stored {currency} balances net {}",
                    format_amount(*total, currency)
                ));
            }
        }
        checked.push(VersionCheck {
            version,
            totals: totals.clone(),
            stored_net: net,
        });
        previous = snapshot;
    }
    Ok(AuditReport {
        version: head.version(),
        entries,
        postings,
        accounts: ledger.accounts.len(),
        checked,
        problems,
    })
}
