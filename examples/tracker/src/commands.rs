use std::io::Write;

use ferro::table::IndexIter;
use ferro::{Change, Committed, Db, Val};

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
) -> Res<Committed<R>> {
    db.try_commit_with::<_, _, Box<dyn std::error::Error>>(|t| f(t).map(|value| (message, value)))
}

fn report<R>(w: &mut dyn Write, done: &Committed<R>, what: &str) -> Res<()> {
    if done.changed {
        writeln!(w, "{what} (v{})", done.version)?;
    } else {
        writeln!(w, "no change: {what}")?;
    }
    Ok(())
}

fn find(t: &Tracker, id: u64) -> Res<Val<Issue>> {
    t.issues
        .get(&id)
        .ok_or_else(|| format!("no such issue #{id}").into())
}

fn change<R>(t: &mut Tracker, id: u64, f: impl FnOnce(&mut Issue) -> R) -> Res<R> {
    t.issues
        .update(&id, f)
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
    let done = mutate(db, format!("{verb} #{id}"), |t| {
        change(t, id, |issue| {
            if issue.status != status {
                issue.status = status;
                issue.updated_ms = now_ms();
            }
        })
    })?;
    report(w, &done, &format!("{verb} #{id}"))
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
            let done = db.try_commit_with::<_, _, Box<dyn std::error::Error>>(|t| {
                t.next_id += 1;
                let id = t.next_id;
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
                    id,
                    title: title.clone(),
                    body,
                    status: Status::Open,
                    assignee,
                    labels: unique,
                    priority,
                    created_ms: now,
                    updated_ms: now,
                    comments: Default::default(),
                });
                Ok((format!("add #{id}: {title}"), id))
            })?;
            report(w, &done, &format!("added #{}", done.value))
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
            let done = mutate(db, format!("edit #{id}"), |t| {
                change(t, id, |issue| {
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
                })
            })?;
            report(w, &done, &format!("edited #{id}"))
        }
        Command::Assign { id, user } => {
            let done = mutate(db, format!("assign #{id} to {user}"), |t| {
                let moved = change(t, id, |issue| {
                    let moved = issue.assignee.as_deref() != Some(user.as_str());
                    if moved {
                        issue.assignee = Some(user.clone());
                        issue.updated_ms = now_ms();
                    }
                    moved
                })?;
                if moved {
                    ensure_user(t, &user);
                }
                Ok(())
            })?;
            report(w, &done, &format!("assigned #{id}"))
        }
        Command::Unassign { id } => {
            let done = mutate(db, format!("unassign #{id}"), |t| {
                change(t, id, |issue| {
                    if issue.assignee.take().is_some() {
                        issue.updated_ms = now_ms();
                    }
                })
            })?;
            report(w, &done, &format!("unassigned #{id}"))
        }
        Command::Label { id, label } => {
            let done = mutate(db, format!("label #{id} {label}"), |t| {
                change(t, id, |issue| {
                    if !issue.labels.contains(&label) {
                        issue.labels.push(label.clone());
                        issue.updated_ms = now_ms();
                    }
                })
            })?;
            report(w, &done, &format!("labelled #{id}"))
        }
        Command::Unlabel { id, label } => {
            let done = mutate(db, format!("unlabel #{id} {label}"), |t| {
                change(t, id, |issue| {
                    if issue.labels.contains(&label) {
                        issue.labels.retain(|existing| existing != &label);
                        issue.updated_ms = now_ms();
                    }
                })
            })?;
            report(w, &done, &format!("unlabelled #{id}"))
        }
        Command::Comment { id, text, author } => {
            let done = mutate(db, format!("comment on #{id}"), |t| {
                change(t, id, |issue| {
                    let now = now_ms();
                    issue.comments.push(Comment {
                        author,
                        text,
                        at_ms: now,
                    });
                    issue.updated_ms = now;
                })
            })?;
            report(w, &done, &format!("commented on #{id}"))
        }
        Command::Start { id } => set_status(w, db, id, "start", Status::InProgress),
        Command::Close { id } => set_status(w, db, id, "close", Status::Closed),
        Command::Reopen { id } => set_status(w, db, id, "reopen", Status::Open),
        Command::Undo => undo(w, db),
        Command::Restore { version } => {
            let snapshot = db.at(version)?;
            let done = db.restore(&snapshot)?;
            report(w, &done, &format!("restored v{version}"))
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
        Command::Merge { from, theirs } => merge(w, db, &from, theirs),
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

fn merge(w: &mut dyn Write, db: &Db<Tracker>, branch: &str, take_theirs: bool) -> Res<()> {
    let other = db.branch(branch)?;
    let done = db.merge_with(&other, |t, base, theirs| {
        t.next_id = t.next_id.max(theirs.next_id);
        let mut conflicts: Vec<u64> = Vec::new();
        let mut collisions: Vec<Issue> = Vec::new();
        let mut applied = t.issues.merge(&base.issues, &theirs.issues, |conflict| {
            if conflict.base.is_none() {
                if let Some(row) = conflict.theirs.as_deref() {
                    collisions.push(row.clone());
                }
                return conflict.keep_ours();
            }
            conflicts.push(conflict.key);
            if take_theirs {
                conflict.keep_theirs()
            } else {
                conflict.keep_ours()
            }
        });
        let mut renumbered = Vec::new();
        for mut issue in collisions {
            let old = issue.id;
            t.next_id += 1;
            issue.id = t.next_id;
            renumbered.push((old, issue.id));
            t.issues.insert(issue);
        }
        applied += t.users.merge(&base.users, &theirs.users, |conflict| {
            match (conflict.ours.as_deref(), conflict.theirs.as_deref()) {
                (Some(ours), Some(theirs)) if theirs.created_ms < ours.created_ms => {
                    Some(theirs.clone())
                }
                (ours, theirs) => ours.or(theirs).cloned(),
            }
        });
        applied -= conflicts.len();
        conflicts.sort_unstable();
        let message = format!(
            "merge {branch}: {applied} applied, {} conflicts",
            conflicts.len()
        );
        (message.clone(), (message, conflicts, renumbered))
    })?;
    let (message, conflicts, renumbered) = &done.value;
    report(w, &done, message)?;
    for (old, new) in renumbered {
        writeln!(w, "renumbered #{old} from {branch} as #{new}")?;
    }
    for id in conflicts {
        let kept = if take_theirs {
            branch
        } else {
            db.branch_name()
        };
        writeln!(w, "conflict #{id}: kept {kept}")?;
    }
    Ok(())
}

fn undo_target(db: &Db<Tracker>) -> u64 {
    let log = db.log();
    let mut version = db.version();
    while let Some(restored) = log.get(&version).and_then(|commit| commit.restored) {
        version = restored;
    }
    version
}

fn undo(w: &mut dyn Write, db: &Db<Tracker>) -> Res<()> {
    let head = db.version();
    let current = undo_target(db);
    if current == 0 {
        writeln!(w, "nothing to undo")?;
        return Ok(());
    }
    let target = current - 1;
    let done = db.restore_with(&db.at(target)?, format!("undo v{head}: restore v{target}"))?;
    if done.changed {
        writeln!(
            w,
            "undid v{current}, restored state of v{target} as v{}",
            done.version
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
                let open = t.issues.by_assignee_some(&name).len();
                writeln!(w, "{name:<12} {open} assigned")?;
            }
            Ok(())
        }
    }
}

fn contains_ci(haystack: &str, needle_lower: &str) -> bool {
    haystack.to_lowercase().contains(needle_lower)
}

fn list(w: &mut dyn Write, t: &Tracker, args: &ListArgs) -> Res<()> {
    let mut candidates: Vec<(&str, IndexIter<Issue>)> = Vec::new();
    if let Some(status) = args.status {
        candidates.push(("status", t.issues.by_status(&status)));
    }
    if let Some(name) = &args.assignee {
        candidates.push(("assignee", t.issues.by_assignee_some(name)));
    }
    if args.unassigned {
        candidates.push(("assignee", t.issues.by_assignee(&None)));
    }
    if let Some(label) = &args.label {
        candidates.push(("labels", t.issues.by_labels(label.as_str())));
    }
    if let Some(priority) = args.priority {
        candidates.push(("priority", t.issues.by_priority(&priority)));
    }
    match candidates.into_iter().min_by_key(|(_, rows)| rows.len()) {
        Some((name, rows)) => {
            if args.explain {
                eprintln!("plan: index {name} ({} candidates)", rows.len());
            }
            print_matching(w, rows.rev(), args)
        }
        None => {
            if args.explain {
                eprintln!("plan: full scan ({} rows)", t.issues.len());
            }
            print_matching(w, t.issues.iter().rev(), args)
        }
    }
}

fn print_matching(
    w: &mut dyn Write,
    rows: impl Iterator<Item = Val<Issue>>,
    args: &ListArgs,
) -> Res<()> {
    let needle = args.text.as_ref().map(|text| text.to_lowercase());
    let matching = rows.filter(|issue| {
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

fn count_of<K: PartialEq>(counts: &[(K, usize)], key: &K) -> usize {
    counts
        .iter()
        .find(|(k, _)| k == key)
        .map_or(0, |(_, count)| *count)
}

fn stats(w: &mut dyn Write, t: &Tracker, meta: &Meta) -> Res<()> {
    writeln!(w, "branch:   {}", meta.branch)?;
    writeln!(w, "version:  v{}", meta.version)?;
    writeln!(w, "database: {} bytes", meta.file_bytes)?;
    writeln!(w, "issues:   {}", t.issues.len())?;
    writeln!(w, "users:    {}", t.users.len())?;
    let statuses = t.issues.by_status_counts();
    writeln!(w, "by status:")?;
    for status in Status::ALL {
        writeln!(w, "  {status:<12} {}", count_of(&statuses, &status))?;
    }
    let priorities = t.issues.by_priority_counts();
    writeln!(w, "by priority:")?;
    for priority in Priority::ALL {
        writeln!(w, "  {priority:<12} {}", count_of(&priorities, &priority))?;
    }
    let assignees = t.issues.by_assignee_counts();
    writeln!(w, "by assignee:")?;
    for (name, count) in &assignees {
        if let Some(name) = name {
            writeln!(w, "  {name:<12} {count}")?;
        }
    }
    writeln!(w, "  {:<12} {}", "unassigned", count_of(&assignees, &None))?;
    writeln!(w, "by label:")?;
    for (label, count) in t.issues.by_labels_counts() {
        writeln!(w, "  {label:<12} {count}")?;
    }
    Ok(())
}

fn history(w: &mut dyn Write, db: &Db<Tracker>, id: u64) -> Res<()> {
    let mut previous: Option<Val<Issue>> = None;
    let mut seen = false;
    let mut changes: Vec<_> = db.history_of(|t| t.issues.get(&id)).collect();
    changes.reverse();
    for (commit, current) in changes {
        let lines = match (&previous, &current) {
            (None, Some(new)) => vec![format!("created: {}", new.title)],
            (Some(_), None) => vec!["removed".to_string()],
            (Some(old), Some(new)) => {
                let mut lines = describe(old, new);
                if lines.is_empty() {
                    lines.push("changed".to_string());
                }
                lines
            }
            (None, None) => Vec::new(),
        };
        if !lines.is_empty() {
            seen = true;
            writeln!(
                w,
                "v{} {}  {}",
                commit.version,
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
