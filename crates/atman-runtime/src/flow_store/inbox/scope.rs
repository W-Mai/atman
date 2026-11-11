//! Scope invalidation cancels unread input without changing recipient execution.

use super::*;
use crate::flow_store::{
    FlowFact, FlowStore,
    task_scope::{ScopeCancellation, TaskScopeRef},
};

/// The host verifies this cancellation against the source owner's committed scope record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputScopeInvalidation {
    pub scope: TaskScopeRef,
    pub cancellation: ScopeCancellation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InvalidatedInputScope {
    pub observation: InputScopeInvalidation,
    pub receipt: FlowPosition,
}

impl Inbox {
    fn validate_scope_invalidation(&self, observation: &InputScopeInvalidation) -> io::Result<()> {
        observation.scope.validate()?;
        let source = &observation.cancellation.receipt;
        if source.journal_id() != observation.scope.owner.instance_id.0.0 || source.sequence() == 0
        {
            return Err(invalid(
                "input scope cancellation does not identify its source journal",
            ));
        }
        if self.invalid_scopes.contains_key(&observation.scope) {
            return Err(invalid("duplicate input scope invalidation"));
        }
        if !self
            .entries
            .iter()
            .any(|entry| entry.envelope.request.scope.as_ref() == Some(&observation.scope))
        {
            return Err(invalid("input scope has no accepted messages"));
        }
        Ok(())
    }

    fn apply_scope_invalidation(
        &mut self,
        observation: InputScopeInvalidation,
        receipt: FlowPosition,
    ) {
        self.cancel_matching(&receipt, |envelope| {
            envelope.request.scope.as_ref() == Some(&observation.scope)
        });
        self.invalid_scopes.insert(
            observation.scope.clone(),
            InvalidatedInputScope {
                observation,
                receipt,
            },
        );
    }

    pub fn replay_scope_invalidation(
        &mut self,
        observation: InputScopeInvalidation,
        receipt: FlowPosition,
    ) -> io::Result<()> {
        self.validate_scope_invalidation(&observation)?;
        self.apply_scope_invalidation(observation, receipt);
        Ok(())
    }
}

impl FlowStore {
    pub fn input_scope_invalidation(&self, scope: &TaskScopeRef) -> Option<&InvalidatedInputScope> {
        self.inbox.invalid_scopes.get(scope)
    }

    /// Authenticate the observer and verify the source cancellation before calling. Matching retries retain the original recipient receipt, including after closure; conflicting observations are rejected.
    pub fn invalidate_input_scope(
        &mut self,
        observation: InputScopeInvalidation,
    ) -> io::Result<FlowPosition> {
        if let Some(record) = self.inbox.invalid_scopes.get(&observation.scope) {
            return if record.observation == observation {
                Ok(record.receipt.clone())
            } else {
                Err(invalid(
                    "input scope invalidation conflicts with its recorded source",
                ))
            };
        }
        self.inbox.validate_scope_invalidation(&observation)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::InputScopeInvalidated(Box::new(
                observation.clone(),
            ))])?;
        self.inbox
            .apply_scope_invalidation(observation, receipt.clone());
        self.cursor = receipt.clone();
        Ok(receipt)
    }
}
