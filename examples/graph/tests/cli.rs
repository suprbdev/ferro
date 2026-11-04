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
        let db = dir.path().join("g.ferro");
        Env { _dir: dir, db }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = cargo_bin_cmd!("graph");
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

    fn version(&self, args: &[&str]) -> u64 {
        let text = self.ok(args);
        let tail = text.rsplit("(v").next().unwrap();
        tail.trim_end().trim_end_matches(')').parse().unwrap()
    }

    fn ids(&self, args: &[&str]) -> Vec<u32> {
        self.ok(args)
            .lines()
            .filter_map(|line| line.split('\t').next()?.parse().ok())
            .collect()
    }

    fn small(&self) {
        for i in 0..8 {
            self.ok(&["user", &format!("n{i}")]);
        }
        for (a, b) in [(0, 1), (0, 2), (1, 3), (2, 3), (3, 4), (4, 5), (6, 7)] {
            self.ok(&["follow", &a.to_string(), &b.to_string()]);
        }
    }
}

#[test]
fn users_and_follows() {
    let env = Env::new();
    assert!(env.ok(&["user", "alice"]).starts_with("user 0 alice"));
    assert!(env.ok(&["user", "bob"]).starts_with("user 1 bob"));
    assert!(env.fail(&["user", "alice"]).contains("taken"));
    assert!(env.fail(&["user", "42"]).contains("invalid"));
    assert!(
        env.ok(&["follow", "alice", "bob"])
            .starts_with("followed 0 -> 1")
    );
    assert!(env.ok(&["follow", "0", "1"]).contains("already following"));
    assert!(env.fail(&["follow", "0", "0"]).contains("yourself"));
    assert!(env.fail(&["follow", "0", "9"]).contains("no such user"));
    assert_eq!(env.ids(&["following", "alice"]), vec![1]);
    assert_eq!(env.ids(&["followers", "bob"]), vec![0]);
    assert!(
        env.ok(&["count", "bob"])
            .contains("followers 1 following 0")
    );
    assert!(
        env.ok(&["count", "bob", "--scan"])
            .contains("followers 1 following 0")
    );
    assert!(
        env.ok(&["unfollow", "alice", "bob"])
            .starts_with("unfollowed")
    );
    assert!(
        env.ok(&["unfollow", "alice", "bob"])
            .contains("was not following")
    );
    assert!(env.ids(&["followers", "bob"]).is_empty());
    assert!(
        env.ok(&["count", "bob"])
            .contains("followers 0 following 0")
    );
    env.ok(&["verify"]);
}

#[test]
fn pagination_walks_every_follower_once() {
    let env = Env::new();
    for i in 0..40 {
        env.ok(&["user", &format!("p{i}")]);
    }
    for i in 1..40 {
        env.ok(&["follow", &i.to_string(), "0"]);
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut args = vec!["followers", "0", "--limit", "7"];
        if let Some(c) = &cursor {
            args.push("--after");
            args.push(c);
        }
        let text = env.ok(&args);
        seen.extend(env.ids(&args));
        pages += 1;
        match text.lines().last().unwrap().strip_prefix("next ") {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }
    assert_eq!(pages, 6);
    assert_eq!(seen, (1..40).collect::<Vec<u32>>());
    assert!(
        env.ok(&["followers", "0", "--limit", "39"])
            .ends_with("end\n")
    );
}

#[test]
fn mutuals_intersects_both_directions() {
    let env = Env::new();
    env.small();
    let text = env.ok(&["mutuals", "1", "2"]);
    assert!(text.starts_with("count 1\n3\t"), "{text}");
    let text = env.ok(&["mutuals", "1", "2", "--followers"]);
    assert!(text.starts_with("count 1\n0\t"), "{text}");
    assert!(env.ok(&["mutuals", "0", "6"]).starts_with("count 0"));
    assert!(
        env.ok(&["mutuals", "0", "1"])
            .contains("0 follows 1: true, 1 follows 0: false")
    );
}

#[test]
fn mutuals_matches_brute_force_for_celebrity_and_balanced_pairs() {
    let env = Env::new();
    env.ok(&[
        "seed", "--users", "3000", "--edges", "40000", "--seed", "11",
    ]);
    let stats = env.ok(&["stats"]);
    let tops: Vec<u32> = stats
        .lines()
        .filter_map(|l| l.strip_prefix("top "))
        .map(|l| l.split_whitespace().next().unwrap().parse().unwrap())
        .collect();
    let all = |kind: &str, user: u32| -> Vec<u32> {
        env.ids(&[kind, &user.to_string(), "--limit", "100000"])
    };
    let brute = |kind: &str, a: u32, b: u32| -> Vec<u32> {
        let (x, y) = (all(kind, a), all(kind, b));
        x.into_iter().filter(|id| y.contains(id)).collect()
    };
    let small = (0..3000u32)
        .find(|&u| {
            let n = all("followers", u).len();
            (1..=4).contains(&n) && !tops.contains(&u)
        })
        .unwrap();
    let pairs = [(tops[0], small), (tops[0], tops[1]), (tops[2], tops[3])];
    for (a, b) in pairs {
        for (flag, kind) in [("--followers", "followers"), ("", "following")] {
            let mut args = vec!["mutuals".to_string(), a.to_string(), b.to_string()];
            args.push("--limit".to_string());
            args.push("100000".to_string());
            if !flag.is_empty() {
                args.push(flag.to_string());
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let text = env.ok(&refs);
            let mut got: Vec<u32> = text
                .lines()
                .skip(1)
                .filter_map(|l| l.split('\t').next()?.parse().ok())
                .collect();
            got.sort_unstable();
            let want = brute(kind, a, b);
            assert_eq!(got, want, "{kind} {a} {b}");
            assert!(text.starts_with(&format!("count {}\n", want.len())));
        }
    }
}

#[test]
fn path_is_shortest_and_respects_depth() {
    let env = Env::new();
    env.small();
    assert!(
        env.ok(&["path", "0", "5"])
            .starts_with("length 4 path 0 -> 1 -> 3 -> 4 -> 5")
    );
    assert!(
        env.ok(&["path", "0", "5", "--max-depth", "3"])
            .contains("no path within 3")
    );
    assert!(env.ok(&["path", "0", "6"]).contains("no path"));
    assert!(env.ok(&["path", "5", "0"]).contains("no path"));
    assert!(env.ok(&["path", "2", "2"]).starts_with("length 0"));
    env.ok(&["follow", "0", "4"]);
    assert!(
        env.ok(&["path", "0", "5"])
            .starts_with("length 2 path 0 -> 4 -> 5")
    );
}

#[test]
fn recommend_ranks_by_mutual_count_and_skips_follows_and_blocks() {
    let env = Env::new();
    env.small();
    let text = env.ok(&["recommend", "0"]);
    assert_eq!(text.lines().collect::<Vec<_>>().len(), 1, "{text}");
    assert!(text.starts_with("3\tn3\t2"));
    env.ok(&["follow", "0", "3"]);
    assert!(env.ok(&["recommend", "0"]).starts_with("4\t"));
    env.ok(&["block", "0", "4"]);
    assert!(env.ok(&["recommend", "0"]).is_empty());
    env.ok(&["block", "5", "3"]);
    assert!(env.ok(&["recommend", "1"]).contains("4\t"));
    assert!(env.ok(&["recommend", "4"]).is_empty());
}

#[test]
fn block_removes_follows_and_forbids_new_ones() {
    let env = Env::new();
    env.small();
    env.ok(&["follow", "1", "0"]);
    assert!(env.ok(&["block", "0", "1"]).starts_with("blocked 0 -> 1"));
    assert!(env.ok(&["block", "0", "1"]).contains("already blocked"));
    assert_eq!(env.ids(&["following", "0"]), vec![2]);
    assert_eq!(env.ids(&["following", "1"]), vec![3]);
    assert!(env.fail(&["follow", "0", "1"]).contains("block"));
    assert!(env.fail(&["follow", "1", "0"]).contains("block"));
    env.ok(&["verify"]);
}

#[test]
fn timeline_merges_followed_authors_newest_first() {
    let env = Env::new();
    env.small();
    let mut ids = Vec::new();
    for (author, text) in [
        ("1", "one"),
        ("2", "two"),
        ("3", "three"),
        ("1", "four"),
        ("2", "five"),
    ] {
        let out = env.ok(&["post", author, text]);
        ids.push(out.split_whitespace().nth(1).unwrap().to_string());
    }
    let text = env.ok(&["timeline", "0"]);
    let rows: Vec<Vec<&str>> = text.lines().map(|l| l.split('\t').collect()).collect();
    let texts: Vec<&str> = rows.iter().map(|r| r[3]).collect();
    assert_eq!(texts, vec!["five", "four", "two", "one"]);
    let limited = env.ok(&["timeline", "0", "--limit", "2"]);
    assert_eq!(limited.lines().count(), 2);
    assert!(env.ok(&["timeline", "7"]).is_empty());
    assert!(env.fail(&["post", "99", "x"]).contains("no such user"));
}

#[test]
fn inbox_timeline_matches_computed_timeline_for_fanout_posts() {
    let env = Env::new();
    env.small();
    env.ok(&["post", "1", "early"]);
    env.ok(&["post", "1", "a", "--fanout"]);
    env.ok(&["post", "2", "b", "--fanout"]);
    let computed = env.ok(&["timeline", "0", "--limit", "2"]);
    let inbox = env.ok(&["timeline", "0", "--limit", "2", "--inbox"]);
    assert_eq!(computed, inbox);
    assert!(
        env.ok(&["timeline", "0", "--limit", "10", "--inbox"])
            .lines()
            .count()
            == 2
    );
}

#[test]
fn likes_are_counted_once() {
    let env = Env::new();
    env.small();
    env.ok(&["post", "1", "hello"]);
    assert!(env.ok(&["like", "0", "1"]).starts_with("liked post 1 by 0"));
    assert!(env.ok(&["like", "0", "1"]).contains("already liked"));
    assert!(env.ok(&["like", "2", "1"]).starts_with("liked"));
    assert!(env.fail(&["like", "0", "77"]).contains("no such post"));
    assert!(env.ok(&["stats"]).contains("likes 2"));
}

#[test]
fn reach_counts_distinct_users_per_hop() {
    let env = Env::new();
    env.small();
    assert!(
        env.ok(&["reach", "0", "--hops", "1"])
            .starts_with("reach 2 levels 2")
    );
    assert!(
        env.ok(&["reach", "0", "--hops", "2"])
            .starts_with("reach 3 levels 2 1")
    );
    assert!(
        env.ok(&["reach", "0", "--hops", "9"])
            .starts_with("reach 5 levels 2 1 1 1")
    );
    assert!(
        env.ok(&["reach", "3", "--hops", "2", "--dir", "in"])
            .starts_with("reach 3 levels 2 1")
    );
    assert!(
        env.ok(&["reach", "3", "--hops", "1", "--dir", "both"])
            .starts_with("reach 3 levels 3")
    );
}

#[test]
fn graph_at_and_diff_see_old_versions() {
    let env = Env::new();
    env.small();
    let before = env.version(&["follow", "5", "6"]);
    let after = env.version(&["unfollow", "0", "1"]);
    let newer = env.version(&["follow", "7", "0"]);
    assert_eq!(env.ids(&["following", "0"]), vec![2]);
    assert_eq!(
        env.ids(&["graph-at", &before.to_string(), "following", "0"]),
        vec![1, 2]
    );
    assert_eq!(
        env.ids(&["--at", &before.to_string(), "following", "0"]),
        vec![1, 2]
    );
    assert_eq!(
        env.ids(&["graph-at", &after.to_string(), "following", "0"]),
        vec![2]
    );
    assert_eq!(
        env.ids(&["graph-at", &after.to_string(), "followers", "0"]),
        Vec::<u32>::new()
    );
    assert_eq!(
        env.ids(&["graph-at", &newer.to_string(), "followers", "0"]),
        vec![7]
    );
    assert!(
        env.ok(&["graph-at", &before.to_string(), "stats"])
            .contains("follows 8")
    );
    assert!(env.ok(&["stats"]).contains("follows 8"));
    assert!(env.fail(&["graph-at", "9999", "stats"]).contains("version"));
    let text = env.ok(&["diff", &before.to_string(), &newer.to_string()]);
    assert!(text.starts_with("added 1 removed 1"), "{text}");
    assert!(text.contains("+ 7 -> 0"));
    assert!(text.contains("- 0 -> 1"));
    let none = env.ok(&["diff", &newer.to_string(), &newer.to_string()]);
    assert!(none.starts_with("added 0 removed 0"));
    let back = env.ok(&["diff", &newer.to_string(), &before.to_string()]);
    assert!(back.contains("+ 0 -> 1") && back.contains("- 7 -> 0"));
    assert!(
        env.ok(&["log", "--limit", "3"])
            .starts_with(&format!("v{newer}\t"))
    );
}

#[test]
fn diff_across_large_adjacency_with_block_splits() {
    let env = Env::new();
    env.ok(&["seed", "--users", "3000", "--edges", "9000", "--seed", "3"]);
    let start = env.version(&["follow", "1", "2"]);
    for i in 10..700 {
        env.ok(&["follow", &i.to_string(), "9"]);
    }
    let end = env.version(&["follow", "1", "3"]);
    let text = env.ok(&[
        "diff",
        &start.to_string(),
        &end.to_string(),
        "--limit",
        "1000",
    ]);
    let added = text.lines().filter(|l| l.starts_with("+ ")).count();
    assert!(text.contains(&format!("added {added} removed 0")));
    assert!(added > 600 && added <= 691, "{added}");
    assert!(text.contains("+ 1 -> 3"));
    env.ok(&["verify"]);
}

#[test]
fn pagerank_and_communities() {
    let env = Env::new();
    env.small();
    let text = env.ok(&[
        "pagerank",
        "--iterations",
        "30",
        "--threads",
        "3",
        "--top",
        "3",
    ]);
    assert!(text.starts_with("sum 1.0000"), "{text}");
    let top: Vec<&str> = text.lines().nth(1).unwrap().split('\t').collect();
    assert!(top[0] == "5" || top[0] == "7", "{text}");
    let one = env.ok(&["pagerank", "--iterations", "10", "--threads", "1"]);
    let many = env.ok(&["pagerank", "--iterations", "10", "--threads", "5"]);
    assert_eq!(one, many);
    let wcc = env.ok(&["communities"]);
    assert!(wcc.starts_with("communities 2"), "{wcc}");
    assert!(wcc.lines().nth(1).unwrap().ends_with("\t6"));
    assert!(wcc.lines().nth(2).unwrap().ends_with("\t2"));
    let lpa = env.ok(&["communities", "--algo", "lpa", "--iterations", "20"]);
    let count: usize = lpa.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert!((2..=8).contains(&count), "{lpa}");
    assert!(
        env.ok(&["graph-at", "1", "communities"])
            .starts_with("communities 1")
    );
}

#[test]
fn seed_stats_verify() {
    let env = Env::new();
    let text = env.ok(&[
        "seed", "--users", "800", "--edges", "6000", "--posts", "300", "--likes", "200",
    ]);
    assert!(text.contains("users 800"));
    assert!(text.contains("posts 300"));
    let stats = env.ok(&["stats"]);
    assert!(stats.contains("users 800"));
    assert!(stats.contains("posts 300"));
    assert!(stats.contains("likes 200"));
    assert!(stats.contains("top "));
    let verify = env.ok(&["verify"]);
    assert!(verify.contains("ok: "), "{verify}");
    assert!(env.fail(&["seed", "--users", "10"]).contains("empty"));
    let page = env.ok(&["followers", "u1", "--limit", "5"]);
    assert!(page.lines().count() >= 1);
    let timeline = env.ok(&["timeline", "u2", "--limit", "5"]);
    assert!(timeline.lines().count() <= 5);
    let before = env.ok(&["stats"]);
    env.ok(&["follow", "u1", "u700"]);
    env.ok(&["unfollow", "u1", "u700"]);
    assert_eq!(
        before.lines().find(|l| l.starts_with("follows")),
        env.ok(&["stats"])
            .lines()
            .find(|l| l.starts_with("follows"))
    );
    env.ok(&["verify"]);
}

#[test]
fn reads_work_on_a_read_only_handle_and_names_resolve() {
    let env = Env::new();
    env.ok(&["user", "alice"]);
    env.ok(&["user", "bob"]);
    env.ok(&["follow", "alice", "bob"]);
    assert!(env.fail(&["following", "carol"]).contains("no such user"));
    assert_eq!(env.ids(&["following", "alice"]), vec![1]);
    assert!(env.ok(&["following", "alice"]).contains("bob"));
}

#[test]
fn experiment_and_bench_run() {
    let env = Env::new();
    for rep in ["nested", "flat", "both", "bucketed"] {
        let out = env.ok(&[
            "experiment",
            rep,
            "--users",
            "2000",
            "--edges",
            "8000",
            "--celebrity-followers",
            "700",
        ]);
        assert!(out.contains("bytes/edge"), "{out}");
        assert!(out.contains("celebrity row update"));
    }
    env.ok(&[
        "seed", "--users", "1500", "--edges", "9000", "--posts", "500",
    ]);
    assert!(
        env.ok(&["bench", "counts", "--commits", "20"])
            .contains("cached")
    );
    assert!(
        env.ok(&["bench", "fanout", "--commits", "3"])
            .contains("fanout-on-write")
    );
    assert!(
        env.ok(&["bench", "overhead", "--lookups", "2000"])
            .contains("contains")
    );
    assert!(
        env.ok(&["bench", "pagerank", "--iterations", "2"])
            .contains("pagerank")
    );
    let writers = env.ok(&["bench", "writers", "--seconds", "1", "--iterations", "3"]);
    assert!(
        writers.contains("identical idle vs during writes: true"),
        "{writers}"
    );
}
