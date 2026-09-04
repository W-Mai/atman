use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::event::TurnId;

/// Persistent identity of a message part, independent of its position or payload.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct MessagePartId(pub uuid::Uuid);

/// An attachment address uses either a stable identity or a legacy event position.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum AttachmentTarget {
    Part { part_id: MessagePartId },
    Legacy { message_seq: u64, part_index: usize },
}

impl<'de> Deserialize<'de> for AttachmentTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut fields = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let part_id = fields.remove("part_id");
        let message_seq = fields.remove("message_seq");
        let part_index = fields.remove("part_index");
        match (part_id, message_seq, part_index) {
            (Some(id), None, None) => Ok(Self::Part {
                part_id: serde_json::from_value(id).map_err(serde::de::Error::custom)?,
            }),
            (None, Some(seq), Some(index)) => Ok(Self::Legacy {
                message_seq: serde_json::from_value(seq).map_err(serde::de::Error::custom)?,
                part_index: serde_json::from_value(index).map_err(serde::de::Error::custom)?,
            }),
            _ => Err(serde::de::Error::custom(
                "attachment target requires exactly one complete address",
            )),
        }
    }
}

impl AttachmentTarget {
    pub fn matches(self, id: Option<MessagePartId>, message_seq: u64, part_index: usize) -> bool {
        match self {
            Self::Part { part_id } => id == Some(part_id),
            Self::Legacy {
                message_seq: seq,
                part_index: index,
            } => seq == message_seq && index == part_index,
        }
    }
}

/// An image-only replacement applied inside one already-selected context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentPatch {
    #[serde(flatten)]
    pub target: AttachmentTarget,
    pub file_basename: String,
    pub reason: String,
}

impl AttachmentPatch {
    pub fn text(&self) -> String {
        format!(
            "[attachment unavailable: {} — {}]",
            self.file_basename, self.reason
        )
    }

    pub fn apply(&self, message_seq: u64, message: &mut Message) -> bool {
        let mut changed = false;
        for (index, part) in message.parts.iter_mut().enumerate() {
            if let MessagePart::Image { id, .. } = part
                && self.target.matches(*id, message_seq, index)
            {
                *part = MessagePart::Text { text: self.text() };
                changed = true;
            }
        }
        changed
    }
}

pub const TOOL_CALL_INTENT_FIELD: &str = "_atman_intent";
pub const TOOL_CALL_INTENT_MAX_CHARS: usize = 120;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct ToolCallIntent(String);

impl ToolCallIntent {
    pub fn new(value: impl AsRef<str>) -> Option<Self> {
        let normalized = value
            .as_ref()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(TOOL_CALL_INTENT_MAX_CHARS)
            .collect::<String>();
        (!normalized.is_empty()).then_some(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ToolCallIntent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("tool call intent is empty"))
    }
}

pub fn decode_tool_call_input(
    wire_input: serde_json::Value,
    tool_name: &str,
    tools: &[crate::tool::ToolSpec],
) -> (serde_json::Value, Option<ToolCallIntent>) {
    if !crate::tool::tool_spec_supports_call_intent(tool_name, tools) {
        return (wire_input, None);
    }
    let serde_json::Value::Object(mut input) = wire_input else {
        return (wire_input, None);
    };
    let intent = input
        .remove(TOOL_CALL_INTENT_FIELD)
        .and_then(|value| value.as_str().and_then(ToolCallIntent::new));
    (serde_json::Value::Object(input), intent)
}

pub fn encode_tool_call_input(
    clean_input: &serde_json::Value,
    intent: Option<&ToolCallIntent>,
    tool_name: &str,
    tools: &[crate::tool::ToolSpec],
) -> serde_json::Value {
    let Some(intent) = intent else {
        return clean_input.clone();
    };
    if crate::tool::tool_spec_blocks_call_intent(tool_name, tools) {
        return clean_input.clone();
    }
    let serde_json::Value::Object(mut input) = clean_input.clone() else {
        return clean_input.clone();
    };
    input.insert(
        TOOL_CALL_INTENT_FIELD.into(),
        serde_json::Value::String(intent.as_str().into()),
    );
    serde_json::Value::Object(input)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    #[default]
    User,
    Watcher,
    Interjection,
    Internal,
}

fn is_default_origin(origin: &MessageOrigin) -> bool {
    matches!(origin, MessageOrigin::User)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Message {
    pub role: MessageRole,
    pub parts: Vec<MessagePart>,
    pub turn_id: TurnId,
    #[serde(default, skip_serializing_if = "is_default_origin")]
    pub origin: MessageOrigin,
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
            #[serde(default)]
            origin: MessageOrigin,
        }

        let raw = RawMessage::deserialize(deserializer)?;
        let RawMessage {
            role,
            parts,
            turn_id,
            origin,
        } = raw;
        Ok(Self {
            role,
            parts: normalize_legacy_compact_summary(role, parts),
            turn_id,
            origin,
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
    ContextRecord(crate::context_plan::ContextRecord),
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<MessagePartId>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        intent: Option<ToolCallIntent>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "core::ops::Not::not")]
        is_error: bool,
    },
}

impl Message {
    pub(crate) fn ensure_part_ids(&mut self) {
        for part in &mut self.parts {
            if let MessagePart::Image { id, .. } = part {
                id.get_or_insert_with(|| MessagePartId(uuid::Uuid::now_v7()));
            }
        }
    }

    pub(crate) fn replayed(&self, seq: u64, checkpoint_index: Option<usize>) -> Self {
        let mut message = self.clone();
        for (index, part) in message.parts.iter_mut().enumerate() {
            if let MessagePart::Image { id, .. } = part {
                *id = self.part_id(seq, checkpoint_index, index);
            }
        }
        message
    }

    /// Returns the stored image identity or derives its legacy replay identity without copying image data.
    pub fn part_id(
        &self,
        seq: u64,
        checkpoint_index: Option<usize>,
        index: usize,
    ) -> Option<MessagePartId> {
        let MessagePart::Image { id, .. } = self.parts.get(index)? else {
            return None;
        };
        Some(id.unwrap_or_else(|| {
            let mut hash = blake3::Hasher::new();
            hash.update(b"atman.message-part.v1");
            hash.update(self.turn_id.0.as_bytes());
            hash.update(&seq.to_le_bytes());
            hash.update(
                &checkpoint_index
                    .map_or(u64::MAX, |i| i as u64)
                    .to_le_bytes(),
            );
            hash.update(&(index as u64).to_le_bytes());
            let mut bytes = [0; 16];
            bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
            MessagePartId(uuid::Uuid::from_bytes(bytes))
        }))
    }

    /// Borrows model-facing content without storage-only part identities.
    pub(crate) fn content(&self) -> impl Serialize + '_ {
        #[derive(Serialize)]
        struct Content<'a> {
            role: MessageRole,
            parts: ContentParts<'a>,
            turn_id: &'a TurnId,
            #[serde(skip_serializing_if = "is_default_origin")]
            origin: MessageOrigin,
        }
        Content {
            role: self.role,
            parts: ContentParts(&self.parts),
            turn_id: &self.turn_id,
            origin: self.origin,
        }
    }
}

struct ContentParts<'a>(&'a [MessagePart]);

impl Serialize for ContentParts<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum ImageContent<'a> {
            Image { source: &'a ImageSource },
        }
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for part in self.0 {
            match part {
                MessagePart::Image { source, .. } => {
                    seq.serialize_element(&ImageContent::Image { source })?;
                }
                _ => seq.serialize_element(part)?,
            }
        }
        seq.end()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageSource {
    pub media_type: String,
    pub data: ImageData,
    #[serde(default, skip_serializing_if = "is_auto_image_detail")]
    pub detail: crate::provider::ImageDetail,
}

fn is_auto_image_detail(detail: &crate::provider::ImageDetail) -> bool {
    matches!(detail, crate::provider::ImageDetail::Auto)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageData {
    Base64 {
        data: String,
    },
    Path {
        path: PathBuf,
    },
    Artifact {
        id: String,
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl Message {
    pub fn context_record(turn_id: TurnId, record: crate::context_plan::ContextRecord) -> Self {
        Self {
            role: MessageRole::System,
            parts: vec![MessagePart::ContextRecord(record)],
            turn_id,
            origin: MessageOrigin::Internal,
        }
    }

    pub fn user_text(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            parts: vec![MessagePart::Text { text: text.into() }],
            turn_id,
            origin: MessageOrigin::User,
        }
    }

    pub fn assistant_text(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text { text: text.into() }],
            turn_id,
            origin: MessageOrigin::User,
        }
    }

    pub fn system_text(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            parts: vec![MessagePart::Text { text: text.into() }],
            turn_id,
            origin: MessageOrigin::User,
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
            origin: MessageOrigin::User,
        }
    }

    pub fn text_concat(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                MessagePart::ContextRecord(record) => out.push_str(&record.render_for_model()),
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

    pub fn contains_context_record(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, MessagePart::ContextRecord(_)))
    }
}

pub fn retain_complete_tool_pairs(messages: &mut Vec<Message>) {
    let use_ids: std::collections::HashSet<String> = messages
        .iter()
        .flat_map(|message| {
            message.parts.iter().filter_map(|part| match part {
                MessagePart::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
        })
        .collect();
    let result_ids: std::collections::HashSet<String> = messages
        .iter()
        .flat_map(|message| {
            message.parts.iter().filter_map(|part| match part {
                MessagePart::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
        })
        .collect();
    let mut seen_uses = std::collections::HashSet::new();
    let mut seen_results = std::collections::HashSet::new();
    for message in messages.iter_mut() {
        message.parts.retain(|part| match part {
            MessagePart::ToolUse { id, .. } => {
                result_ids.contains(id) && seen_uses.insert(id.clone())
            }
            MessagePart::ToolResult { tool_use_id, .. } => {
                use_ids.contains(tool_use_id) && seen_results.insert(tool_use_id.clone())
            }
            _ => true,
        });
    }
    messages.retain(|message| !message.parts.is_empty());
}

pub fn normalize_tool_pairs_for_model(messages: &[Message]) -> Vec<Message> {
    #[derive(Clone)]
    struct ToolResultRecord {
        part: MessagePart,
        turn_id: TurnId,
        origin: MessageOrigin,
    }

    let mut results = std::collections::HashMap::new();
    for message in messages {
        for part in &message.parts {
            if let MessagePart::ToolResult { tool_use_id, .. } = part {
                results
                    .entry(tool_use_id.clone())
                    .or_insert_with(|| ToolResultRecord {
                        part: part.clone(),
                        turn_id: message.turn_id.clone(),
                        origin: message.origin,
                    });
            }
        }
    }

    let mut normalized = Vec::with_capacity(messages.len() + 4);
    for message in messages {
        let tool_use_ids: Vec<String> = message
            .parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        let mut projected = message.clone();
        projected
            .parts
            .retain(|part| !matches!(part, MessagePart::ToolResult { .. }));
        if !projected.parts.is_empty() {
            if projected.role == MessageRole::Tool {
                projected.role = MessageRole::User;
            }
            normalized.push(projected);
        }

        for tool_use_id in tool_use_ids {
            let result = results.get(&tool_use_id);
            normalized.push(Message {
                role: MessageRole::Tool,
                parts: vec![result.map_or_else(
                    || MessagePart::ToolResult {
                        tool_use_id,
                        content: "[tool execution interrupted — no result captured]".into(),
                        is_error: true,
                    },
                    |result| result.part.clone(),
                )],
                turn_id: result
                    .map(|result| result.turn_id.clone())
                    .unwrap_or_else(|| message.turn_id.clone()),
                origin: result.map_or(message.origin, |result| result.origin),
            });
        }
    }
    normalized
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
    fn attachment_patches_only_replace_matching_images_and_are_idempotent() {
        let id = MessagePartId(uuid::Uuid::now_v7());
        let image = MessagePart::Image {
            id: Some(id),
            source: ImageSource {
                media_type: "image/png".into(),
                data: ImageData::Base64 {
                    data: "AA==".into(),
                },
                detail: Default::default(),
            },
        };
        let mut original = Message::user_text(TurnId::now(), "keep text");
        original.parts.extend([image.clone(), image]);
        for (target, affected) in [
            (AttachmentTarget::Part { part_id: id }, 2),
            (
                AttachmentTarget::Legacy {
                    message_seq: 5,
                    part_index: 1,
                },
                1,
            ),
            (
                AttachmentTarget::Legacy {
                    message_seq: 5,
                    part_index: 0,
                },
                0,
            ),
            (
                AttachmentTarget::Legacy {
                    message_seq: 6,
                    part_index: 1,
                },
                0,
            ),
            (
                AttachmentTarget::Part {
                    part_id: MessagePartId(uuid::Uuid::now_v7()),
                },
                0,
            ),
        ] {
            let mut message = original.clone();
            let mut patch = AttachmentPatch {
                target,
                file_basename: "image.png".into(),
                reason: "missing".into(),
            };
            assert_eq!(patch.apply(5, &mut message), affected > 0);
            assert_eq!(message.parts[0], original.parts[0]);
            assert_eq!(
                message
                    .parts
                    .iter()
                    .filter(|part| matches!(part, MessagePart::Image { .. }))
                    .count(),
                2 - affected
            );
            let applied = message.clone();
            patch.reason = "a later error".into();
            assert!(!patch.apply(5, &mut message));
            assert_eq!(message, applied);
        }
    }

    #[test]
    fn image_part_identity_survives_copy_reordering_and_replay() {
        let image = serde_json::json!({
            "type": "image",
            "source": {"media_type": "image/png", "data": {"kind": "base64", "data": "iVBORw0KGgo="}}
        });
        let mut message: Message = serde_json::from_value(serde_json::json!({
            "role": "user", "turn_id": TurnId::now(), "parts": [image.clone(), image]
        }))
        .unwrap();
        let legacy = message.clone();
        let restored = legacy.replayed(10, None);
        assert_eq!(restored, legacy.replayed(10, None));
        assert_ne!(restored, legacy.replayed(11, None));
        assert_ne!(restored.parts[0], restored.parts[1]);
        assert_ne!(restored, legacy.replayed(10, Some(0)));
        assert_ne!(legacy.replayed(10, Some(0)), legacy.replayed(10, Some(1)));

        message.ensure_part_ids();
        assert_ne!(message.parts[0], message.parts[1]);
        let anchored = message.clone();
        message.ensure_part_ids();
        assert_eq!(message, anchored);
        assert_eq!(message, message.replayed(99, Some(7)));
        message.parts.remove(0);
        assert_eq!(message.parts[0], anchored.parts[1]);
        let persisted = serde_json::to_vec(&message).unwrap();
        assert_eq!(
            message,
            serde_json::from_slice::<Message>(&persisted).unwrap()
        );
        let mut invalid = serde_json::to_value(&message).unwrap();
        invalid["parts"][0]["id"] = serde_json::json!("invalid");
        assert!(serde_json::from_value::<Message>(invalid).is_err());
    }

    #[test]
    fn storage_part_ids_do_not_change_model_content_prefixes_or_epochs() {
        use crate::context_plan::ContextPrefixSnapshot;
        use crate::provider::{LlmRequest, Provider};
        let message: Message = serde_json::from_value(serde_json::json!({
            "role": "user", "turn_id": TurnId::now(), "parts": [
                {"type": "text", "text": "inspect"},
                {"type": "image", "source": {"media_type": "image/png", "data": {"kind": "base64", "data": "iVBORw0KGgo="}}}
            ]
        })).unwrap();
        let mut anchored = message.clone();
        anchored.ensure_part_ids();
        assert_ne!(
            serde_json::to_value(&message).unwrap(),
            serde_json::to_value(&anchored).unwrap()
        );
        assert_eq!(
            serde_json::to_vec(&message).unwrap(),
            serde_json::to_vec(&anchored.content()).unwrap()
        );
        assert_eq!(
            crate::context_state::checkpoint_epoch_digest(std::slice::from_ref(&message)),
            crate::context_state::checkpoint_epoch_digest(std::slice::from_ref(&anchored))
        );
        let mut request = LlmRequest {
            model: "test-model".into(),
            messages: vec![message],
            system: None,
            input: crate::Value::Unit,
            schema: None,
            cache_prompt: true,
            prompt_cache_key: None,
            tools: Vec::new(),
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            stall_timeout_secs: 0,
        };
        let providers: Vec<Box<dyn Provider>> = vec![
            Box::new(crate::providers::openai::OpenAiProvider::new(
                "openai", "key",
            )),
            Box::new(crate::providers::anthropic::AnthropicProvider::new(
                "anthropic",
                "key",
            )),
            Box::new(crate::providers::codex::CodexProvider::new(
                "codex", "key", "account",
            )),
        ];
        let before = providers
            .iter()
            .map(|p| p.context_prefix(&request).unwrap())
            .collect::<Vec<_>>();
        let neutral = ContextPrefixSnapshot::provider_neutral(&request).unwrap();
        request.messages = vec![anchored];
        assert_eq!(
            neutral,
            ContextPrefixSnapshot::provider_neutral(&request).unwrap()
        );
        for (provider, expected) in providers.iter().zip(before) {
            assert_eq!(
                expected,
                provider.context_prefix(&request).unwrap(),
                "{}",
                provider.name()
            );
        }
    }

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
            origin: MessageOrigin::User,
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
                    id: None,
                    source: ImageSource {
                        media_type: "image/png".into(),
                        data: ImageData::Path {
                            path: PathBuf::from("/tmp/x.png"),
                        },
                        detail: crate::provider::ImageDetail::Auto,
                    },
                },
                MessagePart::Text { text: "b".into() },
            ],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
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
            origin: MessageOrigin::User,
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
            origin: MessageOrigin::User,
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

    #[test]
    fn default_origin_is_user() {
        assert_eq!(MessageOrigin::default(), MessageOrigin::User);
    }

    #[test]
    fn user_origin_skipped_in_json() {
        let msg = Message::user_text(TurnId::now(), "hi");
        let s = serde_json::to_string(&msg).unwrap();
        assert!(
            !s.contains("origin"),
            "default origin should not be serialized: {s}"
        );
    }

    #[test]
    fn watcher_origin_serialized() {
        let mut msg = Message::user_text(TurnId::now(), "watcher event");
        msg.origin = MessageOrigin::Watcher;
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"origin\":\"watcher\""), "{s}");
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(back.origin, MessageOrigin::Watcher);
    }

    #[test]
    fn old_json_without_origin_defaults_to_user() {
        let json = r#"{"role":"user","parts":[{"type":"text","text":"legacy"}],"turn_id":"019f0000-0000-7000-0000-000000000001"}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert_eq!(msg.origin, MessageOrigin::User);
        assert_eq!(msg.text_concat(), "legacy");
    }

    #[test]
    fn context_record_message_round_trips_as_internal_system_context() {
        let message = Message::context_record(
            TurnId::now(),
            crate::context_plan::ContextRecord::new(
                "session.workspace",
                1,
                crate::context_plan::ContextRecordAuthority::Runtime,
                crate::context_plan::ContextRecordRetention::Latest,
                crate::context_plan::ContextRecordBody::text("/workspace"),
            ),
        );
        let encoded = serde_json::to_string(&message).unwrap();
        let decoded: Message = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, message);
        assert_eq!(decoded.role, MessageRole::System);
        assert_eq!(decoded.origin, MessageOrigin::Internal);
        assert!(decoded.text_concat().contains("/workspace"));
    }

    #[test]
    fn tool_call_intent_normalizes_whitespace_and_caps_unicode_chars() {
        let raw = format!("  inspect\n\t{}  ", "界".repeat(200));
        let intent = ToolCallIntent::new(raw).unwrap();
        assert_eq!(intent.as_str().chars().count(), TOOL_CALL_INTENT_MAX_CHARS);
        assert!(intent.as_str().starts_with("inspect 界"));
        assert!(!intent.as_str().contains('\n'));
    }

    #[test]
    fn legacy_tool_use_defaults_intent_to_none() {
        let part: MessagePart = serde_json::from_value(serde_json::json!({
            "type": "tool_use",
            "id": "call-1",
            "name": "probe",
            "input": {"value": 1}
        }))
        .unwrap();
        assert!(matches!(part, MessagePart::ToolUse { intent: None, .. }));
    }

    #[test]
    fn model_normalization_splits_parallel_results_in_call_order() {
        let turn = TurnId::now();
        let messages = vec![
            Message {
                role: MessageRole::Assistant,
                parts: vec![
                    MessagePart::ToolUse {
                        id: "call-b".into(),
                        name: "probe".into(),
                        input: serde_json::json!({}),
                        intent: None,
                    },
                    MessagePart::ToolUse {
                        id: "call-a".into(),
                        name: "probe".into(),
                        input: serde_json::json!({}),
                        intent: None,
                    },
                ],
                turn_id: turn.clone(),
                origin: MessageOrigin::User,
            },
            Message {
                role: MessageRole::Tool,
                parts: vec![
                    MessagePart::ToolResult {
                        tool_use_id: "call-a".into(),
                        content: "A".into(),
                        is_error: false,
                    },
                    MessagePart::ToolResult {
                        tool_use_id: "call-b".into(),
                        content: "B".into(),
                        is_error: false,
                    },
                ],
                turn_id: turn,
                origin: MessageOrigin::User,
            },
        ];

        let normalized = normalize_tool_pairs_for_model(&messages);
        let results: Vec<(&str, &str)> = normalized
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                MessagePart::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => Some((tool_use_id.as_str(), content.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(results, vec![("call-b", "B"), ("call-a", "A")]);
        assert_eq!(normalize_tool_pairs_for_model(&normalized), normalized);
    }

    #[test]
    fn model_normalization_preserves_mixed_content_and_drops_orphan_results() {
        let turn = TurnId::now();
        let messages = vec![
            Message {
                role: MessageRole::Assistant,
                parts: vec![
                    MessagePart::Text {
                        text: "checking".into(),
                    },
                    MessagePart::ToolUse {
                        id: "call-ok".into(),
                        name: "probe".into(),
                        input: serde_json::json!({}),
                        intent: None,
                    },
                ],
                turn_id: turn.clone(),
                origin: MessageOrigin::User,
            },
            Message {
                role: MessageRole::Tool,
                parts: vec![
                    MessagePart::Text {
                        text: "preserve me".into(),
                    },
                    MessagePart::ToolResult {
                        tool_use_id: "call-ok".into(),
                        content: "done".into(),
                        is_error: false,
                    },
                    MessagePart::ToolResult {
                        tool_use_id: "orphan".into(),
                        content: "drop me".into(),
                        is_error: false,
                    },
                ],
                turn_id: turn,
                origin: MessageOrigin::Watcher,
            },
            Message {
                role: MessageRole::User,
                parts: Vec::new(),
                turn_id: TurnId::now(),
                origin: MessageOrigin::User,
            },
        ];

        let normalized = normalize_tool_pairs_for_model(&messages);
        assert_eq!(normalized.len(), 3);
        assert_eq!(normalized[0].text_concat(), "checking");
        assert!(matches!(
            &normalized[1].parts[..],
            [MessagePart::ToolResult { tool_use_id, content, .. }]
                if tool_use_id == "call-ok" && content == "done"
        ));
        assert_eq!(normalized[2].role, MessageRole::User);
        assert_eq!(normalized[2].origin, MessageOrigin::Watcher);
        assert_eq!(normalized[2].text_concat(), "preserve me");
        assert!(!normalized.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(part, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "orphan")
            })
        }));
    }
}
