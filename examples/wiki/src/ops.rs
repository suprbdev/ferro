use std::sync::Arc;

use crate::Res;
use crate::chunk::{self, DEFAULT};
use crate::index::Tx;
use crate::model::{Page, Wiki};
use crate::text::{heading, rewrite_links, slugify};

#[derive(Clone, Debug, Default)]
pub struct Edit {
    pub slug: String,
    pub title: Option<String>,
    pub text: Option<String>,
    pub tags: Option<Vec<String>>,
    pub author: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Created,
    Updated,
    Unchanged,
}

pub fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut tags: Vec<String> = tags
        .into_iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

pub fn clean_slug(input: &str) -> Res<String> {
    let slug = slugify(input);
    if slug.is_empty() {
        return Err(format!("invalid slug {input:?}").into());
    }
    Ok(slug)
}

pub fn put(tx: &mut Tx, edit: Edit, now: u64) -> Res<Outcome> {
    let slug = clean_slug(&edit.slug)?;
    match tx.by_slug(&slug) {
        Some(old) => {
            let mut page = (*old).clone();
            if let Some(text) = &edit.text {
                chunk::update(&mut page.body, text, &DEFAULT);
            }
            if let Some(title) = edit.title {
                page.title = title;
            }
            if let Some(tags) = edit.tags {
                page.tags = normalize_tags(tags);
            }
            if let Some(author) = edit.author {
                page.author = author;
            }
            if page.title == old.title
                && page.tags == old.tags
                && page.author == old.author
                && page.body == old.body
            {
                return Ok(Outcome::Unchanged);
            }
            page.updated = now;
            tx.put(page);
            Ok(Outcome::Updated)
        }
        None => {
            let Some(text) = edit.text else {
                return Err(format!("{slug} does not exist: give --file or --stdin").into());
            };
            let title = edit
                .title
                .or_else(|| heading(&text))
                .unwrap_or_else(|| slug.rsplit('/').next().unwrap_or(&slug).to_string());
            let id = tx.allocate();
            tx.put(Page {
                id,
                slug,
                title,
                body: chunk::build(&text, &DEFAULT),
                tags: normalize_tags(edit.tags.unwrap_or_default()),
                author: edit.author.unwrap_or_else(|| "anonymous".to_string()),
                updated: now,
            });
            Ok(Outcome::Created)
        }
    }
}

pub fn delete(tx: &mut Tx, slug: &str) -> Res<Arc<Page>> {
    let slug = clean_slug(slug)?;
    let page = tx
        .by_slug(&slug)
        .ok_or_else(|| format!("no such page {slug}"))?;
    tx.remove(page.id);
    Ok(page)
}

pub fn rename(tx: &mut Tx, old: &str, new: &str, now: u64) -> Res<usize> {
    let old = clean_slug(old)?;
    let new = clean_slug(new)?;
    if old == new {
        return Err("old and new slug are the same".into());
    }
    let page = tx
        .by_slug(&old)
        .ok_or_else(|| format!("no such page {old}"))?;
    if tx.by_slug(&new).is_some() {
        return Err(format!("{new} already exists").into());
    }
    let mut ids: Vec<u64> =
        tx.w.links
            .get(old.as_str())
            .map(|sources| sources.iter().collect())
            .unwrap_or_default();
    if !ids.contains(&page.id) {
        ids.push(page.id);
    }
    let mut rewritten = 0;
    for id in ids {
        let mut current = match tx.page(id) {
            Some(current) => (*current).clone(),
            None => continue,
        };
        if id == page.id {
            current.slug = new.clone();
            current.updated = now;
        }
        let text = chunk::text(&current.body);
        let changed = rewrite_links(&text, &old, &new);
        if changed != text {
            chunk::update(&mut current.body, &changed, &DEFAULT);
            current.updated = now;
            if id != page.id {
                rewritten += 1;
            }
        }
        if id == page.id || changed != text {
            tx.put(current);
        }
    }
    Ok(rewritten)
}

pub fn revert(tx: &mut Tx, slug: &str, past: &Wiki, now: u64) -> Res<Outcome> {
    let slug = clean_slug(slug)?;
    let current = tx.by_slug(&slug);
    let then = current
        .as_ref()
        .and_then(|page| past.pages.get(&page.id))
        .or_else(|| {
            past.slugs
                .get(slug.as_str())
                .and_then(|id| past.pages.get(&id))
        })
        .ok_or_else(|| format!("{slug} did not exist at that version"))?;
    let id = match &current {
        Some(page) => page.id,
        None => {
            if tx.page(then.id).is_some() {
                return Err("page id is in use by another page".into());
            }
            then.id
        }
    };
    if let Some(page) = &current
        && page.title == then.title
        && page.tags == then.tags
        && page.author == then.author
        && page.body == then.body
    {
        return Ok(Outcome::Unchanged);
    }
    tx.put(Page {
        id,
        slug,
        title: then.title.clone(),
        body: then.body.clone(),
        tags: then.tags.clone(),
        author: then.author.clone(),
        updated: now,
    });
    Ok(if current.is_some() {
        Outcome::Updated
    } else {
        Outcome::Created
    })
}
