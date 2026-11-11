//! Durable re-admission of retained scheduling through a verified later user acceptance.

use super::*;

impl CatalogRetention {
    pub(in crate::flow_runtime::host) async fn resume_catalog(
        &mut self,
        mut store: SessionStore,
        request: ResumeRequest,
        bindings: CatalogBindings<'_>,
    ) -> Result<(SessionStore, io::Result<()>), tokio::task::JoinError> {
        let ResumeRequest {
            target,
            id: request,
            accepted,
            authorize,
        } = request;
        let checked = (|| {
            authorize(&target)?;
            if self.pending.contains_key(&target.instance_id) {
                return Err(invalid(
                    "Keep must be reconciled before restoring scheduling",
                ));
            }
            let worker = bindings
                .flows
                .lock()
                .unwrap()
                .get(&target.instance_id)
                .cloned()
                .filter(|worker| {
                    !worker.is_transient() && !worker.is_stopped() && worker.address() == &target
                })
                .ok_or_else(|| invalid("retained owner is unavailable"))?;
            let retained = store
                .projection()
                .flows
                .iter()
                .find(|flow| flow.id == target.instance_id)
                .and_then(|flow| flow.retention.clone())
                .filter(|receipt| receipt.request == request && receipt.flow == target)
                .ok_or_else(|| invalid("retained scheduling request does not match the catalog"))?;
            Ok((worker, retained))
        })();
        let (worker, retained) = match checked {
            Ok(checked) => checked,
            Err(error) => return Ok((store, Err(error))),
        };
        let owner = target.clone();
        let permission = authorize.clone();
        let source = FlowPosition::Transient {
            transient: retained.source.clone(),
        };
        let (returned, reconciled) = tokio::task::spawn_blocking(move || {
            let result = permission(&owner).and_then(|_| store.retention_receipt(request, &source));
            (store, result)
        })
        .await?;
        store = returned;
        *bindings.access.write().unwrap() = store.projection().access.clone();
        match reconciled {
            Ok(Some(receipt)) if receipt == retained => {}
            Ok(_) => {
                return Ok((
                    store,
                    Err(invalid("retention registration changed before scheduling")),
                ));
            }
            Err(error) => return Ok((store, Err(error))),
        }
        let permission = authorize.clone();
        let verified = worker
            .request(move |store| {
                permission(store.state().address())?;
                let entry = store.inbox_entry(accepted.message_id)?.ok_or_else(|| {
                    invalid("retained scheduling requires an accepted user input")
                })?;
                if entry.acceptance.receipt != accepted.receipt
                    || accepted.receipt.require_durable()?.journal_id
                        != retained.destination.journal_id
                    || accepted.receipt.sequence() <= retained.destination.sequence
                    || !entry.envelope.request.is_editable_user_input()
                {
                    return Err(invalid(
                        "retained scheduling requires a new user acceptance after Keep",
                    ));
                }
                Ok(entry.acceptance)
            })
            .await;
        let accepted = match verified {
            Ok(accepted) => accepted,
            Err(error) => return Ok((store, Err(error))),
        };
        let permission = authorize.clone();
        let owner = target.clone();
        let (returned, result) = tokio::task::spawn_blocking(move || {
            let result = permission(&owner)
                .and_then(|_| store.resume_retained_flow(&owner.instance_id, request, accepted));
            (store, result)
        })
        .await?;
        store = returned;
        *bindings.access.write().unwrap() = store.projection().access.clone();
        let result =
            result.and_then(|_| self.resume(&target, request, authorize, &store, bindings));
        Ok((store, result))
    }
}
