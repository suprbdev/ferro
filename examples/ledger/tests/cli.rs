use std::path::PathBuf;

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use ferro::Db;
use ledger::Ledger;
use tempfile::TempDir;

struct Env {
    _dir: TempDir,
    db: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let db = dir.path().join("l.ferro");
        Env { _dir: dir, db }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = cargo_bin_cmd!("ledger");
        cmd.arg("--db").arg(&self.db).args(args);
        cmd
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cmd(args).assert().success().get_output().clone();
        String::from_utf8(out.stdout).unwrap()
    }

    fn fail(&self, args: &[&str]) -> String {
        let out = self.cmd(args).assert().failure().get_output().clone();
        String::from_utf8(out.stderr).unwrap()
    }

    fn version(&self) -> u64 {
        let out = self.ok(&["audit", "--samples", "1"]);
        let line = out.lines().find(|l| l.starts_with("audited v")).unwrap();
        line.trim_start_matches("audited v")
            .split(':')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn chart(&self) {
        for (name, kind, cur) in [
            ("assets:cash", "asset", "USD"),
            ("assets:bank", "asset", "USD"),
            ("liabilities:card", "liability", "USD"),
            ("equity:capital", "equity", "USD"),
            ("income:sales", "income", "USD"),
            ("expense:rent", "expense", "USD"),
            ("assets:euro", "asset", "EUR"),
            ("equity:euro-capital", "equity", "EUR"),
        ] {
            self.ok(&["account", "add", name, "--kind", kind, "--currency", cur]);
        }
        self.ok(&[
            "post",
            "--date",
            "2024-01-01",
            "--memo",
            "opening",
            "--dr",
            "assets:bank=1000.00",
            "--cr",
            "equity:capital=1000.00",
        ]);
    }
}

#[test]
fn account_add_list_and_duplicates() {
    let env = Env::new();
    env.ok(&["account", "add", "assets:cash", "--kind", "asset"]);
    let err = env.fail(&["account", "add", "assets:cash", "--kind", "asset"]);
    assert!(err.contains("already exists"), "{err}");
    env.fail(&[
        "account",
        "add",
        "x:y",
        "--kind",
        "asset",
        "--currency",
        "usd",
    ]);
    let list = env.ok(&["account", "list"]);
    assert!(list.contains("assets:cash") && list.contains("asset") && list.contains("USD"));
}

#[test]
fn post_balanced_entry_updates_balances() {
    let env = Env::new();
    env.chart();
    env.ok(&[
        "post",
        "--date",
        "2024-02-01",
        "--memo",
        "split sale",
        "--dr",
        "assets:cash=30.00",
        "--dr",
        "assets:bank=70.25",
        "--cr",
        "income:sales=100.25",
    ]);
    assert!(env.ok(&["balance", "assets:cash"]).contains("30.00 USD"));
    assert!(env.ok(&["balance", "assets:bank"]).contains("1070.25 USD"));
    assert!(env.ok(&["balance", "income:sales"]).contains("100.25 USD"));
    let shown = env.ok(&["show", "2"]);
    assert!(shown.contains("split sale") && shown.contains("Dr") && shown.contains("Cr"));
}

#[test]
fn unbalanced_post_is_rejected_atomically() {
    let env = Env::new();
    env.chart();
    let before = env.version();
    let err = env.fail(&[
        "post",
        "--date",
        "2024-02-01",
        "--dr",
        "assets:cash=10.00",
        "--cr",
        "income:sales=9.99",
    ]);
    assert!(err.contains("unbalanced"), "{err}");
    assert_eq!(env.version(), before);
    assert!(env.ok(&["balance", "assets:cash"]).contains("0.00 USD"));
    env.fail(&["show", "2"]);
}

#[test]
fn single_line_and_zero_amount_are_rejected() {
    let env = Env::new();
    env.chart();
    env.fail(&["post", "--dr", "assets:cash=10.00"]);
    env.fail(&[
        "post",
        "--dr",
        "assets:cash=0.00",
        "--cr",
        "income:sales=0.00",
    ]);
    env.fail(&[
        "post",
        "--dr",
        "assets:cash=abc",
        "--cr",
        "income:sales=1.00",
    ]);
    env.fail(&[
        "post",
        "--dr",
        "assets:cash=1.005",
        "--cr",
        "income:sales=1.005",
    ]);
    env.fail(&[
        "post",
        "--dr",
        "nope:cash=1.00",
        "--cr",
        "income:sales=1.00",
    ]);
}

#[test]
fn closed_account_is_refused() {
    let env = Env::new();
    env.chart();
    let err = env.fail(&["account", "close", "assets:bank"]);
    assert!(err.contains("non-zero balance"), "{err}");
    env.ok(&["account", "close", "expense:rent"]);
    let before = env.version();
    let err = env.fail(&[
        "post",
        "--date",
        "2024-02-01",
        "--dr",
        "expense:rent=5.00",
        "--cr",
        "assets:bank=5.00",
    ]);
    assert!(err.contains("closed"), "{err}");
    assert!(env.ok(&["balance", "assets:bank"]).contains("1000.00 USD"));
    assert_eq!(env.version(), before);
}

#[test]
fn currency_mismatch_is_refused() {
    let env = Env::new();
    env.chart();
    let err = env.fail(&[
        "post",
        "--date",
        "2024-02-01",
        "--dr",
        "assets:euro=5.00 USD",
        "--cr",
        "income:sales=5.00 USD",
    ]);
    assert!(err.contains("currency mismatch"), "{err}");
    let err = env.fail(&[
        "post",
        "--date",
        "2024-02-01",
        "--dr",
        "assets:euro=5.00",
        "--cr",
        "income:sales=5.00",
    ]);
    assert!(err.contains("unbalanced"), "{err}");
    env.ok(&[
        "post",
        "--date",
        "2024-02-01",
        "--dr",
        "assets:euro=5.00",
        "--cr",
        "equity:euro-capital=5.00",
    ]);
    assert!(env.ok(&["balance", "assets:euro"]).contains("5.00 EUR"));
}

#[test]
fn transfer_moves_money() {
    let env = Env::new();
    env.chart();
    env.ok(&[
        "transfer",
        "assets:bank",
        "assets:cash",
        "250.50",
        "--date",
        "2024-03-01",
        "--memo",
        "atm",
    ]);
    assert!(env.ok(&["balance", "assets:bank"]).contains("749.50 USD"));
    assert!(env.ok(&["balance", "assets:cash"]).contains("250.50 USD"));
    env.fail(&["transfer", "assets:bank", "assets:euro", "1.00"]);
    env.fail(&[
        "transfer",
        "assets:bank",
        "assets:euro",
        "1.00",
        "--currency",
        "USD",
    ]);
    env.fail(&["transfer", "assets:bank", "assets:cash", "-3.00"]);
}

#[test]
fn balance_as_of_reads_history_by_entry_date() {
    let env = Env::new();
    env.chart();
    for (date, amount) in [
        ("2024-02-01", "10.00"),
        ("2024-03-01", "20.00"),
        ("2024-04-01", "40.00"),
    ] {
        env.ok(&[
            "transfer",
            "assets:bank",
            "assets:cash",
            amount,
            "--date",
            date,
        ]);
    }
    let at = |date: &str| env.ok(&["balance", "assets:cash", "--as-of", date]);
    assert!(at("2024-01-31").contains("0.00 USD"));
    assert!(at("2024-02-01").contains("10.00 USD"));
    assert!(at("2024-03-15").contains("30.00 USD"));
    assert!(at("2024-12-31").contains("70.00 USD"));
    assert!(env.ok(&["balance", "assets:cash"]).contains("70.00 USD"));
    env.fail(&["balance", "assets:cash", "--as-of", "2024-13-01"]);
    env.fail(&["balance", "assets:cash", "--as-of", "2023-02-29"]);
}

#[test]
fn trial_balance_balances_now_and_as_of() {
    let env = Env::new();
    env.chart();
    env.ok(&[
        "post",
        "--date",
        "2024-05-01",
        "--dr",
        "expense:rent=500.00",
        "--cr",
        "assets:bank=500.00",
    ]);
    let now = env.ok(&["trial-balance"]);
    assert!(now.contains("balanced"), "{now}");
    assert!(now.contains("expense:rent") && now.contains("500.00"));
    let old = env.ok(&["trial-balance", "--as-of", "2024-02-01"]);
    assert!(
        old.contains("balanced") && !old.contains("expense:rent"),
        "{old}"
    );
    assert!(old.contains("1000.00"));
}

#[test]
fn statement_has_opening_running_and_closing_balance() {
    let env = Env::new();
    env.chart();
    for (date, amount) in [
        ("2024-01-10", "100.00"),
        ("2024-02-10", "50.00"),
        ("2024-03-10", "25.00"),
    ] {
        env.ok(&[
            "transfer",
            "assets:bank",
            "assets:cash",
            amount,
            "--date",
            date,
            "--memo",
            date,
        ]);
    }
    let out = env.ok(&[
        "statement",
        "assets:cash",
        "--from",
        "2024-02-01",
        "--to",
        "2024-02-28",
    ]);
    assert!(out.contains("opening balance 100.00 USD"), "{out}");
    assert!(
        out.contains("2024-02-10") && !out.contains("2024-03-10"),
        "{out}"
    );
    assert!(out.contains("closing balance 150.00 USD"), "{out}");
}

#[test]
fn reverse_creates_new_entry_and_never_edits() {
    let env = Env::new();
    env.chart();
    env.ok(&[
        "transfer",
        "assets:bank",
        "assets:cash",
        "40.00",
        "--date",
        "2024-02-01",
    ]);
    let out = env.ok(&["reverse", "2", "--date", "2024-02-02"]);
    assert!(out.contains("reversed #2 as #3"), "{out}");
    assert!(env.ok(&["balance", "assets:cash"]).contains("0.00 USD"));
    assert!(env.ok(&["balance", "assets:bank"]).contains("1000.00 USD"));
    let original = env.ok(&["show", "2"]);
    assert!(original.contains("reversed by #3"), "{original}");
    assert!(env.ok(&["show", "3"]).contains("reverses #2"));
    let err = env.fail(&["reverse", "2", "--date", "2024-02-03"]);
    assert!(err.contains("already reversed"), "{err}");
    env.fail(&["reverse", "99"]);
    let old = env.ok(&["balance", "assets:cash", "--as-of", "2024-02-01"]);
    assert!(old.contains("40.00 USD"), "{old}");
}

#[test]
fn audit_passes_and_detects_tampering() {
    let env = Env::new();
    env.chart();
    for day in 1..=5 {
        env.ok(&[
            "transfer",
            "assets:bank",
            "assets:cash",
            "10.00",
            "--date",
            &format!("2024-02-0{day}"),
        ]);
    }
    let out = env.ok(&["audit", "--samples", "0"]);
    assert!(out.contains("audit ok"), "{out}");
    assert!(out.contains("v14:"), "{out}");
    {
        let db: Db<Ledger> = Db::open(&env.db).unwrap();
        db.update(|l| {
            let id = l.account_id("assets:cash").unwrap();
            l.accounts.update(&id, |a| a.balance += 1);
        })
        .unwrap();
    }
    let out = String::from_utf8(
        env.cmd(&["audit"])
            .assert()
            .failure()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(out.contains("DISCREPANCY"), "{out}");
    assert!(out.contains("assets:cash"), "{out}");
    assert!(out.contains("balances net"), "{out}");
}

#[test]
fn audit_detects_forged_posting_index() {
    let env = Env::new();
    env.chart();
    env.ok(&[
        "transfer",
        "assets:bank",
        "assets:cash",
        "10.00",
        "--date",
        "2024-02-01",
    ]);
    {
        let db: Db<Ledger> = Db::open(&env.db).unwrap();
        db.update(|l| {
            let id = l.account_id("assets:cash").unwrap();
            l.postings.insert((id, 20_000, 99, 0), 5);
        })
        .unwrap();
    }
    let out = String::from_utf8(
        env.cmd(&["audit"])
            .assert()
            .failure()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(out.contains("indexed but not in journal"), "{out}");
}

#[test]
fn audit_trail_lists_journal_and_commits() {
    let env = Env::new();
    env.chart();
    env.ok(&[
        "transfer",
        "assets:bank",
        "assets:cash",
        "40.00",
        "--date",
        "2024-02-01",
        "--memo",
        "atm run",
    ]);
    env.ok(&["reverse", "2", "--date", "2024-02-02"]);
    let out = env.ok(&["audit-trail", "assets:cash"]);
    assert!(
        out.contains("atm run") && out.contains("reversed by #3"),
        "{out}"
    );
    assert!(out.contains("reverses #2"), "{out}");
    assert!(
        out.contains("transfer #2") && out.contains("reverse #2 as #3"),
        "{out}"
    );
    let short = env.ok(&["audit-trail", "assets:cash", "--limit", "1"]);
    assert!(short.contains("earlier postings not shown"), "{short}");
}

#[test]
fn forecast_leaves_main_untouched() {
    let env = Env::new();
    env.chart();
    let before = env.version();
    let out = env.ok(&[
        "forecast",
        "--name",
        "q2-plan",
        "--transfer",
        "assets:bank>expense:rent=300.00@2024-06-01",
        "--transfer",
        "assets:bank>expense:rent=300.00@2024-07-01",
    ]);
    assert!(out.contains("2 hypothetical entries"), "{out}");
    assert!(out.contains("700.00") || out.contains("400.00"), "{out}");
    assert!(out.contains("main unchanged"), "{out}");
    assert_eq!(env.version(), before);
    assert!(env.ok(&["balance", "assets:bank"]).contains("1000.00 USD"));
    let hypo = env.ok(&["--branch", "q2-plan", "balance", "assets:bank"]);
    assert!(hypo.contains("400.00 USD"), "{hypo}");
    env.fail(&[
        "forecast",
        "--transfer",
        "assets:bank>expense:rent=1.00",
        "--name",
        "q2-plan",
    ]);
    env.fail(&["forecast"]);
}

#[test]
fn close_period_refuses_postings_but_allows_adjusting_entries() {
    let env = Env::new();
    env.chart();
    env.ok(&[
        "transfer",
        "assets:bank",
        "assets:cash",
        "10.00",
        "--date",
        "2024-03-15",
    ]);
    env.ok(&["close-period", "--through", "2024-03-31"]);
    let err = env.fail(&[
        "transfer",
        "assets:bank",
        "assets:cash",
        "1.00",
        "--date",
        "2024-03-31",
    ]);
    assert!(err.contains("period is closed through 2024-03-31"), "{err}");
    env.fail(&["close-period", "--through", "2024-02-01"]);
    let err = env.fail(&["reverse", "2", "--date", "2024-03-20"]);
    assert!(err.contains("period is closed"), "{err}");
    let err = env.fail(&[
        "post",
        "--date",
        "2024-03-20",
        "--adjusts",
        "2024-03-15",
        "--dr",
        "assets:cash=1.00",
        "--cr",
        "assets:bank=1.00",
    ]);
    assert!(err.contains("adjusting entries are dated after"), "{err}");
    env.fail(&[
        "post",
        "--date",
        "2024-04-02",
        "--adjusts",
        "2024-04-01",
        "--dr",
        "assets:cash=1.00",
        "--cr",
        "assets:bank=1.00",
    ]);
    env.ok(&[
        "post",
        "--date",
        "2024-04-02",
        "--adjusts",
        "2024-03-15",
        "--memo",
        "late correction",
        "--dr",
        "assets:cash=2.00",
        "--cr",
        "assets:bank=2.00",
    ]);
    let closed = env.ok(&["balance", "assets:cash", "--as-of", "2024-03-31"]);
    assert!(closed.contains("10.00 USD"), "{closed}");
    assert!(env.ok(&["balance", "assets:cash"]).contains("12.00 USD"));
    assert!(env.ok(&["show", "3"]).contains("adjusts 2024-03-15"));
    env.ok(&["reverse", "2", "--date", "2024-04-03"]);
    env.ok(&["audit"]);
}

#[test]
fn adjusting_without_closed_period_is_refused() {
    let env = Env::new();
    env.chart();
    let err = env.fail(&[
        "post",
        "--date",
        "2024-04-02",
        "--adjusts",
        "2024-03-15",
        "--dr",
        "assets:cash=2.00",
        "--cr",
        "assets:bank=2.00",
    ]);
    assert!(err.contains("no closed period"), "{err}");
}

#[test]
fn seed_audit_verify_round_trip() {
    let env = Env::new();
    let out = env.ok(&["seed", "500", "--batch", "100"]);
    assert!(out.contains("seeded 500 entries in 6 commits"), "{out}");
    let tb = env.ok(&["trial-balance"]);
    assert!(tb.contains("balanced"), "{tb}");
    let old = env.ok(&["trial-balance", "--as-of", "2023-06-30"]);
    assert!(old.contains("balanced"), "{old}");
    let audit = env.ok(&["audit"]);
    assert!(
        audit.contains("audit ok") && audit.contains("506 entries"),
        "{audit}"
    );
    let verify = env.ok(&["verify"]);
    assert!(verify.starts_with("ok:"), "{verify}");
    let statement = env.ok(&[
        "statement",
        "assets:bank-1",
        "--from",
        "2023-01-01",
        "--to",
        "2023-03-31",
    ]);
    assert!(statement.contains("closing balance"), "{statement}");
}

#[test]
fn read_commands_fail_cleanly_on_missing_database() {
    let env = Env::new();
    let err = env.fail(&["balance", "assets:cash"]);
    assert!(err.starts_with("error:"), "{err}");
}
