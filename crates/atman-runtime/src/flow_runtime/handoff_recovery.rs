//! Explicit crash recovery after the authenticated coordinator lost both original owners.

use super::*;
use crate::peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan};

impl RuntimeHost {
    /// The trusted coordinator confirms the previous destination is dead and authenticates the original release or source loss; original writer locks remain mandatory.
    /// Recovered running turns become Interrupted, and no running scheduling policy is restored.
    pub async fn recover_handoff(
        &self,
        data: &std::path::Path,
        session_id: Uuid,
        source_generation: Uuid,
        replacement_request: Uuid,
        original_release: Option<&SessionHandoffReceipt>,
    ) -> io::Result<SessionHandoffReceipt> {
        if session_id.is_nil() || source_generation.is_nil() || replacement_request.is_nil() {
            return Err(invalid("handoff recovery identities must not be nil"));
        }
        let original = original_release.cloned();
        if original.as_ref().is_some_and(|receipt| {
            receipt.session_id != session_id
                || receipt.source_generation != source_generation
                || receipt.catalog.journal_id != session_id
        }) {
            return Err(invalid(
                "handoff recovery belongs to another original owner",
            ));
        }
        let data = data.to_owned();
        let host = self.clone();
        tokio::spawn(async move {
            let domain = host.peer_authority(session_id)?;
            let guard = Arc::new(
                domain
                    .acquire(
                        AuthorityPlan::new([(
                            AuthorityKey::Session(session_id),
                            AuthorityMode::Exclusive,
                        )])?,
                        tokio_util::sync::CancellationToken::new(),
                        std::time::Instant::now() + std::time::Duration::from_secs(15),
                    )
                    .await?,
            );
            let _opening = host.directory.opening.clone().lock_owned().await;
            if let Some(receipt) =
                host.installed_handoff(session_id, replacement_request, source_generation)?
            {
                return Ok(receipt);
            }
            if host
                .directory
                .sessions
                .lock()
                .unwrap()
                .contains_key(&session_id)
            {
                return Err(invalid(
                    "handoff recovery cannot replace a live Session owner",
                ));
            }
            let terminal: HashMap<_, _> = original
                .as_ref()
                .into_iter()
                .flat_map(|receipt| &receipt.flows)
                .filter_map(|flow| match &flow.scheduling {
                    Some(
                        status @ (HandoffScheduling::Stopped | HandoffScheduling::Failed { .. }),
                    ) => Some((flow.instance_id.clone(), status.clone())),
                    _ => None,
                })
                .collect();
            let expected_root = domain.data_root().to_owned();
            let (store, stores) = tokio::task::spawn_blocking(move || {
                if data.canonicalize()? != expected_root {
                    return Err(invalid("handoff recovery storage domain changed"));
                }
                let store = RuntimeStore::open(&data)?.open_session(session_id)?;
                let boundaries = if let Some(original) = original {
                    store.validate_handoff_ancestor(&original.catalog)?;
                    original
                        .flows
                        .iter()
                        .map(|flow| {
                            Ok((
                                flow.instance_id.clone(),
                                flow.cursor.require_durable()?.clone(),
                            ))
                        })
                        .collect::<io::Result<Vec<_>>>()?
                } else {
                    Vec::new()
                };
                let stores = store.recover_handoff_members(&boundaries)?;
                Ok::<_, io::Error>((store, stores))
            })
            .await
            .map_err(io::Error::other)??;
            let session = host.attach_session(store)?;
            let (reply, received) = oneshot::channel();
            session
                .commands
                .send(CatalogEnvelope {
                    command: CatalogCommand::InstallRecoveredHandoff {
                        request_id: replacement_request,
                        source_generation,
                        stores,
                        terminal,
                        reply,
                    },
                    admission: None,
                    authority: Some(guard),
                })
                .await
                .map_err(|_| invalid("recovery owner stopped before installation"))?;
            received
                .await
                .map_err(|_| invalid("recovery installation did not acknowledge"))?
        })
        .await
        .map_err(io::Error::other)?
    }
}
