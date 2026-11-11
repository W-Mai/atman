//! Flow-local names with independent versions and actor-scoped retry receipts.

use std::{collections::HashMap, io};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FlowFact, FlowStore, inbox::SenderIdentity, invalid};
use crate::flow_store::position::FlowPosition;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NameSource {
    Initial,
    Automatic,
    Explicit,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct NameSnapshot {
    pub name: Option<String>,
    pub source: Option<NameSource>,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NameRequest {
    pub id: Uuid,
    pub sender: SenderIdentity,
    pub expected_revision: Option<u64>,
    pub name: String,
    pub source: NameSource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NameUpdate {
    pub request: NameRequest,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommittedName {
    pub snapshot: NameSnapshot,
    pub receipt: FlowPosition,
}

#[derive(Clone, Default)]
pub(super) struct Names {
    pub(super) snapshot: NameSnapshot,
    requests: HashMap<(SenderIdentity, Uuid), (NameRequest, CommittedName)>,
}

pub fn validate_name(name: &str) -> io::Result<()> {
    if name.trim().is_empty() || name != name.trim() || name.chars().any(char::is_control) {
        return Err(invalid(
            "flow name must be nonempty, trimmed, single-line text",
        ));
    }
    Ok(())
}

impl Names {
    pub(super) fn display_title(&self, content: &super::FlowContent) -> String {
        self.snapshot.name.clone().unwrap_or_else(|| match content {
            super::FlowContent::History(history) => history.title().to_owned(),
            super::FlowContent::Entry(_) => "Unnamed flow".into(),
        })
    }
    /// Provisional labels are projections of recorded inputs, not additional writes.
    pub(super) fn observe_task(&mut self, text: &str) {
        if self.snapshot.name.is_some() {
            return;
        }
        let name = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if validate_name(&name).is_ok() {
            self.snapshot.name = Some(name);
            self.snapshot.source = Some(NameSource::Initial);
        }
    }

    pub(super) fn observe_launch(&mut self, input: &super::launch::LaunchInput) {
        for text in [
            input.display_label.as_deref(),
            input.initial_goal.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            self.observe_task(text);
        }
    }

    pub(super) fn observe_input(
        &mut self,
        content: &super::inbox::InputContent,
        descriptor: &super::FlowDescriptor,
    ) {
        if self.snapshot.name.is_some() {
            return;
        }
        if let super::inbox::InputContent::InvocationInput {
            command, arguments, ..
        } = content
        {
            let entry = command.is_none().then(|| descriptor.entry().ok()).flatten();
            if let Some(entry) = entry
                && let Some(source) = entry.bundle.sources.get(&entry.bundle.entry)
                && let Ok(file) = atman_rt::parse_file(source)
                && let Some(flow) = file.flows.iter().find(|flow| flow.name.name == entry.name)
                && let Ok(Some(parameter)) = crate::validate::invocation_message_parameter(flow)
                && let Some(serde_json::Value::String(text)) = arguments.get(parameter)
            {
                self.observe_task(text);
            }
        }
        for message in content.messages() {
            self.observe_message(message);
        }
    }

    pub(super) fn observe_message(&mut self, message: &crate::message::Message) {
        if message.role == crate::message::MessageRole::User
            && matches!(
                message.origin,
                crate::message::MessageOrigin::User | crate::message::MessageOrigin::Interjection
            )
        {
            self.observe_task(&message.text_concat());
        }
    }

    fn previous(&self, request: &NameRequest) -> io::Result<Option<CommittedName>> {
        match self.requests.get(&(request.sender.clone(), request.id)) {
            Some((original, _)) if original != request => Err(invalid(
                "name request identity conflicts with recorded data",
            )),
            record => Ok(record.map(|(_, result)| result.clone())),
        }
    }

    pub(super) fn prepare(&self, request: &NameRequest) -> io::Result<NameSnapshot> {
        validate_name(&request.name)?;
        if request.id.is_nil() || self.previous(request)?.is_some() {
            return Err(invalid("invalid or duplicate name request"));
        }
        if request
            .expected_revision
            .is_some_and(|value| value != self.snapshot.revision)
        {
            return Err(invalid("flow name changed since it was read"));
        }
        match request.source {
            NameSource::Initial if self.snapshot.name.is_some() => {
                return Err(invalid("flow already has an initial name"));
            }
            NameSource::Automatic
                if request.expected_revision.is_none()
                    || matches!(
                        self.snapshot.source,
                        Some(NameSource::Explicit | NameSource::Automatic)
                    ) =>
            {
                return Err(invalid(
                    "automatic naming requires an unchanged provisional name",
                ));
            }
            _ => {}
        }
        Ok(NameSnapshot {
            name: Some(request.name.clone()),
            source: Some(request.source),
            revision: self
                .snapshot
                .revision
                .checked_add(1)
                .ok_or_else(|| invalid("name revision exhausted"))?,
        })
    }

    pub(super) fn apply(
        &mut self,
        request: NameRequest,
        snapshot: NameSnapshot,
        receipt: FlowPosition,
    ) -> CommittedName {
        self.snapshot = snapshot.clone();
        let record = CommittedName { snapshot, receipt };
        self.requests.insert(
            (request.sender.clone(), request.id),
            (request, record.clone()),
        );
        record
    }

    pub(super) fn initialize(&mut self, name: &str) -> io::Result<()> {
        validate_name(name)?;
        if self.snapshot.revision != 0 {
            return Err(invalid("activation cannot replace a flow name"));
        }
        self.snapshot = NameSnapshot {
            name: Some(name.into()),
            source: Some(NameSource::Explicit),
            revision: 1,
        };
        Ok(())
    }
}

impl FlowStore {
    pub fn name(&self) -> &NameSnapshot {
        &self.names.snapshot
    }

    /// Authorization belongs to the owner adapter, including retries. Names never alter execution or Goal state.
    pub fn update_name(&mut self, request: NameRequest) -> io::Result<CommittedName> {
        if let Some(previous) = self.names.previous(&request)? {
            return Ok(previous);
        }
        let snapshot = self.names.prepare(&request)?;
        let update = NameUpdate {
            request,
            at: chrono::Utc::now(),
        };
        let receipt = self
            .journal
            .commit(&[FlowFact::NameUpdated(Box::new(update.clone()))])?;
        self.cursor = receipt.clone();
        Ok(self.names.apply(update.request, snapshot, receipt))
    }
}
