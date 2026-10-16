//! Weak context binding to the flow's existing serialized writer.

use std::{io, sync::Weak};

use super::{Command, request};
use crate::{flow_store::context::ContextCheckpoint, session::Session};

pub(crate) struct ContextWriter {
    commands: tokio::sync::mpsc::WeakSender<Command>,
    context: Weak<Session>,
}

impl ContextWriter {
    pub(super) fn new(
        commands: tokio::sync::mpsc::WeakSender<Command>,
        context: Weak<Session>,
    ) -> Self {
        Self { commands, context }
    }

    pub async fn commit(&self, checkpoint: ContextCheckpoint) -> io::Result<()> {
        let commands = self
            .commands
            .upgrade()
            .ok_or_else(|| io::Error::other("flow storage writer is unavailable"))?;
        let context = self
            .context
            .upgrade()
            .ok_or_else(|| io::Error::other("flow context is unavailable"))?;
        request(&commands, move |store| {
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
