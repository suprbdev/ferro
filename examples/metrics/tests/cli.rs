use std::fmt::Write as _;
use std::path::PathBuf;

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use ferro::Summary;
use metrics::model::Rollup;
use tempfile::TempDir;

const T0: u64 = 1_699_999_200_000;
const HOUR: u64 = 3_600_000;

struct Env {
    dir: TempDir,
    db: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.ferro");
        Env { dir, db }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = cargo_bin_cmd!("metrics");
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

    fn ingest_text(&self, text: &str) -> String {
        let path = self.dir.path().join("input.txt");
        std::fs::write(&path, text).unwrap();
        self.ok(&["ingest", path.to_str().unwrap()])
    }

    fn seed(&self) {
        let mut text = String::new();
        for host in ["a", "b", "c"] {
            for status in ["200", "500"] {
                for i in 0..120u64 {
                    let ts = T0 + i * 60_000;
                    let value = i as f64 * if status == "200" { 2.0 } else { 1.0 };
                    writeln!(
                        text,
                        "http_requests{{host=\"{host}\",status=\"{status}\"}} {value} {ts}"
                    )
                    .unwrap();
                }
            }
        }
        for i in 0..120u64 {
            let ts = T0 + i * 60_000;
            writeln!(text, "temp{{host=\"a\"}} {} {ts}", 20 + i % 10).unwrap();
        }
        self.ingest_text(&text);
    }
}

fn field(line: &str, index: usize) -> &str {
    line.split('\t').nth(index).unwrap()
}

#[test]
fn ingest_reports_and_rejects_bad_lines() {
    let env = Env::new();
    let out = env.ingest_text("up{job=\"x\"} 1 1000\n# comment\n\nnot a metric line\nup{job=\"x\"} 2 2000\n");
    assert!(out.contains("ingested 2 points"), "{out}");
    assert!(out.contains("rejected 1 lines"), "{out}");
    assert!(out.contains("1 new series"), "{out}");
}

#[test]
fn ingest_unifies_label_order_and_repeated_series() {
    let env = Env::new();
    env.ingest_text(
        "m{a=\"1\",b=\"x}y\"} 1 1000\nm{b=\"x}y\",a=\"1\"} 2 2000\nm{a=\"1\",b=\"x}y\"} 3 3000\n",
    );
    assert_eq!(env.ok(&["series", "--count"]).trim(), "1");
    assert_eq!(env.lines(&["query", "m"]).len(), 3);
    let out = env.ingest_text("m{a=\"1\",b=\"x}y\"} 4 4000\nm{a=\"1\",b=\"x}y\"} nope 5000\n");
    assert!(out.contains("ingested 1 points"), "{out}");
    assert!(out.contains("rejected 1 lines"), "{out}");
}

#[test]
fn ingest_strict_fails_on_bad_line() {
    let env = Env::new();
    let path = env.dir.path().join("bad.txt");
    std::fs::write(&path, "up 1 1000\nbroken\n").unwrap();
    let err = env.fail(&["ingest", "--strict", path.to_str().unwrap()]);
    assert!(err.contains("rejected") || err.contains("lines rejected"), "{err}");
}

#[test]
fn ingest_from_stdin_with_small_batches() {
    let env = Env::new();
    let mut text = String::new();
    for i in 0..250u64 {
        writeln!(text, "m{{k=\"v\"}} {i} {}", 5_000 + i).unwrap();
    }
    let out = env
        .cmd(&["ingest", "--batch-points", "100"])
        .write_stdin(text)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("ingested 250 points in 3 commits"), "{out}");
    assert_eq!(env.lines(&["query", "m"]).len(), 250);
}

#[test]
fn ingest_without_timestamp_uses_now() {
    let env = Env::new();
    env.ingest_text("up 1\n");
    let lines = env.lines(&["query", "up"]);
    assert_eq!(lines.len(), 1);
    let ts: u64 = lines[0].rsplit(' ').next().unwrap().parse().unwrap();
    assert!(ts > T0);
}

#[test]
fn ingest_overwrites_same_timestamp_and_accepts_out_of_order() {
    let env = Env::new();
    env.ingest_text("m 1 3000\nm 2 1000\nm 3 2000\n");
    env.ingest_text("m 9 2000\n");
    let lines = env.lines(&["query", "m"]);
    assert_eq!(lines, vec!["m 2 1000", "m 9 2000", "m 1 3000"]);
}

#[test]
fn query_filters_by_labels_and_time() {
    let env = Env::new();
    env.seed();
    let all = env.lines(&["query", "http_requests"]);
    assert_eq!(all.len(), 6 * 120);
    let a = env.lines(&["query", r#"http_requests{host="a"}"#]);
    assert_eq!(a.len(), 2 * 120);
    let ne = env.lines(&["query", r#"http_requests{host!="a",status="500"}"#]);
    assert_eq!(ne.len(), 2 * 120);
    let glob = env.lines(&["query", r#"http_requests{host=~"a*"}"#]);
    assert_eq!(glob.len(), 2 * 120);
    let glob = env.lines(&["query", r#"{host=~"*"}"#]);
    assert_eq!(glob.len(), 7 * 120);
    let from = (T0 + 10 * 60_000).to_string();
    let to = (T0 + 20 * 60_000).to_string();
    let window = env.lines(&["query", r#"http_requests{host="a",status="200"}"#, "--from", &from, "--to", &to]);
    assert_eq!(window.len(), 10);
    assert!(window[0].ends_with(&from));
    let limited = env.lines(&["query", "http_requests", "--limit", "3"]);
    assert_eq!(limited.len(), 3 * 6);
    assert!(env.lines(&["query", "nothing"]).is_empty());
}

#[test]
fn query_output_is_valid_line_protocol() {
    let env = Env::new();
    env.seed();
    let lines = env.ok(&["query", r#"http_requests{host="b",status="200"}"#]);
    let copy = Env::new();
    copy.ingest_text(&lines);
    assert_eq!(
        copy.ok(&["query", "http_requests"]),
        env.ok(&["query", r#"http_requests{host="b",status="200"}"#])
    );
}

#[test]
fn query_rejects_bad_selector() {
    let env = Env::new();
    env.seed();
    let err = env.fail(&["query", "m{a}"]);
    assert!(err.contains("expected"), "{err}");
}

#[test]
fn query_relative_times() {
    let env = Env::new();
    env.ingest_text("up 1\n");
    assert_eq!(env.lines(&["query", "up", "--from", "now-1h"]).len(), 1);
    assert_eq!(env.lines(&["query", "up", "--to", "1000"]).len(), 0);
}

fn rows(env: &Env, args: &[&str]) -> Vec<(String, f64)> {
    env.lines(args)
        .iter()
        .map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            (parts[parts.len() - 2].to_string(), parts[parts.len() - 1].parse().unwrap())
        })
        .collect()
}

#[test]
fn aggregate_functions_over_whole_range() {
    let env = Env::new();
    env.ingest_text("g{h=\"a\"} 1 1000\ng{h=\"a\"} 3 2000\ng{h=\"b\"} 10 1500\ng{h=\"b\"} 20 2500\n");
    let agg = |f: &str| rows(&env, &["aggregate", "g", "--fn", f]);
    assert_eq!(agg("sum")[0].1, 34.0);
    assert_eq!(agg("avg")[0].1, 8.5);
    assert_eq!(agg("min")[0].1, 1.0);
    assert_eq!(agg("max")[0].1, 20.0);
    assert_eq!(agg("count")[0].1, 4.0);
}

#[test]
fn aggregate_percentiles_use_nearest_rank() {
    let env = Env::new();
    let mut text = String::new();
    for i in 1..=100u64 {
        writeln!(text, "lat {i} {}", i * 1000).unwrap();
    }
    env.ingest_text(&text);
    let agg = |f: &str| rows(&env, &["aggregate", "lat", "--fn", f])[0].1;
    assert_eq!(agg("p50"), 50.0);
    assert_eq!(agg("p95"), 95.0);
    assert_eq!(agg("p99"), 99.0);
}

#[test]
fn aggregate_groups_by_label_and_steps() {
    let env = Env::new();
    env.seed();
    let step = (10 * 60_000).to_string();
    let by_host = env.lines(&[
        "aggregate",
        "http_requests",
        "--fn",
        "count",
        "--by",
        "host",
        "--step",
        &step,
    ]);
    assert_eq!(by_host.len(), 3 * 12);
    assert!(by_host.iter().all(|line| field(line, 2) == "20"), "{by_host:?}");
    assert!(by_host[0].starts_with("host=a\t"));
    let by_status = env.lines(&["aggregate", "http_requests", "--fn", "max", "--by", "status"]);
    assert_eq!(by_status.len(), 2);
    assert_eq!(field(&by_status[0], 2), "238");
    assert_eq!(field(&by_status[1], 2), "119");
}

#[test]
fn aggregate_rate_handles_counter_resets() {
    let env = Env::new();
    env.ingest_text(
        "c 0 0\nc 10 10000\nc 20 20000\nc 5 30000\nc 15 40000\n",
    );
    let rate = rows(&env, &["aggregate", "c", "--fn", "rate", "--from", "0", "--to", "50000"]);
    assert_eq!(rate.len(), 1);
    let expected = (10.0 + 10.0 + 5.0 + 10.0) / 40.0;
    assert!((rate[0].1 - expected).abs() < 1e-9, "{rate:?}");
}

#[test]
fn aggregate_rate_sums_across_series() {
    let env = Env::new();
    env.ingest_text("c{h=\"a\"} 0 0\nc{h=\"a\"} 10 10000\nc{h=\"b\"} 0 0\nc{h=\"b\"} 30 10000\n");
    let rate = rows(&env, &["aggregate", "c", "--fn", "rate"]);
    assert!((rate[0].1 - 4.0).abs() < 1e-9, "{rate:?}");
}

#[test]
fn aggregate_across_many_series_is_exact() {
    let env = Env::new();
    let mut text = String::new();
    for id in 0..300u64 {
        for i in 0..4u64 {
            writeln!(
                text,
                "many{{id=\"{id}\",grp=\"g{}\"}} {} {}",
                id % 3,
                id + i * 1000,
                T0 + i * 10_000
            )
            .unwrap();
        }
    }
    env.ingest_text(&text);
    let sum = |args: &[&str]| rows(&env, args);
    assert_eq!(sum(&["aggregate", "many", "--fn", "sum"])[0].1, (0..300u64).map(|id| 4 * id + 6000).sum::<u64>() as f64);
    assert_eq!(sum(&["aggregate", "many", "--fn", "count"])[0].1, 1200.0);
    assert_eq!(sum(&["aggregate", "many", "--fn", "max"])[0].1, 3299.0);
    assert_eq!(sum(&["aggregate", "many", "--fn", "min"])[0].1, 0.0);
    let p50 = sum(&["aggregate", "many", "--fn", "p50"])[0].1;
    assert!((1000.0..=3000.0).contains(&p50), "{p50}");
    let by_group = env.lines(&["aggregate", "many", "--fn", "count", "--by", "grp", "--step", "20s"]);
    assert_eq!(by_group.len(), 3 * 2, "{by_group:?}");
    assert!(by_group.iter().all(|l| field(l, 2) == "200"), "{by_group:?}");
    let rate = sum(&["aggregate", "many", "--fn", "rate", "--step", "1h"])[0].1;
    assert!((rate - 300.0 * 1000.0 / 10.0).abs() < 1e-6, "{rate}");
}

#[test]
fn aggregate_rejects_unknown_function() {
    let env = Env::new();
    env.seed();
    let err = env.fail(&["aggregate", "temp", "--fn", "median"]);
    assert!(err.contains("unknown function"), "{err}");
}

#[test]
fn aggregate_on_empty_selection_prints_nothing() {
    let env = Env::new();
    env.seed();
    assert!(env.lines(&["aggregate", "missing", "--fn", "sum"]).is_empty());
}

#[test]
fn series_lists_and_counts() {
    let env = Env::new();
    env.seed();
    assert_eq!(env.lines(&["series"]).len(), 7);
    assert_eq!(env.ok(&["series", "--count"]).trim(), "7");
    let a = env.lines(&["series", r#"{host="a"}"#]);
    assert_eq!(a.len(), 3);
    assert!(a.contains(&r#"http_requests{host="a",status="200"}"#.to_string()), "{a:?}");
    assert!(a.contains(&r#"temp{host="a"}"#.to_string()), "{a:?}");
    assert_eq!(env.lines(&["series", r#"http_requests{status="500",host=~"b*"}"#]).len(), 1);
    assert_eq!(env.lines(&["series", r#"http_requests{status!="500"}"#]).len(), 3);
}

#[test]
fn labels_summarises_names_and_values() {
    let env = Env::new();
    env.seed();
    let names = env.lines(&["labels"]);
    assert_eq!(names, vec!["__name__\t2", "host\t3", "status\t2"]);
    let hosts = env.lines(&["labels", "host"]);
    assert_eq!(hosts, vec!["a\t3", "b\t2", "c\t2"]);
    let metrics = env.lines(&["labels", "__name__"]);
    assert_eq!(metrics, vec!["http_requests\t6", "temp\t1"]);
}

#[test]
fn stats_reports_counts() {
    let env = Env::new();
    env.seed();
    let out = env.ok(&["stats"]);
    assert!(out.contains("series       7"), "{out}");
    assert!(out.contains("metrics      2"), "{out}");
    assert!(out.contains("points       840"), "{out}");
    assert!(out.contains("bytes/point"), "{out}");
}

#[test]
fn verify_passes_on_healthy_database() {
    let env = Env::new();
    env.seed();
    let out = env.ok(&["verify"]);
    assert!(out.contains("840 points"), "{out}");
    assert!(out.trim_end().ends_with("ok"), "{out}");
}

#[test]
fn verify_detects_corruption() {
    let env = Env::new();
    env.seed();
    let mut bytes = std::fs::read(&env.db).unwrap();
    let len = bytes.len();
    let mid = 8192 + (len - 8192) / 2;
    for byte in &mut bytes[mid..mid + 64] {
        *byte ^= 0xff;
    }
    std::fs::write(&env.db, bytes).unwrap();
    env.cmd(&["verify"]).assert().failure();
}

#[test]
fn downsample_builds_rollups_in_one_commit() {
    let env = Env::new();
    env.seed();
    let before = env.ok(&["stats"]);
    let out = env.ok(&["downsample", "--step", "10m"]);
    assert!(out.contains("into 84"), "{out}");
    let after = env.ok(&["stats"]);
    assert!(after.contains("rollups      600000ms: 84"), "{after}");
    assert!(!before.contains("rollups"), "{before}");
    let versions = |s: &str| {
        s.lines()
            .find(|l| l.starts_with("versions"))
            .unwrap()
            .to_string()
    };
    assert!(versions(&before).contains("(head v1)"), "{before}");
    assert!(versions(&after).contains("(head v2)"), "{after}");
}

#[test]
fn downsample_is_idempotent() {
    let env = Env::new();
    env.seed();
    env.ok(&["downsample", "--step", "10m"]);
    let first = env.ok(&["aggregate", "http_requests", "--rollup", "10m", "--fn", "sum", "--step", "1h"]);
    env.ok(&["downsample", "--step", "10m"]);
    let second = env.ok(&["aggregate", "http_requests", "--rollup", "10m", "--fn", "sum", "--step", "1h"]);
    assert_eq!(first, second);
}

#[test]
fn rollup_aggregates_match_raw_aggregates() {
    let env = Env::new();
    env.seed();
    env.ok(&["downsample", "--step", "10m"]);
    for func in ["sum", "avg", "min", "max", "count"] {
        let raw = env.ok(&[
            "aggregate",
            "http_requests",
            "--fn",
            func,
            "--by",
            "host",
            "--step",
            "1h",
        ]);
        let rolled = env.ok(&[
            "aggregate",
            "http_requests",
            "--fn",
            func,
            "--by",
            "host",
            "--step",
            "1h",
            "--rollup",
            "10m",
        ]);
        let parse = |text: &str| -> Vec<(String, f64)> {
            text.lines()
                .map(|l| (format!("{}{}", field(l, 0), field(l, 1)), field(l, 2).parse().unwrap()))
                .collect()
        };
        let (raw, rolled) = (parse(&raw), parse(&rolled));
        assert_eq!(raw.len(), rolled.len(), "{func}");
        for (a, b) in raw.iter().zip(rolled.iter()) {
            assert_eq!(a.0, b.0, "{func}");
            assert!((a.1 - b.1).abs() < 1e-6, "{func}: {a:?} {b:?}");
        }
    }
}

#[test]
fn rollup_folds_match_raw_aggregates() {
    let env = Env::new();
    env.seed();
    env.ok(&["downsample", "--step", "10m"]);
    let from = (T0 + 1_800_000).to_string();
    let to = (T0 + 6_000_000).to_string();
    let spans: [&[&str]; 6] = [
        &[],
        &["--step", "2h"],
        &["--step", "80m"],
        &["--from", &from, "--to", &to],
        &["--from", &from, "--to", &to, "--step", "2h"],
        &["--from", &from, "--to", &to, "--step", "80m"],
    ];
    let parse = |text: &str| -> Vec<(String, f64)> {
        text.lines()
            .map(|l| (format!("{}{}", field(l, 0), field(l, 1)), field(l, 2).parse().unwrap()))
            .collect()
    };
    for func in ["sum", "avg", "min", "max", "count"] {
        for span in spans {
            let mut args = vec!["aggregate", "http_requests", "--fn", func, "--by", "host"];
            args.extend_from_slice(span);
            let raw = parse(&env.ok(&args));
            args.extend_from_slice(&["--rollup", "10m"]);
            let rolled = parse(&env.ok(&args));
            assert!(!raw.is_empty(), "{func} {span:?}");
            assert_eq!(raw.len(), rolled.len(), "{func} {span:?}");
            for (a, b) in raw.iter().zip(rolled.iter()) {
                assert_eq!(a.0, b.0, "{func} {span:?}");
                assert!((a.1 - b.1).abs() < 1e-6, "{func} {span:?}: {a:?} {b:?}");
            }
        }
    }
}

#[test]
fn a_rollup_fold_matches_adding_the_points_in_order() {
    let points = [4.0, -3.0, 9.5, 0.25, -7.0, 2.0, 12.0, 1.0];
    for split in 1..points.len() {
        let mut whole = Rollup::of(points[0]);
        for point in &points[1..] {
            whole.add(*point);
        }
        let roll = |part: &[f64]| {
            part.iter()
                .map(|point| Rollup::of(*point))
                .reduce(|total, next| total.combine(&next))
                .unwrap()
        };
        let folded = roll(&points[..split]).combine(&roll(&points[split..]));
        assert_eq!(folded, whole, "split at {split}");
    }
}

#[test]
fn rollup_rejects_rate_and_bad_step() {
    let env = Env::new();
    env.seed();
    env.ok(&["downsample", "--step", "10m"]);
    let err = env.fail(&["aggregate", "temp", "--rollup", "10m", "--fn", "rate"]);
    assert!(err.contains("raw points"), "{err}");
    let err = env.fail(&["aggregate", "temp", "--rollup", "10m", "--fn", "sum", "--step", "15m"]);
    assert!(err.contains("multiple"), "{err}");
}

#[test]
fn downsample_with_selector_and_range() {
    let env = Env::new();
    env.seed();
    let to = (T0 + HOUR).to_string();
    let out = env.ok(&[
        "downsample",
        "--step",
        "30m",
        "--selector",
        "temp",
        "--to",
        &to,
    ]);
    assert!(out.contains("of 1 series"), "{out}");
}

fn old_and_new(env: &Env) {
    let mut text = String::new();
    for hour in 0..6u64 {
        for host in ["a", "b"] {
            for i in 0..6u64 {
                let ts = T0 + hour * HOUR + i * 600_000;
                writeln!(text, "m{{host=\"{host}\"}} {} {ts}", hour * 10 + i).unwrap();
            }
        }
    }
    env.ingest_text(&text);
}

#[test]
fn retain_drops_old_points_and_shrinks_file() {
    let env = Env::new();
    old_and_new(&env);
    for i in 0..30u64 {
        env.ingest_text(&format!("m{{host=\"a\"}} 1 {}\n", T0 + 7 * HOUR + i));
    }
    let size_before = std::fs::metadata(&env.db).unwrap().len();
    let now = (T0 + 6 * HOUR).to_string();
    let out = env.ok(&["retain", "--older-than", "3h", "--now", &now]);
    assert!(out.contains("dropped 36 points in 18 partitions"), "{out}");
    assert!(out.contains("compacted"), "{out}");
    let size_after = std::fs::metadata(&env.db).unwrap().len();
    assert!(size_after < size_before, "{size_after} < {size_before}");
    let lines = env.lines(&["query", r#"m{host="b"}"#]);
    assert_eq!(lines.len(), 18);
    assert!(lines[0].ends_with(&(T0 + 3 * HOUR).to_string()), "{lines:?}");
    let stats = env.ok(&["stats"]);
    assert!(stats.contains("versions     1 "), "{stats}");
    assert!(stats.contains(&format!("raw floor    {}", T0 + 3 * HOUR)), "{stats}");
    env.ok(&["verify"]);
}

#[test]
fn retain_trims_the_boundary_partition_exactly() {
    let env = Env::new();
    old_and_new(&env);
    let now = (T0 + 6 * HOUR).to_string();
    let hour0 = T0 / HOUR * HOUR;
    let cutoff_age = (T0 + 6 * HOUR - (hour0 + 3 * HOUR + 1_200_000)).to_string() + "ms";
    env.ok(&["retain", "--older-than", &cutoff_age, "--now", &now]);
    let lines = env.lines(&["query", r#"m{host="a"}"#, "--from", "0"]);
    let first: u64 = lines[0].rsplit(' ').next().unwrap().parse().unwrap();
    assert!(first >= hour0 + 3 * HOUR + 1_200_000, "{first}");
    env.ok(&["verify"]);
}

#[test]
fn retain_inside_a_partition_keeps_its_counter_right() {
    let env = Env::new();
    let start = T0 / 600_000 * 600_000;
    env.ingest_text(&format!(
        "m{{host=\"a\"}} 1 {}\nm{{host=\"a\"}} 2 {}\nm{{host=\"b\"}} 3 {}\n",
        start + 1_000,
        start + 300_000,
        start + 2_000,
    ));
    let now = (start + 200_000 + HOUR).to_string();
    let out = env.ok(&["retain", "--older-than", "1h", "--now", &now]);
    assert!(out.contains("dropped 2 points in 0 partitions"), "{out}");
    assert_eq!(env.lines(&["query", r#"m{host="a"}"#, "--from", "0"]).len(), 1);
    env.ok(&["verify"]);
    env.ingest_text(&format!("m{{host=\"a\"}} 4 {}\n", start + 400_000));
    env.ok(&["verify"]);
    let now = (start + 500_000 + HOUR).to_string();
    let out = env.ok(&["retain", "--older-than", "1h", "--now", &now]);
    assert!(out.contains("dropped 2 points in 0 partitions"), "{out}");
    env.ok(&["verify"]);
    let stats = env.ok(&["stats"]);
    assert!(stats.contains("partitions   0 "), "{stats}");
}

#[test]
fn retain_keep_versions_controls_history() {
    let env = Env::new();
    for i in 0..10u64 {
        env.ingest_text(&format!("m {i} {}\n", T0 + i * 1000));
    }
    let now = T0.to_string();
    env.ok(&["retain", "--older-than", "1h", "--now", &now, "--keep-versions", "4"]);
    assert!(env.ok(&["stats"]).contains("versions     4 "));
    env.ok(&["retain", "--older-than", "1h", "--now", &now, "--keep-since", "1h"]);
    assert!(env.ok(&["stats"]).contains("versions     4 "));
    env.ok(&["retain", "--older-than", "1h", "--now", &now]);
    assert!(env.ok(&["stats"]).contains("versions     1 "));
}

#[test]
fn retain_without_compact_leaves_file_size() {
    let env = Env::new();
    old_and_new(&env);
    let size_before = std::fs::metadata(&env.db).unwrap().len();
    let now = (T0 + 6 * HOUR).to_string();
    let out = env.ok(&["retain", "--older-than", "1h", "--now", &now, "--no-compact"]);
    assert!(!out.contains("compacted"), "{out}");
    assert!(std::fs::metadata(&env.db).unwrap().len() >= size_before);
    assert!(env.ok(&["stats"]).contains("versions     2 "));
}

#[test]
fn retain_keeps_rollups_after_dropping_raw() {
    let env = Env::new();
    old_and_new(&env);
    env.ok(&["downsample", "--step", "1h"]);
    let now = (T0 + 6 * HOUR).to_string();
    env.ok(&["retain", "--older-than", "3h", "--now", &now]);
    let rolled = env.lines(&["aggregate", "m", "--rollup", "1h", "--fn", "count", "--step", "1h"]);
    assert_eq!(rolled.len(), 6, "{rolled:?}");
    env.ok(&["downsample", "--step", "1h"]);
    let again = env.lines(&["aggregate", "m", "--rollup", "1h", "--fn", "count", "--step", "1h"]);
    assert_eq!(rolled, again);
}

#[test]
fn reads_work_while_file_is_missing_or_empty() {
    let env = Env::new();
    env.cmd(&["stats"]).assert().failure();
    env.ingest_text("");
    assert!(env.ok(&["stats"]).contains("series       0"));
    assert!(env.lines(&["query", "x"]).is_empty());
}

#[test]
fn load_reports_rate_and_latency() {
    let env = Env::new();
    let out = env.ok(&[
        "load",
        "20000",
        "2",
        "--series",
        "100",
        "--threads",
        "2",
        "--batch-ms",
        "50",
        "--no-sync",
    ]);
    assert!(out.contains("achieved"), "{out}");
    assert!(out.contains("commit latency p50"), "{out}");
    assert!(out.contains("p99"), "{out}");
    assert_eq!(env.ok(&["series", "--count"]).trim(), "100");
    env.ok(&["verify"]);
    let stats = env.ok(&["stats"]);
    let points: u64 = stats
        .lines()
        .find(|l| l.starts_with("points"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert!(points > 20_000, "{points}");
}

#[test]
fn load_reclaims_while_writers_run() {
    let env = Env::new();
    let out = env.ok(&[
        "load",
        "200000",
        "3",
        "--series",
        "100",
        "--threads",
        "2",
        "--batch-ms",
        "2",
        "--reclaim-mb",
        "1",
        "--no-sync",
    ]);
    assert!(out.contains("reclaim: "), "{out}");
    assert!(out.contains("longest writer pause"), "{out}");
    assert!(!out.contains("reclaim: 0 cycles"), "{out}");
    env.ok(&["verify"]);
}

#[test]
fn concurrent_reader_sees_committed_data_while_writer_ingests() {
    let env = Env::new();
    env.seed();
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin!("metrics"))
        .arg("--db")
        .arg(&env.db)
        .args(["ingest", "--batch-ms", "20"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "live 1 {}", T0).unwrap();
        stdin.flush().unwrap();
    }
    let mut seen = false;
    for _ in 0..100 {
        if env.lines(&["query", "live"]).len() == 1 {
            seen = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(child.stdin.take());
    child.wait().unwrap();
    assert!(seen, "reader never saw the streamed point");
    assert_eq!(env.lines(&["query", "http_requests"]).len(), 720);
}
