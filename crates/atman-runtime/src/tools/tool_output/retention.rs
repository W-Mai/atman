//! Explicit source references for inheriting complete registered tool results.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::PathBuf,
};

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
    /// Capture only outputs referenced by the supplied messages, without transferring ownership.
    pub fn capture_references(
        &self,
        messages: &[Message],
    ) -> Result<Vec<OutputSource>, RuntimeError> {
        let mut sources = BTreeMap::new();
        for (id, _, content) in references(messages) {
            if sources.contains_key(&id) {
                continue;
            }
            let (_, full) = self.retain_reference(content)?.ok_or_else(|| {
                RuntimeError::ToolFailed("registered output reference disappeared".into())
            })?;
            let directory = fs::canonicalize(self.session_dir.as_deref().ok_or_else(|| {
                RuntimeError::ToolFailed("output inheritance requires a source directory".into())
            })?)
            .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?;
            sources.insert(
                id.clone(),
                OutputSource {
                    id,
                    directory,
                    bytes: full.len(),
                    digest: blake3::hash(full.as_bytes()).to_hex().to_string(),
                },
            );
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
            let full = source.read()?;
            let retained = Self::retained(BTreeMap::from([(source.id.clone(), full)]))
                .map_err(io::Error::other)?;
            for (id, _, content) in references(messages).filter(|(id, _, _)| id == &source.id) {
                if !retained.validates_reference(content) {
                    return Err(io::Error::other(format!(
                        "inherited output reference does not match {id}"
                    )));
                }
            }
            crate::retained_file::publish(
                &output_dir(destination).join(format!("{}.txt", source.id)),
                retained.retained[&source.id].as_bytes(),
            )?;
        }
        Ok(())
    }
}

impl OutputSource {
    fn read(&self) -> io::Result<String> {
        let directory = output_dir(&self.directory);
        let path = directory.join(format!("{}.txt", self.id));
        if !fs::symlink_metadata(&directory)?.is_dir() || !fs::symlink_metadata(&path)?.is_file() {
            return Err(io::Error::other(
                "output source is not a regular registered file",
            ));
        }
        let file = fs::File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != self.bytes as u64 {
            return Err(io::Error::other("output source size changed"));
        }
        let limit = (self.bytes as u64)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("output source size overflow"))?;
        let mut full = String::new();
        file.take(limit).read_to_string(&mut full)?;
        if full.len() != self.bytes
            || blake3::hash(full.as_bytes()).to_hex().as_str() != self.digest
        {
            return Err(io::Error::other("output source contents changed"));
        }
        Ok(full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MessageRole;

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
