use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::event::EventSink;
use crate::event_writer::EventWriter;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub struct Session {
    id: SessionId,
    dir: PathBuf,
    writer: Option<EventWriter>,
    sink: EventSink,
}

impl Session {
    pub fn open(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let id = SessionId::now();
        let dir = root.as_ref().join("sessions").join(id.to_string());
        let writer = EventWriter::spawn(&dir)?;
        let sink = EventSink::new().with_forwarder(writer.sender());
        Ok(Self {
            id,
            dir,
            writer: Some(writer),
            sink,
        })
    }

    pub fn open_ephemeral() -> Self {
        Self {
            id: SessionId::now(),
            dir: PathBuf::new(),
            writer: None,
            sink: EventSink::new(),
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn events_path(&self) -> Option<&Path> {
        self.writer.as_ref().map(|w| w.events_path())
    }

    pub fn sink(&self) -> &EventSink {
        &self.sink
    }

    pub async fn shutdown(mut self) {
        if let Some(writer) = self.writer.take() {
            writer.shutdown().await;
        }
    }
}
