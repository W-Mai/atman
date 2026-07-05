use std::collections::HashSet;

use regex::{Regex, RegexSet};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RedactMode {
    #[default]
    Full,
    Partial,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactHit {
    pub kind: String,
    pub start: usize,
    pub end: usize,
}

pub struct Redactor {
    kinds: Vec<String>,
    regexes: Vec<Regex>,
    set: RegexSet,
    mode: RedactMode,
    allowlist: HashSet<String>,
}

impl Redactor {
    pub fn builtin() -> Self {
        Self::from_pairs(BUILTIN_PATTERNS, RedactMode::default())
    }

    pub fn from_pairs(pairs: &[(&str, &str)], mode: RedactMode) -> Self {
        let mut kinds = Vec::with_capacity(pairs.len());
        let mut regexes = Vec::with_capacity(pairs.len());
        let mut src = Vec::with_capacity(pairs.len());
        for (kind, pattern) in pairs {
            let compiled = Regex::new(pattern)
                .unwrap_or_else(|e| panic!("invalid redact regex `{kind}`: {e}"));
            kinds.push((*kind).to_string());
            regexes.push(compiled);
            src.push(*pattern);
        }
        let set = RegexSet::new(&src).expect("build RegexSet");
        Self {
            kinds,
            regexes,
            set,
            mode,
            allowlist: HashSet::new(),
        }
    }

    pub fn with_mode(mut self, mode: RedactMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_allowlist(mut self, items: impl IntoIterator<Item = String>) -> Self {
        self.allowlist = items.into_iter().collect();
        self
    }

    pub fn is_enabled(&self) -> bool {
        !self.regexes.is_empty()
    }

    pub fn scan(&self, text: &str) -> Vec<RedactHit> {
        if !self.set.is_match(text) {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for &idx in self.set.matches(text).iter().collect::<Vec<_>>().iter() {
            let regex = &self.regexes[idx];
            let kind = &self.kinds[idx];
            for m in regex.find_iter(text) {
                if self.allowlist.contains(m.as_str()) {
                    continue;
                }
                hits.push(RedactHit {
                    kind: kind.clone(),
                    start: m.start(),
                    end: m.end(),
                });
            }
        }
        hits.sort_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then_with(|| (b.end - b.start).cmp(&(a.end - a.start)))
        });
        dedup_overlaps(hits)
    }

    pub fn redact(&self, text: &str) -> (String, Vec<RedactHit>) {
        let hits = self.scan(text);
        if hits.is_empty() {
            return (text.to_string(), hits);
        }
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for hit in &hits {
            out.push_str(&text[cursor..hit.start]);
            let matched = &text[hit.start..hit.end];
            out.push_str(&render_replacement(matched, &hit.kind, self.mode));
            cursor = hit.end;
        }
        out.push_str(&text[cursor..]);
        (out, hits)
    }

    pub fn redact_json(&self, value: &mut serde_json::Value) -> Vec<RedactHit> {
        let mut hits = Vec::new();
        walk_json(value, self, &mut hits);
        hits
    }
}

fn dedup_overlaps(mut hits: Vec<RedactHit>) -> Vec<RedactHit> {
    let mut out: Vec<RedactHit> = Vec::with_capacity(hits.len());
    for hit in hits.drain(..) {
        if let Some(last) = out.last_mut()
            && hit.start < last.end
        {
            if hit.end > last.end {
                last.end = hit.end;
            }
            continue;
        }
        out.push(hit);
    }
    out
}

fn render_replacement(matched: &str, kind: &str, mode: RedactMode) -> String {
    match mode {
        RedactMode::Full => format!("<REDACTED:{kind}>"),
        RedactMode::Partial => {
            if matched.len() <= 8 {
                format!("<REDACTED:{kind}>")
            } else {
                let head: String = matched.chars().take(3).collect();
                let tail: String = matched
                    .chars()
                    .rev()
                    .take(3)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                format!("{head}***{tail} <REDACTED:{kind}>")
            }
        }
    }
}

fn walk_json(value: &mut serde_json::Value, r: &Redactor, hits: &mut Vec<RedactHit>) {
    match value {
        serde_json::Value::String(s) => {
            let (redacted, mut new_hits) = r.redact(s);
            if !new_hits.is_empty() {
                *s = redacted;
                hits.append(&mut new_hits);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                walk_json(v, r, hits);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, v) in map {
                walk_json(v, r, hits);
            }
        }
        _ => {}
    }
}

pub const BUILTIN_PATTERNS: &[(&str, &str)] = &[
    ("anthropic_api_key", r"sk-ant-[A-Za-z0-9_-]{20,}"),
    ("openai_api_key", r"sk-[A-Za-z0-9_-]{20,}"),
    ("github_token", r"gh[pousr]_[A-Za-z0-9]{20,}"),
    ("google_api_key", r"AIza[0-9A-Za-z_-]{35}"),
    ("aws_access_key", r"AKIA[0-9A-Z]{16}"),
    ("bearer_token", r"Bearer\s+[A-Za-z0-9\-_.]{20,}"),
    ("private_key_header", r"-----BEGIN [A-Z ]+PRIVATE KEY-----"),
    (
        "jwt",
        r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
    ),
    (
        "email",
        r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
    ),
    ("credit_card", r"\b(?:\d[ -]*?){13,16}\b"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_key_is_replaced_with_full_marker() {
        let r = Redactor::builtin();
        let (out, hits) = r.redact("token=sk-abcdefghijklmnop1234567890 rest");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "openai_api_key");
        assert!(out.contains("<REDACTED:openai_api_key>"), "out: {out}");
        assert!(!out.contains("sk-abcdef"), "sensitive prefix leaked: {out}");
    }

    #[test]
    fn anthropic_key_wins_over_openai_prefix() {
        let r = Redactor::builtin();
        let (out, hits) = r.redact("key=sk-ant-abcdefghijklmn12345678 end");
        let kinds: Vec<_> = hits.iter().map(|h| h.kind.as_str()).collect();
        assert!(kinds.contains(&"anthropic_api_key"), "kinds: {kinds:?}");
        assert!(out.contains("<REDACTED:"), "out: {out}");
    }

    #[test]
    fn multiple_patterns_in_one_string_are_all_replaced() {
        let r = Redactor::builtin();
        let input = "gh=ghp_abcdefghij1234567890xyzXYZ11 email=alice@example.com";
        let (out, hits) = r.redact(input);
        assert_eq!(hits.len(), 2, "hits: {hits:?}");
        assert!(out.contains("<REDACTED:github_token>"));
        assert!(out.contains("<REDACTED:email>"));
    }

    #[test]
    fn partial_mode_shows_prefix_and_suffix() {
        let r = Redactor::builtin().with_mode(RedactMode::Partial);
        let (out, _) = r.redact("token=sk-abcdefghijklmnop1234567890 rest");
        assert!(out.contains("sk-***"), "partial marker missing: {out}");
        assert!(out.contains("<REDACTED:openai_api_key>"));
        assert!(!out.contains("sk-abcdefghijklmnop"), "full leak: {out}");
    }

    #[test]
    fn allowlist_lets_specific_matches_pass_through() {
        let r =
            Redactor::builtin().with_allowlist(["sk-test-fixture-value-1234567890".to_string()]);
        let (out, hits) = r.redact("cfg=sk-test-fixture-value-1234567890");
        assert!(hits.is_empty(), "allowlist should suppress: {hits:?}");
        assert_eq!(out, "cfg=sk-test-fixture-value-1234567890");
    }

    #[test]
    fn clean_input_is_left_alone() {
        let r = Redactor::builtin();
        let (out, hits) = r.redact("no secrets here, just prose");
        assert!(hits.is_empty());
        assert_eq!(out, "no secrets here, just prose");
    }

    #[test]
    fn overlapping_matches_are_deduplicated() {
        let r = Redactor::from_pairs(
            &[("kind_a", r"foo\d+"), ("kind_b", r"foo123bar")],
            RedactMode::Full,
        );
        let (out, hits) = r.redact("foo123bar tail");
        assert_eq!(hits.len(), 1, "overlapping match should collapse: {hits:?}");
        assert!(out.starts_with("<REDACTED:"), "out: {out}");
    }

    #[test]
    fn redact_json_walks_strings_arrays_and_objects() {
        let r = Redactor::builtin();
        let mut v = serde_json::json!({
            "token": "sk-abcdefghijklmn1234567890",
            "notes": [
                "safe text",
                "email me at bob@example.com",
                {"nested": "gh=ghp_abcdefghij1234567890xyzXYZ11"}
            ]
        });
        let hits = r.redact_json(&mut v);
        assert_eq!(hits.len(), 3, "hits: {hits:?}");
        let flat = v.to_string();
        assert!(flat.contains("<REDACTED:openai_api_key>"));
        assert!(flat.contains("<REDACTED:email>"));
        assert!(flat.contains("<REDACTED:github_token>"));
        assert!(!flat.contains("bob@example.com"));
    }
}
