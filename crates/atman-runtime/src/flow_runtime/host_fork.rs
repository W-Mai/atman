//! Publish prepared Fork data under the existing destination Session owner.

use super::{CatalogCommand, HostOperationToken, RuntimeHost};
use crate::{
    event_log::journal::CommitReceipt,
    flow_runtime::source_capture::{PreparedForkImport, SourceCapture},
    flow_store::{
        FlowDescriptor, FlowRef, SessionStore, invalid, launch::LaunchInput,
        source_capture::ForkImportPlan,
    },
};
use std::io;

/// Trusted host input after the user has reviewed the source and destination binding.
pub struct ForkPublicationRequest {
    pub expected_source_catalog: CommitReceipt,
    pub captured: SourceCapture<PreparedForkImport>,
    pub plan: ForkImportPlan,
    pub input: LaunchInput,
}

/// A complete destination publication and its original Waiting activation receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkDestinationReceipt {
    pub destination: FlowRef,
    pub activation: CommitReceipt,
}

type Authorize = Box<dyn FnOnce(&FlowDescriptor, &LaunchInput) -> io::Result<()> + Send>;

pub(super) struct PublishFork {
    request: ForkPublicationRequest,
    authorize: Authorize,
    _source: tokio::sync::OwnedRwLockReadGuard<()>,
    _host: HostOperationToken,
}

impl PublishFork {
    pub(super) fn apply(
        self,
        destination: &mut SessionStore,
    ) -> io::Result<ForkDestinationReceipt> {
        let publication = destination.create_seeded_fork_import(
            self.request.plan,
            self.request.captured.product.entry(),
            self.request.captured.product.snapshot(),
            self.request.input,
            self.authorize,
        )?;
        Ok(ForkDestinationReceipt {
            destination: publication.destination,
            activation: publication.destination_receipt,
        })
    }
}

impl RuntimeHost {
    /// Accept publication through the existing catalog owner. A queued publication keeps its
    /// host admission and source fence after the requesting observer disappears.
    pub async fn publish_fork_import(
        &self,
        request: ForkPublicationRequest,
        authorize: impl FnOnce(&FlowDescriptor, &LaunchInput) -> io::Result<()> + Send + 'static,
    ) -> io::Result<ForkDestinationReceipt> {
        request.plan.validate()?;
        let source = self.session(request.plan.source.flow.session_id.0)?;
        let destination = self.session(request.plan.destination.session_id.0)?;
        let host = self.retain_host_operation()?;
        let fence = source.retain_source().await?;
        let current = source.snapshot().await?;
        if request.expected_source_catalog != current.cursor
            || request.captured.origin.catalog != current.cursor
            || request.captured.origin.session_project != current.catalog.project
        {
            return Err(invalid("Fork source catalog changed after review"));
        }
        destination
            .request(|reply| CatalogCommand::PublishFork {
                publication: Box::new(PublishFork {
                    request,
                    authorize: Box::new(authorize),
                    _source: fence,
                    _host: host,
                }),
                reply,
            })
            .await
    }
}
