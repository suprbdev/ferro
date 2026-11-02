use std::sync::Arc;

use crate::error::{LedgerError, Result};
use crate::model::{Account, Entry, Kind, Ledger, Line, PostingKey};
use crate::money::{format_amount, format_date};

#[derive(Clone, Debug)]
pub struct NewLine {
    pub account: String,
    pub amount: i64,
    pub currency: Option<String>,
}

impl NewLine {
    pub fn new(account: impl Into<String>, amount: i64) -> Self {
        NewLine {
            account: account.into(),
            amount,
            currency: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NewEntry {
    pub date: i32,
    pub memo: String,
    pub lines: Vec<NewLine>,
    pub adjusts: Option<i32>,
    pub reverses: Option<u64>,
}

impl NewEntry {
    pub fn new(date: i32, memo: impl Into<String>, lines: Vec<NewLine>) -> Self {
        NewEntry {
            date,
            memo: memo.into(),
            lines,
            adjusts: None,
            reverses: None,
        }
    }

    pub fn transfer(date: i32, memo: impl Into<String>, from: &str, to: &str, amount: i64) -> Self {
        NewEntry::new(
            date,
            memo,
            vec![
                NewLine::new(to, amount),
                NewLine::new(from, amount.saturating_neg()),
            ],
        )
    }
}

pub struct StatementRow {
    pub date: i32,
    pub entry: u64,
    pub memo: String,
    pub amount: i64,
    pub balance: i128,
}

pub struct Statement {
    pub opening: i128,
    pub rows: Vec<StatementRow>,
    pub closing: i128,
}

fn bucket(sums: &mut Vec<(String, i128, i128)>, currency: &str) -> usize {
    if let Some(index) = sums.iter().position(|(name, _, _)| name == currency) {
        return index;
    }
    sums.push((currency.to_string(), 0, 0));
    sums.len() - 1
}

impl Ledger {
    pub fn account_id(&self, name: &str) -> Result<u32> {
        self.accounts
            .key_by_name(name)
            .ok_or_else(|| LedgerError::NoAccount(name.to_string()))
    }

    pub fn account(&self, name: &str) -> Result<Arc<Account>> {
        self.accounts
            .by_name(name)
            .ok_or_else(|| LedgerError::NoAccount(name.to_string()))
    }

    pub fn add_account(&mut self, name: &str, kind: Kind, currency: &str) -> Result<u32> {
        if name.is_empty() || name.contains('=') || name.chars().any(char::is_whitespace) {
            return Err(LedgerError::Usage(format!("invalid account name `{name}`")));
        }
        if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(LedgerError::Usage(format!(
                "invalid currency `{currency}`, expected three capital letters"
            )));
        }
        let id = self.next_account + 1;
        self.accounts
            .try_insert(Account {
                id,
                name: name.to_string(),
                kind,
                currency: currency.to_string(),
                closed: false,
                balance: 0,
            })
            .map_err(|_| LedgerError::AccountExists(name.to_string()))?;
        self.next_account = id;
        Ok(id)
    }

    pub fn close_account(&mut self, name: &str) -> Result<()> {
        let account = self.account(name)?;
        if account.balance != 0 {
            return Err(LedgerError::NonZeroBalance(name.to_string()));
        }
        self.accounts.update(&account.id, |row| row.closed = true);
        Ok(())
    }

    pub fn close_period(&mut self, through: i32) -> Result<()> {
        if let Some(locked) = self.locked_through
            && through <= locked
        {
            return Err(LedgerError::PeriodAlreadyClosed(format_date(locked)));
        }
        self.locked_through = Some(through);
        Ok(())
    }

    fn check_period(&self, date: i32, adjusts: Option<i32>) -> Result<()> {
        match (self.locked_through, adjusts) {
            (None, None) => Ok(()),
            (None, Some(_)) => Err(LedgerError::NoClosedPeriod),
            (Some(locked), None) if date <= locked => Err(LedgerError::PeriodClosed {
                through: format_date(locked),
                date: format_date(date),
            }),
            (Some(_), None) => Ok(()),
            (Some(locked), Some(adjusted)) => {
                if adjusted > locked || date <= locked {
                    Err(LedgerError::BadAdjustment {
                        through: format_date(locked),
                    })
                } else {
                    Ok(())
                }
            }
        }
    }

    pub fn post(&mut self, new: NewEntry, now_ms: u64) -> Result<u64> {
        if new.lines.len() < 2 {
            return Err(LedgerError::TooFewLines);
        }
        if new.lines.len() > usize::from(u16::MAX) {
            return Err(LedgerError::TooManyLines);
        }
        self.check_period(new.date, new.adjusts)?;
        if let Some(original) = new.reverses
            && let Some(by) = self.reversed.get(&original)
        {
            return Err(LedgerError::AlreadyReversed(original, *by));
        }
        let mut lines: Vec<Line> = Vec::with_capacity(new.lines.len());
        let mut sums: Vec<(String, i128, i128)> = Vec::new();
        let mut deltas: Vec<(u32, i64)> = Vec::new();
        for line in &new.lines {
            if line.amount == 0 {
                return Err(LedgerError::ZeroAmount);
            }
            let account = self.account(&line.account)?;
            if account.closed {
                return Err(LedgerError::AccountClosed(account.name.clone()));
            }
            if let Some(given) = &line.currency
                && *given != account.currency
            {
                return Err(LedgerError::CurrencyMismatch {
                    account: account.name.clone(),
                    expected: account.currency.clone(),
                    given: given.clone(),
                });
            }
            let slot = bucket(&mut sums, &account.currency);
            if line.amount > 0 {
                sums[slot].1 += i128::from(line.amount);
            } else {
                sums[slot].2 += i128::from(line.amount).abs();
            }
            match deltas.iter_mut().find(|(id, _)| *id == account.id) {
                Some((_, total)) => {
                    *total = total
                        .checked_add(line.amount)
                        .ok_or(LedgerError::Overflow)?
                }
                None => deltas.push((account.id, line.amount)),
            }
            lines.push(Line {
                account: account.id,
                amount: line.amount,
            });
        }
        for (currency, debits, credits) in &sums {
            if debits != credits {
                return Err(LedgerError::Unbalanced {
                    currency: currency.clone(),
                    debits: format_amount(*debits, currency),
                    credits: format_amount(*credits, currency),
                });
            }
        }
        let mut balances: Vec<(u32, i64)> = Vec::with_capacity(deltas.len());
        for (id, delta) in &deltas {
            let account = self
                .accounts
                .get(id)
                .ok_or_else(|| LedgerError::NoAccount(id.to_string()))?;
            let next = account
                .balance
                .checked_add(*delta)
                .ok_or(LedgerError::Overflow)?;
            balances.push((*id, next));
        }
        self.next_entry += 1;
        let id = self.next_entry;
        for (index, line) in lines.iter().enumerate() {
            let key: PostingKey = (line.account, new.date, id, index as u16);
            self.postings.insert(key, line.amount);
        }
        for (account, balance) in balances {
            self.accounts.update(&account, |row| row.balance = balance);
        }
        if let Some(original) = new.reverses {
            self.reversed.insert(original, id);
        }
        self.entries.insert(Entry {
            id,
            date: new.date,
            memo: new.memo,
            lines,
            posted_ms: now_ms,
            reverses: new.reverses,
            adjusts: new.adjusts,
        });
        Ok(id)
    }

    pub fn reverse(
        &mut self,
        id: u64,
        date: i32,
        memo: Option<String>,
        now_ms: u64,
    ) -> Result<u64> {
        let original = self.entries.get(&id).ok_or(LedgerError::NoEntry(id))?;
        let lines = original
            .lines
            .iter()
            .map(|line| {
                let account = self
                    .accounts
                    .get(&line.account)
                    .ok_or_else(|| LedgerError::NoAccount(line.account.to_string()))?;
                Ok(NewLine::new(
                    account.name.clone(),
                    line.amount.checked_neg().ok_or(LedgerError::Overflow)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let memo = memo.unwrap_or_else(|| format!("reversal of #{id}: {}", original.memo));
        let mut entry = NewEntry::new(date, memo, lines);
        entry.reverses = Some(id);
        self.post(entry, now_ms)
    }

    pub fn sum_postings(&self, account: u32, from: i32, to: i32) -> i128 {
        if from > to {
            return 0;
        }
        let low: PostingKey = (account, from, 0, 0);
        let high: PostingKey = (account, to, u64::MAX, u16::MAX);
        self.postings
            .range_summary::<PostingKey, _>(low..=high)
            .map_or(0, |net| net.0)
    }

    pub fn balance_at(&self, account: &Account, as_of: Option<i32>) -> i128 {
        match as_of {
            None => i128::from(account.balance),
            Some(date) => self.sum_postings(account.id, i32::MIN, date),
        }
    }

    pub fn trial_balance(&self, as_of: Option<i32>) -> Vec<(Arc<Account>, i128)> {
        self.accounts
            .iter()
            .map(|account| {
                let balance = self.balance_at(&account, as_of);
                (account, balance)
            })
            .collect()
    }

    pub fn statement(&self, account: u32, from: i32, to: i32) -> Statement {
        let opening = if from == i32::MIN {
            0
        } else {
            self.sum_postings(account, i32::MIN, from - 1)
        };
        let mut balance = opening;
        let mut rows = Vec::new();
        if from <= to {
            let low: PostingKey = (account, from, 0, 0);
            let high: PostingKey = (account, to, u64::MAX, u16::MAX);
            for ((_, date, entry, _), amount) in self.postings.range::<PostingKey, _>(low..=high) {
                balance += i128::from(*amount);
                let memo = self
                    .entries
                    .get(&entry)
                    .map(|row| row.memo.clone())
                    .unwrap_or_default();
                rows.push(StatementRow {
                    date,
                    entry,
                    memo,
                    amount: *amount,
                    balance,
                });
            }
        }
        Statement {
            opening,
            rows,
            closing: balance,
        }
    }
}

pub fn net_by_currency(balances: &[(Arc<Account>, i128)]) -> Vec<(String, i128)> {
    let mut totals: Vec<(String, i128)> = Vec::new();
    for (account, balance) in balances {
        match totals
            .iter_mut()
            .find(|(name, _)| *name == account.currency)
        {
            Some((_, total)) => *total += balance,
            None => totals.push((account.currency.clone(), *balance)),
        }
    }
    totals.sort();
    totals
}
