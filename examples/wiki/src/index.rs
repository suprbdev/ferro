use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

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
        self.w.slugs.get(slug).and_then(|id| self.w.pages.get(&id))
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
        if let Some(old) = &old
            && old.slug != new.slug
            && self.w.slugs.get(&old.slug).as_deref() == Some(&new.id)
        {
            self.w.slugs.remove(&old.slug);
        }
        self.w.slugs.insert(new.slug.clone(), new.id);
        self.w.pages.insert(new);
        old
    }

    pub fn remove(&mut self, id: u64) -> Option<Arc<Page>> {
        let old = self.page(id)?;
        let before = analyze(&old);
        self.record(id, Some(before), None);
        if self.w.slugs.get(&old.slug).as_deref() == Some(&id) {
            self.w.slugs.remove(&old.slug);
        }
        self.w.pages.remove(&id);
        Some(old)
    }

    pub fn finish(self) {
        let Tx { w, terms, links } = self;
        let mut terms: Vec<_> = terms.into_iter().collect();
        terms.sort_by(|a, b| a.0.cmp(&b.0));
        let mut links: Vec<_> = links.into_iter().collect();
        links.sort_by(|a, b| a.0.cmp(&b.0));
        for (term, updates) in terms {
            let emptied = {
                let posting = w.terms.get_or_insert_with(term.clone(), Default::default);
                if updates.iter().all(|(_, count)| count.is_some()) {
                    posting.extend(
                        updates
                            .into_iter()
                            .filter_map(|(id, count)| Some((id, count?))),
                    );
                } else {
                    for (id, count) in updates {
                        match count {
                            Some(count) => {
                                posting.insert(id, count);
                            }
                            None => {
                                posting.remove(&id);
                            }
                        }
                    }
                }
                posting.is_empty()
            };
            if emptied {
                w.terms.remove(&term);
            }
        }
        for (target, updates) in links {
            let emptied = {
                let sources = w.links.get_or_insert_with(target.clone(), Default::default);
                for (id, present) in updates {
                    if present {
                        sources.insert(id);
                    } else {
                        sources.remove(&id);
                    }
                }
                sources.is_empty()
            };
            if emptied {
                w.links.remove(&target);
            }
        }
    }
}
