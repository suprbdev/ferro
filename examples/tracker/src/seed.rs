use ferro::{Db, List};

use crate::Res;
use crate::model::{Comment, Issue, Priority, Status, Tracker, User, now_ms};

const USERS: [&str; 12] = [
    "alice", "bob", "carol", "dave", "erin", "frank", "grace", "heidi", "ivan", "judy", "mallory",
    "niaj",
];
const LABELS: [&str; 10] = [
    "bug",
    "feature",
    "docs",
    "ui",
    "backend",
    "perf",
    "security",
    "chore",
    "regression",
    "tests",
];
const VERBS: [&str; 10] = [
    "Fix",
    "Improve",
    "Investigate",
    "Refactor",
    "Document",
    "Add",
    "Remove",
    "Speed up",
    "Rework",
    "Handle",
];
const COMPONENTS: [&str; 14] = [
    "login form",
    "search index",
    "billing page",
    "export job",
    "notification service",
    "settings dialog",
    "upload pipeline",
    "rate limiter",
    "session cache",
    "dashboard charts",
    "email templates",
    "api pagination",
    "audit log",
    "mobile layout",
];
const PROBLEMS: [&str; 10] = [
    "crashing on empty input",
    "timing out under load",
    "showing stale data",
    "leaking memory",
    "ignoring user locale",
    "failing on large payloads",
    "returning duplicate rows",
    "missing validation",
    "slow on first load",
    "inconsistent after retry",
];
const SENTENCES: [&str; 8] = [
    "Reported by a customer in the last release.",
    "Steps to reproduce are attached to the ticket.",
    "Happens intermittently and only in production.",
    "We should add a regression test before closing this.",
    "Related to the work done in the previous sprint.",
    "Needs a decision from the product team first.",
    "The logs show a spike right before the failure.",
    "Workaround exists but it is awkward for users.",
];
const REPLIES: [&str; 6] = [
    "Looking into it now.",
    "Can reproduce on the latest build.",
    "Fix is up for review.",
    "Not able to reproduce, can you share more details?",
    "Deployed to staging, please verify.",
    "Closing after confirming in production.",
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
}

fn make_issue(id: u64, rng: &mut Rng, now: u64) -> Issue {
    let created = now - rng.next() % (365 * 86_400_000);
    let updated = created + rng.next() % (now - created + 1);
    let status = match rng.below(100) {
        0..40 => Status::Open,
        40..55 => Status::InProgress,
        _ => Status::Closed,
    };
    let priority = match rng.below(100) {
        0..25 => Priority::Low,
        25..70 => Priority::Medium,
        70..92 => Priority::High,
        _ => Priority::Critical,
    };
    let assignee = if rng.below(100) < 70 {
        Some(rng.pick(&USERS).to_string())
    } else {
        None
    };
    let mut labels: Vec<String> = Vec::new();
    for _ in 0..1 + rng.below(3) {
        let label = rng.pick(&LABELS).to_string();
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    let title = format!(
        "{} {} {}",
        rng.pick(&VERBS),
        rng.pick(&COMPONENTS),
        rng.pick(&PROBLEMS)
    );
    let body = format!("{} {}", rng.pick(&SENTENCES), rng.pick(&SENTENCES));
    let mut comments = List::new();
    if rng.below(100) < 20 {
        for _ in 0..1 + rng.below(3) {
            comments.push(Comment {
                author: rng.pick(&USERS).to_string(),
                text: rng.pick(&REPLIES).to_string(),
                at_ms: created + rng.next() % (updated - created + 1),
            });
        }
    }
    Issue {
        id,
        title,
        body,
        status,
        assignee,
        labels,
        priority,
        created_ms: created,
        updated_ms: updated,
        comments,
    }
}

pub fn run(db: &Db<Tracker>, n: u64, batch: u64) -> Res<()> {
    let now = now_ms();
    db.commit("seed users", |t| {
        for name in USERS {
            if !t.users.contains_key(name) {
                t.users.insert(
                    name.to_string(),
                    User {
                        name: name.to_string(),
                        created_ms: now,
                    },
                );
            }
        }
    })?;
    let mut done = 0;
    while done < n {
        let size = batch.min(n - done);
        let first = db.read().next_id + 1;
        let last = first + size - 1;
        db.commit(format!("seed #{first}..#{last}"), |t| {
            let mut rng = Rng(first.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            for _ in 0..size {
                t.next_id += 1;
                let issue = make_issue(t.next_id, &mut rng, now);
                t.issues.insert(issue);
            }
        })?;
        done += size;
    }
    Ok(())
}
