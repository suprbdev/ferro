use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use tempfile::TempDir;

struct Env {
    dir: TempDir,
    db: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("w.ferro");
        Env { dir, db }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = cargo_bin_cmd!("wiki");
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

    fn put(&self, slug: &str, body: &str, extra: &[&str]) -> String {
        let mut args = vec!["put", slug, "--stdin"];
        args.extend_from_slice(extra);
        let out = self
            .cmd(&args)
            .write_stdin(body.to_string())
            .assert()
            .success()
            .get_output()
            .clone();
        String::from_utf8(out.stdout).unwrap()
    }

    fn put_in(&self, draft: &str, slug: &str, body: &str) -> String {
        let out = self
            .cmd(&["--draft", draft, "put", slug, "--stdin"])
            .write_stdin(body.to_string())
            .assert()
            .success()
            .get_output()
            .clone();
        String::from_utf8(out.stdout).unwrap()
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

    fn size(&self) -> u64 {
        fs::metadata(&self.db).unwrap().len()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

fn slugs(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect()
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn put_and_get_round_trip() {
    let env = Env::new();
    let out = env.put(
        "Home Page",
        "# Welcome\nhello world\n",
        &["--author", "ada", "--tag", "intro"],
    );
    assert!(out.contains("created home-page (v1)"));
    assert_eq!(env.ok(&["get", "home-page"]), "# Welcome\nhello world\n");
    assert_eq!(env.ok(&["get", "Home Page"]), "# Welcome\nhello world\n");
    let meta = env.ok(&["get", "home-page", "--meta"]);
    assert!(meta.contains("title:   Welcome"));
    assert!(meta.contains("author:  ada"));
    assert!(meta.contains("tags:    intro"));
}

#[test]
fn put_from_file_and_metadata_only_update() {
    let env = Env::new();
    let file = env.path("body.md");
    fs::write(&file, "plain body\n").unwrap();
    env.cmd(&[
        "put",
        "doc",
        "--file",
        file.to_str().unwrap(),
        "--title",
        "Doc",
    ])
    .assert()
    .success();
    assert_eq!(env.ok(&["get", "doc"]), "plain body\n");
    let out = env.ok(&[
        "put", "doc", "--tag", "b", "--tag", "a", "--author", "grace",
    ]);
    assert!(out.contains("updated doc"));
    let meta = env.ok(&["get", "doc", "--meta"]);
    assert!(meta.contains("title:   Doc"));
    assert!(meta.contains("tags:    a, b"));
    assert!(meta.contains("author:  grace"));
    assert_eq!(env.ok(&["get", "doc"]), "plain body\n");
}

#[test]
fn identical_put_is_not_a_commit() {
    let env = Env::new();
    env.put("a", "same\n", &["--title", "A"]);
    let version = env.version();
    let out = env.put("a", "same\n", &["--title", "A"]);
    assert!(out.contains("no change"));
    assert_eq!(env.version(), version);
}

#[test]
fn put_requires_body_for_new_page() {
    let env = Env::new();
    let err = env.fail(&["put", "ghost", "--title", "Ghost"]);
    assert!(err.contains("does not exist"));
    env.cmd(&["put", "x", "--stdin", "--file", "y"])
        .assert()
        .failure();
}

#[test]
fn get_missing_page_fails() {
    let env = Env::new();
    env.put("a", "x\n", &[]);
    assert!(env.fail(&["get", "nope"]).contains("no such page"));
}

#[test]
fn get_at_version_returns_old_body() {
    let env = Env::new();
    env.put("a", "first\n", &[]);
    env.put("a", "second\n", &[]);
    env.put("a", "third\n", &[]);
    assert_eq!(env.ok(&["get", "a", "--at", "1"]), "first\n");
    assert_eq!(env.ok(&["get", "a", "--at", "2"]), "second\n");
    assert_eq!(env.ok(&["get", "a"]), "third\n");
    env.cmd(&["get", "a", "--at", "99"]).assert().failure();
}

#[test]
fn delete_removes_page_but_history_remains() {
    let env = Env::new();
    env.put("a", "alpha body\n", &[]);
    env.put("b", "beta body\n", &[]);
    let out = env.ok(&["delete", "a"]);
    assert!(out.contains("deleted a"));
    assert!(env.fail(&["get", "a"]).contains("no such page"));
    assert_eq!(slugs(&env.ok(&["list"])), vec!["b"]);
    assert!(env.ok(&["search", "alpha"]).starts_with("0 matching"));
    assert_eq!(env.ok(&["get", "a", "--at", "1"]), "alpha body\n");
    assert!(env.fail(&["delete", "a"]).contains("no such page"));
    env.cmd(&["verify"]).assert().success();
}

#[test]
fn rename_rewrites_links_and_backlinks() {
    let env = Env::new();
    env.put("old-name", "# Old\ntarget\n", &[]);
    env.put("p1", "see [[old-name]] and [[Old Name|the label]]\n", &[]);
    env.put("p2", "also [[old-name#anchor]]\nunrelated [[other]]\n", &[]);
    env.put("self", "ok\n", &[]);
    assert_eq!(slugs(&env.ok(&["backlinks", "old-name"])), vec!["p1", "p2"]);
    let out = env.ok(&["rename", "old-name", "new-name"]);
    assert!(out.contains("rewrote links in 2 pages"));
    assert_eq!(slugs(&env.ok(&["backlinks", "new-name"])), vec!["p1", "p2"]);
    assert!(env.ok(&["backlinks", "old-name"]).is_empty());
    assert_eq!(
        env.ok(&["get", "p1"]),
        "see [[new-name]] and [[new-name|the label]]\n"
    );
    assert_eq!(
        env.ok(&["get", "p2"]),
        "also [[new-name]]\nunrelated [[other]]\n"
    );
    assert_eq!(env.ok(&["get", "new-name"]), "# Old\ntarget\n");
    assert!(env.fail(&["get", "old-name"]).contains("no such page"));
    assert!(env.ok(&["search", "target"]).contains("new-name"));
    env.cmd(&["verify"]).assert().success();
}

#[test]
fn rename_rejects_collisions_and_missing() {
    let env = Env::new();
    env.put("a", "x\n", &[]);
    env.put("b", "y\n", &[]);
    assert!(env.fail(&["rename", "a", "b"]).contains("already exists"));
    assert!(env.fail(&["rename", "zzz", "c"]).contains("no such page"));
    assert!(env.fail(&["rename", "a", "a"]).contains("same"));
}

#[test]
fn rename_handles_self_link() {
    let env = Env::new();
    env.put("loop", "refers to [[loop]] itself\n", &[]);
    env.ok(&["rename", "loop", "circle"]);
    assert_eq!(env.ok(&["get", "circle"]), "refers to [[circle]] itself\n");
    env.cmd(&["verify"]).assert().success();
}

#[test]
fn list_filters_by_tag_and_prefix() {
    let env = Env::new();
    env.put("docs/a", "x\n", &["--tag", "rust", "--tag", "db"]);
    env.put("docs/b", "x\n", &["--tag", "db"]);
    env.put("notes/c", "x\n", &["--tag", "rust"]);
    assert_eq!(
        slugs(&env.ok(&["list"])),
        vec!["docs/a", "docs/b", "notes/c"]
    );
    assert_eq!(
        slugs(&env.ok(&["list", "--tag", "rust"])),
        vec!["docs/a", "notes/c"]
    );
    assert_eq!(
        slugs(&env.ok(&["list", "--tag", "DB"])),
        vec!["docs/a", "docs/b"]
    );
    assert_eq!(
        slugs(&env.ok(&["list", "--prefix", "docs/"])),
        vec!["docs/a", "docs/b"]
    );
    assert_eq!(
        slugs(&env.ok(&["list", "--tag", "rust", "--prefix", "notes"])),
        vec!["notes/c"]
    );
    assert_eq!(slugs(&env.ok(&["list", "--limit", "1"])), vec!["docs/a"]);
    assert!(env.ok(&["list", "--tag", "none"]).is_empty());
}

#[test]
fn search_ranks_by_term_frequency() {
    let env = Env::new();
    env.put("once", "ferro appears once here\n", &[]);
    env.put("thrice", "ferro ferro ferro appears often\n", &[]);
    env.put("twice", "ferro and ferro again\n", &[]);
    env.put("none", "completely different text\n", &[]);
    let out = env.ok(&["search", "ferro"]);
    assert!(out.starts_with("3 matching pages"));
    let order: Vec<String> = out
        .lines()
        .skip(1)
        .filter(|line| !line.starts_with("        "))
        .map(|line| line.split_whitespace().nth(1).unwrap().to_string())
        .collect();
    assert_eq!(order, vec!["thrice", "twice", "once"]);
    assert!(out.contains("ferro ferro ferro appears often"));
}

#[test]
fn search_requires_all_terms_and_is_case_insensitive() {
    let env = Env::new();
    env.put("a", "Alpha Beta\n", &[]);
    env.put("b", "alpha only\n", &[]);
    let out = env.ok(&["search", "ALPHA", "beta"]);
    assert!(out.starts_with("1 matching pages"));
    assert!(out.contains(" a "));
    assert!(
        env.ok(&["search", "alpha", "missingterm"])
            .starts_with("0 matching")
    );
    let limited = env.ok(&["search", "alpha", "--limit", "1"]);
    assert!(limited.starts_with("2 matching pages"));
    assert_eq!(
        limited
            .lines()
            .filter(|l| !l.starts_with("        "))
            .count(),
        2
    );
}

#[test]
fn search_tracks_edits_and_title() {
    let env = Env::new();
    env.put("a", "old words here\n", &["--title", "Zebra Crossing"]);
    assert!(env.ok(&["search", "zebra"]).contains(" a "));
    assert!(env.ok(&["search", "old"]).contains(" a "));
    env.put("a", "new words here\n", &[]);
    assert!(env.ok(&["search", "old"]).starts_with("0 matching"));
    assert!(env.ok(&["search", "new"]).contains(" a "));
    env.cmd(&["verify"]).assert().success();
}

#[test]
fn backlinks_follow_edits_and_dangling_targets() {
    let env = Env::new();
    env.put("a", "links [[b]] and [[c]]\n", &[]);
    env.put("b", "links [[c]]\n", &[]);
    assert_eq!(slugs(&env.ok(&["backlinks", "c"])), vec!["a", "b"]);
    env.put("a", "links [[b]] only\n", &[]);
    assert_eq!(slugs(&env.ok(&["backlinks", "c"])), vec!["b"]);
    assert_eq!(slugs(&env.ok(&["backlinks", "b"])), vec!["a"]);
    assert!(env.ok(&["backlinks", "a"]).is_empty());
    env.cmd(&["verify"]).assert().success();
}

#[test]
fn orphans_are_pages_nobody_links_to() {
    let env = Env::new();
    env.put("home", "[[a]] [[b]]\n", &[]);
    env.put("a", "[[b]] [[a]]\n", &[]);
    env.put("b", "leaf\n", &[]);
    env.put("lonely", "nothing\n", &[]);
    assert_eq!(slugs(&env.ok(&["orphans"])), vec!["home", "lonely"]);
}

#[test]
fn history_lists_every_revision_of_one_page() {
    let env = Env::new();
    env.put("a", "line one\nline two\n", &["--author", "ada"]);
    env.put("b", "other\n", &[]);
    env.put("a", "line one\nline 2\nline three\n", &["--author", "ada"]);
    env.ok(&["rename", "a", "c"]);
    env.ok(&["delete", "c"]);
    let out = env.ok(&["history", "c"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[0].starts_with("v1 ") && lines[0].contains("created"));
    assert!(lines[1].starts_with("v3 ") && lines[1].contains("body +2 -1 lines"));
    assert!(lines[2].starts_with("v4 ") && lines[2].contains("renamed from a"));
    assert!(lines[3].starts_with("v5 ") && lines[3].contains("deleted"));
    assert!(
        env.fail(&["history", "never-existed"])
            .contains("no history")
    );
}

#[test]
fn diff_shows_line_changes_between_versions() {
    let env = Env::new();
    env.put("a", "one\ntwo\nthree\n", &["--title", "T"]);
    env.put("a", "one\n2\nthree\nfour\n", &["--title", "T2"]);
    let out = env.ok(&["diff", "a", "1", "2"]);
    assert!(out.contains("title: T -> T2"));
    assert!(out.contains("--- a@v1"));
    assert!(out.contains("+++ a@v2"));
    assert!(out.contains("-two"));
    assert!(out.contains("+2"));
    assert!(out.contains("+four"));
    assert!(env.fail(&["diff", "zzz", "1", "2"]).contains("neither"));
}

#[test]
fn revert_restores_one_page_and_leaves_others() {
    let env = Env::new();
    env.put("a", "a original\n", &["--tag", "x"]);
    env.put("b", "b original\n", &[]);
    env.put("a", "a edited\n", &["--tag", "y"]);
    env.put("b", "b edited\n", &[]);
    let out = env.ok(&["revert", "a", "1"]);
    assert!(out.contains("reverted a to v1"));
    assert_eq!(env.ok(&["get", "a"]), "a original\n");
    assert_eq!(env.ok(&["get", "b"]), "b edited\n");
    assert!(env.ok(&["get", "a", "--meta"]).contains("tags:    x"));
    assert!(env.ok(&["search", "edited"]).contains(" b "));
    assert!(!env.ok(&["search", "edited"]).contains(" a "));
    assert!(env.ok(&["revert", "a", "1"]).contains("no change"));
    env.cmd(&["verify"]).assert().success();
}

#[test]
fn revert_recreates_deleted_page_and_rejects_unknown() {
    let env = Env::new();
    env.put("a", "keep me\n", &[]);
    env.put("b", "other\n", &[]);
    env.ok(&["delete", "a"]);
    env.ok(&["revert", "a", "1"]);
    assert_eq!(env.ok(&["get", "a"]), "keep me\n");
    assert!(env.fail(&["revert", "b", "1"]).contains("did not exist"));
    env.cmd(&["verify"]).assert().success();
}

#[test]
fn revert_keeps_current_slug_after_rename() {
    let env = Env::new();
    env.put("a", "content one\n", &[]);
    env.put("a", "content two\n", &[]);
    env.ok(&["rename", "a", "z"]);
    env.ok(&["revert", "z", "1"]);
    assert_eq!(env.ok(&["get", "z"]), "content one\n");
    assert_eq!(slugs(&env.ok(&["list"])), vec!["z"]);
}

#[test]
fn import_batches_files_and_is_idempotent() {
    let env = Env::new();
    let dir = env.path("corpus");
    for i in 0..7 {
        write(
            &dir.join(format!("cat{}/page{i}.md", i % 2)),
            &format!("# Page {i}\nshared token and unique{i}word [[cat0/page0]]\n"),
        );
    }
    write(&dir.join("top.md"), "# Top\nroot level\n");
    write(&dir.join("ignored.txt"), "not markdown\n");
    let out = env.ok(&[
        "import",
        dir.to_str().unwrap(),
        "--batch-pages",
        "3",
        "--author",
        "bot",
    ]);
    assert!(
        out.contains("imported 8 new, 0 updated, 0 unchanged, 0 skipped in 3 commits"),
        "{out}"
    );
    assert_eq!(env.version(), 3);
    assert!(env.ok(&["get", "cat1/page3"]).contains("unique3word"));
    assert!(
        env.ok(&["get", "cat1/page3", "--meta"])
            .contains("title:   Page 3")
    );
    assert!(
        env.ok(&["get", "cat1/page3", "--meta"])
            .contains("author:  bot")
    );
    assert!(
        env.ok(&["get", "cat1/page3", "--meta"])
            .contains("tags:    cat1")
    );
    assert!(env.ok(&["search", "unique5word"]).contains("cat1/page5"));
    assert!(
        env.ok(&["search", "shared", "token"])
            .starts_with("7 matching")
    );
    assert_eq!(env.ok(&["backlinks", "cat0/page0"]).lines().count(), 6);
    assert_eq!(env.ok(&["list", "--tag", "cat0"]).lines().count(), 4);
    let again = env.ok(&["import", dir.to_str().unwrap()]);
    assert!(again.contains("0 new, 0 updated, 8 unchanged"), "{again}");
    let version = env.version();
    write(&dir.join("top.md"), "# Top\nchanged\n");
    let third = env.ok(&["import", dir.to_str().unwrap()]);
    assert!(third.contains("0 new, 1 updated, 7 unchanged"), "{third}");
    assert_eq!(env.version(), version + 1);
    env.cmd(&["verify"]).assert().success();
}

#[test]
fn export_round_trips_and_honours_version() {
    let env = Env::new();
    let src = env.path("src");
    write(&src.join("a/one.md"), "# One\nfirst body\n");
    write(&src.join("two.md"), "# Two\nsecond body, no newline");
    env.ok(&["import", src.to_str().unwrap()]);
    env.put("two", "# Two\nchanged\n", &[]);
    let out_dir = env.path("out");
    let msg = env.ok(&["export", out_dir.to_str().unwrap()]);
    assert!(msg.contains("exported 2 pages"));
    assert_eq!(
        fs::read_to_string(out_dir.join("a/one.md")).unwrap(),
        "# One\nfirst body\n"
    );
    assert_eq!(
        fs::read_to_string(out_dir.join("two.md")).unwrap(),
        "# Two\nchanged\n"
    );
    let old_dir = env.path("old");
    env.ok(&["export", old_dir.to_str().unwrap(), "--at", "1"]);
    assert_eq!(
        fs::read_to_string(old_dir.join("two.md")).unwrap(),
        "# Two\nsecond body, no newline"
    );
}

#[test]
fn draft_publish_merges_disjoint_changes() {
    let env = Env::new();
    let base: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    env.put("doc", &base, &["--tag", "base"]);
    env.put("keep", "unchanged\n", &[]);
    env.put("gone", "to be deleted\n", &[]);
    env.ok(&["draft", "start", "d1"]);
    assert!(env.ok(&["draft", "list"]).contains("d1"));
    let theirs = base.replace("line 18\n", "line 18 draft\n");
    env.put_in("d1", "doc", &theirs);
    env.put_in("d1", "fresh", "draft only page\n");
    env.cmd(&["--draft", "d1", "delete", "gone"])
        .assert()
        .success();
    env.ok(&["rename", "keep", "kept"]);
    let ours = base.replace("line 2\n", "line 2 main\n");
    env.put("doc", &ours, &[]);
    assert!(env.fail(&["get", "fresh"]).contains("no such page"));
    assert_eq!(
        env.ok(&["--draft", "d1", "get", "fresh"]),
        "draft only page\n"
    );
    let out = env.ok(&["draft", "publish", "d1"]);
    assert!(out.contains("0 conflicts"), "{out}");
    let merged = env.ok(&["get", "doc"]);
    assert!(merged.contains("line 2 main\n"));
    assert!(merged.contains("line 18 draft\n"));
    assert_eq!(env.ok(&["get", "fresh"]), "draft only page\n");
    assert!(env.fail(&["get", "gone"]).contains("no such page"));
    assert_eq!(env.ok(&["get", "kept"]), "unchanged\n");
    assert!(env.ok(&["search", "draft"]).contains("fresh"));
    env.cmd(&["verify"]).assert().success();
    assert!(!env.ok(&["draft", "list"]).contains("d1"));
    assert!(env.fail(&["draft", "publish", "d1"]).contains("no such branch"));
}

#[test]
fn draft_publish_reports_conflicts_and_keeps_both_sides() {
    let env = Env::new();
    env.put(
        "doc",
        "title line\nshared line\nend\n",
        &["--title", "Base"],
    );
    env.put("both", "x\n", &[]);
    env.ok(&["draft", "start", "d2"]);
    env.put_in("d2", "doc", "title line\ndraft version\nend\n");
    env.cmd(&["--draft", "d2", "put", "doc", "--title", "Draft Title"])
        .assert()
        .success();
    env.put_in("d2", "both", "from draft\n");
    env.put(
        "doc",
        "title line\nmain version\nend\n",
        &["--title", "Main Title"],
    );
    env.put("both", "from main\n", &[]);
    let out = env.ok(&["draft", "publish", "d2"]);
    assert!(
        out.contains("conflict: doc: title changed on both sides"),
        "{out}"
    );
    assert!(out.contains("conflict: doc: 1 body conflict(s)"), "{out}");
    assert!(out.contains("conflict: both: 1 body conflict(s)"), "{out}");
    let doc = env.ok(&["get", "doc"]);
    assert!(
        doc.contains("<<<<<<< main\nmain version\n=======\ndraft version\n>>>>>>> d2\n"),
        "{doc}"
    );
    assert!(
        env.ok(&["get", "doc", "--meta"])
            .contains("title:   Main Title")
    );
    env.cmd(&["verify"]).assert().success();
}

#[test]
fn draft_publish_reports_delete_edit_conflicts() {
    let env = Env::new();
    env.put("a", "one\n", &[]);
    env.put("b", "two\n", &[]);
    env.ok(&["draft", "start", "d3"]);
    env.cmd(&["--draft", "d3", "delete", "a"])
        .assert()
        .success();
    env.put_in("d3", "b", "two edited in draft\n");
    env.put("a", "one edited in main\n", &[]);
    env.ok(&["delete", "b"]);
    let out = env.ok(&["draft", "publish", "d3"]);
    assert!(
        out.contains("conflict: a: deleted in d3 but edited in main"),
        "{out}"
    );
    assert!(
        out.contains("conflict: b: edited in d3 but deleted in main"),
        "{out}"
    );
    assert_eq!(env.ok(&["get", "a"]), "one edited in main\n");
    assert!(env.fail(&["get", "b"]).contains("no such page"));
    assert!(!out.contains("dropped draft"), "{out}");
    assert!(env.ok(&["draft", "list"]).contains("d3"));
    assert!(env.ok(&["draft", "drop", "d3"]).contains("dropped draft d3"));
    assert!(!env.ok(&["draft", "list"]).contains("d3"));
}

#[test]
fn draft_requires_unknown_branch_to_fail() {
    let env = Env::new();
    env.put("a", "x\n", &[]);
    env.cmd(&["draft", "publish", "ghost"]).assert().failure();
    env.cmd(&["draft", "start", "main"]).assert().failure();
}

#[test]
fn stats_reports_counts() {
    let env = Env::new();
    env.put(
        "a",
        "[[b]] shared words\n",
        &["--author", "ada", "--tag", "t1"],
    );
    env.put(
        "b",
        "shared words\n",
        &["--author", "ada", "--tag", "t1", "--tag", "t2"],
    );
    env.put("c", "shared words\n", &["--author", "bob"]);
    let out = env.ok(&["stats", "--deep"]);
    assert!(out.contains("pages:     3"));
    assert!(out.contains("links:     1 to 1 targets"));
    assert!(out.contains("bodies:"));
    assert!(out.contains("ada              2"));
    assert!(out.contains("bob              1"));
    assert!(out.contains("t1               2"));
    assert!(out.contains("t2               1"));
    assert!(out.contains("version:   v3"));
}

#[test]
fn verify_checks_database_and_indexes() {
    let env = Env::new();
    env.put("a", "[[b]] hello world\n", &[]);
    env.put("b", "hello\n", &[]);
    env.cmd(&["verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("indexes consistent with 2 pages"));
}

#[test]
fn large_page_edit_shares_storage() {
    let env = Env::new();
    let mut body = String::new();
    let mut i = 0u64;
    while body.len() < 300_000 {
        i += 1;
        body.push_str(&format!(
            "Paragraph {i} mentions token{} and word{} with some filler text to vary content {}.\n\n",
            i * 7919 % 1013,
            i * 104_729 % 997,
            i.wrapping_mul(0x9e37_79b9) % 100_003
        ));
    }
    env.put("big", &body, &[]);
    let before = env.size();
    let edited = body.replacen("Paragraph 700 mentions", "Paragraph 700 now mentions", 1);
    assert_ne!(edited, body);
    env.put("big", &edited, &[]);
    let growth = env.size() - before;
    assert!(growth < 30_000, "edit grew the file by {growth} bytes");
    assert_eq!(env.ok(&["get", "big"]), edited);
    assert_eq!(env.ok(&["get", "big", "--at", "1"]), body);
    env.cmd(&["verify"]).assert().success();
}

#[test]
fn unicode_bodies_survive_chunking() {
    let env = Env::new();
    let body: String = "naïve café 日本語のテキスト 🚀 ".repeat(2000);
    env.put("u", &body, &[]);
    assert_eq!(env.ok(&["get", "u"]), body);
    let edited = format!("{body}tail");
    env.put("u", &edited, &[]);
    assert_eq!(env.ok(&["get", "u"]), edited);
    assert!(env.ok(&["search", "café"]).contains(" u "));
}

#[test]
fn verify_detects_tampered_indexes() {
    let env = Env::new();
    env.put("a", "hello world [[b]]\n", &[]);
    env.put("b", "other\n", &[]);
    {
        let db: ferro::Db<wiki::model::Wiki> = ferro::Options::new().open(&env.db).unwrap();
        db.commit("tamper", |w| {
            w.terms.remove("hello");
            w.links.remove("b");
        })
        .unwrap();
    }
    let out = env.cmd(&["verify"]).assert().failure().get_output().clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("index mismatch: term \"hello\" has no postings"),
        "{stdout}"
    );
    assert!(stdout.contains("missing backlink b <- a"), "{stdout}");
}
