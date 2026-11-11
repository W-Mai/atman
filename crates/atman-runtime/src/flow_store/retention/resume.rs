//! Persistent scheduling admission after a retained conversation receives new user input.

use super::*;
use crate::flow_store::{FlowInstanceId, inbox::Acceptance};

impl SessionStore {
    /// Record the host-validated accepted user input that releases the retention scheduling gate.
    ///
    /// The host verifies this receipt against the live Flow inbox before entering the catalog operation. A repeated request observes the original admission rather than appending another fact.
    pub(crate) fn resume_retained_flow(
        &mut self,
        id: &FlowInstanceId,
        request: Uuid,
        accepted: Acceptance,
    ) -> io::Result<CommitReceipt> {
        self.reconcile_retention_catalog()?;
        let registration = self
            .projection
            .flows
            .iter()
            .find(|flow| &flow.id == id)
            .ok_or_else(|| invalid("retained Flow is not registered"))?;
        if registration
            .retention
            .as_ref()
            .is_none_or(|receipt| receipt.request != request)
        {
            return Err(invalid(
                "retained scheduling request does not match its transfer",
            ));
        }
        if registration.retention_resume.is_some() {
            return Ok(self.cursor.clone());
        }
        self.commit(vec![SessionFact::FlowRetentionResumed {
            id: id.clone(),
            request,
            accepted,
        }])
    }
}
