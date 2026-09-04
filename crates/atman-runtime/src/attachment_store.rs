use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;

use crate::error::RuntimeError;
use crate::event::{ContextBase, ContextId, Event, EventEnvelope, EventSink, FlowRunId};
use crate::message::{
    AttachmentPatch, AttachmentTarget, ImageData, ImageSource, Message, MessagePart, MessagePartId,
};

const ATTACHMENTS_DIR: &str = "attachments";
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
type SequencedMessages = Vec<(u64, Message)>;

#[derive(Debug, Clone)]
pub struct AttachmentStore {
    root: PathBuf,
    persistent: bool,
}

/// One independently mutable message context addressed by a maintenance repair.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AttachmentContext {
    Legacy { flow_run_id: Option<FlowRunId> },
    Scoped { context_id: ContextId },
}

impl std::fmt::Display for AttachmentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Legacy { flow_run_id: None } => f.write_str("root"),
            Self::Legacy {
                flow_run_id: Some(run_id),
            } => write!(f, "run:{run_id}"),
            Self::Scoped { context_id } => write!(f, "context:{context_id}"),
        }
    }
}

/// A missing or invalid image that remains after replaying all existing patches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSanitizeFinding {
    pub context: AttachmentContext,
    pub patch: AttachmentPatch,
}

impl AttachmentStore {
    pub fn at(session_dir: impl AsRef<Path>) -> Self {
        let session_dir = session_dir.as_ref();
        Self {
            root: session_dir.join(ATTACHMENTS_DIR),
            persistent: !session_dir.as_os_str().is_empty(),
        }
    }

    pub fn import_path(&self, path: impl AsRef<Path>) -> Result<ImageSource, RuntimeError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|error| RuntimeError::AttachmentError {
            part_id: None,
            reason: format!("cannot read {}: {error}", path.display()),
        })?;
        self.import_bytes(&bytes, path.file_name().and_then(|name| name.to_str()))
    }

    pub fn import_base64(
        &self,
        data: &str,
        name: Option<&str>,
    ) -> Result<ImageSource, RuntimeError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| RuntimeError::AttachmentError {
                reason: format!("invalid base64 image: {error}"),
                part_id: None,
            })?;
        self.import_bytes(&bytes, name)
    }

    pub fn import_bytes(
        &self,
        bytes: &[u8],
        name: Option<&str>,
    ) -> Result<ImageSource, RuntimeError> {
        validate_size(bytes)?;
        let (media_type, extension) = detect_image_type(bytes)?;
        if !self.persistent {
            return Ok(ImageSource {
                media_type: media_type.into(),
                data: ImageData::Base64 {
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                },
                detail: crate::provider::ImageDetail::Auto,
            });
        }
        let id = blake3::hash(bytes).to_hex().to_string();
        let path = self.root.join(format!("{id}.{extension}"));
        if !path.is_file() {
            std::fs::create_dir_all(&self.root).map_err(|error| RuntimeError::AttachmentError {
                part_id: None,
                reason: format!("cannot create attachment store: {error}"),
            })?;
            let temp_path = self
                .root
                .join(format!(".{id}.{}.tmp", uuid::Uuid::new_v4()));
            let write_result = (|| -> std::io::Result<()> {
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temp_path)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                std::fs::rename(&temp_path, &path)
            })();
            if let Err(error) = write_result {
                let _ = std::fs::remove_file(&temp_path);
                if !path.is_file() {
                    return Err(RuntimeError::AttachmentError {
                        reason: format!("cannot persist attachment: {error}"),
                        part_id: None,
                    });
                }
            }
        }
        Ok(ImageSource {
            media_type: media_type.into(),
            data: ImageData::Artifact {
                id,
                path,
                name: name.map(ToOwned::to_owned),
            },
            detail: crate::provider::ImageDetail::Auto,
        })
    }
}

pub fn image_bytes(source: &ImageSource) -> Result<Vec<u8>, RuntimeError> {
    let bytes = match &source.data {
        ImageData::Base64 { data } => base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| RuntimeError::AttachmentError {
                reason: format!("invalid base64 image: {error}"),
                part_id: None,
            })?,
        ImageData::Path { path } | ImageData::Artifact { path, .. } => std::fs::read(path)
            .map_err(|error| RuntimeError::AttachmentError {
                reason: format!("cannot read {}: {error}", path.display()),
                part_id: None,
            })?,
    };
    validate_size(&bytes)?;
    let (actual_media_type, _) = detect_image_type(&bytes)?;
    if source.media_type != actual_media_type {
        return Err(RuntimeError::AttachmentError {
            part_id: None,
            reason: format!(
                "image media type mismatch: declared {}, detected {actual_media_type}",
                source.media_type
            ),
        });
    }
    if let ImageData::Artifact { id, .. } = &source.data {
        let actual_id = blake3::hash(&bytes).to_hex().to_string();
        if actual_id != *id {
            return Err(RuntimeError::AttachmentError {
                reason: format!("attachment integrity check failed for {id}"),
                part_id: None,
            });
        }
    }
    Ok(bytes)
}

pub fn image_base64(
    source: &ImageSource,
    part_id: Option<MessagePartId>,
) -> Result<String, RuntimeError> {
    let bytes = image_bytes(source).map_err(|error| match error {
        RuntimeError::AttachmentError { reason, .. } => {
            RuntimeError::AttachmentError { reason, part_id }
        }
        error => error,
    })?;
    if let ImageData::Base64 { data } = &source.data {
        return Ok(data.clone());
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

pub fn display_name(source: &ImageSource) -> String {
    match &source.data {
        ImageData::Artifact { id, name, .. } => name.clone().unwrap_or_else(|| id.clone()),
        ImageData::Path { path } => path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image")
            .to_string(),
        ImageData::Base64 { .. } => "image".into(),
    }
}

/// Finds attachment references that are still invalid after canonical replay.
///
/// Raw history and the materialized window are both inspected because a
/// checkpoint may contain an image that has no surviving raw message. Stable
/// part identities deduplicate references retained in both views.
pub fn sanitize_findings(
    events: &[EventEnvelope],
) -> std::io::Result<Vec<AttachmentSanitizeFinding>> {
    let ownership = crate::projection::message_window::FlowOwnership::from_events(
        events.iter().map(|envelope| &envelope.event),
    );
    let last_seq = events.iter().map(|event| event.seq).max().unwrap_or(0);
    let mut findings = Vec::new();
    let mut seen = HashSet::new();

    let mut legacy_contexts = Vec::new();
    let mut known_legacy_contexts = HashSet::new();
    for envelope in events
        .iter()
        .filter(|envelope| envelope.context_id.is_none())
    {
        let Some(flow_run_id) = attachment_event_flow_run_id(&envelope.event) else {
            continue;
        };
        let owner = ownership.context_run(flow_run_id);
        if known_legacy_contexts.insert(owner.clone()) {
            legacy_contexts.push(owner);
        }
    }
    for flow_run_id in legacy_contexts {
        let (raw, window) = replay_legacy_context(events, &ownership, flow_run_id.as_ref());
        inspect_messages(
            AttachmentContext::Legacy { flow_run_id },
            raw.iter().chain(&window),
            &mut seen,
            &mut findings,
        );
    }

    let mut context_ids = Vec::new();
    let mut known_context_ids = HashSet::new();
    for envelope in events {
        if matches!(envelope.event, Event::ContextCreated { .. })
            && let Some(context_id) = &envelope.context_id
            && known_context_ids.insert(context_id.clone())
        {
            context_ids.push(context_id.clone());
        }
    }
    for context_id in context_ids {
        let replay = crate::projection::context::replay_context(
            events.iter(),
            &ContextBase::Context {
                context_id: context_id.clone(),
                through_seq: last_seq,
            },
        )?;
        inspect_messages(
            AttachmentContext::Scoped { context_id },
            replay.raw.iter().chain(replay.window()),
            &mut seen,
            &mut findings,
        );
    }

    Ok(findings)
}

/// Persists one sanitizer repair in the context where the reference was found.
pub fn emit_sanitize_finding(sink: &EventSink, finding: &AttachmentSanitizeFinding) {
    let (sink, flow_run_id) = match &finding.context {
        AttachmentContext::Legacy { flow_run_id } => (sink.clone(), flow_run_id.clone()),
        AttachmentContext::Scoped { context_id } => {
            (sink.clone().with_context(context_id.clone()), None)
        }
    };
    sink.emit(Event::AttachmentDegraded {
        turn_id: None,
        flow_run_id,
        patch: finding.patch.clone(),
    });
}

fn inspect_messages<'a>(
    context: AttachmentContext,
    messages: impl Iterator<Item = &'a (u64, Message)>,
    seen: &mut HashSet<(AttachmentContext, MessagePartId)>,
    findings: &mut Vec<AttachmentSanitizeFinding>,
) {
    for (_, message) in messages {
        for part in &message.parts {
            let MessagePart::Image {
                id: Some(part_id),
                source,
            } = part
            else {
                continue;
            };
            if !seen.insert((context.clone(), *part_id)) {
                continue;
            }
            if let Err(error) = image_bytes(source) {
                findings.push(AttachmentSanitizeFinding {
                    context: context.clone(),
                    patch: AttachmentPatch {
                        target: AttachmentTarget::Part { part_id: *part_id },
                        file_basename: display_name(source),
                        reason: format!("sanitize:{error}"),
                    },
                });
            }
        }
    }
}

fn attachment_event_flow_run_id(event: &Event) -> Option<Option<&FlowRunId>> {
    if let Some((_, flow_run_id)) = event.context_message() {
        return Some(flow_run_id);
    }
    match event {
        Event::ContextCompact { flow_run_id, .. }
        | Event::Checkpoint { flow_run_id, .. }
        | Event::AttachmentDegraded { flow_run_id, .. } => Some(flow_run_id.as_ref()),
        _ => None,
    }
}

fn replay_legacy_context(
    events: &[EventEnvelope],
    ownership: &crate::projection::message_window::FlowOwnership,
    target: Option<&FlowRunId>,
) -> (SequencedMessages, SequencedMessages) {
    let mut raw = Vec::new();
    let mut raw_positions = std::collections::HashMap::new();
    let mut window = Vec::new();
    let mut window_positions = std::collections::HashMap::new();
    let no_exclusions = HashSet::new();
    for envelope in events
        .iter()
        .filter(|envelope| envelope.context_id.is_none())
    {
        let Some(flow_run_id) = attachment_event_flow_run_id(&envelope.event) else {
            continue;
        };
        if ownership.context_run(flow_run_id).as_ref() != target {
            continue;
        }
        crate::projection::message_window::apply_envelope_to_messages(
            envelope,
            &no_exclusions,
            &mut window,
            &mut window_positions,
        );
        if let Some((message, _)) = envelope.event.context_message() {
            raw_positions.insert(envelope.seq, raw.len());
            raw.push((envelope.seq, message.replayed(envelope.seq, None)));
        } else if matches!(envelope.event, Event::AttachmentDegraded { .. }) {
            crate::projection::message_window::apply_envelope_to_messages(
                envelope,
                &no_exclusions,
                &mut raw,
                &mut raw_positions,
            );
        }
    }
    (raw, window)
}

fn validate_size(bytes: &[u8]) -> Result<(), RuntimeError> {
    if bytes.is_empty() {
        return Err(RuntimeError::AttachmentError {
            reason: "image is empty".into(),
            part_id: None,
        });
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(RuntimeError::AttachmentError {
            part_id: None,
            reason: format!(
                "image is too large: {} bytes exceeds the {} byte limit",
                bytes.len(),
                MAX_IMAGE_BYTES
            ),
        });
    }
    Ok(())
}

fn detect_image_type(bytes: &[u8]) -> Result<(&'static str, &'static str), RuntimeError> {
    let detected = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(("image/jpeg", "jpg"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("image/gif", "gif"))
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(("image/webp", "webp"))
    } else {
        None
    };
    detected.ok_or_else(|| RuntimeError::AttachmentError {
        reason: "unsupported image format; expected PNG, JPEG, GIF, or WebP".into(),
        part_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ContextInheritance, TurnId};

    const PNG_1X1: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
    ];

    #[test]
    fn import_is_content_addressed_and_deduplicated() {
        let session = tempfile::tempdir().unwrap();
        let store = AttachmentStore::at(session.path());
        let first = store.import_bytes(PNG_1X1, Some("first.png")).unwrap();
        let second = store.import_bytes(PNG_1X1, Some("second.png")).unwrap();

        let ImageData::Artifact {
            id: first_id,
            path: first_path,
            ..
        } = first.data
        else {
            panic!("expected artifact")
        };
        let ImageData::Artifact {
            id: second_id,
            path: second_path,
            ..
        } = second.data
        else {
            panic!("expected artifact")
        };
        assert_eq!(first_id, second_id);
        assert_eq!(first_path, second_path);
        assert_eq!(std::fs::read(first_path).unwrap(), PNG_1X1);
    }

    #[test]
    fn integrity_mismatch_is_rejected() {
        let session = tempfile::tempdir().unwrap();
        let store = AttachmentStore::at(session.path());
        let source = store.import_bytes(PNG_1X1, None).unwrap();
        let ImageData::Artifact { path, .. } = &source.data else {
            panic!("expected artifact")
        };
        std::fs::write(path, b"not an image").unwrap();
        assert!(image_bytes(&source).is_err());
    }

    fn missing_image(path: &Path, id: Option<MessagePartId>) -> Message {
        Message {
            role: crate::message::MessageRole::User,
            parts: vec![MessagePart::Image {
                id,
                source: ImageSource {
                    media_type: "image/png".into(),
                    data: ImageData::Path {
                        path: path.to_path_buf(),
                    },
                    detail: Default::default(),
                },
            }],
            turn_id: TurnId::now(),
            origin: crate::message::MessageOrigin::User,
        }
    }

    fn user_event(message: Message, flow_run_id: Option<FlowRunId>) -> Event {
        Event::UserMsg {
            turn_id: message.turn_id.clone(),
            flow_run_id,
            message,
        }
    }

    #[test]
    fn sanitize_replays_patches_before_checking_checkpoint_only_images() {
        let dir = tempfile::tempdir().unwrap();
        let sink = EventSink::new();
        let raw_id = MessagePartId(uuid::Uuid::now_v7());
        sink.emit(user_event(
            missing_image(&dir.path().join("raw.png"), Some(raw_id)),
            None,
        ));
        emit_sanitize_finding(
            &sink,
            &AttachmentSanitizeFinding {
                context: AttachmentContext::Legacy { flow_run_id: None },
                patch: AttachmentPatch {
                    target: AttachmentTarget::Part { part_id: raw_id },
                    file_basename: "raw.png".into(),
                    reason: "already handled".into(),
                },
            },
        );
        sink.emit(Event::Checkpoint {
            session_id: "session".into(),
            flow_run_id: None,
            messages: vec![missing_image(&dir.path().join("checkpoint.png"), None)],
            window_tokens: 1,
        });

        let findings = sanitize_findings(&sink.snapshot_envelopes()).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].context,
            AttachmentContext::Legacy { flow_run_id: None }
        );
        assert_eq!(findings[0].patch.file_basename, "checkpoint.png");
        assert!(matches!(
            findings[0].patch.target,
            AttachmentTarget::Part { part_id } if part_id != raw_id
        ));

        emit_sanitize_finding(&sink, &findings[0]);
        assert!(
            sanitize_findings(&sink.snapshot_envelopes())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn sanitize_keeps_typed_branch_repairs_inside_their_fixed_ancestry() {
        let dir = tempfile::tempdir().unwrap();
        let sink = EventSink::new();
        let image_id = MessagePartId(uuid::Uuid::now_v7());
        let root_seq = sink.emit_returning_seq(user_event(
            missing_image(&dir.path().join("shared.png"), Some(image_id)),
            None,
        ));
        let old_child_id = ContextId::now();
        let old_child = sink.clone().with_context(old_child_id.clone());
        old_child.emit(Event::ContextCreated {
            base: Some(ContextBase::LegacyRoot {
                through_seq: root_seq,
            }),
            inheritance: ContextInheritance::Full,
        });

        let initial = sanitize_findings(&sink.snapshot_envelopes()).unwrap();
        assert_eq!(initial.len(), 2);
        let root_finding = initial
            .iter()
            .find(|finding| finding.context == AttachmentContext::Legacy { flow_run_id: None })
            .unwrap();
        emit_sanitize_finding(&sink, root_finding);

        let new_child_id = ContextId::now();
        sink.clone()
            .with_context(new_child_id.clone())
            .emit(Event::ContextCreated {
                base: Some(ContextBase::LegacyRoot {
                    through_seq: sink.published_seq(),
                }),
                inheritance: ContextInheritance::Full,
            });
        let remaining = sanitize_findings(&sink.snapshot_envelopes()).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].context,
            AttachmentContext::Scoped {
                context_id: old_child_id
            }
        );
        assert_ne!(
            remaining[0].context,
            AttachmentContext::Scoped {
                context_id: new_child_id
            }
        );

        emit_sanitize_finding(&sink, &remaining[0]);
        assert!(
            sanitize_findings(&sink.snapshot_envelopes())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn sanitize_preserves_legacy_spawned_owners() {
        let dir = tempfile::tempdir().unwrap();
        let sink = EventSink::new();
        let root = FlowRunId::now();
        let child = FlowRunId::now();
        sink.emit(Event::FlowStart {
            run_id: root.clone(),
            turn_id: None,
            flow_name: "root".into(),
            parent_run_id: None,
            parent_node_id: None,
            spawned: false,
        });
        sink.emit(Event::FlowStart {
            run_id: child.clone(),
            turn_id: None,
            flow_name: "child".into(),
            parent_run_id: Some(root.clone()),
            parent_node_id: None,
            spawned: true,
        });
        sink.emit(user_event(
            missing_image(
                &dir.path().join("root.png"),
                Some(MessagePartId(uuid::Uuid::now_v7())),
            ),
            Some(root),
        ));
        sink.emit(user_event(
            missing_image(
                &dir.path().join("child.png"),
                Some(MessagePartId(uuid::Uuid::now_v7())),
            ),
            Some(child.clone()),
        ));

        let findings = sanitize_findings(&sink.snapshot_envelopes()).unwrap();
        assert_eq!(findings.len(), 2);
        let child_finding = findings
            .iter()
            .find(|finding| {
                finding.context
                    == AttachmentContext::Legacy {
                        flow_run_id: Some(child.clone()),
                    }
            })
            .unwrap();
        emit_sanitize_finding(&sink, child_finding);

        let remaining = sanitize_findings(&sink.snapshot_envelopes()).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].context,
            AttachmentContext::Legacy { flow_run_id: None }
        );
    }
}
