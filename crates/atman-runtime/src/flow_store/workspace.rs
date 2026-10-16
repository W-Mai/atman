//! Immutable workspace acquisition intent, recorded before external allocation.

use std::{io, path::Component};

use atman_rt::InstanceState;
use serde::{Deserialize, Serialize};

use super::{FlowFact, FlowRef, FlowStore, invalid};
use crate::{event::AtmanUuid, event_log::journal::CommitReceipt, git_workspace::WorkspaceBinding};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceRequest {
    pub binding: WorkspaceBinding,
    pub source: WorkspaceSource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceSource {
    Allocate { base: String, retained: bool },
    Share { sponsor: FlowRef },
}

impl WorkspaceRequest {
    pub(crate) fn validate(&self, owner: &FlowRef) -> io::Result<()> {
        let binding = &self.binding;
        let path = &binding.path;
        if !path.is_absolute()
            || !binding.repository_root.is_absolute()
            || [&binding.path, &binding.repository_root]
                .iter()
                .any(|path| {
                    path.components()
                        .any(|part| matches!(part, Component::ParentDir))
                })
            || path.file_name().and_then(|name| name.to_str()) != Some(&binding.workspace_id)
            || path
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                != Some("worktrees")
            || path
                .parent()
                .and_then(|parent| parent.parent())
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                != Some(".atman")
        {
            return Err(invalid("invalid managed workspace binding"));
        }
        match &self.source {
            WorkspaceSource::Allocate { base, .. }
                if binding.branch.is_some()
                    || ![40, 64].contains(&base.len())
                    || !base.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
            {
                Err(invalid("workspace allocation requires a detached commit"))
            }
            WorkspaceSource::Share { sponsor }
                if sponsor == owner
                    || sponsor.session_id.0.is_nil()
                    || sponsor.instance_id.0.0.is_nil() =>
            {
                Err(invalid("workspace sharing requires another valid instance"))
            }
            _ => Ok(()),
        }
    }

    pub(super) fn replay(
        &self,
        state: &atman_rt::FlowInstance<AtmanUuid>,
        existing: Option<&Self>,
    ) -> io::Result<()> {
        self.validate(state.address())?;
        if state.state() != &InstanceState::Prepared || existing.is_some() {
            return Err(invalid(
                "workspace request must precede instance activation",
            ));
        }
        Ok(())
    }
}

impl FlowStore {
    pub fn workspace_request(&self) -> Option<&WorkspaceRequest> {
        self.workspace.as_ref()
    }

    /// Authenticate before calling. Retried requests preserve the original intent without reopening execution or acquiring resources.
    pub fn request_workspace(&mut self, request: WorkspaceRequest) -> io::Result<CommitReceipt> {
        if let Some(existing) = &self.workspace {
            return if existing == &request {
                Ok(self.cursor.clone())
            } else {
                Err(invalid(
                    "workspace request conflicts with the recorded intent",
                ))
            };
        }
        request.replay(&self.state, None)?;
        self.cursor = self
            .journal
            .commit(&[FlowFact::WorkspaceRequested(Box::new(request.clone()))])?;
        self.workspace = Some(request);
        self.refresh_projection();
        Ok(self.cursor.clone())
    }
}
