//! Operator history queries over committed Flow transcripts and retained project indexes.

use std::{
    collections::VecDeque,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use anyhow::{Context, Result, ensure};
use uuid::Uuid;

use crate::{
    flow_store::view::FlowUpdate,
    flow_store::{FlowRef, RuntimeStore},
    index::AnchorIndex,
    message::{Message, MessageOrigin, MessagePart},
    projection::flow::FlowRecordId,
    session_meta::{SessionMeta, fingerprint_from_root},
};

const LIMIT: usize = 50;
const PREVIEW_CHARS: usize = 12_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchScope {
    #[default]
    Session,
    Project,
}

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub query: String,
    pub scope: SearchScope,
}

#[derive(Clone, Debug)]
pub enum SearchSource {
    Instance(FlowRef),
    Legacy(Uuid),
}

/// Locations retain their storage identity even when Session IDs or sequence numbers match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryLocation {
    Indexed {
        index: PathBuf,
        session: String,
        seq: u64,
    },
    Flow {
        flow: FlowRef,
        record: FlowRecordId,
        message: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryHit {
    pub location: HistoryLocation,
    pub session_id: String,
    pub seq: u64,
    pub ts: String,
    pub kind: String,
    pub snippet: String,
    pub preview: Vec<String>,
}

#[derive(Default)]
pub struct SearchResults {
    pub hits: Vec<HistoryHit>,
    pub warnings: Vec<String>,
}

struct Record {
    hit: HistoryHit,
    text: String,
}

/// Serializes storage scans across views; cancelled waiters never start blocking work.
#[derive(Clone)]
pub struct SearchService {
    permit: Arc<Semaphore>,
}

impl Default for SearchService {
    fn default() -> Self {
        Self {
            permit: Arc::new(Semaphore::new(1)),
        }
    }
}

impl SearchService {
    pub async fn search(
        &self,
        data: PathBuf,
        source: SearchSource,
        request: SearchRequest,
        cancellation: CancellationToken,
    ) -> Result<SearchResults> {
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let permit = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "History search cancelled").into()),
            permit = self.permit.clone().acquire_owned() => permit?,
        };
        tokio::task::spawn_blocking(move || {
            // Keep the permit until the blocking scan actually exits, even if its caller disappears.
            let _permit = permit;
            search(&data, &source, &request, &cancellation)
        })
        .await?
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> io::Result<()> {
    if cancellation.is_cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "History search cancelled",
        ))
    } else {
        Ok(())
    }
}

/// Retains only the latest hits and three bounded neighbors on either side.
#[derive(Default)]
struct PreviewWindow {
    before: VecDeque<String>,
    hits: VecDeque<(HistoryHit, usize)>,
}

impl PreviewWindow {
    fn push(&mut self, mut record: Record, matcher: &regex::Regex, flow: &FlowRef) {
        let line = preview_line(false, &record.hit.kind, record.hit.seq, &record.text);
        for (hit, remaining) in &mut self.hits {
            if *remaining > 0 {
                hit.preview.push(line.clone());
                *remaining -= 1;
            }
        }
        if matcher.is_match(&record.text) {
            record.hit.preview.push(format!(
                "Session {} · Flow {}",
                flow.session_id, flow.instance_id
            ));
            record.hit.preview.extend(self.before.iter().cloned());
            record.hit.preview.push(preview_line(
                true,
                &record.hit.kind,
                record.hit.seq,
                &record.text,
            ));
            self.hits.push_back((record.hit, 3));
            if self.hits.len() > LIMIT {
                self.hits.pop_front();
            }
        }
        self.before.push_back(line);
        if self.before.len() > 3 {
            self.before.pop_front();
        }
    }
}

/// Read without attaching execution, creating an index, or changing original history. Call outside the UI event loop.
pub fn search(
    data: &Path,
    source: &SearchSource,
    request: &SearchRequest,
    cancellation: &CancellationToken,
) -> Result<SearchResults> {
    check_cancelled(cancellation)?;
    let query = request.query.trim();
    ensure!(!query.is_empty(), "Enter a history search query");
    let pattern = if query.len() >= 2 && query.starts_with('/') && query.ends_with('/') {
        query[1..query.len() - 1].to_owned()
    } else {
        regex::escape(query)
    };
    let matcher = regex::RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()?;
    let (current, project, mut index_dirs) = match source {
        SearchSource::Instance(flow) => {
            let read = RuntimeStore::inspect_session(data, flow.session_id.0)?;
            let registration = read
                .projection
                .flows
                .iter()
                .find(|entry| entry.id == flow.instance_id)
                .context("Search source Flow is no longer registered")?;
            (
                flow.session_id.0,
                registration
                    .project
                    .as_ref()
                    .map(|project| project.root().to_owned()),
                registration
                    .project
                    .as_ref()
                    .map(|project| vec![project.storage_root().to_owned()])
                    .unwrap_or_default(),
            )
        }
        SearchSource::Legacy(id) => {
            let meta = SessionMeta::load(&data.join("sessions").join(id.to_string()))
                .context("Original history has no readable project metadata")?;
            let project = meta.project_root;
            let indexes = project
                .as_ref()
                .map(|root| vec![root.join(".atman")])
                .unwrap_or_default();
            (*id, project, indexes)
        }
    };
    if request.scope == SearchScope::Project {
        ensure!(
            project.is_some(),
            "This conversation has no project history scope"
        );
    }
    if let Some(project) = &project {
        index_dirs.push(project.join(".atman"));
        index_dirs.push(data.join("projects").join(fingerprint_from_root(project)));
    }
    for directory in &mut index_dirs {
        *directory = crate::fs_access::canonicalize_stable(directory);
    }
    index_dirs.sort();
    index_dirs.dedup();
    let mut results = SearchResults::default();
    let mut ranked = Vec::new();
    if matches!(source, SearchSource::Instance(_)) || request.scope == SearchScope::Project {
        let ids = if request.scope == SearchScope::Session {
            vec![current]
        } else {
            RuntimeStore::session_ids(data)?
        };
        for id in ids {
            check_cancelled(cancellation)?;
            let read = match RuntimeStore::inspect_session(data, id) {
                Ok(read) => read,
                Err(error) => {
                    results.warnings.push(format!("Session {id}: {error}"));
                    continue;
                }
            };
            for registration in &read.projection.flows {
                if request.scope == SearchScope::Project
                    && registration.project.as_ref().map(|binding| binding.root())
                        != project.as_deref()
                {
                    continue;
                }
                let flow = FlowRef {
                    session_id: id.into(),
                    instance_id: registration.id.clone(),
                };
                let mut window = PreviewWindow::default();
                let inspected = read.inspect_flow_updates(&registration.id, |receipt, facts| {
                    check_cancelled(cancellation)?;
                    for (fact, update) in facts.iter().enumerate() {
                        let id = FlowRecordId {
                            commit: receipt.sequence,
                            fact,
                            item: 0,
                        };
                        for (message, value) in update.shared_messages().iter().enumerate() {
                            check_cancelled(cancellation)?;
                            if value.origin != MessageOrigin::Internal {
                                window.push(
                                    message_record(&flow, id, message, value),
                                    &matcher,
                                    &flow,
                                );
                            }
                        }
                        if let FlowUpdate::LegacyRecords(records) = update {
                            for (item, record) in records.iter().enumerate() {
                                check_cancelled(cancellation)?;
                                if let Some(event) = record.project_event()? {
                                    let kind = crate::event_writer::event_kind(&event);
                                    if let Some(text) =
                                        extract_event_text(kind, &serde_json::to_string(&event)?)
                                    {
                                        window.push(
                                            Record {
                                                hit: HistoryHit {
                                                    location: HistoryLocation::Flow {
                                                        flow: flow.clone(),
                                                        record: FlowRecordId { item, ..id },
                                                        message: 0,
                                                    },
                                                    session_id: flow.session_id.to_string(),
                                                    seq: receipt.sequence,
                                                    ts: record
                                                        .source
                                                        .timestamp
                                                        .map(|ts| ts.to_rfc3339())
                                                        .unwrap_or_default(),
                                                    kind: kind.into(),
                                                    snippet: text.chars().take(240).collect(),
                                                    preview: Vec::new(),
                                                },
                                                text,
                                            },
                                            &matcher,
                                            &flow,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Ok(())
                });
                check_cancelled(cancellation)?;
                match inspected {
                    Err(error) => {
                        results
                            .warnings
                            .push(format!("Flow {}: {error}", registration.id));
                        continue;
                    }
                    Ok(journal) if journal.trailing_bytes != 0 => results.warnings.push(format!(
                        "Flow {} has an incomplete journal tail",
                        registration.id
                    )),
                    Ok(_) => {}
                }
                let activity = read
                    .dir
                    .join("flows")
                    .join(registration.id.to_string())
                    .join("events.jsonl")
                    .metadata()?
                    .modified()?;
                let rank = chrono::DateTime::<chrono::Utc>::from(activity).to_rfc3339();
                ranked.extend(window.hits.into_iter().map(|(hit, _)| (rank.clone(), hit)));
                retain_recent(&mut ranked);
            }
        }
    }
    if matches!(source, SearchSource::Legacy(_)) || request.scope == SearchScope::Project {
        let sid = current.to_string();
        let mut found_index = false;
        for directory in index_dirs {
            check_cancelled(cancellation)?;
            let index = match AnchorIndex::open_project_read_only(&directory) {
                Ok(Some(index)) => index,
                Ok(None) => continue,
                Err(error) => {
                    results.warnings.push(error.to_string());
                    continue;
                }
            };
            let cancelled = cancellation.clone();
            index
                .conn()
                .progress_handler(1000, Some(move || cancelled.is_cancelled()));
            found_index = true;
            let filter = (request.scope == SearchScope::Session).then_some(sid.as_str());
            let rows = match index.fts_search_project_events(query, filter, LIMIT) {
                Ok(rows) => rows,
                Err(error) => {
                    results
                        .warnings
                        .push(format!("{}: {error}", index.path().display()));
                    continue;
                }
            };
            for row in rows {
                check_cancelled(cancellation)?;
                let text = extract_event_text(&row.kind, &row.payload)
                    .unwrap_or_else(|| row.payload.chars().take(PREVIEW_CHARS).collect());
                let neighbors = index.find_project_events_around(&row.session_id, row.seq, 3)?;
                let mut preview = vec![format!("Original history · Session {}", row.session_id)];
                for neighbor in neighbors {
                    if let Some(text) = extract_event_text(&neighbor.kind, &neighbor.payload) {
                        preview.push(preview_line(
                            neighbor.seq == row.seq,
                            &neighbor.kind,
                            neighbor.seq,
                            &text,
                        ));
                    } else if neighbor.seq == row.seq {
                        preview.push(preview_line(true, &row.kind, row.seq, &text));
                    }
                }
                ranked.push((
                    row.ts.clone(),
                    HistoryHit {
                        location: HistoryLocation::Indexed {
                            index: index.path().to_owned(),
                            session: row.session_id.clone(),
                            seq: row.seq,
                        },
                        session_id: row.session_id,
                        seq: row.seq,
                        ts: row.ts,
                        kind: row.kind,
                        snippet: text.chars().take(240).collect(),
                        preview,
                    },
                ));
            }
        }
        if !found_index && matches!(source, SearchSource::Legacy(_)) {
            results
                .warnings
                .push("Original history has no readable project index".into());
        }
    }
    check_cancelled(cancellation)?;
    retain_recent(&mut ranked);
    results.hits = ranked.into_iter().map(|(_, hit)| hit).collect();
    Ok(results)
}

fn retain_recent(hits: &mut Vec<(String, HistoryHit)>) {
    hits.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.seq.cmp(&left.1.seq))
    });
    hits.truncate(LIMIT);
}

fn message_record(flow: &FlowRef, record: FlowRecordId, message: usize, value: &Message) -> Record {
    let kind = match value.role.as_str() {
        "tool" => "tool_result_msg".into(),
        role => format!("{role}_msg"),
    };
    let text = value
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text }
            | MessagePart::FinalAnswerSummary { text }
            | MessagePart::PeerInput { text, .. }
                if !text.is_empty() =>
            {
                Some(text.clone())
            }
            MessagePart::Thinking { thinking, .. } if !thinking.is_empty() => {
                Some(format!("_{thinking}_"))
            }
            MessagePart::CompactSummary { summary, .. } if !summary.is_empty() => {
                Some(summary.clone())
            }
            MessagePart::ToolResult { content, .. } if !content.is_empty() => {
                Some(format!("```\n{content}\n```"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Record {
        hit: HistoryHit {
            location: HistoryLocation::Flow {
                flow: flow.clone(),
                record,
                message,
            },
            session_id: flow.session_id.to_string(),
            seq: record.commit,
            ts: String::new(),
            kind,
            snippet: text.chars().take(240).collect(),
            preview: Vec::new(),
        },
        text,
    }
}

fn preview_line(selected: bool, kind: &str, seq: u64, text: &str) -> String {
    let marker = if selected { "▶" } else { " " };
    format!(
        "{marker} **[{kind}]** seq={seq}  \n{}",
        text.chars().take(PREVIEW_CHARS).collect::<String>()
    )
}

pub fn extract_event_text(kind: &str, payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    match kind {
        "user_msg" | "assistant_msg" | "system_msg" | "tool_result_msg" => {
            let parts = v.get("message")?.get("parts")?.as_array()?;
            let mut chunks = Vec::new();
            for p in parts {
                if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        chunks.push(text.to_string());
                    }
                } else if let Some(thinking) = p.get("thinking").and_then(|t| t.as_str()) {
                    if !thinking.is_empty() {
                        chunks.push(format!("_{thinking}_"));
                    }
                } else if let Some(summary) = p.get("summary").and_then(|t| t.as_str()) {
                    if !summary.is_empty() {
                        chunks.push(summary.to_string());
                    }
                } else if let Some(content) = p.get("content").and_then(|t| t.as_str()) {
                    if !content.is_empty() {
                        chunks.push(format!("```\n{content}\n```"));
                    }
                }
            }
            if chunks.is_empty() {
                None
            } else {
                Some(chunks.join("\n\n"))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_search_retention_is_bounded_by_results_and_neighbors() {
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: atman_rt::FlowInstanceId(Uuid::now_v7().into()),
        };
        let mut window = PreviewWindow::default();
        let matcher = regex::Regex::new("needle").unwrap();
        for commit in 0..1000 {
            let text = if commit % 2 == 0 {
                format!("needle {commit}")
            } else {
                format!("neighbor {commit}")
            };
            window.push(
                message_record(
                    &flow,
                    FlowRecordId {
                        commit,
                        fact: 0,
                        item: 0,
                    },
                    0,
                    &Message::user_text(atman_rt::TurnId::now(), &text),
                ),
                &matcher,
                &flow,
            );
            assert!(window.before.len() <= 3);
            assert!(window.hits.len() <= LIMIT);
            assert!(window.hits.iter().all(|(hit, _)| hit.preview.len() <= 8));
        }
        let (first, _) = window.hits.front().unwrap();
        assert_eq!(first.seq, 900);
        assert!(first.preview.join("\n").contains("neighbor 897"));
        assert!(first.preview.join("\n").contains("neighbor 903"));
        assert!(!first.preview.join("\n").contains("needle 904"));
    }

    #[tokio::test]
    async fn history_search_cancelled_waiter_does_not_scan_or_take_the_storage_permit() {
        let service = SearchService::default();
        let held = service.permit.acquire().await.unwrap();
        let cancellation = CancellationToken::new();
        let operation = service.search(
            PathBuf::from("/unavailable-history"),
            SearchSource::Legacy(Uuid::now_v7()),
            SearchRequest {
                query: "needle".into(),
                scope: SearchScope::Session,
            },
            cancellation.clone(),
        );
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => panic!("scan ran without a storage permit: {:?}", result.err()),
            _ = tokio::task::yield_now() => {}
        }
        cancellation.cancel();
        let error = operation.await.err().unwrap();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
        drop(held);
        assert!(service.permit.try_acquire().is_ok());
    }
}
