//! Historical prefix validation for explicit owner-loss recovery.

use super::{FlowInstanceId, FlowStore, SessionStore, invalid};
use crate::event_log::journal::CommitReceipt;
use std::io;

impl SessionStore {
    pub(crate) fn validate_handoff_ancestor(&self, original: &CommitReceipt) -> io::Result<()> {
        self.journal.previous_receipt(original).map(|_| ())
    }

    pub(crate) fn recover_handoff_members(
        &self,
        boundaries: &[(FlowInstanceId, CommitReceipt)],
    ) -> io::Result<Vec<FlowStore>> {
        let mut seen = std::collections::HashSet::new();
        for (flow, receipt) in boundaries {
            if !seen.insert(flow.clone())
                || receipt.journal_id != flow.0.0
                || !self
                    .projection
                    .flows
                    .iter()
                    .any(|registered| &registered.id == flow)
            {
                return Err(invalid("handoff recovery contains a foreign Flow journal"));
            }
        }
        let mut stores = self
            .projection
            .flows
            .iter()
            .map(|flow| self.restore_flow(&flow.id))
            .collect::<io::Result<Vec<_>>>()?;
        for (flow, receipt) in boundaries {
            let store = stores
                .iter()
                .find(|store| &store.descriptor.address.instance_id == flow)
                .ok_or_else(|| invalid("handoff recovery Flow is unavailable"))?;
            store.journal.previous_durable_receipt(receipt)?;
        }
        // Validate all original boundaries before normal crash recovery can append a new fact.
        for store in &mut stores {
            store.interrupt_lost_turn()?;
        }
        Ok(stores)
    }
}
