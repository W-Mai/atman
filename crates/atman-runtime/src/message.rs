use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::event::TurnId;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Message {
    pub role: MessageRole,
    pub parts: Vec<MessagePart>,
    pub turn_id: TurnId,
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawMessage {
            role: MessageRole,
            parts: Vec<MessagePart>,
            turn_id: TurnId,
        }

        let raw = RawMessage::deserialize(deserializer)?;
        let RawMessage {
            role,
            parts,
            turn_id,
        } = raw;
        Ok(Self {
            role,
            parts: normalize_legacy_compact_summary(role, parts),
            turn_id,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    CompactSummary {
        summary: String,
        seq_start: u64,
        seq_end: u64,
        count: usize,
    },
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "core::ops::Not::not")]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageSource {
    pub media_type: String,
    pub data: ImageData,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageData {
    Base64 { data: String },
    Path { path: PathBuf },
}

impl Message {
    pub fn user_text(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            parts: vec![MessagePart::Text { text: text.into() }],
            turn_id,
        }
    }

    pub fn assistant_text(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text { text: text.into() }],
            turn_id,
        }
    }

    pub fn system_text(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            parts: vec![MessagePart::Text { text: text.into() }],
            turn_id,
        }
    }

    pub fn system_compact_summary(
        turn_id: TurnId,
        summary: impl Into<String>,
        seq_start: u64,
        seq_end: u64,
        count: usize,
    ) -> Self {
        Self {
            role: MessageRole::System,
            parts: vec![MessagePart::CompactSummary {
                summary: summary.into(),
                seq_start,
                seq_end,
                count,
            }],
            turn_id,
        }
    }

    pub fn text_concat(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                MessagePart::Text { text } => out.push_str(text),
                MessagePart::CompactSummary { summary, .. } => out.push_str(summary),
                _ => {}
            }
        }
        out
    }

    pub fn thinking_concat(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            if let MessagePart::Thinking { thinking, .. } = p {
                out.push_str(thinking);
            }
        }
        out
    }

    pub fn thinking_signature(&self) -> Option<String> {
        self.parts.iter().rev().find_map(|p| {
            if let MessagePart::Thinking { signature, .. } = p {
                signature.clone()
            } else {
                None
            }
        })
    }
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
            MessageRole::Tool => "tool",
        }
    }
}

fn normalize_legacy_compact_summary(
    role: MessageRole,
    parts: Vec<MessagePart>,
) -> Vec<MessagePart> {
    if role != MessageRole::System {
        return parts;
    }
    if parts.len() != 1 {
        return parts;
    }
    let MessagePart::Text { text } = &parts[0] else {
        return parts;
    };
    let Some((summary, seq_start, seq_end, count)) = parse_legacy_compact_summary_text(text) else {
        return parts;
    };
    vec![MessagePart::CompactSummary {
        summary,
        seq_start,
        seq_end,
        count,
    }]
}

pub(crate) fn parse_legacy_compact_summary_text(text: &str) -> Option<(String, u64, u64, usize)> {
    let start_marker = "[atman:compact ";
    let start = text.rfind(start_marker)?;
    let after = &text[start + start_marker.len()..];
    let end = after.find(']')?;
    let inner = &after[..end];
    let mut seq_start = None;
    let mut seq_end = None;
    let mut count = None;
    for token in inner.split_whitespace() {
        let Some((k, v)) = token.split_once('=') else {
            continue;
        };
        match k {
            "seq_start" => seq_start = v.parse().ok(),
            "seq_end" => seq_end = v.parse().ok(),
            "count" => count = v.parse().ok(),
            _ => {}
        }
    }
    let summary = text[..start].trim_end().to_string();
    Some((summary, seq_start?, seq_end?, count?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_roundtrips_via_serde_json() {
        let msg = Message::user_text(TurnId::now(), "hello");
        let s = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn legacy_compact_summary_deserializes_to_structured_variant() {
        let turn_id = TurnId::now();
        let msg = Message {
            role: MessageRole::System,
            parts: vec![MessagePart::Text {
                text: "handoff\n\n[atman:compact seq_start=2 seq_end=7 count=6]".into(),
            }],
            turn_id,
        };
        let s = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back.parts.as_slice(),
            [MessagePart::CompactSummary { .. }]
        ));
        assert_eq!(back.text_concat(), "handoff");
    }

    #[test]
    fn text_concat_skips_non_text_parts() {
        let msg = Message {
            role: MessageRole::User,
            parts: vec![
                MessagePart::Text { text: "a ".into() },
                MessagePart::Image {
                    source: ImageSource {
                        media_type: "image/png".into(),
                        data: ImageData::Path {
                            path: PathBuf::from("/tmp/x.png"),
                        },
                    },
                },
                MessagePart::Text { text: "b".into() },
            ],
            turn_id: TurnId::now(),
        };
        assert_eq!(msg.text_concat(), "a b");
    }

    #[test]
    fn tool_result_is_error_defaults_to_false_and_skips_serialize_when_false() {
        let msg = Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "ok".into(),
                is_error: false,
            }],
            turn_id: TurnId::now(),
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(!s.contains("is_error"), "should skip when false: {s}");

        let err_msg = Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "nope".into(),
                is_error: true,
            }],
            turn_id: TurnId::now(),
        };
        let s = serde_json::to_string(&err_msg).unwrap();
        assert!(s.contains("\"is_error\":true"), "{s}");
    }

    #[test]
    fn role_as_str_matches_wire_format() {
        assert_eq!(MessageRole::User.as_str(), "user");
        assert_eq!(MessageRole::Assistant.as_str(), "assistant");
        assert_eq!(MessageRole::System.as_str(), "system");
        assert_eq!(MessageRole::Tool.as_str(), "tool");
    }
}
