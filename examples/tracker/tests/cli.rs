use std::path::PathBuf;

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::TempDir;

struct Env {
    _dir: TempDir,
    db: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.ferro");
        Env { _dir: dir, db }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = cargo_bin_cmd!("tracker");
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

    fn lines(&self, args: &[&str]) -> Vec<String> {
        self.ok(args).lines().map(str::to_string).collect()
    }

    fn ids(&self, args: &[&str]) -> Vec<u64> {
        self.lines(args)
            .iter()
            .map(|line| {
                line.trim_start_matches('#')
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect()
    }

    fn version(&self) -> u64 {
        let stats = self.ok(&["stats"]);
        let line = stats.lines().find(|l| l.starts_with("version:")).unwrap();
        line.trim_start_matches("version:")
            .trim()
            .trim_start_matches('v')
            .parse()
            .unwrap()
    }
}

fn populated() -> Env {
    let env = Env::new();
    env.ok(&[
        "add",
        "Login crashes",
        "--label",
        "bug",
        "--label",
        "ui",
        "--assignee",
        "alice",
        "--priority",
        "high",
    ]);
    env.ok(&[
        "add",
        "Add dark mode",
        "--label",
        "feature",
        "--body",
        "Users want a Night theme",
    ]);
    env.ok(&["add", "Slow search", "--label", "bug", "--assignee", "bob"]);
    env.ok(&["add", "Docs typo", "--priority", "low"]);
    env.ok(&["start", "3"]);
    env.ok(&["close", "4"]);
    env
}

#[test]
fn add_persists_between_processes() {
    let env = Env::new();
    let out = env.ok(&["add", "Hello world", "--body", "details here"]);
    assert!(out.contains("added #1"));
    let out = env.ok(&["add", "Second"]);
    assert!(out.contains("added #2"));
    let shown = env.ok(&["show", "1"]);
    assert!(shown.contains("#1 Hello world"));
    assert!(shown.contains("status:   open"));
    assert!(shown.contains("priority: medium"));
    assert!(shown.contains("assignee: unassigned"));
    assert!(shown.contains("details here"));
    assert!(shown.contains("comments (0):"));
    assert_eq!(env.ids(&["list"]), vec![2, 1]);
}

#[test]
fn add_with_options() {
    let env = Env::new();
    env.ok(&[
        "add",
        "Thing",
        "--assignee",
        "carol",
        "--label",
        "a",
        "--label",
        "b",
        "--label",
        "a",
        "--priority",
        "critical",
    ]);
    let shown = env.ok(&["show", "1"]);
    assert!(shown.contains("assignee: carol"));
    assert!(shown.contains("labels:   a, b"));
    assert!(shown.contains("priority: critical"));
    assert!(env.ok(&["users"]).contains("carol"));
}

#[test]
fn show_missing_issue_fails() {
    let env = populated();
    assert!(env.fail(&["show", "99"]).contains("no such issue #99"));
    assert!(env.fail(&["close", "99"]).contains("no such issue #99"));
}

#[test]
fn missing_database_fails_for_reads() {
    let env = Env::new();
    assert!(env.fail(&["list"]).contains("no database"));
    assert!(!env.db.exists());
}

#[test]
fn list_filters() {
    let env = populated();
    assert_eq!(env.ids(&["list"]), vec![4, 3, 2, 1]);
    assert_eq!(env.ids(&["list", "--status", "open"]), vec![2, 1]);
    assert_eq!(env.ids(&["list", "--status", "in-progress"]), vec![3]);
    assert_eq!(env.ids(&["list", "--status", "closed"]), vec![4]);
    assert_eq!(env.ids(&["list", "--assignee", "alice"]), vec![1]);
    assert_eq!(env.ids(&["list", "--unassigned"]), vec![4, 2]);
    assert_eq!(env.ids(&["list", "--label", "bug"]), vec![3, 1]);
    assert_eq!(env.ids(&["list", "--label", "ui"]), vec![1]);
    assert_eq!(env.ids(&["list", "--label", "nope"]), Vec::<u64>::new());
    assert_eq!(env.ids(&["list", "--priority", "low"]), vec![4]);
    assert_eq!(env.ids(&["list", "--priority", "medium"]), vec![3, 2]);
    assert_eq!(env.ids(&["list", "--text", "NIGHT"]), vec![2]);
    assert_eq!(env.ids(&["list", "--text", "search"]), vec![3]);
    assert_eq!(env.ids(&["list", "--limit", "2"]), vec![4, 3]);
}

#[test]
fn list_combined_filters() {
    let env = populated();
    assert_eq!(
        env.ids(&["list", "--label", "bug", "--status", "open"]),
        vec![1]
    );
    assert_eq!(
        env.ids(&[
            "list",
            "--label",
            "bug",
            "--assignee",
            "bob",
            "--status",
            "in-progress"
        ]),
        vec![3]
    );
    assert_eq!(
        env.ids(&["list", "--label", "bug", "--text", "login"]),
        vec![1]
    );
    assert_eq!(
        env.ids(&[
            "list",
            "--status",
            "open",
            "--priority",
            "high",
            "--limit",
            "5"
        ]),
        vec![1]
    );
    assert_eq!(
        env.ids(&["list", "--label", "bug", "--status", "closed"]),
        Vec::<u64>::new()
    );
}

#[test]
fn list_uses_index_when_filter_maps_to_one() {
    let env = populated();
    let plan = |args: &[&str]| {
        let mut full = vec!["list", "--explain"];
        full.extend_from_slice(args);
        let out = env.cmd(&full).assert().success().get_output().clone();
        String::from_utf8(out.stderr).unwrap()
    };
    assert!(plan(&["--label", "ui"]).contains("index labels (1 candidates)"));
    assert!(plan(&["--status", "open"]).contains("index status"));
    assert!(plan(&["--text", "x"]).contains("full scan (4 rows)"));
    assert!(plan(&["--status", "open", "--label", "ui"]).contains("index labels"));
}

#[test]
fn edit_command() {
    let env = populated();
    env.ok(&[
        "edit",
        "2",
        "--title",
        "Dark mode please",
        "--body",
        "new body",
        "--priority",
        "critical",
    ]);
    let shown = env.ok(&["show", "2"]);
    assert!(shown.contains("#2 Dark mode please"));
    assert!(shown.contains("new body"));
    assert!(shown.contains("priority: critical"));
    assert_eq!(env.ids(&["list", "--priority", "critical"]), vec![2]);
    assert!(env.fail(&["edit", "2"]).contains("nothing to edit"));
}

#[test]
fn assign_and_unassign() {
    let env = populated();
    env.ok(&["assign", "2", "dave"]);
    assert_eq!(env.ids(&["list", "--assignee", "dave"]), vec![2]);
    env.ok(&["assign", "2", "erin"]);
    assert_eq!(env.ids(&["list", "--assignee", "dave"]), Vec::<u64>::new());
    assert_eq!(env.ids(&["list", "--assignee", "erin"]), vec![2]);
    let out = env.ok(&["assign", "2", "erin"]);
    assert!(out.contains("no change"));
    env.ok(&["unassign", "2"]);
    assert_eq!(env.ids(&["list", "--unassigned"]), vec![4, 2]);
    let users = env.ok(&["users"]);
    assert!(users.contains("dave"));
    assert!(users.contains("erin"));
}

#[test]
fn label_and_unlabel() {
    let env = populated();
    env.ok(&["label", "2", "urgent"]);
    assert_eq!(env.ids(&["list", "--label", "urgent"]), vec![2]);
    assert!(env.ok(&["label", "2", "urgent"]).contains("no change"));
    env.ok(&["unlabel", "2", "urgent"]);
    assert_eq!(env.ids(&["list", "--label", "urgent"]), Vec::<u64>::new());
    assert!(env.ok(&["unlabel", "2", "urgent"]).contains("no change"));
    env.ok(&["unlabel", "1", "bug"]);
    assert_eq!(env.ids(&["list", "--label", "bug"]), vec![3]);
}

#[test]
fn comments() {
    let env = populated();
    env.ok(&["comment", "1", "first comment", "--author", "bob"]);
    env.ok(&["comment", "1", "second comment"]);
    let shown = env.ok(&["show", "1"]);
    assert!(shown.contains("comments (2):"));
    assert!(shown.contains("bob: first comment"));
    assert!(shown.contains("anonymous: second comment"));
    let first = shown.find("first comment").unwrap();
    let second = shown.find("second comment").unwrap();
    assert!(first < second);
}

#[test]
fn status_transitions() {
    let env = populated();
    env.ok(&["start", "1"]);
    assert_eq!(env.ids(&["list", "--status", "in-progress"]), vec![3, 1]);
    env.ok(&["close", "1"]);
    assert_eq!(env.ids(&["list", "--status", "closed"]), vec![4, 1]);
    env.ok(&["reopen", "1"]);
    assert_eq!(env.ids(&["list", "--status", "closed"]), vec![4]);
    assert_eq!(env.ids(&["list", "--status", "open"]), vec![2, 1]);
    let before = env.version();
    assert!(env.ok(&["reopen", "1"]).contains("no change"));
    assert_eq!(env.version(), before);
}

#[test]
fn log_is_newest_first_with_messages() {
    let env = populated();
    let lines = env.lines(&["log"]);
    assert_eq!(lines.len(), 6);
    assert!(lines[0].starts_with("v6"));
    assert!(lines[0].ends_with("close #4"));
    assert!(lines[1].ends_with("start #3"));
    assert!(lines[2].ends_with("add #4: Docs typo"));
    assert!(lines[5].ends_with("add #1: Login crashes"));
    assert_eq!(env.lines(&["log", "--limit", "2"]).len(), 2);
}

#[test]
fn history_reports_each_change() {
    let env = Env::new();
    env.ok(&["add", "Track me", "--label", "bug"]);
    env.ok(&["add", "Other"]);
    env.ok(&["start", "1"]);
    env.ok(&["assign", "2", "zed"]);
    env.ok(&["assign", "1", "alice"]);
    env.ok(&["label", "1", "ui"]);
    env.ok(&["unlabel", "1", "bug"]);
    env.ok(&["comment", "1", "a remark", "--author", "bob"]);
    env.ok(&["edit", "1", "--title", "Tracked", "--priority", "high"]);
    env.ok(&["close", "1"]);
    let out = env.ok(&["history", "1"]);
    let expect = [
        ("v1", "add #1: Track me", vec!["created: Track me"]),
        ("v3", "start #1", vec!["status: open -> in-progress"]),
        ("v5", "assign #1 to alice", vec!["assignee: - -> alice"]),
        ("v6", "label #1 ui", vec!["+label ui"]),
        ("v7", "unlabel #1 bug", vec!["-label bug"]),
        ("v8", "comment on #1", vec!["comment by bob: a remark"]),
        (
            "v9",
            "edit #1",
            vec![
                "title: \"Track me\" -> \"Tracked\"",
                "priority: medium -> high",
            ],
        ),
        ("v10", "close #1", vec!["status: in-progress -> closed"]),
    ];
    let mut cursor = out.lines();
    for (version, message, details) in expect {
        let header = cursor.next().unwrap();
        assert!(header.starts_with(&format!("{version} ")), "{header}");
        assert!(header.ends_with(message), "{header}");
        for detail in details {
            assert_eq!(cursor.next().unwrap(), format!("    {detail}"));
        }
    }
    assert!(cursor.next().is_none());
    assert!(!out.contains("v2 "));
    assert!(!out.contains("v4 "));
    assert!(env.fail(&["history", "77"]).contains("no history"));
}

#[test]
fn history_follows_undo() {
    let env = Env::new();
    env.ok(&["add", "One"]);
    env.ok(&["close", "1"]);
    env.ok(&["undo"]);
    let out = env.ok(&["history", "1"]);
    assert!(out.contains("v2 "));
    assert!(out.contains("v3 "));
    assert!(out.contains("status: open -> closed"));
    assert!(out.contains("status: closed -> open"));
}

#[test]
fn diff_between_versions() {
    let env = populated();
    let out = env.ok(&["diff", "2", "6"]);
    assert!(out.starts_with("2 added, 0 removed, 0 changed"));
    assert!(out.contains("+ #3 Slow search"));
    assert!(out.contains("+ #4 Docs typo"));
    let reverse = env.ok(&["diff", "6", "2"]);
    assert!(reverse.starts_with("0 added, 2 removed, 0 changed"));
    assert!(reverse.contains("- #3 Slow search"));
    let same = env.ok(&["diff", "3", "3"]);
    assert!(same.starts_with("0 added, 0 removed, 0 changed"));
    let limited = env.ok(&["diff", "0", "6", "--limit", "1"]);
    assert!(limited.starts_with("4 added"));
    assert!(limited.contains("3 more not shown"));
    env.fail(&["diff", "0", "999"]);
}

#[test]
fn diff_describes_field_changes() {
    let env = populated();
    let out = env.ok(&["diff", "4", "5"]);
    assert!(out.starts_with("0 added, 0 removed, 1 changed"));
    assert!(out.contains("~ #3 Slow search"));
    assert!(out.contains("    status: open -> in-progress"));
}

#[test]
fn undo_restores_previous_state() {
    let env = populated();
    assert_eq!(env.ids(&["list", "--status", "closed"]), vec![4]);
    let out = env.ok(&["undo"]);
    assert!(out.contains("undid v6"));
    assert_eq!(env.ids(&["list", "--status", "closed"]), Vec::<u64>::new());
    assert_eq!(env.ids(&["list", "--status", "open"]), vec![4, 2, 1]);
    assert_eq!(env.version(), 7);
    let log = env.lines(&["log", "--limit", "2"]);
    assert!(log[0].ends_with("undo v6: restore v5"));
    assert!(log[1].ends_with("close #4"));
    assert_eq!(env.lines(&["log", "--limit", "100"]).len(), 7);
}

#[test]
fn undo_twice_walks_back_two_changes() {
    let env = populated();
    env.ok(&["undo"]);
    env.ok(&["undo"]);
    assert_eq!(
        env.ids(&["list", "--status", "in-progress"]),
        Vec::<u64>::new()
    );
    assert_eq!(env.ids(&["list", "--status", "open"]), vec![4, 3, 2, 1]);
    env.ok(&["undo"]);
    assert_eq!(env.ids(&["list"]), vec![3, 2, 1]);
    assert_eq!(env.version(), 9);
}

#[test]
fn undo_after_new_write_undoes_only_that_write() {
    let env = populated();
    env.ok(&["undo"]);
    env.ok(&["comment", "1", "after undo"]);
    env.ok(&["undo"]);
    assert!(env.ok(&["show", "1"]).contains("comments (0):"));
    assert_eq!(env.ids(&["list", "--status", "open"]), vec![4, 2, 1]);
}

#[test]
fn undo_all_the_way_then_nothing() {
    let env = Env::new();
    env.ok(&["add", "A"]);
    env.ok(&["add", "B"]);
    env.ok(&["undo"]);
    assert_eq!(env.ids(&["list"]), vec![1]);
    env.ok(&["undo"]);
    assert_eq!(env.ids(&["list"]), Vec::<u64>::new());
    assert!(env.ok(&["undo"]).contains("nothing to undo"));
    assert_eq!(env.version(), 4);
    let fresh = Env::new();
    fresh.ok(&["add", "x"]);
    fresh.ok(&["undo"]);
    assert!(fresh.ok(&["undo"]).contains("nothing to undo"));
}

#[test]
fn undo_keeps_history_intact() {
    let env = populated();
    env.ok(&["undo"]);
    for version in 1..=6 {
        env.ok(&["at", &version.to_string(), "list"]);
    }
    assert_eq!(env.ids(&["at", "6", "list", "--status", "closed"]), vec![4]);
}

#[test]
fn restore_brings_back_old_version() {
    let env = populated();
    env.ok(&["undo"]);
    env.ok(&["restore", "6"]);
    assert_eq!(env.ids(&["list", "--status", "closed"]), vec![4]);
    let log = env.lines(&["log", "--limit", "1"]);
    assert!(log[0].ends_with("restore v6"));
    env.fail(&["restore", "500"]);
}

#[test]
fn at_reads_old_versions() {
    let env = populated();
    assert_eq!(env.ids(&["at", "1", "list"]), vec![1]);
    assert_eq!(env.ids(&["at", "2", "list"]), vec![2, 1]);
    assert_eq!(
        env.ids(&["at", "5", "list", "--status", "in-progress"]),
        vec![3]
    );
    assert_eq!(
        env.ids(&["at", "4", "list", "--status", "closed"]),
        Vec::<u64>::new()
    );
    assert_eq!(env.ids(&["at", "0", "list"]), Vec::<u64>::new());
    let shown = env.ok(&["at", "1", "show", "1"]);
    assert!(shown.contains("status:   open"));
    assert!(
        env.fail(&["at", "1", "show", "4"])
            .contains("no such issue #4")
    );
    let stats = env.ok(&["at", "3", "stats"]);
    assert!(stats.contains("version:  v3"));
    assert!(stats.contains("issues:   3"));
    env.fail(&["at", "99", "list"]);
}

#[test]
fn at_is_read_only() {
    let env = populated();
    let before = env.version();
    env.ok(&["at", "2", "list"]);
    env.ok(&["at", "2", "stats"]);
    assert_eq!(env.version(), before);
    env.cmd(&["at", "2", "close", "1"]).assert().failure();
}

#[test]
fn as_of_reads_by_time() {
    let env = populated();
    assert_eq!(env.ids(&["as-of", "0", "list"]), Vec::<u64>::new());
    assert_eq!(
        env.ids(&["as-of", "99999999999999", "list"]),
        vec![4, 3, 2, 1]
    );
    let log = env.ok(&["log", "--limit", "1"]);
    assert!(log.contains("v6"));
}

#[test]
fn stats_counts() {
    let env = populated();
    let out = env.ok(&["stats"]);
    assert!(out.contains("branch:   main"));
    assert!(out.contains("version:  v6"));
    assert!(out.contains("issues:   4"));
    assert!(out.contains("users:    2"));
    let row = |name: &str| -> u64 {
        out.lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap()
            .split_whitespace()
            .last()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert_eq!(row("open"), 2);
    assert_eq!(row("in-progress"), 1);
    assert_eq!(row("closed"), 1);
    assert_eq!(row("alice"), 1);
    assert_eq!(row("bob"), 1);
    assert_eq!(row("unassigned"), 2);
    assert_eq!(row("bug"), 2);
    assert_eq!(row("feature"), 1);
    assert_eq!(row("ui"), 1);
    assert_eq!(row("critical"), 0);
    assert_eq!(row("low"), 1);
    assert_eq!(row("high"), 1);
    let bytes: u64 = out
        .lines()
        .find(|l| l.starts_with("database:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert!(bytes > 8192);
}

#[test]
fn verify_passes() {
    let env = populated();
    let out = env.ok(&["verify"]);
    assert!(out.starts_with("ok:"));
}

#[test]
fn seed_batches_commits() {
    let env = Env::new();
    let out = env.ok(&["seed", "2500"]);
    assert!(out.contains("seeded 2500 issues in 4 commits"));
    let stats = env.ok(&["stats"]);
    assert!(stats.contains("issues:   2500"));
    assert!(stats.contains("version:  v4"));
    let log = env.lines(&["log"]);
    assert!(log[0].ends_with("seed #2001..#2500"));
    assert!(log[1].ends_with("seed #1001..#2000"));
    assert!(log[2].ends_with("seed #1..#1000"));
    assert_eq!(env.ids(&["list", "--limit", "3"]), vec![2500, 2499, 2498]);
    env.ok(&["verify"]);
    let again = env.ok(&["seed", "10", "--batch", "5"]);
    assert!(again.contains("seeded 10 issues in 2 commits"));
    assert_eq!(env.ids(&["list", "--limit", "1"]), vec![2510]);
}

#[test]
fn seeded_data_filters_agree_with_scan() {
    let env = Env::new();
    env.ok(&["seed", "1500"]);
    let by_index = env.ids(&["list", "--label", "bug", "--status", "open"]);
    let all = env.ids(&["list"]);
    assert_eq!(all.len(), 1500);
    assert!(!by_index.is_empty());
    for id in &by_index {
        let shown = env.ok(&["show", &id.to_string()]);
        assert!(shown.contains("status:   open"));
        assert!(shown.contains("bug"));
    }
    let open = env.ids(&["list", "--status", "open"]).len();
    let in_progress = env.ids(&["list", "--status", "in-progress"]).len();
    let closed = env.ids(&["list", "--status", "closed"]).len();
    assert_eq!(open + in_progress + closed, 1500);
    let mut sorted = by_index.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(by_index, sorted);
}

#[test]
fn branches_fork_and_isolate() {
    let env = populated();
    let out = env.ok(&["branch", "whatif"]);
    assert!(out.contains("created branch whatif from main at v6"));
    assert!(env.fail(&["branch", "whatif"]).contains("already exists"));
    let listing = env.ok(&["branches"]);
    assert!(listing.contains("* main"));
    assert!(listing.contains("  whatif"));
    env.ok(&["--branch", "whatif", "close", "1"]);
    env.ok(&["--branch", "whatif", "close", "2"]);
    env.ok(&["--branch", "whatif", "add", "Only here"]);
    assert_eq!(
        env.ids(&["--branch", "whatif", "list", "--status", "closed"]),
        vec![4, 2, 1]
    );
    assert_eq!(env.ids(&["list", "--status", "closed"]), vec![4]);
    assert_eq!(env.ids(&["list"]), vec![4, 3, 2, 1]);
    assert_eq!(
        env.ids(&["--branch", "whatif", "list"]),
        vec![5, 4, 3, 2, 1]
    );
    assert!(
        env.ok(&["--branch", "whatif", "stats"])
            .contains("branch:   whatif")
    );
    let log = env.lines(&["--branch", "whatif", "log", "--limit", "1"]);
    assert!(log[0].starts_with("v9"));
    assert!(log[0].ends_with("add #5: Only here"));
    assert!(env.lines(&["log", "--limit", "1"])[0].starts_with("v6"));
    env.fail(&["--branch", "ghost", "list"]);
}

#[test]
fn compare_branches() {
    let env = populated();
    env.ok(&["branch", "whatif"]);
    env.ok(&["--branch", "whatif", "close", "1"]);
    env.ok(&["--branch", "whatif", "add", "Only here"]);
    let out = env.ok(&["compare", "whatif"]);
    assert!(out.contains("main v6 -> whatif v8"));
    assert!(out.contains("1 added, 0 removed, 1 changed"));
    assert!(out.contains("+ #5 Only here"));
    assert!(out.contains("~ #1 Login crashes"));
    assert!(out.contains("status: open -> closed"));
    let back = env.ok(&["--branch", "whatif", "compare", "main"]);
    assert!(back.contains("0 added, 1 removed, 1 changed"));
    env.fail(&["compare", "ghost"]);
}

#[test]
fn undo_on_branch_leaves_main_alone() {
    let env = populated();
    env.ok(&["branch", "b"]);
    env.ok(&["--branch", "b", "close", "1"]);
    env.ok(&["--branch", "b", "undo"]);
    assert_eq!(
        env.ids(&["--branch", "b", "list", "--status", "closed"]),
        vec![4]
    );
    assert_eq!(env.version(), 6);
}

#[test]
fn state_survives_many_processes() {
    let env = Env::new();
    for i in 0..15 {
        env.ok(&["add", &format!("issue {i}")]);
    }
    for i in 1..=15 {
        env.ok(&["comment", &i.to_string(), "hello"]);
    }
    assert_eq!(env.ids(&["list"]).len(), 15);
    assert_eq!(env.version(), 30);
    env.ok(&["verify"]);
    assert!(env.ok(&["show", "15"]).contains("comments (1):"));
}

fn forked() -> Env {
    let env = populated();
    env.ok(&["branch", "side"]);
    env
}

#[test]
fn merge_applies_disjoint_changes() {
    let env = forked();
    env.ok(&["--branch", "side", "close", "1"]);
    env.ok(&["--branch", "side", "assign", "2", "carol"]);
    env.ok(&["comment", "3", "on main"]);
    let out = env.ok(&["merge", "side"]);
    assert!(out.contains("merge side: 3 applied, 0 conflicts"), "{out}");
    assert!(env.lines(&["log", "--limit", "1"])[0].ends_with("merge side: 3 applied, 0 conflicts"));
    assert_eq!(env.ids(&["list", "--status", "closed"]), vec![4, 1]);
    assert_eq!(env.ids(&["list", "--assignee", "carol"]), vec![2]);
    assert!(env.ok(&["show", "3"]).contains("on main"));
    assert!(env.ok(&["users"]).contains("carol"));
}

#[test]
fn merge_conflict_keeps_ours_by_default() {
    let env = forked();
    env.ok(&["--branch", "side", "edit", "1", "--title", "Theirs"]);
    env.ok(&["edit", "1", "--title", "Ours"]);
    env.ok(&["--branch", "side", "close", "2"]);
    let out = env.ok(&["merge", "side"]);
    assert!(out.contains("merge side: 1 applied, 1 conflicts"), "{out}");
    assert!(out.contains("conflict #1: kept main"), "{out}");
    assert!(env.ok(&["show", "1"]).contains("Ours"));
    assert!(env.ids(&["list", "--status", "closed"]).contains(&2));
}

#[test]
fn merge_conflict_with_theirs_flag_takes_theirs() {
    let env = forked();
    env.ok(&["--branch", "side", "edit", "1", "--title", "Theirs"]);
    env.ok(&["edit", "1", "--title", "Ours"]);
    let out = env.ok(&["merge", "side", "--theirs"]);
    assert!(out.contains("merge side: 0 applied, 1 conflicts"), "{out}");
    assert!(out.contains("conflict #1: kept side"), "{out}");
    assert!(env.ok(&["show", "1"]).contains("Theirs"));
}

#[test]
fn merge_without_changes_is_a_no_op() {
    let env = forked();
    let before = env.version();
    let out = env.ok(&["merge", "side"]);
    assert!(
        out.contains("no change: merge side: 0 applied, 0 conflicts"),
        "{out}"
    );
    assert_eq!(env.version(), before);
    env.fail(&["merge", "ghost"]);
}

#[test]
fn merge_same_id_adds_are_renumbered_not_lost() {
    let env = forked();
    env.ok(&["add", "Ours five"]);
    env.ok(&["--branch", "side", "add", "Theirs five"]);
    env.ok(&["--branch", "side", "add", "Theirs six"]);
    let out = env.ok(&["merge", "side"]);
    assert!(out.contains("merge side: 2 applied, 0 conflicts"), "{out}");
    assert!(out.contains("renumbered #5 from side as #7"), "{out}");
    let titles = env.ok(&["list"]);
    assert!(titles.contains("Ours five"));
    assert!(titles.contains("Theirs five"));
    assert!(titles.contains("Theirs six"));
    assert_eq!(env.ids(&["list"]), vec![7, 6, 5, 4, 3, 2, 1]);
    assert!(env.ok(&["show", "5"]).contains("Ours five"));
    assert!(env.ok(&["show", "7"]).contains("Theirs five"));
    env.ok(&["add", "After"]);
    assert_eq!(env.ids(&["list", "--limit", "1"]), vec![8]);
}

#[test]
fn stats_are_correct_after_merge() {
    let env = forked();
    env.ok(&["--branch", "side", "close", "1"]);
    env.ok(&["--branch", "side", "assign", "2", "bob"]);
    env.ok(&["--branch", "side", "label", "3", "ui"]);
    env.ok(&[
        "--branch",
        "side",
        "add",
        "New",
        "--assignee",
        "dave",
        "--priority",
        "critical",
    ]);
    env.ok(&["merge", "side"]);
    let out = env.ok(&["stats"]);
    let row = |name: &str| -> u64 {
        out.lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap()
            .split_whitespace()
            .last()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert!(out.contains("issues:   5"));
    assert_eq!(row("open"), 2);
    assert_eq!(row("closed"), 2);
    assert_eq!(row("in-progress"), 1);
    assert_eq!(row("bob"), 2);
    assert_eq!(row("dave"), 1);
    assert_eq!(row("alice"), 1);
    assert_eq!(row("critical"), 1);
    assert_eq!(row("ui"), 2);
    assert_eq!(env.ids(&["list", "--assignee", "dave"]), vec![5]);
    assert_eq!(env.ids(&["list", "--label", "ui"]), vec![3, 1]);
}
