//! Portable watch-rule compilation and stream decisions.

use alloc::{
    collections::BTreeSet,
    format,
    string::{String, ToString},
    vec::Vec,
};

use crate::ast::{CmpOp, Expr, Literal, WatchAction, WatchDecl, WatchEvent};

#[derive(Clone, Default)]
pub struct WatchRules {
    token_abort: Vec<String>,
    tokens_abort_gt: Option<u64>,
    elapsed_abort_gt: Option<u64>,
    token_warn: Vec<WarnRule>,
    tokens_warn_gt: Vec<(u64, WarnRule)>,
    elapsed_warn_gt: Vec<(u64, WarnRule)>,
}

#[derive(Clone)]
struct WarnRule {
    target: String,
    message: String,
    pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchWarning {
    pub target: String,
    pub trigger: String,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct WatchObservation {
    pub abort_reason: Option<String>,
    pub warnings: Vec<WatchWarning>,
}

impl WatchRules {
    pub fn compile(watches: &[&WatchDecl]) -> Self {
        let mut rules = Self::default();
        for watch in watches {
            for block in &watch.on_blocks {
                let abort = block
                    .actions
                    .iter()
                    .any(|action| matches!(action, WatchAction::Abort { .. }));
                let warn = block.actions.iter().find_map(|action| match action {
                    WatchAction::Warn { msg } => Some(msg),
                    _ => None,
                });
                if !abort && warn.is_none() {
                    continue;
                }
                match &block.event {
                    WatchEvent::Token { patterns } => {
                        for pattern in patterns {
                            if abort {
                                rules.token_abort.push(pattern.clone());
                            }
                            if let Some(msg) = warn {
                                rules.token_warn.push(WarnRule {
                                    target: watch.target.name.clone(),
                                    message: render_message(
                                        msg,
                                        &format!("watch warn: token `{pattern}`"),
                                    ),
                                    pattern: pattern.clone(),
                                });
                            }
                        }
                    }
                    WatchEvent::TokensConsumed { cmp, value } if is_upper_bound(*cmp) => {
                        let threshold = threshold(*cmp, *value);
                        if abort {
                            rules.tokens_abort_gt = Some(
                                rules
                                    .tokens_abort_gt
                                    .map_or(threshold, |old| old.min(threshold)),
                            );
                        }
                        if let Some(msg) = warn {
                            rules.tokens_warn_gt.push((
                                threshold,
                                WarnRule {
                                    target: watch.target.name.clone(),
                                    message: render_message(
                                        msg,
                                        &format!("watch warn: tokens_consumed > {threshold}"),
                                    ),
                                    pattern: format!("tokens_consumed>{threshold}"),
                                },
                            ));
                        }
                    }
                    WatchEvent::Elapsed { cmp, duration_ms } if is_upper_bound(*cmp) => {
                        let threshold = threshold(*cmp, *duration_ms);
                        if abort {
                            rules.elapsed_abort_gt = Some(
                                rules
                                    .elapsed_abort_gt
                                    .map_or(threshold, |old| old.min(threshold)),
                            );
                        }
                        if let Some(msg) = warn {
                            rules.elapsed_warn_gt.push((
                                threshold,
                                WarnRule {
                                    target: watch.target.name.clone(),
                                    message: render_message(
                                        msg,
                                        &format!("watch warn: elapsed > {threshold}ms"),
                                    ),
                                    pattern: format!("elapsed>{threshold}ms"),
                                },
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        rules
    }

    pub fn is_active(&self) -> bool {
        !self.token_abort.is_empty()
            || self.tokens_abort_gt.is_some()
            || self.elapsed_abort_gt.is_some()
            || !self.token_warn.is_empty()
            || !self.tokens_warn_gt.is_empty()
            || !self.elapsed_warn_gt.is_empty()
    }

    pub fn elapsed_abort_deadline_ms(&self) -> Option<u64> {
        self.elapsed_abort_gt
    }
}

fn is_upper_bound(cmp: CmpOp) -> bool {
    matches!(cmp, CmpOp::Gt | CmpOp::Ge)
}

fn threshold(cmp: CmpOp, value: u64) -> u64 {
    if matches!(cmp, CmpOp::Ge) {
        value.saturating_sub(1)
    } else {
        value
    }
}

fn render_message(msg: &Option<Expr>, fallback: &str) -> String {
    match msg {
        Some(Expr::Literal(Literal::Str(text))) => text.clone(),
        _ => fallback.to_string(),
    }
}

#[derive(Default)]
pub struct WatchState {
    window: String,
    tokens_seen: u64,
    abort_reason: Option<String>,
    fired_token: BTreeSet<usize>,
    fired_tokens: BTreeSet<usize>,
    fired_elapsed: BTreeSet<usize>,
}

impl WatchState {
    pub fn tokens_seen(&self) -> u64 {
        self.tokens_seen
    }

    pub fn abort_reason(&self) -> Option<&str> {
        self.abort_reason.as_deref()
    }

    pub fn on_chunk(
        &mut self,
        text: &str,
        cumulative_tokens: u64,
        elapsed_ms: u64,
        rules: &WatchRules,
    ) -> WatchObservation {
        self.tokens_seen = self.tokens_seen.max(cumulative_tokens);
        self.window.push_str(text);
        let observation = self.check_progress(elapsed_ms, rules);
        while self.window.len() > 512 {
            let mut drop = self.window.len() - 512;
            while drop < self.window.len() && !self.window.is_char_boundary(drop) {
                drop += 1;
            }
            self.window.drain(..drop);
        }
        observation
    }

    pub fn on_done(
        &mut self,
        total_tokens: u64,
        elapsed_ms: u64,
        rules: &WatchRules,
    ) -> WatchObservation {
        self.tokens_seen = self.tokens_seen.max(total_tokens);
        self.check_progress(elapsed_ms, rules)
    }

    pub fn on_elapsed(&mut self, elapsed_ms: u64, rules: &WatchRules) -> WatchObservation {
        self.check_progress(elapsed_ms, rules)
    }

    fn check_progress(&mut self, elapsed_ms: u64, rules: &WatchRules) -> WatchObservation {
        if self.abort_reason.is_none() {
            for pattern in &rules.token_abort {
                if self.window.contains(pattern) {
                    self.abort_reason = Some(format!("token match: {pattern}"));
                    break;
                }
            }
        }
        if self.abort_reason.is_none()
            && let Some(limit) = rules.tokens_abort_gt
            && self.tokens_seen > limit
        {
            self.abort_reason = Some(format!("tokens_consumed > {limit}"));
        }
        if self.abort_reason.is_none()
            && let Some(limit) = rules.elapsed_abort_gt
            && elapsed_ms > limit
        {
            self.abort_reason = Some(format!("elapsed > {limit}ms"));
        }
        let mut observation = WatchObservation {
            abort_reason: self.abort_reason.clone(),
            warnings: Vec::new(),
        };
        for (index, rule) in rules.token_warn.iter().enumerate() {
            if self.window.contains(&rule.pattern) && self.fired_token.insert(index) {
                observation.warnings.push(WatchWarning {
                    target: rule.target.clone(),
                    trigger: format!("token({})", rule.pattern),
                    message: rule.message.clone(),
                });
            }
        }
        for (index, (threshold, rule)) in rules.tokens_warn_gt.iter().enumerate() {
            if self.tokens_seen > *threshold && self.fired_tokens.insert(index) {
                observation.warnings.push(WatchWarning {
                    target: rule.target.clone(),
                    trigger: rule.pattern.clone(),
                    message: rule.message.clone(),
                });
            }
        }
        for (index, (threshold, rule)) in rules.elapsed_warn_gt.iter().enumerate() {
            if elapsed_ms > *threshold && self.fired_elapsed.insert(index) {
                observation.warnings.push(WatchWarning {
                    target: rule.target.clone(),
                    trigger: rule.pattern.clone(),
                    message: rule.message.clone(),
                });
            }
        }
        observation
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;
    use crate::ast::{Ident, OnBlock, Span};

    fn watch(event: WatchEvent, actions: Vec<WatchAction>) -> WatchDecl {
        WatchDecl {
            target: Ident::new("answer", Span::default()),
            on_blocks: vec![OnBlock { event, actions }],
        }
    }

    #[test]
    fn token_abort_and_warn_across_chunks() {
        let decl = watch(
            WatchEvent::Token {
                patterns: vec!["stop".into()],
            },
            vec![
                WatchAction::Abort { msg: None },
                WatchAction::Warn { msg: None },
            ],
        );
        let rules = WatchRules::compile(&[&decl]);
        let mut state = WatchState::default();
        assert!(state.on_chunk("st", 1, 1, &rules).abort_reason.is_none());
        let hit = state.on_chunk("op", 2, 2, &rules);
        assert_eq!(hit.abort_reason.as_deref(), Some("token match: stop"));
        assert_eq!(hit.warnings.len(), 1);
        assert!(state.on_chunk("stop", 3, 3, &rules).warnings.is_empty());
    }

    #[test]
    fn elapsed_abort_uses_portable_elapsed_input() {
        let decl = watch(
            WatchEvent::Elapsed {
                cmp: CmpOp::Gt,
                duration_ms: 100,
            },
            vec![WatchAction::Abort { msg: None }],
        );
        let rules = WatchRules::compile(&[&decl]);
        assert_eq!(rules.elapsed_abort_deadline_ms(), Some(100));
        let mut state = WatchState::default();
        assert!(state.on_elapsed(100, &rules).abort_reason.is_none());
        assert_eq!(
            state.on_elapsed(101, &rules).abort_reason.as_deref(),
            Some("elapsed > 100ms")
        );
    }

    #[test]
    fn large_chunk_keeps_a_trigger_at_its_start() {
        let decl = watch(
            WatchEvent::Token {
                patterns: vec!["danger".into()],
            },
            vec![WatchAction::Abort { msg: None }],
        );
        let rules = WatchRules::compile(&[&decl]);
        let mut state = WatchState::default();
        let chunk = format!("danger{}", "x".repeat(600));
        assert_eq!(
            state.on_chunk(&chunk, 1, 1, &rules).abort_reason.as_deref(),
            Some("token match: danger")
        );
    }

    #[test]
    fn separate_rules_with_the_same_trigger_each_warn_once() {
        let first = watch(
            WatchEvent::Token {
                patterns: vec!["same".into()],
            },
            vec![WatchAction::Warn {
                msg: Some(Expr::Literal(Literal::Str("first".into()))),
            }],
        );
        let second = watch(
            WatchEvent::Token {
                patterns: vec!["same".into()],
            },
            vec![WatchAction::Warn {
                msg: Some(Expr::Literal(Literal::Str("second".into()))),
            }],
        );
        let rules = WatchRules::compile(&[&first, &second]);
        let mut state = WatchState::default();
        let warnings = state.on_chunk("same", 1, 1, &rules).warnings;
        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].message, "first");
        assert_eq!(warnings[1].message, "second");
        assert!(state.on_chunk("same", 2, 2, &rules).warnings.is_empty());
    }
}
