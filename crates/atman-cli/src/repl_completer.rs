use std::path::PathBuf;

use rustyline::Context;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;

const BUILTINS: &[&str] = &[
    "help", "exit", "quit", "session", "sessions", "cost", "attach", "suggest", "goal", "model",
];

const INTERJECTIONS: &[&str] = &["nudge", "course-correct", "redirect", "stop"];

pub struct AtmanCompleter {
    config_dir: Option<PathBuf>,
    project_root: Option<PathBuf>,
}

impl AtmanCompleter {
    pub fn new(config_dir: Option<PathBuf>) -> Self {
        Self {
            config_dir,
            project_root: atman_runtime::tools::flow_source::current_project_root(),
        }
    }

    #[cfg(test)]
    fn with_project_root(config_dir: Option<PathBuf>, project_root: Option<PathBuf>) -> Self {
        Self {
            config_dir,
            project_root,
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
        atman_runtime::tools::flow_source::installed_sources(
            self.config_dir.as_deref(),
            self.project_root.as_deref(),
        )
        .into_iter()
        .filter_map(|source| {
            source
                .path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .collect()
    }
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
    fn slash_prefix_merges_project_and_user_commands_with_project_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = seed(&tmp, &["review", "global"]);
        let project = tmp.path().join("project");
        let commands = project.join(".atman/commands");
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(commands.join("review.at"), "project\n").unwrap();
        std::fs::write(commands.join("local.at"), "project\n").unwrap();
        let c = AtmanCompleter::with_project_root(Some(cfg), Some(project));

        let (_, cand) = c.complete_line("/", 1);
        let mut names = cand
            .iter()
            .map(|pair| pair.display.as_str())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["global", "local", "review"]);
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
