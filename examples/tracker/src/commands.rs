use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

use ferro::{Change, Commit, Db, Map};

use crate::Res;
use crate::cli::{Command, ListArgs, ReadCmd};
use crate::model::{Comment, Issue, Priority, Status, Tracker, User, now_ms};
use crate::render::{describe, issue_detail, issue_line, timestamp};
use crate::seed;

pub struct Meta {
    pub branch: String,
    pub version: u64,
    pub file_bytes: u64,
}

fn meta(db: &Db<Tracker>, version: u64) -> Meta {
    Meta {
        branch: db.branch_name().to_string(),
        version,
        file_bytes: db.size_on_disk(),
    }
}

fn mutate<R>(
    db: &Db<Tracker>,
    message: String,
    f: impl FnOnce(&mut Tracker) -> Res<R>,
) -> Res<(R, bool)> {
    let before = db.version();
    let out = db.try_commit(message, f)?;
    Ok((out, db.version() != before))
}

fn report(w: &mut dyn Write, db: &Db<Tracker>, changed: bool, what: &str) -> Res<()> {
    if changed {
        writeln!(w, "{what} (v{})", db.version())?;
    } else {
        writeln!(w, "no change: {what}")?;
    }
    Ok(())
}

fn find(t: &Tracker, id: u64) -> Res<Arc<Issue>> {
    t.issues
        .get(&id)
        .ok_or_else(|| format!("no such issue #{id}").into())
}

fn ensure_user(t: &mut Tracker, name: &str) {
    if !t.users.contains_key(name) {
        t.users.insert(
            name.to_string(),
            User {
                name: name.to_string(),
                created_ms: now_ms(),
            },
        );
    }
}

fn set_status(w: &mut dyn Write, db: &Db<Tracker>, id: u64, verb: &str, status: Status) -> Res<()> {
    let (_, changed) = mutate(db, format!("{verb} #{id}"), |t| {
        if find(t, id)?.status == status {
            return Ok(());
        }
        t.issues.update(&id, |issue| {
            issue.status = status;
            issue.updated_ms = now_ms();
        });
        Ok(())
    })?;
    report(w, db, changed, &format!("{verb} #{id}"))
}

pub fn run_write(w: &mut dyn Write, db: &Db<Tracker>, command: Command) -> Res<()> {
    match command {
        Command::Add {
            title,
            body,
            assignee,
            labels,
            priority,
        } => {
            let id = db.read().next_id + 1;
            let (_, changed) = mutate(db, format!("add #{id}: {title}"), |t| {
                t.next_id += 1;
                let now = now_ms();
                if let Some(name) = &assignee {
                    ensure_user(t, name);
                }
                let mut unique: Vec<String> = Vec::new();
                for label in labels {
                    if !unique.contains(&label) {
                        unique.push(label);
                    }
                }
                t.issues.insert(Issue {
                    id: t.next_id,
                    title,
                    body,
                    status: Status::Open,
                    assignee,
                    labels: unique,
                    priority,
                    created_ms: now,
                    updated_ms: now,
                    comments: Default::default(),
                });
                Ok(())
            })?;
            report(w, db, changed, &format!("added #{id}"))
        }
        Command::Edit {
            id,
            title,
            body,
            priority,
        } => {
            if title.is_none() && body.is_none() && priority.is_none() {
                return Err("nothing to edit: pass --title, --body or --priority".into());
            }
            let (_, changed) = mutate(db, format!("edit #{id}"), |t| {
                find(t, id)?;
                t.issues.update(&id, |issue| {
                    if let Some(title) = title {
                        issue.title = title;
                    }
                    if let Some(body) = body {
                        issue.body = body;
                    }
                    if let Some(priority) = priority {
                        issue.priority = priority;
                    }
                    issue.updated_ms = now_ms();
                });
                Ok(())
            })?;
            report(w, db, changed, &format!("edited #{id}"))
        }
        Command::Assign { id, user } => {
            let (_, changed) = mutate(db, format!("assign #{id} to {user}"), |t| {
                if find(t, id)?.assignee.as_deref() == Some(user.as_str()) {
                    return Ok(());
                }
                ensure_user(t, &user);
                t.issues.update(&id, |issue| {
                    issue.assignee = Some(user.clone());
                    issue.updated_ms = now_ms();
                });
                Ok(())
            })?;
            report(w, db, changed, &format!("assigned #{id}"))
        }
        Command::Unassign { id } => {
            let (_, changed) = mutate(db, format!("unassign #{id}"), |t| {
                if find(t, id)?.assignee.is_none() {
                    return Ok(());
                }
                t.issues.update(&id, |issue| {
                    issue.assignee = None;
                    issue.updated_ms = now_ms();
                });
                Ok(())
            })?;
            report(w, db, changed, &format!("unassigned #{id}"))
        }
        Command::Label { id, label } => {
            let (_, changed) = mutate(db, format!("label #{id} {label}"), |t| {
                if find(t, id)?.labels.contains(&label) {
                    return Ok(());
                }
                t.issues.update(&id, |issue| {
                    issue.labels.push(label.clone());
                    issue.updated_ms = now_ms();
                });
                Ok(())
            })?;
            report(w, db, changed, &format!("labelled #{id}"))
        }
        Command::Unlabel { id, label } => {
            let (_, changed) = mutate(db, format!("unlabel #{id} {label}"), |t| {
                if !find(t, id)?.labels.contains(&label) {
                    return Ok(());
                }
                t.issues.update(&id, |issue| {
                    issue.labels.retain(|existing| existing != &label);
                    issue.updated_ms = now_ms();
                });
                Ok(())
            })?;
            report(w, db, changed, &format!("unlabelled #{id}"))
        }
        Command::Comment { id, text, author } => {
            let (_, changed) = mutate(db, format!("comment on #{id}"), |t| {
                find(t, id)?;
                t.issues.update(&id, |issue| {
                    let now = now_ms();
                    issue.comments.push(Comment {
                        author,
                        text,
                        at_ms: now,
                    });
                    issue.updated_ms = now;
                });
                Ok(())
            })?;
            report(w, db, changed, &format!("commented on #{id}"))
        }
        Command::Start { id } => set_status(w, db, id, "start", Status::InProgress),
        Command::Close { id } => set_status(w, db, id, "close", Status::Closed),
        Command::Reopen { id } => set_status(w, db, id, "reopen", Status::Open),
        Command::Undo => undo(w, db),
        Command::Restore { version } => {
            let snapshot = db.at(version)?;
            let value = (*snapshot).clone();
            let (_, changed) = mutate(db, format!("restore v{version}"), |t| {
                *t = value;
                Ok(())
            })?;
            report(w, db, changed, &format!("restored v{version}"))
        }
        Command::Seed { n, batch } => {
            let before = db.version();
            let start = std::time::Instant::now();
            seed::run(db, n, batch)?;
            writeln!(
                w,
                "seeded {n} issues in {} commits ({:.2}s), now v{}",
                db.version() - before,
                start.elapsed().as_secs_f64(),
                db.version()
            )?;
            Ok(())
        }
        Command::Branch { name } => {
            let from = db.version();
            db.fork(&name)?;
            writeln!(
                w,
                "created branch {name} from {} at v{from}",
                db.branch_name()
            )?;
            Ok(())
        }
        _ => Err("not a write command".into()),
    }
}

fn undo_target(log: &Map<u64, Commit>, mut version: u64) -> u64 {
    loop {
        let Some(commit) = log.get(&version) else {
            return version;
        };
        let restored = commit
            .message
            .strip_prefix("undo v")
            .and_then(|rest| rest.split_once(": restore v"))
            .and_then(|(_, target)| target.parse::<u64>().ok());
        match restored {
            Some(target) => version = target,
            None => return version,
        }
    }
}

fn undo(w: &mut dyn Write, db: &Db<Tracker>) -> Res<()> {
    let head = db.version();
    let current = undo_target(&db.log(), head);
    if current == 0 {
        writeln!(w, "nothing to undo")?;
        return Ok(());
    }
    let target = current - 1;
    let snapshot = db.at(target)?;
    let value = (*snapshot).clone();
    let (_, changed) = mutate(db, format!("undo v{head}: restore v{target}"), |t| {
        *t = value;
        Ok(())
    })?;
    if changed {
        writeln!(
            w,
            "undid v{current}, restored state of v{target} as v{}",
            db.version()
        )?;
    } else {
        writeln!(w, "nothing to undo")?;
    }
    Ok(())
}

pub fn run_read(w: &mut dyn Write, db: &Db<Tracker>, command: Command) -> Res<()> {
    match command {
        Command::Read(cmd) => {
            let snapshot = db.read();
            read_cmd(w, &cmd, &snapshot, &meta(db, snapshot.version()))
        }
        Command::At { version, read } => {
            let snapshot = db.at(version)?;
            read_cmd(w, &read, &snapshot, &meta(db, snapshot.version()))
        }
        Command::AsOf { unix_ms, read } => {
            let snapshot = db.as_of(unix_ms)?;
            read_cmd(w, &read, &snapshot, &meta(db, snapshot.version()))
        }
        Command::History { id } => history(w, db, id),
        Command::Log { limit } => {
            for (version, commit) in db.log().iter().rev().take(limit) {
                writeln!(
                    w,
                    "v{version:<7} {}  {}",
                    timestamp(commit.time_ms),
                    commit.message
                )?;
            }
            Ok(())
        }
        Command::Diff { from, to, limit } => {
            let a = db.at(from)?;
            let b = db.at(to)?;
            diff_trackers(w, &a, &b, limit)
        }
        Command::Compare { other, limit } => {
            let current = db.read();
            let target = db.branch(&other)?.read();
            writeln!(
                w,
                "{} v{} -> {} v{}",
                db.branch_name(),
                current.version(),
                other,
                target.version()
            )?;
            diff_trackers(w, &current, &target, limit)
        }
        Command::Verify => {
            let report = db.verify()?;
            writeln!(
                w,
                "ok: {} objects, {} logical bytes, {} file bytes",
                report.objects, report.bytes, report.file_bytes
            )?;
            Ok(())
        }
        Command::Branches => {
            for name in db.branches() {
                let marker = if name == db.branch_name() { "*" } else { " " };
                writeln!(w, "{marker} {name}")?;
            }
            Ok(())
        }
        _ => Err("not a read command".into()),
    }
}

fn read_cmd(w: &mut dyn Write, cmd: &ReadCmd, t: &Tracker, meta: &Meta) -> Res<()> {
    match cmd {
        ReadCmd::List(args) => list(w, t, args),
        ReadCmd::Show { id } => issue_detail(w, &*find(t, *id)?),
        ReadCmd::Stats => stats(w, t, meta),
        ReadCmd::Users => {
            for (name, _) in t.users.iter() {
                let open = t.issues.by_assignee(&Some(name.clone())).len();
                writeln!(w, "{name:<12} {open} assigned")?;
            }
            Ok(())
        }
    }
}

type Source = Box<dyn DoubleEndedIterator<Item = Arc<Issue>>>;

fn contains_ci(haystack: &str, needle_lower: &str) -> bool {
    haystack.to_lowercase().contains(needle_lower)
}

fn list(w: &mut dyn Write, t: &Tracker, args: &ListArgs) -> Res<()> {
    let mut candidates: Vec<(&str, usize, Source)> = Vec::new();
    if let Some(status) = args.status {
        let rows = t.issues.by_status(&status);
        candidates.push(("status", rows.len(), Box::new(rows)));
    }
    if let Some(name) = &args.assignee {
        let rows = t.issues.by_assignee(&Some(name.clone()));
        candidates.push(("assignee", rows.len(), Box::new(rows)));
    }
    if args.unassigned {
        let rows = t.issues.by_assignee(&None);
        candidates.push(("assignee", rows.len(), Box::new(rows)));
    }
    if let Some(label) = &args.label {
        let rows = t.issues.by_labels(label.as_str());
        candidates.push(("labels", rows.len(), Box::new(rows)));
    }
    if let Some(priority) = args.priority {
        let rows = t.issues.by_priority(&priority);
        candidates.push(("priority", rows.len(), Box::new(rows)));
    }
    let (plan, source): (String, Source) =
        match candidates.into_iter().min_by_key(|(_, len, _)| *len) {
            Some((name, len, rows)) => (format!("index {name} ({len} candidates)"), rows),
            None => (
                format!("full scan ({} rows)", t.issues.len()),
                Box::new(t.issues.iter()),
            ),
        };
    if args.explain {
        eprintln!("plan: {plan}");
    }
    let needle = args.text.as_ref().map(|text| text.to_lowercase());
    let matching = source.rev().filter(|issue| {
        args.status.is_none_or(|status| issue.status == status)
            && args
                .assignee
                .as_ref()
                .is_none_or(|name| issue.assignee.as_ref() == Some(name))
            && (!args.unassigned || issue.assignee.is_none())
            && args
                .label
                .as_ref()
                .is_none_or(|label| issue.labels.contains(label))
            && args
                .priority
                .is_none_or(|priority| issue.priority == priority)
            && needle.as_ref().is_none_or(|needle| {
                contains_ci(&issue.title, needle) || contains_ci(&issue.body, needle)
            })
    });
    for issue in matching.take(args.limit.unwrap_or(usize::MAX)) {
        issue_line(w, &issue)?;
    }
    Ok(())
}

fn stats(w: &mut dyn Write, t: &Tracker, meta: &Meta) -> Res<()> {
    writeln!(w, "branch:   {}", meta.branch)?;
    writeln!(w, "version:  v{}", meta.version)?;
    writeln!(w, "database: {} bytes", meta.file_bytes)?;
    writeln!(w, "issues:   {}", t.issues.len())?;
    writeln!(w, "users:    {}", t.users.len())?;
    writeln!(w, "by status:")?;
    for status in Status::ALL {
        writeln!(w, "  {status:<12} {}", t.issues.by_status(&status).len())?;
    }
    writeln!(w, "by priority:")?;
    for priority in Priority::ALL {
        writeln!(
            w,
            "  {priority:<12} {}",
            t.issues.by_priority(&priority).len()
        )?;
    }
    writeln!(w, "by assignee:")?;
    for (name, _) in t.users.iter() {
        let count = t.issues.by_assignee(&Some(name.clone())).len();
        if count > 0 {
            writeln!(w, "  {name:<12} {count}")?;
        }
    }
    writeln!(
        w,
        "  {:<12} {}",
        "unassigned",
        t.issues.by_assignee(&None).len()
    )?;
    let mut labels: BTreeMap<String, usize> = BTreeMap::new();
    for issue in t.issues.iter() {
        for label in &issue.labels {
            *labels.entry(label.clone()).or_default() += 1;
        }
    }
    writeln!(w, "by label:")?;
    for (label, count) in labels {
        writeln!(w, "  {label:<12} {count}")?;
    }
    Ok(())
}

fn history(w: &mut dyn Write, db: &Db<Tracker>, id: u64) -> Res<()> {
    let mut previous: Option<Arc<Issue>> = None;
    let mut seen = false;
    for (version, commit) in db.log().iter() {
        let current = db.at(version)?.issues.get(&id);
        let lines = match (&previous, &current) {
            (None, Some(new)) => vec![format!("created: {}", new.title)],
            (Some(_), None) => vec!["removed".to_string()],
            (Some(old), Some(new)) if old != new => {
                let mut lines = describe(old, new);
                if lines.is_empty() {
                    lines.push("changed".to_string());
                }
                lines
            }
            _ => Vec::new(),
        };
        if !lines.is_empty() {
            seen = true;
            writeln!(
                w,
                "v{version} {}  {}",
                timestamp(commit.time_ms),
                commit.message
            )?;
            for line in lines {
                writeln!(w, "    {line}")?;
            }
        }
        previous = current;
    }
    if !seen {
        return Err(format!("no history for #{id}").into());
    }
    Ok(())
}

fn diff_trackers(w: &mut dyn Write, a: &Tracker, b: &Tracker, limit: usize) -> Res<()> {
    let (mut added, mut removed, mut changed) = (0usize, 0usize, 0usize);
    let mut lines: Vec<String> = Vec::new();
    for change in a.issues.diff(&b.issues) {
        let mut entry = Vec::new();
        match &change {
            Change::Added(id, issue) => {
                added += 1;
                entry.push(format!("+ #{id} {}", issue.title));
            }
            Change::Removed(id, issue) => {
                removed += 1;
                entry.push(format!("- #{id} {}", issue.title));
            }
            Change::Changed(id, old, new) => {
                changed += 1;
                entry.push(format!("~ #{id} {}", new.title));
                for line in describe(old, new) {
                    entry.push(format!("    {line}"));
                }
            }
        }
        if added + removed + changed <= limit {
            lines.extend(entry);
        }
    }
    writeln!(w, "{added} added, {removed} removed, {changed} changed")?;
    for line in &lines {
        writeln!(w, "{line}")?;
    }
    let total = added + removed + changed;
    if total > limit {
        writeln!(w, "... {} more not shown", total - limit)?;
    }
    Ok(())
}
