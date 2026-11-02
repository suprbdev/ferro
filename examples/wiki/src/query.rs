use std::collections::BTreeMap;
use std::sync::Arc;

use ferro::Map;

use crate::chunk;
use crate::index::analyze;
use crate::model::{Page, Wiki};

pub fn find(w: &Wiki, slug: &str) -> Option<Arc<Page>> {
    w.slugs.get(slug).and_then(|id| w.pages.get(&id))
}

pub struct Hit {
    pub score: u32,
    pub page: Arc<Page>,
}

pub fn search(w: &Wiki, terms: &[String], limit: usize) -> (usize, Vec<Hit>) {
    let mut postings: Vec<Arc<Map<u64, u32>>> = Vec::new();
    for term in terms {
        match w.terms.get(term.as_str()) {
            Some(posting) => postings.push(posting),
            None => return (0, Vec::new()),
        }
    }
    postings.sort_by_key(|posting| posting.len());
    let Some((first, rest)) = postings.split_first() else {
        return (0, Vec::new());
    };
    let mut scored: Vec<(u32, u64)> = first
        .iter()
        .filter_map(|(id, tf)| {
            let mut score = *tf;
            for posting in rest {
                score += *posting.get(&id)?;
            }
            Some((score, id))
        })
        .collect();
    let total = scored.len();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let hits = scored
        .into_iter()
        .take(limit)
        .filter_map(|(score, id)| w.pages.get(&id).map(|page| Hit { score, page }))
        .collect();
    (total, hits)
}

pub fn snippet(page: &Page, terms: &[String]) -> Option<String> {
    for chunk in page.body.values() {
        for line in chunk.lines() {
            let lower = line.to_lowercase();
            if terms.iter().any(|term| lower.contains(term.as_str())) {
                let trimmed = line.trim();
                let cut: String = trimmed.chars().take(100).collect();
                return Some(cut);
            }
        }
    }
    None
}

pub fn backlinks(w: &Wiki, slug: &str) -> Vec<Arc<Page>> {
    let mut pages: Vec<Arc<Page>> = w
        .links
        .get(slug)
        .map(|sources| sources.iter().filter_map(|id| w.pages.get(&id)).collect())
        .unwrap_or_default();
    pages.sort_by(|a, b| a.slug.cmp(&b.slug));
    pages
}

pub fn orphans(w: &Wiki) -> Vec<String> {
    w.slugs
        .iter()
        .filter(|(slug, _)| w.links.get(slug.as_str()).is_none_or(|s| s.is_empty()))
        .map(|(slug, _)| slug)
        .collect()
}

pub fn list(w: &Wiki, tag: Option<&str>, prefix: Option<&str>, limit: usize) -> Vec<Arc<Page>> {
    let wanted = |page: &Page| prefix.is_none_or(|prefix| page.slug.starts_with(prefix));
    match tag {
        Some(tag) => {
            let mut pages: Vec<Arc<Page>> = w
                .pages
                .by_tags(tag.to_lowercase().as_str())
                .filter(|page| wanted(page))
                .collect();
            pages.sort_by(|a, b| a.slug.cmp(&b.slug));
            pages.truncate(limit);
            pages
        }
        None => w
            .slugs
            .iter()
            .filter(|(slug, _)| prefix.is_none_or(|prefix| slug.starts_with(prefix)))
            .filter_map(|(_, id)| w.pages.get(&id))
            .take(limit)
            .collect(),
    }
}

pub struct Stats {
    pub pages: usize,
    pub terms: usize,
    pub postings: usize,
    pub link_targets: usize,
    pub links: usize,
    pub tags: Vec<(String, usize)>,
    pub authors: Vec<(String, usize)>,
    pub body_bytes: Option<(u64, u64)>,
}

pub fn stats(w: &Wiki, deep: bool) -> Stats {
    let body_bytes = deep.then(|| {
        w.pages.iter().fold((0u64, 0u64), |(bytes, chunks), page| {
            (
                bytes + chunk::bytes(&page.body) as u64,
                chunks + page.body.len() as u64,
            )
        })
    });
    Stats {
        pages: w.pages.len(),
        terms: w.terms.len(),
        postings: w.terms.values().map(|posting| posting.len()).sum(),
        link_targets: w.links.len(),
        links: w.links.values().map(|sources| sources.len()).sum(),
        tags: w.pages.by_tags_counts(),
        authors: w.pages.by_author_counts(),
        body_bytes,
    }
}

fn check_group(
    w: &Wiki,
    group: &mut BTreeMap<String, Vec<(u64, u32)>>,
    lo: u64,
    hi: u64,
    problems: &mut Vec<String>,
) {
    for (term, expected) in std::mem::take(group) {
        match w.terms.get(term.as_str()) {
            None => problems.push(format!("term {term:?} has no postings")),
            Some(posting) => {
                let actual: Vec<(u64, u32)> = posting
                    .range(lo..=hi)
                    .map(|(id, count)| (id, *count))
                    .collect();
                if actual != expected {
                    problems.push(format!(
                        "term {term:?} postings for pages {lo}..={hi} differ: stored {} entries, expected {}",
                        actual.len(),
                        expected.len()
                    ));
                }
            }
        }
    }
}

pub fn check_indexes(w: &Wiki) -> Vec<String> {
    const GROUP: usize = 5000;
    let mut problems = Vec::new();
    if w.slugs.len() != w.pages.len() {
        problems.push(format!(
            "slugs has {} entries for {} pages",
            w.slugs.len(),
            w.pages.len()
        ));
    }
    let mut expected_postings = 0usize;
    let mut expected_links = 0usize;
    let mut group: BTreeMap<String, Vec<(u64, u32)>> = BTreeMap::new();
    let (mut lo, mut hi, mut in_group) = (0u64, 0u64, 0usize);
    for page in w.pages.iter() {
        if w.slugs.get(page.slug.as_str()).as_deref() != Some(&page.id) {
            problems.push(format!(
                "slug {} does not map to page {}",
                page.slug, page.id
            ));
        }
        let analysis = analyze(&page);
        if in_group == 0 {
            lo = page.id;
        }
        hi = page.id;
        in_group += 1;
        expected_postings += analysis.terms.len();
        for (term, count) in analysis.terms {
            group.entry(term).or_default().push((page.id, count));
        }
        for target in &analysis.links {
            expected_links += 1;
            if !w
                .links
                .get(target.as_str())
                .is_some_and(|sources| sources.contains(&page.id))
            {
                problems.push(format!("missing backlink {target} <- {}", page.slug));
            }
        }
        if in_group == GROUP {
            check_group(w, &mut group, lo, hi, &mut problems);
            in_group = 0;
        }
        if problems.len() > 50 {
            problems.push("too many problems, stopping".to_string());
            return problems;
        }
    }
    check_group(w, &mut group, lo, hi, &mut problems);
    let postings: usize = w.terms.values().map(|posting| posting.len()).sum();
    if postings != expected_postings {
        problems.push(format!(
            "terms hold {postings} postings, pages imply {expected_postings}"
        ));
    }
    let links: usize = w.links.values().map(|sources| sources.len()).sum();
    if links != expected_links {
        problems.push(format!(
            "links hold {links} entries, pages imply {expected_links}"
        ));
    }
    problems
}
