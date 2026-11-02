use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use ferro::{Map, Set};

use crate::chunk;
use crate::model::{Page, Wiki};
use crate::text;

#[derive(Default)]
pub struct Analysis {
    pub terms: HashMap<String, u32>,
    pub links: BTreeSet<String>,
}

pub fn analyze(page: &Page) -> Analysis {
    let body = chunk::text(&page.body);
    let mut terms = HashMap::new();
    text::term_counts(&page.title, &mut terms);
    text::term_counts(&body, &mut terms);
    let mut links = text::extract_links(&body);
    links.remove(&page.slug);
    Analysis { terms, links }
}

fn stage<V>(map: &mut HashMap<String, Vec<(u64, V)>>, key: &str, id: u64, value: V) {
    match map.get_mut(key) {
        Some(updates) => updates.push((id, value)),
        None => {
            map.insert(key.to_string(), vec![(id, value)]);
        }
    }
}

pub struct Tx<'a> {
    pub w: &'a mut Wiki,
    terms: HashMap<String, Vec<(u64, Option<u32>)>>,
    links: HashMap<String, Vec<(u64, bool)>>,
}

impl<'a> Tx<'a> {
    pub fn new(w: &'a mut Wiki) -> Self {
        Tx {
            w,
            terms: HashMap::new(),
            links: HashMap::new(),
        }
    }

    pub fn page(&self, id: u64) -> Option<Arc<Page>> {
        self.w.pages.get(&id)
    }

    pub fn by_slug(&self, slug: &str) -> Option<Arc<Page>> {
        self.w.pages.by_slug(slug)
    }

    pub fn allocate(&mut self) -> u64 {
        self.w.next_id += 1;
        self.w.next_id
    }

    fn record(&mut self, id: u64, old: Option<Analysis>, new: Option<Analysis>) {
        let old = old.unwrap_or_default();
        let new = new.unwrap_or_default();
        for (term, count) in &new.terms {
            if old.terms.get(term) != Some(count) {
                stage(&mut self.terms, term, id, Some(*count));
            }
        }
        for term in old.terms.keys() {
            if !new.terms.contains_key(term) {
                stage(&mut self.terms, term, id, None);
            }
        }
        for target in new.links.difference(&old.links) {
            stage(&mut self.links, target, id, true);
        }
        for target in old.links.difference(&new.links) {
            stage(&mut self.links, target, id, false);
        }
    }

    pub fn put(&mut self, new: Page) -> Option<Arc<Page>> {
        let old = self.page(new.id);
        let unchanged_text = old.as_ref().is_some_and(|old| {
            old.body == new.body && old.title == new.title && old.slug == new.slug
        });
        if !unchanged_text {
            let before = old.as_deref().map(analyze);
            let after = analyze(&new);
            self.record(new.id, before, Some(after));
        }
        self.w.pages.insert(new);
        old
    }

    pub fn remove(&mut self, id: u64) -> Option<Arc<Page>> {
        let old = self.page(id)?;
        let before = analyze(&old);
        self.record(id, Some(before), None);
        self.w.pages.remove(&id);
        Some(old)
    }

    pub fn finish(self) {
        let Tx { w, terms, links } = self;
        let mut postings = Vec::with_capacity(terms.len());
        for (term, updates) in terms {
            let mut posting = w.terms.get(&term).map_or_else(Map::new, |list| (*list).clone());
            posting.apply(updates);
            postings.push((term, (!posting.is_empty()).then_some(posting)));
        }
        w.terms.apply(postings);
        let mut backlinks = Vec::with_capacity(links.len());
        for (target, updates) in links {
            let mut sources = w.links.get(&target).map_or_else(Set::new, |set| (*set).clone());
            sources.apply(updates);
            backlinks.push((target, (!sources.is_empty()).then_some(sources)));
        }
        w.links.apply(backlinks);
    }
}
