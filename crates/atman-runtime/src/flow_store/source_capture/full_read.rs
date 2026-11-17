//! Complete cold conversation reads without execution ownership or journal recovery.

use super::*;
use crate::flow_store::{RuntimeStore, publication::PublishedMessage};

pub(crate) struct RecordedFullRead {
    pub source: SourceSelection,
    pub origin: CapturedOrigin,
    pub messages: Vec<Message>,
    pub output: Option<Vec<PublishedMessage>>,
}

struct Capacity(usize);
impl io::Write for Capacity {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(invalid("conversation snapshot exceeds its read capacity"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl RuntimeStore {
    /// Run on a read worker while the original Session retains its source; no writer or cache is installed.
    pub(crate) fn capture_recorded_full_read(
        data: &std::path::Path,
        flow: &FlowRef,
        kind: FullReadKind,
        maximum: usize,
        expected_catalog: &CommitReceipt,
    ) -> io::Result<RecordedFullRead> {
        if maximum == 0 {
            return Err(invalid("conversation snapshot capacity must be positive"));
        }
        let inspected = Self::inspect_session(data, flow.session_id.0)?;
        if inspected.journal.trailing_bytes != 0
            || inspected.journal.cursor.as_ref() != Some(expected_catalog)
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "source catalog changed during snapshot preparation",
            ));
        }
        let mut recorded = crate::flow_store::read_registered_flow(
            &inspected.dir,
            &inspected.projection,
            &flow.instance_id,
            |_, _| Ok(()),
        )?;
        if recorded.journal.trailing_bytes != 0 {
            return Err(invalid("source Flow journal has incomplete trailing data"));
        }
        let (descriptor, _, inbox) = recorded
            .replay
            .projection
            .as_ref()
            .ok_or_else(|| invalid("recorded Flow creation is unavailable"))?;
        if &descriptor.address != flow {
            return Err(invalid("recorded snapshot belongs to another Flow"));
        }
        let source = SourceSelection {
            flow: flow.clone(),
            branch: inbox.head().branch,
            cursor: recorded
                .journal
                .cursor
                .clone()
                .ok_or_else(|| invalid("recorded Flow cursor is unavailable"))?,
        };
        let origin = CapturedOrigin {
            catalog: expected_catalog.clone(),
            session_project: inspected.projection.project.clone(),
            flow_project: recorded.project,
            configuration: inspected.projection.descriptor.configuration.clone(),
            execution: recorded
                .replay
                .activation
                .as_ref()
                .and_then(|(activation, _)| activation.input.execution.clone()),
        };
        let directory = inspected
            .dir
            .join("flows")
            .join(flow.instance_id.to_string());
        let (mut messages, mut output) = match kind {
            FullReadKind::Context => (recorded.replay.context_window, None),
            FullReadKind::Output => {
                let selected = recorded
                    .replay
                    .publications
                    .records()
                    .iter()
                    .filter(|publication| publication.is_shared_final())
                    .cloned()
                    .collect::<Vec<_>>();
                let mut output = Vec::with_capacity(selected.len());
                for selected in selected {
                    let batch = recorded
                        .reader
                        .read_batch::<FlowFact>(selected.receipt.require_durable()?)?;
                    let facts = crate::flow_store::images::resolve(&directory, &batch.facts)?;
                    let publication = facts
                        .into_iter()
                        .find_map(|fact| match fact {
                            FlowFact::MessagePublished(message) if message.id == selected.id => {
                                Some(PublishedMessage {
                                    publication: *message,
                                    receipt: selected.receipt.clone(),
                                })
                            }
                            _ => None,
                        })
                        .ok_or_else(|| {
                            invalid("recorded output is absent from its original batch")
                        })?;
                    output.push(publication);
                }
                (Vec::new(), Some(output))
            }
        };
        let mut remaining = maximum;
        let mut inline =
            |source: &mut crate::message::ImageSource| inline_full_image(source, &mut remaining);
        crate::flow_store::images::messages(&mut messages, &mut inline)?;
        if let Some(output) = &mut output {
            for publication in output {
                crate::flow_store::images::messages(
                    std::slice::from_mut(&mut publication.publication.message),
                    &mut inline,
                )?;
            }
        }
        let mut capacity = Capacity(maximum);
        serde_json::to_writer(&mut capacity, &messages).map_err(invalid)?;
        serde_json::to_writer(&mut capacity, &output).map_err(invalid)?;
        Ok(RecordedFullRead {
            source,
            origin,
            messages,
            output,
        })
    }
}
