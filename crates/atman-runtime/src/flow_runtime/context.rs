//! Weak context binding to the flow's existing serialized writer.

use std::{
    io,
    sync::{Arc, Weak},
};

use super::{Command, StorageFailure, request, storage_error};
use crate::{flow_store::context::ContextCheckpoint, session::Session};

#[derive(Clone)]
pub(crate) struct ContextWriter {
    commands: tokio::sync::mpsc::WeakSender<Command>,
    context: Weak<Session>,
    failure: Arc<StorageFailure>,
}

impl ContextWriter {
    pub async fn record_form(&self, answer: crate::form::DeferredFormAnswer) -> io::Result<()> {
        let commands = self.commands.upgrade().ok_or_else(|| {
            storage_error(
                &self.failure,
                io::Error::other("flow storage writer is unavailable"),
            )
        })?;
        let context = self
            .context
            .upgrade()
            .ok_or_else(|| io::Error::other("flow context is unavailable"))?;
        let address = context
            .flow_instance()
            .cloned()
            .ok_or_else(|| io::Error::other("form reply requires an instance owner"))?;
        // A prompt is an opaque transport identity. Namespace it by owner and purpose for retry reconciliation.
        let mut hash = blake3::Hasher::new();
        hash.update(b"atman.form.reply\0");
        hash.update(address.instance_id.to_string().as_bytes());
        hash.update(answer.prompt_id.as_bytes());
        let request_id =
            uuid::Uuid::from_bytes(hash.finalize().as_bytes()[..16].try_into().unwrap());
        let delivery = crate::flow_store::inbox::DeliveryRequest {
            request_id,
            sender: crate::flow_store::inbox::SenderIdentity::Host(address.instance_id.0.0),
            recipient: address,
            delivery: crate::flow_store::inbox::DeliveryMode::AtBoundary,
            cause: None,
            correlation: None,
            scope: None,
            payload: crate::flow_store::inbox::InboxPayload::FormReply { answer },
        };
        request(&commands, &self.failure, move |store| {
            store.accept_input(delivery, |_| Ok(())).map(|_| ())
        })
        .await
    }

    pub(super) fn new(
        commands: tokio::sync::mpsc::WeakSender<Command>,
        context: Weak<Session>,
        failure: Arc<StorageFailure>,
    ) -> Self {
        Self {
            commands,
            context,
            failure,
        }
    }

    pub async fn commit(&self, checkpoint: ContextCheckpoint) -> io::Result<()> {
        let commands = self.commands.upgrade().ok_or_else(|| {
            storage_error(
                &self.failure,
                io::Error::other("flow storage writer is unavailable"),
            )
        })?;
        let context = self
            .context
            .upgrade()
            .ok_or_else(|| io::Error::other("flow context is unavailable"))?;
        request(&commands, &self.failure, move |store| {
            let previous = store.cursor().sequence;
            let committed = store.checkpoint_context(checkpoint)?;
            if committed.receipt.sequence > previous {
                context.project_checkpoint(&committed.checkpoint);
            }
            Ok(())
        })
        .await
    }
}
