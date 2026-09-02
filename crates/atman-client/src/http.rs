use std::fmt;

use atman_proto::{
    EventCursor, JsonRpcRequest, JsonRpcResponse, ProjectionEventEnvelope, SessionId,
};
use futures::{StreamExt, future::BoxFuture};
use reqwest::Url;

use crate::{RpcTransport, SessionEventStream, TransportError};

const MAX_SSE_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
    rpc_url: Url,
    bearer_token: String,
}

impl HttpTransport {
    pub fn new(base_url: &str, bearer_token: impl Into<String>) -> Result<Self, TransportError> {
        let mut rpc_url = Url::parse(base_url)
            .map_err(|error| TransportError::InvalidEndpoint(error.to_string()))?;
        let path = rpc_url.path().trim_end_matches('/');
        rpc_url.set_path(&format!("{path}/rpc"));
        Ok(Self {
            client: reqwest::Client::new(),
            rpc_url,
            bearer_token: bearer_token.into(),
        })
    }

    pub fn rpc_url(&self) -> &Url {
        &self.rpc_url
    }

    fn session_events_url(&self, session_id: &SessionId, after_cursor: EventCursor) -> Url {
        let mut url = self.rpc_url.clone();
        let path = url
            .path()
            .strip_suffix("/rpc")
            .unwrap_or_else(|| url.path())
            .to_owned();
        url.set_path(&format!("{path}/session-events"));
        url.set_query(None);
        url.query_pairs_mut()
            .append_pair("session_id", &session_id.to_string())
            .append_pair("after_cursor", &after_cursor.0.to_string());
        url
    }
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTransport")
            .field("rpc_url", &self.rpc_url)
            .field("bearer_token", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl RpcTransport for HttpTransport {
    fn send(
        &self,
        request: JsonRpcRequest,
    ) -> BoxFuture<'_, Result<JsonRpcResponse, TransportError>> {
        Box::pin(async move {
            let response = self
                .client
                .post(self.rpc_url.clone())
                .bearer_auth(&self.bearer_token)
                .json(&request)
                .send()
                .await?;
            let status = response.status();
            if !status.is_success() {
                let mut body = response.text().await?;
                body.truncate(4_096);
                return Err(TransportError::HttpStatus {
                    status: status.as_u16(),
                    body,
                });
            }
            Ok(response.json().await?)
        })
    }

    fn session_events(
        &self,
        session_id: SessionId,
        after_cursor: EventCursor,
    ) -> BoxFuture<'_, Result<Option<SessionEventStream>, TransportError>> {
        let client = self.client.clone();
        let url = self.session_events_url(&session_id, after_cursor);
        let bearer_token = self.bearer_token.clone();
        Box::pin(async move {
            let response = client
                .get(url)
                .bearer_auth(bearer_token)
                .header("Last-Event-ID", after_cursor.0.to_string())
                .send()
                .await?;
            let status = response.status();
            if !status.is_success() {
                let mut body = response.text().await?;
                body.truncate(4_096);
                return Err(TransportError::HttpStatus {
                    status: status.as_u16(),
                    body,
                });
            }
            let stream = futures::stream::try_unfold(
                EventStreamState {
                    response,
                    buffer: Vec::new(),
                },
                |mut state| async move {
                    loop {
                        if let Some(frame) = take_sse_frame(&mut state.buffer) {
                            if let Some(event) = parse_sse_frame(&frame)? {
                                return Ok(Some((event, state)));
                            }
                            continue;
                        }
                        match state.response.chunk().await? {
                            Some(chunk) => {
                                state.buffer.extend_from_slice(&chunk);
                                if state.buffer.len() > MAX_SSE_FRAME_BYTES {
                                    return Err(TransportError::InvalidEventStream(format!(
                                        "SSE frame exceeds {MAX_SSE_FRAME_BYTES} bytes"
                                    )));
                                }
                            }
                            None => return Ok(None),
                        }
                    }
                },
            )
            .boxed();
            Ok(Some(stream))
        })
    }
}

struct EventStreamState {
    response: reqwest::Response,
    buffer: Vec<u8>,
}

fn take_sse_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    let (position, delimiter_len) = match (lf, crlf) {
        (Some(lf), Some(crlf)) if lf <= crlf => (lf, 2),
        (Some(_), Some(crlf)) => (crlf, 4),
        (Some(lf), None) => (lf, 2),
        (None, Some(crlf)) => (crlf, 4),
        (None, None) => return None,
    };
    let frame = buffer[..position].to_vec();
    buffer.drain(..position + delimiter_len);
    Some(frame)
}

fn parse_sse_frame(frame: &[u8]) -> Result<Option<ProjectionEventEnvelope>, TransportError> {
    let frame = std::str::from_utf8(frame)
        .map_err(|error| TransportError::InvalidEventStream(error.to_string()))?;
    let mut event_name = None;
    let mut event_id = None;
    let mut data = Vec::new();
    for line in frame.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event_name = Some(value),
            "id" => event_id = Some(value),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() || event_name.is_some_and(|name| name != "session_event") {
        return Ok(None);
    }
    let event: ProjectionEventEnvelope = serde_json::from_str(&data.join("\n"))?;
    let id = event_id
        .ok_or_else(|| TransportError::InvalidEventStream("session event has no SSE id".into()))?
        .parse::<u64>()
        .map_err(|error| TransportError::InvalidEventStream(error.to_string()))?;
    if id != event.cursor.0 {
        return Err(TransportError::InvalidEventStream(format!(
            "SSE id {id} does not match envelope cursor {}",
            event.cursor.0
        )));
    }
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use atman_proto::{DaemonGeneration, PROJECTION_EVENT_SCHEMA_VERSION, ServerEvent};

    use super::*;

    #[test]
    fn base_url_is_normalized_to_rpc_endpoint() {
        let transport = HttpTransport::new("http://127.0.0.1:7777/api/", "secret").unwrap();
        assert_eq!(
            transport.rpc_url().as_str(),
            "http://127.0.0.1:7777/api/rpc"
        );
        assert!(!format!("{transport:?}").contains("secret"));
    }

    #[test]
    fn session_event_url_uses_projection_cursor() {
        let transport = HttpTransport::new("http://127.0.0.1:7777/api/", "secret").unwrap();
        let session_id = SessionId(uuid::Uuid::nil());
        let url = transport.session_events_url(&session_id, EventCursor(42));
        assert_eq!(url.path(), "/api/session-events");
        assert_eq!(
            url.query(),
            Some("session_id=00000000-0000-0000-0000-000000000000&after_cursor=42")
        );
    }

    #[test]
    fn sse_parser_accepts_split_safe_frames_and_checks_cursor_identity() {
        let envelope = ProjectionEventEnvelope {
            schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
            daemon_generation: DaemonGeneration("generation".into()),
            session_id: SessionId(uuid::Uuid::nil()),
            cursor: EventCursor(7),
            ts: chrono::Utc::now(),
            event: ServerEvent::Heartbeat,
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let frame = format!("event: session_event\nid: 7\ndata: {json}\n\n");
        let mut buffer = frame.into_bytes();
        let parsed = parse_sse_frame(&take_sse_frame(&mut buffer).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(parsed, envelope);
        assert!(buffer.is_empty());

        let mismatch = format!("id: 8\ndata: {json}\n\n");
        let mut buffer = mismatch.into_bytes();
        let error = parse_sse_frame(&take_sse_frame(&mut buffer).unwrap()).unwrap_err();
        assert!(error.to_string().contains("does not match"));
    }
}
