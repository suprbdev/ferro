use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ferro::Db;
use similar::{ChangeTag, TextDiff};

use crate::Res;
use crate::chunk;
use crate::cli::{Command, DraftCommand};
use crate::index::Tx;
use crate::merge;
use crate::model::{Page, Wiki, now_ms};
use crate::ops::{self, Edit, Outcome, clean_slug};
use crate::query;
use crate::render::{bytes, timestamp};
use crate::text::{query_terms, slugify};

fn find_page(w: &Wiki, slug: &str) -> Res<Arc<Page>> {
    query::find(w, &clean_slug(slug)?).ok_or_else(|| format!("no such page {slug}").into())
}

fn describe(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Created => "created",
        Outcome::Updated => "updated",
        Outcome::Unchanged => "no change",
    }
}

pub fn run(w: &mut dyn Write, db: &Db<Wiki>, command: Command) -> Res<()> {
    match command {
        Command::Put {
            slug,
            file,
            stdin,
            title,
            tags,
            author,
        } => {
            let text = match (file, stdin) {
                (Some(path), _) => Some(
                    fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?,
                ),
                (None, true) => {
                    let mut buf = String::new();
                    io::stdin().read_to_string(&mut buf)?;
                    Some(buf)
                }
                (None, false) => None,
            };
            let edit = Edit {
                slug,
                title,
                text,
                tags: (!tags.is_empty()).then_some(tags),
                author,
            };
            let done = db.try_commit_with::<_, _, Box<dyn std::error::Error>>(|wiki| {
                let slug = clean_slug(&edit.slug)?;
                let mut tx = Tx::new(wiki);
                let outcome = ops::put(&mut tx, edit, now_ms())?;
                tx.finish();
                Ok((format!("put {slug}"), (slug, outcome)))
            })?;
            let (slug, outcome) = &done.value;
            if *outcome == Outcome::Unchanged {
                writeln!(w, "no change: {slug}")?;
            } else {
                writeln!(w, "{} {slug} (v{})", describe(*outcome), done.version)?;
            }
            Ok(())
        }
        Command::Get { slug, at, meta } => {
            let snapshot = match at {
                Some(version) => db.at(version)?,
                None => db.read(),
            };
            let page = find_page(&snapshot, &slug)?;
            if meta {
                writeln!(w, "slug:    {}", page.slug)?;
                writeln!(w, "title:   {}", page.title)?;
                writeln!(w, "author:  {}", page.author)?;
                writeln!(w, "tags:    {}", page.tags.join(", "))?;
                writeln!(w, "updated: {}", timestamp(page.updated))?;
                writeln!(w, "version: v{}", snapshot.version())?;
                writeln!(
                    w,
                    "size:    {} bytes in {} chunks",
                    chunk::bytes(&page.body),
                    page.body.len()
                )?;
            } else {
                for piece in page.body.values() {
                    w.write_all(piece.as_bytes())?;
                }
            }
            Ok(())
        }
        Command::Delete { slug } => {
            let done = db.try_commit_with::<_, _, Box<dyn std::error::Error>>(|wiki| {
                let mut tx = Tx::new(wiki);
                let page = ops::delete(&mut tx, &slug)?;
                tx.finish();
                Ok((format!("delete {}", page.slug), page.slug.clone()))
            })?;
            writeln!(w, "deleted {} (v{})", done.value, done.version)?;
            Ok(())
        }
        Command::Rename { old, new } => {
            let done = db.try_commit_with::<_, _, Box<dyn std::error::Error>>(|wiki| {
                let mut tx = Tx::new(wiki);
                let rewritten = ops::rename(&mut tx, &old, &new, now_ms())?;
                tx.finish();
                Ok((format!("rename {old} to {new}"), rewritten))
            })?;
            writeln!(
                w,
                "renamed {old} to {new}, rewrote links in {} pages (v{})",
                done.value, done.version
            )?;
            Ok(())
        }
        Command::List { tag, prefix, limit } => {
            let snapshot = db.read();
            for page in query::list(&snapshot, tag.as_deref(), prefix.as_deref(), limit) {
                let tags = if page.tags.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", page.tags.join(","))
                };
                writeln!(w, "{:<40} {}{tags}", page.slug, page.title)?;
            }
            Ok(())
        }
        Command::Search { terms, limit } => {
            let snapshot = db.read();
            let terms = query_terms(&terms);
            if terms.is_empty() {
                return Err("no searchable terms".into());
            }
            let (total, hits) = query::search(&snapshot, &terms, limit);
            writeln!(w, "{total} matching pages")?;
            for hit in hits {
                writeln!(
                    w,
                    "{:>6}  {:<40} {}",
                    hit.score, hit.page.slug, hit.page.title
                )?;
                if let Some(line) = query::snippet(&hit.page, &terms) {
                    writeln!(w, "        {line}")?;
                }
            }
            Ok(())
        }
        Command::Backlinks { slug } => {
            let snapshot = db.read();
            for page in query::backlinks(&snapshot, &clean_slug(&slug)?) {
                writeln!(w, "{:<40} {}", page.slug, page.title)?;
            }
            Ok(())
        }
        Command::Orphans => {
            let snapshot = db.read();
            for slug in query::orphans(&snapshot) {
                writeln!(w, "{slug}")?;
            }
            Ok(())
        }
        Command::History { slug } => history(w, db, &slug),
        Command::Diff { slug, v1, v2 } => {
            let slug = clean_slug(&slug)?;
            let a = db.at(v1)?;
            let b = db.at(v2)?;
            let before = query::find(&a, &slug);
            let after = query::find(&b, &slug);
            if before.is_none() && after.is_none() {
                return Err(format!("{slug} exists in neither v{v1} nor v{v2}").into());
            }
            let body = |page: &Option<Arc<Page>>| {
                page.as_ref()
                    .map(|page| chunk::text(&page.body))
                    .unwrap_or_default()
            };
            if let (Some(x), Some(y)) = (&before, &after) {
                if x.title != y.title {
                    writeln!(w, "title: {} -> {}", x.title, y.title)?;
                }
                if x.tags != y.tags {
                    writeln!(w, "tags: [{}] -> [{}]", x.tags.join(","), y.tags.join(","))?;
                }
                if x.author != y.author {
                    writeln!(w, "author: {} -> {}", x.author, y.author)?;
                }
            }
            let old = body(&before);
            let new = body(&after);
            let diff = TextDiff::from_lines(&old, &new);
            let header_old = format!("{slug}@v{v1}");
            let header_new = format!("{slug}@v{v2}");
            write!(
                w,
                "{}",
                diff.unified_diff()
                    .context_radius(3)
                    .header(&header_old, &header_new)
            )?;
            Ok(())
        }
        Command::Revert { slug, version } => {
            let past = db.at(version)?;
            let done = db.try_commit_with::<_, _, Box<dyn std::error::Error>>(|wiki| {
                let mut tx = Tx::new(wiki);
                let outcome = ops::revert(&mut tx, &slug, &past, now_ms())?;
                tx.finish();
                Ok((format!("revert {slug} to v{version}"), outcome))
            })?;
            if done.value == Outcome::Unchanged {
                writeln!(w, "no change: {slug} already matches v{version}")?;
            } else {
                writeln!(w, "reverted {slug} to v{version} (v{})", done.version)?;
            }
            Ok(())
        }
        Command::Import {
            dir,
            author,
            tags,
            batch_pages,
            batch_mib,
        } => import(w, db, &dir, author, tags, batch_pages, batch_mib << 20),
        Command::Export { dir, at } => {
            let snapshot = match at {
                Some(version) => db.at(version)?,
                None => db.read(),
            };
            let mut count = 0usize;
            for (slug, id) in snapshot.slugs.iter() {
                let Some(page) = snapshot.pages.get(&id) else {
                    continue;
                };
                let path = dir.join(format!("{slug}.md"));
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut file = io::BufWriter::new(fs::File::create(&path)?);
                for piece in page.body.values() {
                    file.write_all(piece.as_bytes())?;
                }
                file.flush()?;
                count += 1;
            }
            writeln!(w, "exported {count} pages to {}", dir.display())?;
            Ok(())
        }
        Command::Draft(command) => draft(w, db, command),
        Command::Stats { deep } => {
            let snapshot = db.read();
            let stats = query::stats(&snapshot, deep);
            writeln!(w, "branch:    {}", db.branch_name())?;
            writeln!(w, "version:   v{}", snapshot.version())?;
            writeln!(w, "database:  {}", bytes(db.size_on_disk()))?;
            writeln!(w, "pages:     {}", stats.pages)?;
            writeln!(w, "terms:     {}", stats.terms)?;
            writeln!(w, "postings:  {}", stats.postings)?;
            writeln!(
                w,
                "links:     {} to {} targets",
                stats.links, stats.link_targets
            )?;
            if let Some((body, chunks)) = stats.body_bytes {
                writeln!(w, "bodies:    {} in {chunks} chunks", bytes(body))?;
            }
            writeln!(w, "by author:")?;
            for (author, count) in &stats.authors {
                writeln!(w, "  {author:<16} {count}")?;
            }
            writeln!(w, "by tag:")?;
            for (tag, count) in &stats.tags {
                writeln!(w, "  {tag:<16} {count}")?;
            }
            Ok(())
        }
        Command::Verify => {
            let report = db.verify()?;
            let snapshot = db.read();
            let problems = query::check_indexes(&snapshot);
            if !problems.is_empty() {
                for problem in &problems {
                    writeln!(w, "index mismatch: {problem}")?;
                }
                w.flush()?;
                return Err(format!("{} index problems", problems.len()).into());
            }
            writeln!(
                w,
                "ok: {} objects, {} logical bytes, {} file bytes, indexes consistent with {} pages",
                report.objects,
                report.bytes,
                report.file_bytes,
                snapshot.pages.len()
            )?;
            Ok(())
        }
    }
}

fn history(w: &mut dyn Write, db: &Db<Wiki>, slug: &str) -> Res<()> {
    let slug = clean_slug(slug)?;
    let head = db.read();
    let id = match query::find(&head, &slug) {
        Some(page) => page.id,
        None => {
            let mut found = None;
            for (version, _) in db.log().iter().rev() {
                let past = db.at(version)?;
                if let Some(page) = query::find(&past, &slug) {
                    found = Some(page.id);
                    break;
                }
            }
            found.ok_or_else(|| format!("no history for {slug}"))?
        }
    };
    drop(head);
    let mut previous: Option<Arc<Page>> = None;
    for (commit, page) in db.history_of(|wiki| wiki.pages.get(&id))? {
        let summary = match (&previous, &page) {
            (_, None) => "deleted".to_string(),
            (None, Some(new)) => format!("created, {} bytes", chunk::bytes(&new.body)),
            (Some(old), Some(new)) => {
                let mut parts = Vec::new();
                if old.slug != new.slug {
                    parts.push(format!("renamed from {}", old.slug));
                }
                if old.title != new.title {
                    parts.push("title".to_string());
                }
                if old.tags != new.tags {
                    parts.push("tags".to_string());
                }
                if old.body != new.body {
                    let a = chunk::text(&old.body);
                    let b = chunk::text(&new.body);
                    let (mut added, mut removed) = (0, 0);
                    for change in TextDiff::from_lines(&a, &b).iter_all_changes() {
                        match change.tag() {
                            ChangeTag::Insert => added += 1,
                            ChangeTag::Delete => removed += 1,
                            ChangeTag::Equal => {}
                        }
                    }
                    parts.push(format!("body +{added} -{removed} lines"));
                }
                if parts.is_empty() {
                    parts.push("touched".to_string());
                }
                parts.join(", ")
            }
        };
        let author = page.as_ref().map_or("-", |page| page.author.as_str());
        writeln!(
            w,
            "v{:<6} {}  {:<12} {}",
            commit.version,
            timestamp(commit.time_ms),
            author,
            summary
        )?;
        previous = page;
    }
    Ok(())
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Res<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?;
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
    Ok(())
}

fn import(
    w: &mut dyn Write,
    db: &Db<Wiki>,
    dir: &Path,
    author: Option<String>,
    tags: Vec<String>,
    batch_pages: usize,
    batch_bytes: usize,
) -> Res<()> {
    let started = Instant::now();
    let mut files = Vec::new();
    collect(dir, &mut files)?;
    let (mut created, mut updated, mut unchanged, mut skipped) = (0usize, 0usize, 0usize, 0usize);
    let mut commits = 0usize;
    let mut phases = (0f64, 0f64, 0f64);
    let mut cursor = 0;
    while cursor < files.len() {
        let mut batch: Vec<Edit> = Vec::new();
        let mut size = 0usize;
        while cursor < files.len() && batch.len() < batch_pages && size < batch_bytes {
            let path = &files[cursor];
            cursor += 1;
            let Ok(text) = fs::read_to_string(path) else {
                skipped += 1;
                continue;
            };
            let relative = path.strip_prefix(dir).unwrap_or(path).with_extension("");
            let relative = relative.to_string_lossy().replace('\\', "/");
            let mut page_tags = tags.clone();
            if let Some((category, _)) = relative.split_once('/') {
                page_tags.push(category.to_string());
            }
            size += text.len();
            batch.push(Edit {
                slug: slugify(&relative),
                title: None,
                text: Some(text),
                tags: (!page_tags.is_empty()).then_some(page_tags),
                author: author.clone(),
            });
        }
        if batch.is_empty() {
            continue;
        }
        let count = batch.len();
        let commit_started = Instant::now();
        let done = db.try_commit_with::<_, _, Box<dyn std::error::Error>>(|wiki| {
            let mut tx = Tx::new(wiki);
            let mut tally = (0usize, 0usize, 0usize);
            let put_started = Instant::now();
            for edit in batch {
                match ops::put(&mut tx, edit, now_ms())? {
                    Outcome::Created => tally.0 += 1,
                    Outcome::Updated => tally.1 += 1,
                    Outcome::Unchanged => tally.2 += 1,
                }
            }
            let put_time = put_started.elapsed();
            let finish_started = Instant::now();
            tx.finish();
            let finish_time = finish_started.elapsed();
            Ok((
                format!("import {count} pages from {}", dir.display()),
                (tally, put_time, finish_time),
            ))
        })?;
        let (tally, put_time, finish_time) = done.value;
        phases.0 += put_time.as_secs_f64();
        phases.1 += finish_time.as_secs_f64();
        phases.2 += commit_started.elapsed().as_secs_f64()
            - put_time.as_secs_f64()
            - finish_time.as_secs_f64();
        commits += 1;
        created += tally.0;
        updated += tally.1;
        unchanged += tally.2;
    }
    writeln!(
        w,
        "imported {created} new, {updated} updated, {unchanged} unchanged, {skipped} skipped in {commits} commits ({:.2}s: pages {:.2}s, postings {:.2}s, flush and sync {:.2}s)",
        started.elapsed().as_secs_f64(),
        phases.0,
        phases.1,
        phases.2
    )?;
    Ok(())
}

fn draft(w: &mut dyn Write, db: &Db<Wiki>, command: DraftCommand) -> Res<()> {
    match command {
        DraftCommand::Start { name } => {
            if name == "main" {
                return Err("main is not a draft".into());
            }
            let from = db.version();
            db.fork(&name)?;
            writeln!(
                w,
                "started draft {name} from {} at v{from}",
                db.branch_name()
            )?;
            Ok(())
        }
        DraftCommand::List => {
            for name in db.branches() {
                let marker = if name == db.branch_name() { "*" } else { " " };
                writeln!(w, "{marker} {name}")?;
            }
            Ok(())
        }
        DraftCommand::Publish { name } => {
            let draft = db.branch(&name)?;
            let ours = db.branch_name().to_string();
            let done = db.merge_with(&draft, |wiki, base, theirs| {
                let report = merge::merge(wiki, base, theirs, &ours, &name, now_ms());
                (
                    format!(
                        "publish {name}: {} applied, {} conflicts",
                        report.applied,
                        report.conflicts.len()
                    ),
                    report,
                )
            })?;
            writeln!(
                w,
                "published {name} into {ours}: {} pages applied, {} conflicts (v{})",
                done.value.applied,
                done.value.conflicts.len(),
                done.version
            )?;
            for conflict in &done.value.conflicts {
                writeln!(w, "conflict: {conflict}")?;
            }
            Ok(())
        }
    }
}
