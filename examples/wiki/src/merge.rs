use std::collections::BTreeSet;

use ferro::Change;

use crate::chunk::{self, DEFAULT};
use crate::index::Tx;
use crate::merge3::merge_lines;
use crate::model::{Page, Wiki};

pub struct Report {
    pub applied: usize,
    pub conflicts: Vec<String>,
}

fn same(a: &Page, b: &Page) -> bool {
    a.slug == b.slug
        && a.title == b.title
        && a.author == b.author
        && a.tags == b.tags
        && a.body == b.body
}

fn scalar<T: PartialEq + Clone>(base: &T, ours: &T, theirs: &T) -> (T, bool) {
    if ours == theirs || theirs == base {
        (ours.clone(), false)
    } else if ours == base {
        (theirs.clone(), false)
    } else {
        (ours.clone(), true)
    }
}

fn tags3(base: &[String], ours: &[String], theirs: &[String]) -> Vec<String> {
    let base: BTreeSet<&String> = base.iter().collect();
    let ours: BTreeSet<&String> = ours.iter().collect();
    let theirs: BTreeSet<&String> = theirs.iter().collect();
    let mut merged: BTreeSet<&String> = ours.clone();
    for tag in theirs.difference(&base) {
        merged.insert(tag);
    }
    for tag in base.difference(&theirs) {
        merged.remove(tag);
    }
    merged.into_iter().cloned().collect()
}

pub fn merge(
    w: &mut Wiki,
    base: &Wiki,
    theirs: &Wiki,
    ours_name: &str,
    theirs_name: &str,
    now: u64,
) -> Report {
    let mut tx = Tx::new(w);
    let mut applied = 0;
    let mut conflicts = Vec::new();
    for change in base.pages.diff(&theirs.pages) {
        match change {
            Change::Added(_, added) => match tx.by_slug(&added.slug) {
                None => {
                    let id = tx.allocate();
                    tx.put(Page {
                        id,
                        ..(*added).clone()
                    });
                    applied += 1;
                }
                Some(ours) if same(&ours, &added) => {}
                Some(ours) => {
                    let merged = merge_lines(
                        "",
                        &chunk::text(&ours.body),
                        &chunk::text(&added.body),
                        ours_name,
                        theirs_name,
                    );
                    let mut page = (*ours).clone();
                    chunk::update(&mut page.body, &merged.text, &DEFAULT);
                    page.updated = now;
                    tx.put(page);
                    applied += 1;
                    conflicts.push(format!(
                        "{}: created on both sides with different content",
                        added.slug
                    ));
                }
            },
            Change::Removed(id, before) => {
                let Some(ours) = tx.page(id) else { continue };
                if same(&ours, &before) {
                    tx.remove(id);
                    applied += 1;
                } else {
                    conflicts.push(format!(
                        "{}: deleted in {theirs_name} but edited in {ours_name}, kept",
                        ours.slug
                    ));
                }
            }
            Change::Changed(id, before, after) => {
                let Some(ours) = tx.page(id) else {
                    conflicts.push(format!(
                        "{}: edited in {theirs_name} but deleted in {ours_name}, kept deleted",
                        after.slug
                    ));
                    continue;
                };
                if same(&ours, &after) {
                    continue;
                }
                let mut merged = if same(&ours, &before) {
                    (*after).clone()
                } else {
                    let mut page = (*ours).clone();
                    let (slug, slug_conflict) = scalar(&before.slug, &ours.slug, &after.slug);
                    let (title, title_conflict) = scalar(&before.title, &ours.title, &after.title);
                    let (author, author_conflict) =
                        scalar(&before.author, &ours.author, &after.author);
                    for (name, conflict) in [
                        ("slug", slug_conflict),
                        ("title", title_conflict),
                        ("author", author_conflict),
                    ] {
                        if conflict {
                            conflicts.push(format!(
                                "{}: {name} changed on both sides, kept {ours_name}",
                                ours.slug
                            ));
                        }
                    }
                    page.slug = slug;
                    page.title = title;
                    page.author = author;
                    page.tags = tags3(&before.tags, &ours.tags, &after.tags);
                    let body = merge_lines(
                        &chunk::text(&before.body),
                        &chunk::text(&ours.body),
                        &chunk::text(&after.body),
                        ours_name,
                        theirs_name,
                    );
                    if body.conflicts > 0 {
                        conflicts.push(format!(
                            "{}: {} body conflict(s), markers left in the text",
                            page.slug, body.conflicts
                        ));
                    }
                    chunk::update(&mut page.body, &body.text, &DEFAULT);
                    page
                };
                merged.id = id;
                if merged.slug != ours.slug
                    && tx.by_slug(&merged.slug).is_some_and(|other| other.id != id)
                {
                    conflicts.push(format!(
                        "{}: slug {} is taken, kept {}",
                        ours.slug, merged.slug, ours.slug
                    ));
                    merged.slug = ours.slug.clone();
                }
                merged.updated = now;
                tx.put(merged);
                applied += 1;
            }
        }
    }
    tx.finish();
    Report { applied, conflicts }
}
