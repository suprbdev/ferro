use crate::model::{SeriesKey, SeriesRec, now_ms};

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub key: SeriesKey,
    pub ts: Option<u64>,
    pub value: f64,
}

fn valid_metric(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == ':')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

fn valid_label(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

struct Cursor<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Cursor { text, at: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += c.len_utf8();
        Some(c)
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_whitespace()) {
            self.bump();
        }
    }

    fn take_while(&mut self, keep: impl Fn(char) -> bool) -> &'a str {
        let start = self.at;
        while self.peek().is_some_and(&keep) {
            self.bump();
        }
        &self.text[start..self.at]
    }

    fn quoted(&mut self) -> Result<String, String> {
        if self.bump() != Some('"') {
            return Err("expected '\"'".to_string());
        }
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err("unterminated string".to_string()),
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    Some('n') => out.push('\n'),
                    Some(c) => out.push(c),
                    None => return Err("unterminated string".to_string()),
                },
                Some(c) => out.push(c),
            }
        }
    }
}

fn parse_value(token: &str) -> Result<f64, String> {
    match token {
        "NaN" | "nan" => Ok(f64::NAN),
        "+Inf" | "Inf" | "inf" | "+inf" => Ok(f64::INFINITY),
        "-Inf" | "-inf" => Ok(f64::NEG_INFINITY),
        _ => token.parse::<f64>().map_err(|_| format!("bad value '{token}'")),
    }
}

pub fn parse_line(line: &str) -> Result<Option<Sample>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let mut cur = Cursor::new(line);
    let metric = cur.take_while(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
    if !valid_metric(metric) {
        return Err(format!("bad metric name '{metric}'"));
    }
    let mut labels: Vec<(String, String)> = Vec::new();
    if cur.peek() == Some('{') {
        cur.bump();
        loop {
            cur.skip_ws();
            if cur.peek() == Some('}') {
                cur.bump();
                break;
            }
            let name = cur.take_while(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid_label(name) {
                return Err(format!("bad label name '{name}'"));
            }
            cur.skip_ws();
            if cur.bump() != Some('=') {
                return Err("expected '=' after label name".to_string());
            }
            cur.skip_ws();
            let value = cur.quoted()?;
            if labels.iter().any(|(k, _)| k == name) {
                return Err(format!("duplicate label '{name}'"));
            }
            labels.push((name.to_string(), value));
            cur.skip_ws();
            match cur.peek() {
                Some(',') => {
                    cur.bump();
                }
                Some('}') => {}
                _ => return Err("expected ',' or '}'".to_string()),
            }
        }
    }
    let (value, ts) = parse_tail(cur.rest())?;
    Ok(Some(Sample {
        key: SeriesKey::new(metric.to_string(), labels),
        ts,
        value,
    }))
}

pub fn parse_tail(rest: &str) -> Result<(f64, Option<u64>), String> {
    let mut fields = rest.split_whitespace();
    let value = parse_value(fields.next().ok_or("missing value")?)?;
    let ts = match fields.next() {
        Some(token) => Some(
            token
                .parse::<u64>()
                .map_err(|_| format!("bad timestamp '{token}'"))?,
        ),
        None => None,
    };
    if fields.next().is_some() {
        return Err("trailing input".to_string());
    }
    Ok((value, ts))
}

pub fn split_series(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let mut at = 0;
    let mut quoted = false;
    let mut braced = false;
    while at < bytes.len() {
        let byte = bytes[at];
        if quoted {
            match byte {
                b'\\' => at += 1,
                b'"' => quoted = false,
                _ => {}
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'{' => braced = true,
                b'}' => return Some(line.split_at(at + 1)),
                b' ' | b'\t' if !braced => return Some(line.split_at(at)),
                _ => {}
            }
        }
        at += 1;
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Glob,
    NotGlob,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Matcher {
    pub name: String,
    pub op: Op,
    pub value: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selector {
    pub metric: Option<String>,
    pub matchers: Vec<Matcher>,
}

pub fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    for (index, part) in parts.iter().enumerate() {
        if index == 0 {
            match rest.strip_prefix(part) {
                Some(next) => rest = next,
                None => return false,
            }
        } else if index == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
    }
    true
}

impl Matcher {
    pub fn matches(&self, value: &str) -> bool {
        match self.op {
            Op::Eq => value == self.value,
            Op::Ne => value != self.value,
            Op::Glob => glob(&self.value, value),
            Op::NotGlob => !glob(&self.value, value),
        }
    }
}

impl Selector {
    pub fn parse(text: &str) -> Result<Selector, String> {
        let text = text.trim();
        let mut cur = Cursor::new(text);
        let metric = cur.take_while(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
        let metric = if metric.is_empty() {
            None
        } else if valid_metric(metric) {
            Some(metric.to_string())
        } else {
            return Err(format!("bad metric name '{metric}'"));
        };
        let mut matchers = Vec::new();
        if cur.peek() == Some('{') {
            cur.bump();
            loop {
                cur.skip_ws();
                if cur.peek() == Some('}') {
                    cur.bump();
                    break;
                }
                let name = cur.take_while(|c| c.is_ascii_alphanumeric() || c == '_');
                if !valid_label(name) {
                    return Err(format!("bad label name '{name}'"));
                }
                cur.skip_ws();
                let op = match (cur.bump(), cur.peek()) {
                    (Some('='), Some('~')) => {
                        cur.bump();
                        Op::Glob
                    }
                    (Some('='), _) => Op::Eq,
                    (Some('!'), Some('=')) => {
                        cur.bump();
                        Op::Ne
                    }
                    (Some('!'), Some('~')) => {
                        cur.bump();
                        Op::NotGlob
                    }
                    _ => return Err("expected one of = != =~ !~".to_string()),
                };
                cur.skip_ws();
                let value = cur.quoted()?;
                matchers.push(Matcher {
                    name: name.to_string(),
                    op,
                    value,
                });
                cur.skip_ws();
                match cur.peek() {
                    Some(',') => {
                        cur.bump();
                    }
                    Some('}') => {}
                    _ => return Err("expected ',' or '}'".to_string()),
                }
            }
        }
        cur.skip_ws();
        if !cur.rest().is_empty() {
            return Err(format!("trailing input '{}'", cur.rest()));
        }
        Ok(Selector { metric, matchers })
    }

    pub fn matches(&self, rec: &SeriesRec) -> bool {
        if let Some(metric) = &self.metric
            && &rec.metric != metric
        {
            return false;
        }
        self.matchers
            .iter()
            .all(|matcher| matcher.matches(rec.label(&matcher.name)))
    }
}

pub fn parse_duration(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let amount: u64 = digits
        .parse()
        .map_err(|_| format!("bad duration '{text}'"))?;
    let scale = match unit {
        "" | "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 7 * 86_400_000,
        _ => return Err(format!("bad duration unit in '{text}'")),
    };
    amount
        .checked_mul(scale)
        .ok_or_else(|| format!("duration '{text}' overflows"))
}

pub fn parse_time(text: &str, now: u64) -> Result<u64, String> {
    let text = text.trim();
    if text == "now" {
        return Ok(now);
    }
    if text == "max" {
        return Ok(u64::MAX);
    }
    if let Some(rest) = text.strip_prefix("now-") {
        return Ok(now.saturating_sub(parse_duration(rest)?));
    }
    if let Some(rest) = text.strip_prefix('-') {
        return Ok(now.saturating_sub(parse_duration(rest)?));
    }
    text.parse::<u64>()
        .map_err(|_| format!("bad time '{text}': use unix ms, now, or now-<duration>"))
}

pub fn parse_time_now(text: &str) -> Result<u64, String> {
    parse_time(text, now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_with_labels_and_timestamp() {
        let sample = parse_line(r#"http_requests{status="200",host="a"} 12.5 1700000000000"#)
            .unwrap()
            .unwrap();
        assert_eq!(sample.key.metric, "http_requests");
        assert_eq!(
            sample.key.labels,
            vec![
                ("host".to_string(), "a".to_string()),
                ("status".to_string(), "200".to_string())
            ]
        );
        assert_eq!(sample.ts, Some(1_700_000_000_000));
        assert_eq!(sample.value, 12.5);
    }

    #[test]
    fn line_without_labels_or_timestamp() {
        let sample = parse_line("up 1").unwrap().unwrap();
        assert!(sample.key.labels.is_empty());
        assert_eq!(sample.ts, None);
    }

    #[test]
    fn rejects_bad_lines() {
        assert!(parse_line("{a=\"b\"} 1").is_err());
        assert!(parse_line("m{a=b} 1").is_err());
        assert!(parse_line("m{a=\"b\"}").is_err());
        assert!(parse_line("m 1 2 3").is_err());
        assert!(parse_line("m{a=\"1\",a=\"2\"} 1").is_err());
        assert_eq!(parse_line("# hi").unwrap(), None);
    }

    #[test]
    fn selectors() {
        let s = Selector::parse(r#"m{a="x",b!="y",c=~"p*q"}"#).unwrap();
        assert_eq!(s.metric.as_deref(), Some("m"));
        assert_eq!(s.matchers.len(), 3);
        assert!(Selector::parse("m{a}").is_err());
        assert_eq!(Selector::parse("").unwrap(), Selector::default());
    }

    #[test]
    fn globs() {
        assert!(glob("a*", "abc"));
        assert!(glob("*c", "abc"));
        assert!(glob("a*c", "abc"));
        assert!(!glob("a*c", "abd"));
        assert!(glob("*", ""));
        assert!(!glob("ab*ba", "aba"));
    }

    #[test]
    fn times() {
        assert_eq!(parse_duration("5m").unwrap(), 300_000);
        assert_eq!(parse_time("now-1h", 10_000_000).unwrap(), 6_400_000);
        assert_eq!(parse_time("123", 0).unwrap(), 123);
        assert!(parse_time("x", 0).is_err());
    }
}
