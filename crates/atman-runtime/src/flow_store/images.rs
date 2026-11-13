//! Batch-local attachment ownership without changing authored request identities.

use std::{io, path::Path};

use serde::{Deserialize, Serialize};

use super::{FlowFact, invalid};
use crate::{
    attachment_store::{AttachmentStore, image_bytes},
    flow_store::position::FlowBatch,
    message::{ImageData, ImageSource, Message, MessagePart},
};

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct RetainedImage {
    source: ImageSource,
    id: String,
}

/// Files are durable before their bindings and the original facts are committed together.
pub(super) fn retain(dir: &Path, mut facts: Vec<FlowFact>) -> io::Result<Vec<FlowFact>> {
    let store = AttachmentStore::at(dir);
    let mut bindings: Vec<RetainedImage> = Vec::new();
    visit(&mut facts, &mut |source| {
        if bindings.iter().any(|binding| binding.source == *source) {
            return Ok(());
        }
        let bytes = image_bytes(source).map_err(invalid)?;
        if matches!(source.data, ImageData::Base64 { .. }) {
            return Ok(());
        }
        let retained = store.import_bytes(&bytes, None).map_err(invalid)?;
        let ImageData::Artifact { id, .. } = retained.data else {
            return Err(invalid("persistent flow requires owned image storage"));
        };
        bindings.push(RetainedImage {
            source: source.clone(),
            id,
        });
        Ok(())
    })?;
    if !bindings.is_empty() {
        facts.push(FlowFact::ImagesRetained(bindings));
    }
    Ok(facts)
}

pub(super) fn split(facts: &[FlowFact]) -> io::Result<(&[FlowFact], &[RetainedImage])> {
    let (body, bindings) = match facts.last() {
        Some(FlowFact::ImagesRetained(bindings)) if !bindings.is_empty() => {
            (&facts[..facts.len() - 1], bindings.as_slice())
        }
        _ => (facts, &[][..]),
    };
    if body
        .iter()
        .any(|fact| matches!(fact, FlowFact::ImagesRetained(_)))
    {
        return Err(invalid("image bindings must end their owning batch"));
    }
    Ok((body, bindings))
}

fn owned(dir: &Path, binding: &RetainedImage) -> io::Result<ImageSource> {
    if binding.id.len() != 64
        || !binding
            .id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(invalid("invalid retained image identity"));
    }
    let name = match &binding.source.data {
        ImageData::Artifact { id, name, .. } if *id == binding.id => name.clone(),
        ImageData::Path { path } => path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned),
        _ => return Err(invalid("retained image does not match its source identity")),
    };
    let extension = match binding.source.media_type.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return Err(invalid("unsupported retained image media type")),
    };
    Ok(ImageSource {
        media_type: binding.source.media_type.clone(),
        detail: binding.source.detail,
        data: ImageData::Artifact {
            id: binding.id.clone(),
            path: dir
                .join("attachments")
                .join(format!("{}.{extension}", binding.id)),
            name,
        },
    })
}

/// Resolve only typed image fields; arbitrary JSON and original fingerprints remain opaque.
pub(super) fn resolve(dir: &Path, facts: &[FlowFact]) -> io::Result<Vec<FlowFact>> {
    let (body, bindings) = split(facts)?;
    let mut used = vec![false; bindings.len()];
    let mut result = body.to_vec();
    visit(&mut result, &mut |source| {
        if matches!(source.data, ImageData::Base64 { .. }) {
            return Ok(());
        }
        let mut matches = bindings
            .iter()
            .enumerate()
            .filter(|(_, binding)| binding.source == *source);
        let (index, binding) = matches
            .next()
            .ok_or_else(|| invalid("file image has no committed ownership binding"))?;
        if matches.next().is_some() {
            return Err(invalid("duplicate retained image binding"));
        }
        used[index] = true;
        *source = owned(dir, binding)?;
        Ok(())
    })?;
    if used.iter().any(|used| !used) {
        return Err(invalid("retained image is absent from its owning facts"));
    }
    Ok(result)
}

pub(super) fn resolve_batch(
    dir: &Path,
    mut batch: FlowBatch<FlowFact>,
) -> io::Result<FlowBatch<FlowFact>> {
    batch.facts = resolve(dir, &batch.facts)?;
    Ok(batch)
}

pub(super) fn messages(
    messages: &mut [Message],
    visit: &mut impl FnMut(&mut ImageSource) -> io::Result<()>,
) -> io::Result<()> {
    for message in messages {
        for part in &mut message.parts {
            match part {
                MessagePart::Image { source } => visit(source)?,
                MessagePart::ContextRecord(record) => {
                    record.visit_images(visit)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Transient facts must own their image bytes before entering the memory journal.
pub(super) fn validate_transient(facts: &mut [FlowFact]) -> io::Result<()> {
    visit(facts, &mut |source| {
        if !matches!(source.data, ImageData::Base64 { .. }) {
            return Err(invalid(
                "transient images require an inline captured source",
            ));
        }
        image_bytes(source).map(|_| ()).map_err(invalid)
    })
}

fn user(
    user: &mut super::inbox::UserContent,
    visit: &mut impl FnMut(&mut ImageSource) -> io::Result<()>,
) -> io::Result<()> {
    for source in &mut user.images {
        visit(source)?;
    }
    Ok(())
}

fn payload(
    payload: &mut super::inbox::InboxPayload,
    visit: &mut impl FnMut(&mut ImageSource) -> io::Result<()>,
) -> io::Result<()> {
    use super::inbox::InboxPayload as P;
    match payload {
        P::Command {
            user: Some(content),
            ..
        }
        | P::Invocation {
            user: Some(content),
            ..
        }
        | P::UserSubmission { content, .. } => user(content, visit),
        P::Command { user: None, .. }
        | P::Invocation { user: None, .. }
        | P::GoalResume { .. }
        | P::GoalContinuation { .. }
        | P::Interjection { .. }
        | P::FormReply { .. }
        | P::FlowEvent { .. }
        | P::Watcher { .. }
        | P::ProcessEvent { .. }
        | P::Timer { .. }
        | P::Internal { .. } => Ok(()),
    }
}

fn visit(
    facts: &mut [FlowFact],
    visit: &mut impl FnMut(&mut ImageSource) -> io::Result<()>,
) -> io::Result<()> {
    for fact in facts {
        match fact {
            FlowFact::Activated(record) => {
                messages(&mut record.input.inherited_context, visit)?;
                if let super::launch::EntryStart::Invoke {
                    user: Some(content),
                } = &mut record.input.request.start
                {
                    user(content, visit)?;
                }
            }
            FlowFact::EnvelopeAccepted(record) => payload(&mut record.request.payload, visit)?,
            FlowFact::SendQueued(record) => payload(&mut record.request.payload, visit)?,
            FlowFact::InboxEdited(record) => {
                if let super::inbox::InboxChange::Replace { content, .. } = &mut record.change {
                    user(content, visit)?;
                }
            }
            FlowFact::InboxRepairCancelled(_) => {}
            FlowFact::InputCommitted(record) => match &mut record.content {
                super::inbox::InputContent::Context { messages: items }
                | super::inbox::InputContent::InvocationInput {
                    messages: items, ..
                } => messages(items, visit)?,
                super::inbox::InputContent::WaitingContinuation { .. } => {}
            },
            FlowFact::MessagePublished(record) => {
                messages(std::slice::from_mut(&mut record.message), visit)?
            }
            FlowFact::ContextCheckpoint(record) => messages(&mut record.messages, visit)?,
            FlowFact::GoalUpdated(record) => {
                if let Some(input) = &mut record.input {
                    payload(&mut input.request.payload, visit)?;
                }
            }
            FlowFact::EventsObserved(record) => {
                for input in &mut record.envelopes {
                    payload(&mut input.request.payload, visit)?;
                }
            }
            FlowFact::FlowWatchesExpired(record) => {
                for input in &mut record.envelopes {
                    payload(&mut input.request.payload, visit)?;
                }
            }
            FlowFact::ProcessObserved(record) => {
                for input in &mut record.envelopes {
                    payload(&mut input.request.payload, visit)?;
                }
            }
            FlowFact::ProcessWatchesExpired(record) => {
                for input in &mut record.envelopes {
                    payload(&mut input.request.payload, visit)?;
                }
            }
            FlowFact::TimersFired(record) => {
                for input in &mut record.envelopes {
                    payload(&mut input.request.payload, visit)?;
                }
            }
            // Legacy records own their retained bytes. Execution facts reject message events.
            FlowFact::LegacyRecords(_)
            | FlowFact::ExecutionEvents(_)
            | FlowFact::Created(_)
            | FlowFact::WorkspaceRequested(_)
            | FlowFact::Transition(_)
            | FlowFact::Controlled(_)
            | FlowFact::TurnCompleted(_)
            | FlowFact::ReplyWaiterRegistered(_)
            | FlowFact::ReplyWaiterCancelled { .. }
            | FlowFact::PlanningUpdated(_)
            | FlowFact::NameUpdated(_)
            | FlowFact::FlowSubscribed(_)
            | FlowFact::FlowUnsubscribed(_)
            | FlowFact::ProcessWatchRegistered(_)
            | FlowFact::ProcessWatchCancelled(_)
            | FlowFact::WatcherReplaced(_)
            | FlowFact::TimerScheduled(_)
            | FlowFact::TimerCancelled(_)
            | FlowFact::TaskScopeCreated(_)
            | FlowFact::TaskScopeCancelled(_)
            | FlowFact::TaskScopeBound(_)
            | FlowFact::InputScopeInvalidated(_)
            | FlowFact::DeliveryConfirmed { .. }
            | FlowFact::DeliveryFailed { .. }
            | FlowFact::ImagesRetained(_)
            | FlowFact::HistoryHeadChanged(_)
            | FlowFact::HistoryOperation(_) => {}
        }
    }
    Ok(())
}
