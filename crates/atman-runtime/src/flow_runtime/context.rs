//! Weak context binding to the flow's existing serialized writer.

use std::{
    io,
    sync::{Arc, Weak},
};

use super::{Command, StorageFailure, request, storage_error};
use crate::{
    flow_store::{
        FlowRef,
        context::ContextCheckpoint,
        inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity},
    },
    form::DeferredFormAnswer,
    session::Session,
};

#[derive(Clone)]
pub(crate) struct ContextWriter {
    commands: tokio::sync::mpsc::WeakSender<Command>,
    context: Weak<Session>,
    failure: Arc<StorageFailure>,
    admission: Arc<super::host::SessionAdmission>,
}

impl ContextWriter {
    pub async fn apply_effect<T: Send + 'static>(
        &self,
        effect: impl FnOnce() -> T + Send + 'static,
    ) -> io::Result<T> {
        let commands = self.commands.upgrade().ok_or_else(|| {
            storage_error(
                &self.failure,
                io::Error::other("Flow effect owner is unavailable"),
            )
        })?;
        request(&commands, &self.failure, &self.admission, move |_| {
            Ok(effect())
        })
        .await
    }

    pub(crate) fn history_head(&self) -> Option<crate::flow_store::branches::HistoryHead> {
        self.context.upgrade()?.history_head()
    }

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
        let delivery = form_delivery(address, answer);
        request(&commands, &self.failure, &self.admission, move |store| {
            store.accept_input(delivery, |_| Ok(())).map(|_| ())
        })
        .await
    }

    pub(super) fn new(
        commands: tokio::sync::mpsc::WeakSender<Command>,
        context: Weak<Session>,
        failure: Arc<StorageFailure>,
        admission: Arc<super::host::SessionAdmission>,
    ) -> Self {
        Self {
            commands,
            context,
            failure,
            admission,
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
        request(&commands, &self.failure, &self.admission, move |store| {
            let previous = store.cursor().sequence();
            let committed = store.checkpoint_context(checkpoint)?;
            if committed.receipt.sequence() > previous {
                context.project_checkpoint(&committed.checkpoint);
            }
            Ok(())
        })
        .await
    }
}

pub(crate) fn form_delivery(address: FlowRef, answer: DeferredFormAnswer) -> DeliveryRequest {
    let request_id = form_delivery_id(&address, &answer.prompt_id);
    DeliveryRequest {
        expected_head: answer.expected_head.clone(),
        request_id,
        sender: SenderIdentity::Host(address.instance_id.0.0),
        recipient: address,
        delivery: DeliveryMode::AtBoundary,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::FormReply { answer },
    }
}

pub(crate) fn form_delivery_id(address: &FlowRef, prompt: &str) -> uuid::Uuid {
    // A prompt is an opaque transport identity. Namespace it by owner and purpose for retry reconciliation.
    let mut hash = blake3::Hasher::new();
    hash.update(b"atman.form.reply\0");
    hash.update(address.instance_id.to_string().as_bytes());
    hash.update(prompt.as_bytes());
    uuid::Uuid::from_bytes(hash.finalize().as_bytes()[..16].try_into().unwrap())
}
