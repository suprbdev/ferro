use std::io::Write;
use std::time::Instant;

use ferro::{Change, Committed, Db};
use ledger::audit::audit;
use ledger::money::{
    format_amount, format_date, format_ms, now_ms, parse_amount, parse_date, today,
};
use ledger::ops::{NewEntry, NewLine, net_by_currency};
use ledger::{Account, Ledger, LedgerError, seed};

use crate::Res;
use crate::cli::{AccountCmd, Command, PostArgs};

fn mutate<R>(
    db: &Db<Ledger>,
    f: impl FnOnce(&mut Ledger) -> Result<(String, R), LedgerError>,
) -> Result<Committed<R>, LedgerError> {
    db.try_commit_with::<_, _, LedgerError>(f)
}

fn natural(account: &Account, balance: i128) -> i128 {
    if account.kind.debit_normal() {
        balance
    } else {
        -balance
    }
}

fn opt_date(text: &Option<String>) -> Result<Option<i32>, LedgerError> {
    text.as_deref().map(parse_date).transpose()
}

fn parse_line(ledger: &Ledger, text: &str, sign: i64) -> Result<NewLine, LedgerError> {
    let (name, rest) = text
        .split_once('=')
        .ok_or_else(|| LedgerError::BadLine(text.to_string()))?;
    let mut parts = rest.split_whitespace();
    let amount_text = parts
        .next()
        .ok_or_else(|| LedgerError::BadLine(text.to_string()))?;
    let currency = parts.next().map(str::to_string);
    if parts.next().is_some() {
        return Err(LedgerError::BadLine(text.to_string()));
    }
    let account = ledger.account(name)?;
    let parse_as = currency.as_deref().unwrap_or(&account.currency);
    let amount = parse_amount(amount_text, parse_as)?;
    if amount <= 0 {
        return Err(LedgerError::BadAmount(amount_text.to_string()));
    }
    Ok(NewLine {
        account: name.to_string(),
        amount: amount * sign,
        currency,
    })
}

fn post_entry(ledger: &mut Ledger, args: &PostArgs) -> Result<(String, u64), LedgerError> {
    let date = opt_date(&args.date)?.unwrap_or_else(today);
    let mut lines = Vec::new();
    for text in &args.debits {
        lines.push(parse_line(ledger, text, 1)?);
    }
    for text in &args.credits {
        lines.push(parse_line(ledger, text, -1)?);
    }
    let mut entry = NewEntry::new(date, args.memo.clone(), lines);
    entry.adjusts = opt_date(&args.adjusts)?;
    let id = ledger.post(entry, now_ms())?;
    Ok((format!("post #{id}: {}", args.memo), id))
}

fn parse_forecast(text: &str) -> Result<(String, String, String, Option<String>), LedgerError> {
    let bad = || {
        LedgerError::Usage(format!(
            "invalid transfer `{text}`, expected FROM>TO=AMOUNT[@DATE]"
        ))
    };
    let (from, rest) = text.split_once('>').ok_or_else(bad)?;
    let (to, rest) = rest.split_once('=').ok_or_else(bad)?;
    let (amount, date) = match rest.split_once('@') {
        Some((amount, date)) => (amount, Some(date.to_string())),
        None => (rest, None),
    };
    Ok((from.to_string(), to.to_string(), amount.to_string(), date))
}

fn transfer(
    ledger: &mut Ledger,
    from: &str,
    to: &str,
    amount: &str,
    currency: Option<&str>,
    date: i32,
    memo: &str,
) -> Result<u64, LedgerError> {
    let source = ledger.account(from)?;
    let value = parse_amount(amount, currency.unwrap_or(&source.currency))?;
    if value <= 0 {
        return Err(LedgerError::BadAmount(amount.to_string()));
    }
    let mut entry = NewEntry::transfer(date, memo, from, to, value);
    for line in &mut entry.lines {
        line.currency = currency.map(str::to_string);
    }
    ledger.post(entry, now_ms())
}

pub fn run_write(w: &mut dyn Write, db: &Db<Ledger>, command: Command) -> Res<()> {
    match command {
        Command::Account { cmd } => match cmd {
            AccountCmd::Add {
                name,
                kind,
                currency,
            } => {
                let done = mutate(db, |l| {
                    let id = l.add_account(&name, kind, &currency)?;
                    Ok((format!("add account {name}"), id))
                })?;
                writeln!(w, "added account {name} (v{})", done.version)?;
                Ok(())
            }
            AccountCmd::Close { name } => {
                let done = mutate(db, |l| {
                    l.close_account(&name)?;
                    Ok((format!("close account {name}"), ()))
                })?;
                writeln!(w, "closed account {name} (v{})", done.version)?;
                Ok(())
            }
            AccountCmd::List => Err("not a write command".into()),
        },
        Command::Post(args) => {
            let done = mutate(db, |l| post_entry(l, &args))?;
            writeln!(w, "posted #{} (v{})", done.value, done.version)?;
            Ok(())
        }
        Command::Transfer {
            from,
            to,
            amount,
            currency,
            date,
            memo,
        } => {
            let date = opt_date(&date)?.unwrap_or_else(today);
            let done = mutate(db, |l| {
                let id = transfer(l, &from, &to, &amount, currency.as_deref(), date, &memo)?;
                Ok((format!("transfer #{id}: {from} -> {to}"), id))
            })?;
            writeln!(w, "posted #{} (v{})", done.value, done.version)?;
            Ok(())
        }
        Command::Reverse { entry, date, memo } => {
            let date = opt_date(&date)?.unwrap_or_else(today);
            let done = mutate(db, |l| {
                let id = l.reverse(entry, date, memo.clone(), now_ms())?;
                Ok((format!("reverse #{entry} as #{id}"), id))
            })?;
            writeln!(
                w,
                "reversed #{entry} as #{} (v{})",
                done.value, done.version
            )?;
            Ok(())
        }
        Command::ClosePeriod { through } => {
            let date = parse_date(&through)?;
            let done = mutate(db, |l| {
                l.close_period(date)?;
                Ok((format!("close period through {through}"), ()))
            })?;
            writeln!(w, "period closed through {through} (v{})", done.version)?;
            Ok(())
        }
        Command::Seed { n, batch } => {
            let before = db.version();
            let start = Instant::now();
            seed::run(db, n, batch)?;
            writeln!(
                w,
                "seeded {n} entries in {} commits ({:.2}s), now v{}",
                db.version() - before,
                start.elapsed().as_secs_f64(),
                db.version()
            )?;
            Ok(())
        }
        Command::Forecast { transfers, name } => forecast(w, db, &transfers, name),
        _ => Err("not a write command".into()),
    }
}

fn forecast(
    w: &mut dyn Write,
    db: &Db<Ledger>,
    transfers: &[String],
    name: Option<String>,
) -> Res<()> {
    if transfers.is_empty() {
        return Err("forecast needs at least one --transfer".into());
    }
    let parsed = transfers
        .iter()
        .map(|text| parse_forecast(text))
        .collect::<Result<Vec<_>, _>>()?;
    let temporary = name.is_none();
    let name = name.unwrap_or_else(|| format!("forecast-{}", now_ms()));
    let branch = db.fork(&name)?;
    let outcome = project_forecast(w, db, &branch, &name, &parsed);
    if temporary {
        db.drop_branch(&name)?;
    }
    outcome
}

fn project_forecast(
    w: &mut dyn Write,
    db: &Db<Ledger>,
    branch: &Db<Ledger>,
    name: &str,
    parsed: &[(String, String, String, Option<String>)],
) -> Res<()> {
    let before = db.version();
    for (from, to, amount, date) in parsed {
        let date = opt_date(date)?.unwrap_or_else(today);
        mutate(branch, |l| {
            let id = transfer(l, from, to, amount, None, date, "forecast")?;
            Ok((format!("forecast #{id}: {from} -> {to}"), id))
        })?;
    }
    let main = db.read();
    let hypothetical = branch.read();
    writeln!(
        w,
        "forecast branch {name}: v{before} -> v{} ({} hypothetical entries)",
        hypothetical.version(),
        parsed.len()
    )?;
    for change in main.accounts.diff(&hypothetical.accounts) {
        if let Change::Changed(_, old, new) = change {
            let (old_bal, new_bal) = (
                natural(&old, i128::from(old.balance)),
                natural(&new, i128::from(new.balance)),
            );
            writeln!(
                w,
                "{:<24} {:>14} -> {:>14}  ({}{} {})",
                new.name,
                format_amount(old_bal, &new.currency),
                format_amount(new_bal, &new.currency),
                if new_bal >= old_bal { "+" } else { "" },
                format_amount(new_bal - old_bal, &new.currency),
                new.currency
            )?;
        }
    }
    if db.version() != before {
        return Err("main moved during forecast".into());
    }
    writeln!(w, "main unchanged at v{}", db.version())?;
    Ok(())
}

pub fn run_read(w: &mut dyn Write, db: &Db<Ledger>, command: Command) -> Res<()> {
    match command {
        Command::Account {
            cmd: AccountCmd::List,
        } => {
            let snapshot = db.read();
            for account in snapshot.accounts.iter() {
                writeln!(
                    w,
                    "{:<28} {:<10} {} {}",
                    account.name,
                    account.kind,
                    account.currency,
                    if account.closed { "closed" } else { "open" }
                )?;
            }
            Ok(())
        }
        Command::Balance { account, as_of } => {
            let snapshot = db.read();
            let row = snapshot.account(&account)?;
            let as_of = opt_date(&as_of)?;
            let value = natural(&row, snapshot.balance_at(&row, as_of));
            match as_of {
                Some(date) => writeln!(
                    w,
                    "{}: {} {} as of {}",
                    row.name,
                    format_amount(value, &row.currency),
                    row.currency,
                    format_date(date)
                )?,
                None => writeln!(
                    w,
                    "{}: {} {}",
                    row.name,
                    format_amount(value, &row.currency),
                    row.currency
                )?,
            }
            Ok(())
        }
        Command::TrialBalance { as_of } => {
            let snapshot = db.read();
            let as_of = opt_date(&as_of)?;
            let rows = snapshot.trial_balance(as_of);
            if let Some(date) = as_of {
                writeln!(w, "trial balance as of {}", format_date(date))?;
            } else {
                writeln!(w, "trial balance at v{}", snapshot.version())?;
            }
            for (account, balance) in &rows {
                if *balance == 0 {
                    continue;
                }
                let text = format_amount(balance.abs(), &account.currency);
                let (debit, credit) = if *balance > 0 {
                    (text, String::new())
                } else {
                    (String::new(), text)
                };
                writeln!(
                    w,
                    "{:<28} {} {:>16} {:>16}",
                    account.name, account.currency, debit, credit
                )?;
            }
            let mut ok = true;
            for (currency, _) in net_by_currency(&rows) {
                let (mut debits, mut credits) = (0i128, 0i128);
                for (account, balance) in &rows {
                    if account.currency == currency {
                        if *balance > 0 {
                            debits += balance;
                        } else {
                            credits -= balance;
                        }
                    }
                }
                ok &= debits == credits;
                writeln!(
                    w,
                    "{:<28} {} {:>16} {:>16}",
                    "total",
                    currency,
                    format_amount(debits, &currency),
                    format_amount(credits, &currency)
                )?;
            }
            if !ok {
                return Err("trial balance is out of balance".into());
            }
            writeln!(w, "balanced")?;
            Ok(())
        }
        Command::Statement { account, from, to } => {
            let snapshot = db.read();
            let row = snapshot.account(&account)?;
            let statement = snapshot.statement(row.id, parse_date(&from)?, parse_date(&to)?);
            let cur = &row.currency;
            writeln!(w, "statement {} {} to {}", row.name, from, to)?;
            writeln!(
                w,
                "opening balance {} {cur}",
                format_amount(natural(&row, statement.opening), cur)
            )?;
            for line in &statement.rows {
                let text = format_amount(i128::from(line.amount).abs(), cur);
                let (debit, credit) = if line.amount > 0 {
                    (text, String::new())
                } else {
                    (String::new(), text)
                };
                writeln!(
                    w,
                    "{} #{:<8} {:<32} {:>14} {:>14} {:>16}",
                    format_date(line.date),
                    line.entry,
                    line.memo,
                    debit,
                    credit,
                    format_amount(natural(&row, line.balance), cur)
                )?;
            }
            writeln!(
                w,
                "closing balance {} {cur}",
                format_amount(natural(&row, statement.closing), cur)
            )?;
            Ok(())
        }
        Command::Audit { samples } => {
            let start = Instant::now();
            let report = audit(db, samples)?;
            writeln!(
                w,
                "audited v{}: {} accounts, {} entries, {} postings in {:.2}s",
                report.version,
                report.accounts,
                report.entries,
                report.postings,
                start.elapsed().as_secs_f64()
            )?;
            for check in &report.checked {
                let totals: Vec<String> = check
                    .totals
                    .iter()
                    .map(|(cur, debits, credits)| {
                        format!(
                            "{cur} dr {} cr {}",
                            format_amount(*debits, cur),
                            format_amount(*credits, cur)
                        )
                    })
                    .collect();
                writeln!(w, "  v{}: {}", check.version, totals.join("; "))?;
            }
            for problem in &report.problems {
                writeln!(w, "DISCREPANCY: {problem}")?;
            }
            if report.problems.is_empty() {
                writeln!(w, "audit ok: stored balances match the journal")?;
                Ok(())
            } else {
                Err(format!("audit found {} discrepancies", report.problems.len()).into())
            }
        }
        Command::AuditTrail { account, limit } => audit_trail(w, db, &account, limit),
        Command::Verify => {
            let report = db.verify()?;
            writeln!(
                w,
                "ok: {} objects, {} logical bytes, {} file bytes",
                report.objects, report.bytes, report.file_bytes
            )?;
            Ok(())
        }
        Command::Show { entry } => {
            let snapshot = db.read();
            let row = snapshot
                .entries
                .get(&entry)
                .ok_or(LedgerError::NoEntry(entry))?;
            writeln!(w, "#{} {} {}", row.id, format_date(row.date), row.memo)?;
            writeln!(w, "posted {}", format_ms(row.posted_ms))?;
            if let Some(original) = row.reverses {
                writeln!(w, "reverses #{original}")?;
            }
            if let Some(by) = snapshot.reversed.get(&entry) {
                writeln!(w, "reversed by #{}", *by)?;
            }
            if let Some(period) = row.adjusts {
                writeln!(w, "adjusts {}", format_date(period))?;
            }
            for line in &row.lines {
                let account = snapshot
                    .accounts
                    .get(&line.account)
                    .ok_or_else(|| LedgerError::NoAccount(line.account.to_string()))?;
                let text = format_amount(i128::from(line.amount).abs(), &account.currency);
                let side = if line.amount > 0 { "Dr" } else { "Cr" };
                writeln!(
                    w,
                    "  {side} {:<28} {} {}",
                    account.name, text, account.currency
                )?;
            }
            Ok(())
        }
        _ => Err("not a read command".into()),
    }
}

fn audit_trail(w: &mut dyn Write, db: &Db<Ledger>, name: &str, limit: usize) -> Res<()> {
    let snapshot = db.read();
    let account = snapshot.account(name)?;
    let id = account.id;
    let cur = &account.currency;
    writeln!(w, "journal for {name}")?;
    let lo = (id, i32::MIN, 0u64, 0u16);
    let hi = (id, i32::MAX, u64::MAX, u16::MAX);
    let total = snapshot.postings.range(lo..=hi).len();
    let skip = total.saturating_sub(limit);
    for ((_, date, entry_id, _), amount) in snapshot.postings.range(lo..=hi).skip(skip) {
        let entry = snapshot.entries.get(&entry_id);
        let memo = entry.as_ref().map_or("", |entry| entry.memo.as_str());
        let mut notes = Vec::new();
        if let Some(entry) = &entry {
            if let Some(original) = entry.reverses {
                notes.push(format!("reverses #{original}"));
            }
            if let Some(period) = entry.adjusts {
                notes.push(format!("adjusts {}", format_date(period)));
            }
            notes.push(format!("posted {}", format_ms(entry.posted_ms)));
        }
        if let Some(by) = snapshot.reversed.get(&entry_id) {
            notes.push(format!("reversed by #{}", *by));
        }
        let side = if *amount > 0 { "Dr" } else { "Cr" };
        writeln!(
            w,
            "  {} #{entry_id} {side} {:>14} {memo} [{}]",
            format_date(date),
            format_amount(i128::from(*amount).abs(), cur),
            notes.join(", ")
        )?;
    }
    if skip > 0 {
        writeln!(w, "  ... {skip} earlier postings not shown")?;
    }
    writeln!(w, "commits that changed {name}")?;
    let mut changes: Vec<_> = db
        .history_of(|l| l.accounts.get(&id).map(|row| (row.balance, row.closed)))
        .take_while(|(_, state)| state.is_some())
        .collect();
    changes.reverse();
    let skip = changes.len().saturating_sub(limit);
    for (commit, state) in changes.iter().skip(skip) {
        if let Some((balance, closed)) = state {
            writeln!(
                w,
                "  v{} {} {:>16} {}{} {}",
                commit.version,
                format_ms(commit.time_ms),
                format_amount(natural(&account, i128::from(*balance)), cur),
                cur,
                if *closed { " closed" } else { "" },
                commit.message
            )?;
        }
    }
    if skip > 0 {
        writeln!(w, "  ... {skip} earlier commits not shown")?;
    }
    Ok(())
}
