use std::io::{Error, ErrorKind};
use std::path::Path;

use atman_proto::{EVENT_SCHEMA_VERSION, EventCursor, GetEventsResponse, ServerEventEnvelope};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};

pub(crate) const MAX_EVENT_PAGE_SIZE: usize = 1_000;

pub(crate) struct EventLogReader {
    reader: BufReader<tokio::fs::File>,
    physical_line: u64,
    last_cursor: EventCursor,
}

impl EventLogReader {
    pub(crate) async fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            reader: BufReader::new(tokio::fs::File::open(path).await?),
            physical_line: 0,
            last_cursor: EventCursor::default(),
        })
    }

    pub(crate) async fn next(&mut self) -> std::io::Result<Option<ServerEventEnvelope>> {
        loop {
            let start = self.reader.stream_position().await?;
            let mut line = String::new();
            if self.reader.read_line(&mut line).await? == 0 {
                return Ok(None);
            }
            if !line.ends_with('\n') {
                self.reader.seek(SeekFrom::Start(start)).await?;
                return Ok(None);
            }
            self.physical_line = self.physical_line.saturating_add(1);
            let text = line.trim();
            if text.is_empty() {
                continue;
            }
            let event: serde_json::Value = serde_json::from_str(text).map_err(|error| {
                Error::new(
                    ErrorKind::InvalidData,
                    format!("invalid event JSON at line {}: {error}", self.physical_line),
                )
            })?;
            let cursor = EventCursor(
                event
                    .get("seq")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(self.physical_line)
                    .max(self.last_cursor.0.saturating_add(1)),
            );
            self.last_cursor = cursor;
            return Ok(Some(ServerEventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION,
                cursor,
                event,
            }));
        }
    }
}

pub(crate) async fn read_event_page(
    path: &Path,
    since: EventCursor,
    limit: usize,
) -> std::io::Result<GetEventsResponse> {
    let limit = limit.min(MAX_EVENT_PAGE_SIZE);
    let mut reader = EventLogReader::open(path).await?;
    let mut events = Vec::with_capacity(limit.saturating_add(1));
    while let Some(event) = reader.next().await? {
        if event.cursor > since {
            events.push(event);
            if events.len() > limit {
                break;
            }
        }
    }
    let has_more = events.len() > limit;
    events.truncate(limit);
    let next_cursor = events.last().map(|event| event.cursor).unwrap_or(since);
    Ok(GetEventsResponse {
        events,
        next_cursor,
        has_more,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn page_uses_persisted_sequences_and_reports_more() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        tokio::fs::write(
            &path,
            "{\"type\":\"one\",\"seq\":10}\n{\"type\":\"two\",\"seq\":20}\n{\"type\":\"three\",\"seq\":30}\n",
        )
        .await
        .unwrap();
        let page = read_event_page(&path, EventCursor(10), 1).await.unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].cursor, EventCursor(20));
        assert_eq!(page.next_cursor, EventCursor(20));
        assert!(page.has_more);
    }

    #[tokio::test]
    async fn partial_eof_line_is_replayed_after_completion() {
        use tokio::io::AsyncWriteExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        tokio::fs::write(&path, "{\"type\":\"one\",\"seq\":1}")
            .await
            .unwrap();
        let mut reader = EventLogReader::open(&path).await.unwrap();
        assert!(reader.next().await.unwrap().is_none());
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        file.write_all(b"\n").await.unwrap();
        file.flush().await.unwrap();
        let event = reader.next().await.unwrap().unwrap();
        assert_eq!(event.cursor, EventCursor(1));
        assert_eq!(event.event["type"], "one");
    }
}
