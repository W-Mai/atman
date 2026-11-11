use super::*;
use crate::flow_store::{completion::Completions, control::ControlUpdate};

#[cfg(test)]
mod tests;

impl HistoryOperations {
    pub(in crate::flow_store) fn validate_stopping(
        &self,
        sender: &SenderIdentity,
        id: Uuid,
        control: &ControlRequest,
        state: &atman_rt::FlowInstance<AtmanUuid>,
    ) -> io::Result<()> {
        let record = self.validate_abort(sender, id)?;
        if record.status != HistoryOperationStatus::Preparing
            || record.request.active.is_none()
            || !matches!(state.state(), InstanceState::Open(InstanceActivity::Active(active))
                if Some(&active.lease) == record.request.active.as_ref())
            || control.sender != *sender
            || control.target != record.request.flow
            || control.action != ControlAction::CancelTurn
        {
            return Err(invalid(
                "history cancellation requires its reviewed active invocation",
            ));
        }
        Ok(())
    }

    pub(in crate::flow_store) fn stopped_completion(
        &self,
        sender: &SenderIdentity,
        id: Uuid,
        state: &atman_rt::FlowInstance<AtmanUuid>,
        completions: &Completions,
    ) -> io::Result<Option<FlowPosition>> {
        let record = self.validate_abort(sender, id)?;
        if !matches!(
            record.status,
            HistoryOperationStatus::Preparing | HistoryOperationStatus::Stopping
        ) || !matches!(state.state(), InstanceState::Open(activity)
                if !matches!(activity, InstanceActivity::Active(_)))
        {
            return Err(invalid(
                "history operation is waiting for execution cleanup",
            ));
        }
        record
            .request
            .active
            .as_ref()
            .map(|lease| {
                completions
                    .get(lease)
                    .filter(|done| &done.completion.lease == lease)
                    .map(|done| done.receipt.clone())
                    .ok_or_else(|| invalid("reviewed invocation has no completion receipt"))
            })
            .transpose()
    }
}

impl FlowStore {
    /// Stop only the reviewed invocation. The cancellation acknowledgement is not execution completion.
    pub fn stop_history_operation(
        &mut self,
        sender: SenderIdentity,
        id: Uuid,
        authorize: impl FnOnce(&HistoryOperationRecord) -> io::Result<()>,
    ) -> io::Result<HistoryOperationRecord> {
        let record = self
            .history_operations
            .find(&sender, id)
            .ok_or_else(|| invalid("history operation is unavailable for this user"))?;
        authorize(record)?;
        if record.status != HistoryOperationStatus::Preparing {
            if record.status == HistoryOperationStatus::Stopping {
                self.reconcile_history_stop()?;
            }
            return Ok(self.history_operations.find(&sender, id).unwrap().clone());
        }
        let active = record.request.active.as_ref();
        if active.is_none() || active.is_some_and(|lease| self.completions.get(lease).is_some()) {
            return self.record_history_stopped(sender, id);
        }
        let control = ControlRequest {
            id: Uuid::new_v4(),
            sender: sender.clone(),
            target: record.request.flow.clone(),
            expected_revision: self.state.revision(),
            pause_key: Uuid::new_v4().into(),
            action: ControlAction::CancelTurn,
        };
        self.history_operations
            .validate_stopping(&sender, id, &control, &self.state)?;
        let candidate = self.controls.prepare(&self.state, &control)?;
        let at = Utc::now();
        let event = HistoryOperationEvent::Stopping {
            sender,
            id,
            control: Box::new(control.clone()),
            at,
        };
        let update = ControlUpdate {
            request: control,
            at,
        };
        let receipt = self.journal.commit(&[
            FlowFact::HistoryOperation(Box::new(event.clone())),
            FlowFact::Controlled(Box::new(update.clone())),
        ])?;
        self.publish_state(candidate.clone(), receipt.clone());
        self.controls.apply(update, candidate, receipt.clone());
        Ok(self.history_operations.apply(event, receipt))
    }

    /// Owner reconciliation observes the original completion and never sends another cancellation.
    pub(crate) fn reconcile_history_stop(&mut self) -> io::Result<()> {
        let Some(record) = self.history_operations.pending() else {
            return Ok(());
        };
        if record.status != HistoryOperationStatus::Stopping {
            return Ok(());
        }
        let Some(lease) = record.request.active.as_ref() else {
            return Err(invalid(
                "stopping history operation has no reviewed invocation",
            ));
        };
        if self.completions.get(lease).is_none() {
            return Ok(());
        }
        // Closure is independent of history. It must remain observable and abortable,
        // but cannot become a ready-to-select open conversation.
        if !matches!(self.state.state(), InstanceState::Open(_)) {
            return Ok(());
        }
        self.record_history_stopped(record.request.sender.clone(), record.request.id)?;
        Ok(())
    }

    fn record_history_stopped(
        &mut self,
        sender: SenderIdentity,
        id: Uuid,
    ) -> io::Result<HistoryOperationRecord> {
        let completion = self.history_operations.stopped_completion(
            &sender,
            id,
            &self.state,
            &self.completions,
        )?;
        let event = HistoryOperationEvent::ExecutionStopped {
            sender,
            id,
            completion,
            at: Utc::now(),
        };
        let receipt = self
            .journal
            .commit(&[FlowFact::HistoryOperation(Box::new(event.clone()))])?;
        self.cursor = receipt.clone();
        Ok(self.history_operations.apply(event, receipt))
    }
}
