use std::collections::HashMap;

use crate::event::{Event, LlmCallStatus};
use crate::provider::TokenUsage;

#[derive(Debug, Default, Clone)]
pub struct CostSummary {
    pub calls: u64,
    pub failures: u64,
    pub usage: TokenUsage,
    pub wallclock_ms: u64,
}

impl CostSummary {
    fn accumulate(&mut self, usage: &TokenUsage, wallclock_ms: u64, failed: bool) {
        self.calls += 1;
        if failed {
            self.failures += 1;
        }
        self.usage.input = self.usage.input.saturating_add(usage.input);
        self.usage.cached_input = self.usage.cached_input.saturating_add(usage.cached_input);
        self.usage.output = self.usage.output.saturating_add(usage.output);
        self.usage.cache_write = self.usage.cache_write.saturating_add(usage.cache_write);
        self.wallclock_ms = self.wallclock_ms.saturating_add(wallclock_ms);
    }
}

pub fn summarize_by_model(events: &[Event]) -> HashMap<String, CostSummary> {
    let mut out: HashMap<String, CostSummary> = HashMap::new();
    for e in events {
        if let Event::LlmCall {
            model,
            usage,
            wallclock_ms,
            status,
            ..
        } = e
        {
            let entry = out.entry(model.clone()).or_default();
            entry.accumulate(
                usage,
                *wallclock_ms,
                matches!(status, LlmCallStatus::Errored { .. }),
            );
        }
    }
    out
}

pub fn summarize_by_provider(events: &[Event]) -> HashMap<String, CostSummary> {
    let mut out: HashMap<String, CostSummary> = HashMap::new();
    for e in events {
        if let Event::LlmCall {
            provider,
            usage,
            wallclock_ms,
            status,
            ..
        } = e
        {
            let entry = out.entry(provider.clone()).or_default();
            entry.accumulate(
                usage,
                *wallclock_ms,
                matches!(status, LlmCallStatus::Errored { .. }),
            );
        }
    }
    out
}

pub fn total(events: &[Event]) -> CostSummary {
    let mut acc = CostSummary::default();
    for e in events {
        if let Event::LlmCall {
            usage,
            wallclock_ms,
            status,
            ..
        } = e
        {
            acc.accumulate(
                usage,
                *wallclock_ms,
                matches!(status, LlmCallStatus::Errored { .. }),
            );
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_call(model: &str, provider: &str, in_tok: u64, out_tok: u64) -> Event {
        Event::LlmCall {
            seq: 0,
            model: model.into(),
            provider: provider.into(),
            usage: TokenUsage {
                input: in_tok,
                output: out_tok,
                ..Default::default()
            },
            wallclock_ms: 100,
            status: LlmCallStatus::Ok,
            ts: chrono::Utc::now(),
        }
    }

    fn err_call(model: &str) -> Event {
        Event::LlmCall {
            seq: 0,
            model: model.into(),
            provider: "p".into(),
            usage: TokenUsage {
                input: 5,
                ..Default::default()
            },
            wallclock_ms: 50,
            status: LlmCallStatus::Errored {
                message: "boom".into(),
            },
            ts: chrono::Utc::now(),
        }
    }

    #[test]
    fn summarize_by_model_groups_multiple_calls() {
        let events = vec![
            ok_call("opus", "anthropic", 10, 20),
            ok_call("opus", "anthropic", 5, 8),
            ok_call("mini", "openai", 3, 4),
        ];
        let s = summarize_by_model(&events);
        assert_eq!(s.get("opus").unwrap().calls, 2);
        assert_eq!(s.get("opus").unwrap().usage.input, 15);
        assert_eq!(s.get("opus").unwrap().usage.output, 28);
        assert_eq!(s.get("mini").unwrap().calls, 1);
    }

    #[test]
    fn failures_counted_separately() {
        let events = vec![ok_call("m", "p", 1, 2), err_call("m"), err_call("m")];
        let s = summarize_by_model(&events);
        assert_eq!(s.get("m").unwrap().calls, 3);
        assert_eq!(s.get("m").unwrap().failures, 2);
    }

    #[test]
    fn total_sums_wallclock() {
        let events = vec![ok_call("a", "p", 1, 1), ok_call("b", "p", 2, 2)];
        let t = total(&events);
        assert_eq!(t.calls, 2);
        assert_eq!(t.wallclock_ms, 200);
    }

    #[test]
    fn non_llm_events_ignored() {
        use crate::event::{FlowRunId, FlowStatus};
        let run_id = FlowRunId::now();
        let events = vec![
            Event::FlowStart {
                seq: 0,
                run_id: run_id.clone(),
                flow_name: "t".into(),
                parent_run_id: None,
                parent_node_id: None,
                ts: chrono::Utc::now(),
            },
            ok_call("m", "p", 5, 5),
            Event::FlowEnd {
                seq: 0,
                run_id,
                flow_name: "t".into(),
                status: FlowStatus::Ok,
                ts: chrono::Utc::now(),
            },
        ];
        let t = total(&events);
        assert_eq!(t.calls, 1);
        assert_eq!(t.usage.input, 5);
    }
}
