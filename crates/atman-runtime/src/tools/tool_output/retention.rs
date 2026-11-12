//! Explicit source references for inheriting complete registered tool results.

use std::{collections::BTreeMap, fs, io, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    Message, MessagePart, OutputStore, RuntimeError, output_dir, output_reference, valid_output_id,
};

/// A captured file identity. Importing validates its contents before accepting inherited context.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct OutputSource {
    id: String,
    directory: PathBuf,
    bytes: usize,
    digest: String,
}

fn references(messages: &[Message]) -> impl Iterator<Item = (String, usize, &str)> {
    messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| {
            let MessagePart::ToolResult { content, .. } = part else {
                return None;
            };
            let (id, bytes) = output_reference(content)?;
            Some((id, bytes, content.as_str()))
        })
}

impl OutputStore {
    /// Capture an archived result through immutable temporary files owned by the caller. Keep the directory alive until activation acknowledges its independent copies.
    pub fn capture_archived_references(
        &self,
        messages: &[Message],
        directory: &std::path::Path,
    ) -> Result<Vec<OutputSource>, RuntimeError> {
        if !self.has_retained_outputs() {
            return self.capture_references(messages);
        }
        for (id, _, content) in references(messages) {
            let full = self.retained_content(content).ok_or_else(|| {
                RuntimeError::ToolFailed(
                    "archived output is missing or does not match its reference".into(),
                )
            })?;
            crate::retained_file::publish(
                &output_dir(directory).join(format!("{id}.txt")),
                full.as_bytes(),
            )
            .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?;
        }
        Self::at(directory).capture_references(messages)
    }

    /// Capture only outputs referenced by the supplied messages, without transferring ownership.
    pub fn capture_references(
        &self,
        messages: &[Message],
    ) -> Result<Vec<OutputSource>, RuntimeError> {
        let mut sources = BTreeMap::new();
        let mut identities = BTreeMap::new();
        for (id, bytes, content) in references(messages) {
            if let Some(identity) = identities.get(&id) {
                if !super::reference_matches_text(content, &id, identity) {
                    return Err(RuntimeError::ToolFailed(
                        "inherited output references disagree".into(),
                    ));
                }
                continue;
            }
            let directory = fs::canonicalize(self.session_dir.as_deref().ok_or_else(|| {
                RuntimeError::ToolFailed("output inheritance requires a source directory".into())
            })?)
            .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?;
            let identity = crate::retained_file::inspect_text(
                &output_dir(&directory).join(format!("{id}.txt")),
                bytes,
            )
            .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?;
            if !super::reference_matches_text(content, &id, &identity) {
                return Err(RuntimeError::ToolFailed(
                    "recorded output reference does not match its contents".into(),
                ));
            }
            sources.insert(
                id.clone(),
                OutputSource {
                    id: id.clone(),
                    directory,
                    bytes: identity.bytes,
                    digest: identity.digest.clone(),
                },
            );
            identities.insert(id, identity);
        }
        let sources = sources.into_values().collect::<Vec<_>>();
        Self::validate_references(messages, &sources)
            .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?;
        Ok(sources)
    }

    /// Validate metadata during both live admission and replay, without touching source files.
    pub(crate) fn validate_references(
        messages: &[Message],
        sources: &[OutputSource],
    ) -> io::Result<()> {
        let mut expected = BTreeMap::new();
        for (id, bytes, _) in references(messages) {
            if expected
                .insert(id, bytes)
                .is_some_and(|previous| previous != bytes)
            {
                return Err(io::Error::other("inherited output references disagree"));
            }
        }
        for source in sources {
            if !valid_output_id(&source.id)
                || !source.directory.is_absolute()
                || source.digest.len() != 64
                || !source
                    .digest
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                || expected.remove(&source.id) != Some(source.bytes)
            {
                return Err(io::Error::other(
                    "invalid or unreferenced inherited output source",
                ));
            }
        }
        if !expected.is_empty() {
            return Err(io::Error::other(
                "inherited tool result is missing its output source",
            ));
        }
        Ok(())
    }

    /// Persist independent copies with their original IDs. Conflicts never overwrite existing content.
    pub(crate) fn import_references(
        &self,
        messages: &[Message],
        sources: &[OutputSource],
    ) -> io::Result<()> {
        Self::validate_references(messages, sources)?;
        if sources.is_empty() {
            return Ok(());
        }
        let destination = self.session_dir.as_deref().ok_or_else(|| {
            io::Error::other("output inheritance requires a destination directory")
        })?;
        for source in sources {
            crate::retained_file::copy_text(
                &output_dir(&source.directory).join(format!("{}.txt", source.id)),
                &output_dir(destination).join(format!("{}.txt", source.id)),
                source.bytes,
                &source.digest,
                |identity| {
                    for (id, _, content) in
                        references(messages).filter(|(id, _, _)| id == &source.id)
                    {
                        if !super::reference_matches_text(content, &id, identity) {
                            return Err(io::Error::other(format!(
                                "inherited output reference does not match {id}"
                            )));
                        }
                    }
                    Ok(())
                },
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MessageRole;

    #[test]
    fn streamed_inheritance_checks_every_reference_and_survives_source_removal() {
        let original = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let store = OutputStore::at(original.path());
        let destination = OutputStore::at(target.path());
        let full = format!("{}文\nlast", "x".repeat(65535));
        let id = store.register("output", &full).unwrap();
        let envelope = |offset| {
            serde_json::json!({
                "output_id": id, "content": "preview", "total_bytes": full.len(),
                "total_lines": 2, "next": {"mode": "lines", "offset": offset, "has_more": true}
            })
            .to_string()
        };
        let message = |content| Message {
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "call".into(),
                content,
                is_error: false,
            }],
            ..Message::user_text(atman_rt::TurnId::now(), "")
        };
        let messages = vec![message(envelope(1))];
        let sources = store.capture_references(&messages).unwrap();
        let conflicting = vec![messages[0].clone(), message(envelope(2))];
        assert!(store.capture_references(&conflicting).is_err());
        assert!(
            destination
                .import_references(&conflicting, &sources)
                .is_err()
        );
        assert!(!output_dir(target.path()).join(format!("{id}.txt")).exists());
        destination.import_references(&messages, &sources).unwrap();
        destination.import_references(&messages, &sources).unwrap();
        fs::write(
            output_dir(original.path()).join(format!("{id}.txt")),
            "z".repeat(full.len()),
        )
        .unwrap();
        assert!(destination.import_references(&messages, &sources).is_err());
        drop(original);
        assert_eq!(&*destination.read_registered(&id).unwrap(), full);
    }

    #[test]
    fn inherited_source_metadata_matches_only_explicit_tool_result_references() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutputStore::at(dir.path());
        let full = "header\nfull output beyond preview\n";
        let content = super::super::truncate_tool_result_content_with_budget(
            full,
            "test",
            Some(&store),
            super::super::ToolOutputBudget {
                max_lines: 1,
                max_bytes: 7,
                max_line_bytes: 7,
            },
        );
        let mut message = Message::user_text(atman_rt::TurnId::now(), &content);
        assert!(
            store
                .capture_references(&[message.clone()])
                .unwrap()
                .is_empty()
        );
        message.role = MessageRole::Tool;
        message.parts = vec![MessagePart::ToolResult {
            tool_use_id: "result".into(),
            content,
            is_error: false,
        }];
        let messages = vec![message.clone(), message];
        let sources = store.capture_references(&messages).unwrap();
        assert_eq!(sources.len(), 1);
        assert!(OutputStore::validate_references(&messages, &[]).is_err());
        assert!(OutputStore::validate_references(&[], &sources).is_err());
        assert!(
            OutputStore::validate_references(&messages, &[sources[0].clone(), sources[0].clone()])
                .is_err()
        );
        let mut invalid = sources.clone();
        invalid[0].id = "../outside".into();
        assert!(OutputStore::validate_references(&messages, &invalid).is_err());
        invalid[0] = sources[0].clone();
        invalid[0].digest = "invalid".into();
        assert!(OutputStore::validate_references(&messages, &invalid).is_err());
    }
}
