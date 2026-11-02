use std::collections::{BTreeSet, HashMap};

const STOPWORDS: [&str; 24] = [
    "the", "and", "of", "to", "in", "is", "it", "for", "on", "as", "at", "by", "an", "be", "or",
    "are", "was", "with", "that", "this", "from", "not", "but", "if",
];

pub fn slugify(input: &str) -> String {
    let mut segments: Vec<String> = Vec::new();
    for segment in input.split('/') {
        let mut out = String::new();
        for c in segment.trim().chars() {
            if c.is_alphanumeric() || c == '_' || c == '.' {
                out.extend(c.to_lowercase());
            } else if !out.ends_with('-') {
                out.push('-');
            }
        }
        let trimmed = out.trim_matches(|c| c == '-' || c == '.').to_string();
        if !trimmed.is_empty() {
            segments.push(trimmed);
        }
    }
    segments.join("/")
}

pub fn for_each_token(text: &str, mut emit: impl FnMut(&str)) {
    let mut token = String::new();
    let mut flush = |token: &mut String| {
        let len = token.chars().count();
        if (2..=40).contains(&len) && !STOPWORDS.contains(&token.as_str()) {
            emit(token);
        }
        token.clear();
    };
    for c in text.chars() {
        if c.is_alphanumeric() {
            token.extend(c.to_lowercase());
        } else if !token.is_empty() {
            flush(&mut token);
        }
    }
    if !token.is_empty() {
        flush(&mut token);
    }
}

pub fn term_counts(text: &str, counts: &mut HashMap<String, u32>) {
    for_each_token(text, |token| match counts.get_mut(token) {
        Some(count) => *count += 1,
        None => {
            counts.insert(token.to_string(), 1);
        }
    });
}

pub fn query_terms(words: &[String]) -> Vec<String> {
    let mut terms = BTreeSet::new();
    for word in words {
        for_each_token(word, |token| {
            terms.insert(token.to_string());
        });
    }
    terms.into_iter().collect()
}

pub struct LinkRef {
    pub start: usize,
    pub end: usize,
    pub target: String,
    pub label: Option<String>,
}

pub fn link_refs(text: &str) -> Vec<LinkRef> {
    let mut refs = Vec::new();
    let mut from = 0;
    while let Some(open) = text[from..].find("[[") {
        let inner_start = from + open + 2;
        let Some(close) = text[inner_start..].find("]]") else {
            break;
        };
        let inner = &text[inner_start..inner_start + close];
        if inner.contains('\n') || inner.contains("[[") {
            from = inner_start;
            continue;
        }
        let (target, label) = match inner.split_once('|') {
            Some((target, label)) => (target, Some(label.to_string())),
            None => (inner, None),
        };
        let target = target.split('#').next().unwrap_or("");
        let slug = slugify(target);
        let end = inner_start + close + 2;
        if !slug.is_empty() {
            refs.push(LinkRef {
                start: from + open,
                end,
                target: slug,
                label,
            });
        }
        from = end;
    }
    refs
}

pub fn extract_links(text: &str) -> BTreeSet<String> {
    link_refs(text)
        .into_iter()
        .map(|link| link.target)
        .collect()
}

pub fn rewrite_links(text: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for link in link_refs(text) {
        if link.target != old {
            continue;
        }
        out.push_str(&text[cursor..link.start]);
        match &link.label {
            Some(label) => out.push_str(&format!("[[{new}|{label}]]")),
            None => out.push_str(&format!("[[{new}]]")),
        }
        cursor = link.end;
    }
    out.push_str(&text[cursor..]);
    out
}

pub fn heading(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("# "))
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
}
