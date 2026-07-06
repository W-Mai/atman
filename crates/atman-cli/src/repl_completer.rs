use std::path::{Path, PathBuf};

use rustyline::Context;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;

const BUILTINS: &[&str] = &[
    "help", "exit", "quit", "session", "sessions", "cost", "attach", "suggest", "goal",
];

const INTERJECTIONS: &[&str] = &["nudge", "course-correct", "redirect", "stop"];

pub struct AtmanCompleter {
    commands_dir: Option<PathBuf>,
}

impl AtmanCompleter {
    pub fn new(config_dir: Option<PathBuf>) -> Self {
        Self {
            commands_dir: config_dir.map(|d| d.join("commands")),
        }
    }

    pub fn complete_line(&self, line: &str, pos: usize) -> (usize, Vec<Pair>) {
        let head = &line[..pos];
        if let Some(rest) = head.strip_prefix(':') {
            let (word, word_start) = last_word(rest, pos - rest.len());
            let candidates = filter_prefix(BUILTINS.iter().map(|s| s.to_string()), word);
            return (word_start, pairs(&candidates));
        }
        if let Some(rest) = head.strip_prefix('!') {
            let (word, word_start) = last_word(rest, pos - rest.len());
            let candidates = filter_prefix(INTERJECTIONS.iter().map(|s| s.to_string()), word);
            return (word_start, pairs(&candidates));
        }
        if let Some(rest) = head.strip_prefix('/') {
            let (word, word_start) = last_word(rest, pos - rest.len());
            let names = self.slash_command_names();
            let candidates = filter_prefix(names, word);
            return (word_start, pairs(&candidates));
        }
        (pos, Vec::new())
    }

    fn slash_command_names(&self) -> Vec<String> {
        let Some(dir) = self.commands_dir.as_deref() else {
            return Vec::new();
        };
        collect_command_names(dir)
    }
}

fn collect_command_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("at") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            out.push(stem.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn last_word(rest: &str, offset: usize) -> (&str, usize) {
    match rest.rfind(char::is_whitespace) {
        Some(i) => (&rest[i + 1..], offset + i + 1),
        None => (rest, offset),
    }
}

fn filter_prefix<I>(iter: I, prefix: &str) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    iter.into_iter().filter(|c| c.starts_with(prefix)).collect()
}

fn pairs(names: &[String]) -> Vec<Pair> {
    names
        .iter()
        .map(|n| Pair {
            display: n.clone(),
            replacement: n.clone(),
        })
        .collect()
}

impl Completer for AtmanCompleter {
    type Candidate = Pair;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        Ok(self.complete_line(line, pos))
    }
}

impl Hinter for AtmanCompleter {
    type Hint = String;
}

impl Highlighter for AtmanCompleter {}
impl Validator for AtmanCompleter {}
impl rustyline::Helper for AtmanCompleter {}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(tmp: &tempfile::TempDir, names: &[&str]) -> PathBuf {
        let dir = tmp.path().join("config/commands");
        std::fs::create_dir_all(&dir).unwrap();
        for n in names {
            std::fs::write(dir.join(format!("{n}.at")), "flow x() { return 1 }\n").unwrap();
        }
        std::fs::write(dir.join("notes.md"), "ignored\n").unwrap();
        tmp.path().join("config")
    }

    #[test]
    fn colon_prefix_completes_builtins() {
        let c = AtmanCompleter::new(None);
        let (start, cand) = c.complete_line(":ex", 3);
        assert_eq!(start, 1);
        let names: Vec<&str> = cand.iter().map(|p| p.display.as_str()).collect();
        assert_eq!(names, vec!["exit"]);
    }

    #[test]
    fn colon_prefix_lists_all_when_empty() {
        let c = AtmanCompleter::new(None);
        let (_, cand) = c.complete_line(":", 1);
        assert!(cand.iter().any(|p| p.display == "help"));
        assert!(cand.iter().any(|p| p.display == "attach"));
    }

    #[test]
    fn bang_prefix_completes_interjections() {
        let c = AtmanCompleter::new(None);
        let (start, cand) = c.complete_line("!course", 7);
        assert_eq!(start, 1);
        let names: Vec<&str> = cand.iter().map(|p| p.display.as_str()).collect();
        assert_eq!(names, vec!["course-correct"]);
    }

    #[test]
    fn slash_prefix_completes_from_commands_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = seed(&tmp, &["review_code", "reset", "hello"]);
        let c = AtmanCompleter::new(Some(cfg));
        let (start, cand) = c.complete_line("/re", 3);
        assert_eq!(start, 1);
        let mut names: Vec<&str> = cand.iter().map(|p| p.display.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["reset", "review_code"]);
    }

    #[test]
    fn slash_prefix_ignores_non_at_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = seed(&tmp, &["ok"]);
        let c = AtmanCompleter::new(Some(cfg));
        let (_, cand) = c.complete_line("/", 1);
        let names: Vec<&str> = cand.iter().map(|p| p.display.as_str()).collect();
        assert_eq!(names, vec!["ok"]);
    }

    #[test]
    fn plain_text_returns_no_candidates() {
        let c = AtmanCompleter::new(None);
        let (_, cand) = c.complete_line("hello world", 11);
        assert!(cand.is_empty());
    }

    #[test]
    fn colon_after_first_word_ignored() {
        let c = AtmanCompleter::new(None);
        let (_, cand) = c.complete_line("something :ex", 13);
        assert!(cand.is_empty());
    }

    #[test]
    fn missing_commands_dir_yields_no_slash_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("nope/config");
        let c = AtmanCompleter::new(Some(cfg));
        let (_, cand) = c.complete_line("/x", 2);
        assert!(cand.is_empty());
    }
}
