use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use atman_dsl::parse::parse_file;
use atman_runtime::provider::{LlmRequest, Provider, user_text_message};
use atman_runtime::value::Value;

const DEFAULT_RECENT_TURNS: usize = 30;
const META_PROMPT_HEADER: &str = "\
You are a meta-agent watching a REPL session. Given the recent turn log, decide \
whether the human has been repeating a reusable pattern (e.g. always calling the \
same sequence of tools, always asking the same shape of question). If yes, extract \
a single reusable atman DSL flow.

Reply with ONE fenced code block tagged `atman` containing a valid `flow` declaration. \
If no reusable pattern is present, reply with EXACTLY: NO_SUGGESTION

Rules for the flow:
- Must be a single top-level `flow <name>(...) { ... }` declaration.
- `<name>` must be snake_case ASCII.
- Only use tools that already appear in the recent turns.
- Keep it small; capture the shared skeleton, not one specific run.
- Do NOT include prose outside the code fence.

Recent turns (most recent last):
";

pub fn build_meta_prompt(recent: &str) -> String {
    let mut out = String::from(META_PROMPT_HEADER);
    if recent.trim().is_empty() {
        out.push_str("(no recent turns)\n");
    } else {
        out.push_str(recent);
        if !recent.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

pub fn extract_code_block(text: &str) -> Option<String> {
    let accept = |tag: &str| matches!(tag, "" | "atman" | "at" | "dsl" | "rust");
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("```") else {
            continue;
        };
        let tag = rest.trim();
        let mut body = String::new();
        let mut found_close = false;
        for inner in lines.by_ref() {
            if inner.trim_start().starts_with("```") {
                found_close = true;
                break;
            }
            body.push_str(inner);
            body.push('\n');
        }
        if !found_close {
            return None;
        }
        if accept(tag) {
            let trimmed_body = body.trim_matches('\n');
            if trimmed_body.is_empty() {
                continue;
            }
            return Some(trimmed_body.to_string());
        }
    }
    None
}

pub fn extract_flow_name(dsl_src: &str) -> Result<String> {
    let file = parse_file(dsl_src).map_err(|e| anyhow::anyhow!("parse suggested flow: {e}"))?;
    if file.flows.len() != 1 {
        bail!(
            "suggested source must contain exactly one flow (got {})",
            file.flows.len()
        );
    }
    let name = file.flows[0].name.name.clone();
    if !is_snake_case(&name) {
        bail!("suggested flow name `{name}` must be snake_case ASCII");
    }
    Ok(name)
}

fn is_snake_case(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && s.chars().next().map(|c| c.is_ascii_lowercase()) == Some(true)
}

pub fn read_recent_events(path: &Path, max_turns: usize) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let contents = std::fs::read_to_string(path)?;
    let mut turns: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();

    for line in contents.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ty = v["type"].as_str().unwrap_or("");
        match ty {
            "turn_start" => {
                if !current.is_empty() {
                    turns.push(std::mem::take(&mut current));
                }
            }
            "user_msg" => {
                if let Some(t) = extract_text(&v) {
                    current.push(format!("user: {t}"));
                }
            }
            "assistant_msg" => {
                if let Some(t) = extract_text(&v) {
                    current.push(format!("assistant: {t}"));
                }
            }
            "tool_result_msg" => {
                if let Some(t) = extract_text(&v) {
                    let clipped = clip(&t, 200);
                    current.push(format!("tool_result: {clipped}"));
                }
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        turns.push(current);
    }

    let start = turns.len().saturating_sub(max_turns.max(1));
    let mut out = String::new();
    for (idx, turn) in turns[start..].iter().enumerate() {
        out.push_str(&format!("--- turn {} ---\n", start + idx + 1));
        for line in turn {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

fn extract_text(v: &serde_json::Value) -> Option<String> {
    let parts = v["message"]["parts"].as_array()?;
    let mut buf = String::new();
    for p in parts {
        if let Some(t) = p["text"].as_str() {
            if !buf.is_empty() {
                buf.push(' ');
            }
            buf.push_str(t);
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

pub async fn generate_suggestion(
    provider: Arc<dyn Provider>,
    model: &str,
    recent_transcript: &str,
    token_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> Result<String> {
    let prompt = build_meta_prompt(recent_transcript);
    let req = LlmRequest {
        model: model.to_string(),
        messages: vec![user_text_message(prompt)],
        system: None,
        input: Value::Unit,
        schema: None,
        cache_prompt: false,
        tools: Vec::new(),
        thinking_enabled: false,
    };
    if let Some(tx) = token_tx {
        let obs = provider.call_streaming(req);
        let mut events = obs.events;
        let output = obs.output;
        let stream_handle = tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if let atman_runtime::event::NodeEvent::LlmChunk { text, .. } = event {
                    let _ = tx.send(text);
                }
            }
        });
        let reply = output
            .await
            .map_err(|e| anyhow::anyhow!("meta-llm call: {e}"))?;
        stream_handle.abort();
        Ok(reply.message.text_concat())
    } else {
        let reply = provider
            .call(req)
            .await
            .map_err(|e| anyhow::anyhow!("meta-llm call: {e}"))?;
        Ok(reply.message.text_concat())
    }
}

pub fn recent_turns_limit() -> usize {
    DEFAULT_RECENT_TURNS
}

pub fn route_line(flow_name: &str, trigger: &str) -> String {
    format!("route \"{trigger}\" {{ flow: {flow_name} }}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::providers::mock::MockProvider;

    #[test]
    fn build_meta_prompt_includes_recent() {
        let p = build_meta_prompt("--- turn 1 ---\nuser: hi\n");
        assert!(p.contains("Recent turns"));
        assert!(p.contains("user: hi"));
    }

    #[test]
    fn build_meta_prompt_empty_transcript() {
        let p = build_meta_prompt("");
        assert!(p.contains("(no recent turns)"));
    }

    #[test]
    fn extract_code_block_tagged_atman() {
        let text = "prose\n```atman\nflow foo() { }\n```\ntrailing";
        let body = extract_code_block(text).expect("has block");
        assert_eq!(body, "flow foo() { }");
    }

    #[test]
    fn extract_code_block_untagged() {
        let text = "```\nflow bar() { }\n```";
        let body = extract_code_block(text).expect("has block");
        assert_eq!(body, "flow bar() { }");
    }

    #[test]
    fn extract_code_block_wrong_tag_skipped() {
        let text = "```python\nprint('hi')\n```\n```at\nflow ok() { }\n```";
        let body = extract_code_block(text).expect("finds atman block");
        assert_eq!(body, "flow ok() { }");
    }

    #[test]
    fn extract_code_block_missing_returns_none() {
        assert!(extract_code_block("just prose no fence").is_none());
    }

    #[test]
    fn extract_flow_name_ok() {
        let src = "flow my_flow() { return 1 }";
        let name = extract_flow_name(src).expect("parses");
        assert_eq!(name, "my_flow");
    }

    #[test]
    fn extract_flow_name_rejects_camelcase() {
        let src = "flow MyFlow() { return 1 }";
        let err = extract_flow_name(src).unwrap_err();
        assert!(err.to_string().contains("snake_case"));
    }

    #[test]
    fn extract_flow_name_rejects_zero_or_multiple() {
        let two = "flow a() { return 1 } flow b() { return 2 }";
        let err = extract_flow_name(two).unwrap_err();
        assert!(err.to_string().contains("exactly one flow"));
    }

    #[test]
    fn route_line_format() {
        assert_eq!(
            route_line("summarize", "sum "),
            "route \"sum \" { flow: summarize }\n"
        );
    }

    #[tokio::test]
    async fn generate_suggestion_via_mock_provider() {
        let canned = "```atman\nflow greet(name: str) { return \"hi \" + name }\n```";
        let provider =
            Arc::new(MockProvider::new("mock").with_model("mini", Value::Str(canned.into())));
        let reply = generate_suggestion(provider, "mini", "user: greet w-mai\n", None)
            .await
            .expect("mock succeeds");
        let body = extract_code_block(&reply).expect("has block");
        assert!(body.starts_with("flow greet"));
        assert_eq!(extract_flow_name(&body).unwrap(), "greet");
    }

    #[test]
    fn read_recent_events_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.jsonl");
        let out = read_recent_events(&path, 10).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn read_recent_events_projects_turn_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let lines = [
            r#"{"type":"turn_start","turn_id":{"raw":"t1"}}"#,
            r#"{"type":"user_msg","message":{"parts":[{"text":"hello"}]}}"#,
            r#"{"type":"assistant_msg","message":{"parts":[{"text":"world"}]}}"#,
            r#"{"type":"turn_start","turn_id":{"raw":"t2"}}"#,
            r#"{"type":"user_msg","message":{"parts":[{"text":"again"}]}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let out = read_recent_events(&path, 10).unwrap();
        assert!(out.contains("--- turn 1 ---"));
        assert!(out.contains("user: hello"));
        assert!(out.contains("assistant: world"));
        assert!(out.contains("--- turn 2 ---"));
        assert!(out.contains("user: again"));
    }
}
