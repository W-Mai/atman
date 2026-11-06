//! Source-bound normalization for large directory search fields.

use super::{Action, page::Row};
use std::sync::Arc;

#[derive(Default)]
pub(super) struct Index {
    source: Option<Arc<Vec<Row>>>,
    ready: Option<Prepared>,
    pending: Option<crate::render_work::Task<Prepared>>,
    pending_query: Option<String>,
    error: Option<String>,
    started: Option<std::time::Instant>,
    requested: String,
}

struct Prepared {
    fields: Arc<Vec<Vec<String>>>,
    query: String,
    indices: Vec<usize>,
}

fn prepare(rows: &Arc<Vec<Row>>, fields: Option<Arc<Vec<Vec<String>>>>, query: String) -> Prepared {
    let fields = fields.unwrap_or_else(|| Arc::new(rows.iter().map(self::fields).collect()));
    let indices = fields
        .iter()
        .enumerate()
        .filter_map(|(index, fields)| {
            query
                .split_whitespace()
                .filter(|term| !term.starts_with("link:"))
                .all(|term| fields.iter().any(|field| field.contains(term)))
                .then_some(index)
        })
        .collect();
    Prepared {
        fields,
        query,
        indices,
    }
}

pub(super) fn large(rows: &[Row]) -> bool {
    rows.iter().fold(0usize, |size, row| {
        size.saturating_add(row.title.len())
            .saturating_add(row.detail.len())
            .saturating_add(match &row.action {
                Action::Inspect { body, .. } => body.len(),
                _ => 0,
            })
    }) > 16_384
}

fn fields(row: &Row) -> Vec<String> {
    let mut fields = vec![row.title.to_lowercase(), row.detail.to_lowercase()];
    if let super::page::RowKey::Flow(address) = &row.key {
        fields.push(address.session_id().to_string());
        if let Some(flow) = address.instance() {
            fields.push(flow.instance_id.to_string());
        }
    }
    if let Action::Inspect { body, .. } = &row.action {
        fields.push(body.to_lowercase());
    }
    fields
}

impl Index {
    pub fn update(&mut self, source: &Arc<Vec<Row>>, needed: bool, query: &str) {
        if self
            .source
            .as_ref()
            .is_none_or(|old| !Arc::ptr_eq(old, source))
        {
            self.pending = None;
            crate::render_work::discard((self.source.replace(source.clone()), self.ready.take()));
            self.error = None;
        }
        if self.requested != query {
            self.requested = query.to_owned();
            self.error = None;
        }
        if !needed {
            self.pending = None;
            self.pending_query = None;
            return;
        }
        if let Some(result) = self
            .pending
            .as_mut()
            .and_then(crate::render_work::Task::poll)
        {
            self.pending = None;
            let completed = self.pending_query.take();
            match result {
                Ok(ready) => {
                    crate::render_work::discard(self.ready.replace(ready));
                }
                Err(error) if completed.as_deref() == Some(query) => self.error = Some(error),
                Err(_) => {}
            }
        }
        if self.pending.is_none()
            && self.ready.as_ref().is_none_or(|ready| ready.query != query)
            && self.error.is_none()
        {
            let rows = source.clone();
            let fields = self.ready.as_ref().map(|ready| ready.fields.clone());
            let query = query.to_owned();
            self.pending_query = Some(query.clone());
            self.started = Some(std::time::Instant::now());
            self.pending = Some(crate::render_work::Task::spawn(move || {
                prepare(&rows, fields, query)
            }));
        }
    }

    pub fn matches(&self, source: &Arc<Vec<Row>>, index: usize, query: &str) -> bool {
        self.source
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, source))
            && self.ready.as_ref().is_some_and(|ready| {
                ready.query == query && ready.indices.binary_search(&index).is_ok()
            })
    }

    pub fn ready_for(&self, source: &Arc<Vec<Row>>, query: &str) -> bool {
        self.source
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, source))
            && self
                .ready
                .as_ref()
                .is_some_and(|ready| ready.query == query)
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn frame(&self) -> u32 {
        self.started
            .map_or(0, |started| (started.elapsed().as_millis() / 100) as u32)
    }
    pub fn retry(&mut self) {
        self.error = None;
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        crate::render_work::discard((self.source.take(), self.ready.take()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    #[test]
    fn directory_index_publishes_only_on_paint_and_rejects_source_replacement() {
        let rows = Arc::new(vec![Row {
            key: super::super::page::RowKey::FlowField("value"),
            title: "Original".into(),
            detail: String::new(),
            action: Action::Inspect {
                title: "Detail".into(),
                body: "Large 汉\n".repeat(10_000),
            },
        }]);
        let pool = futures::executor::ThreadPool::builder()
            .pool_size(1)
            .create()
            .unwrap();
        let (started, entered) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        pool.spawn_ok(async move {
            started.send(()).unwrap();
            let _ = wait.recv();
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let captured = rows.clone();
        let mut index = Index {
            source: Some(rows.clone()),
            pending: Some(crate::render_work::Task::spawn_on(pool, move || {
                prepare(&captured, None, "large 汉".into())
            })),
            ready: None,
            error: None,
            started: None,
            requested: "large 汉".into(),
            pending_query: Some("large 汉".into()),
        };
        let start = Instant::now();
        assert!(!index.matches(&rows, 0, "large 汉"));
        assert!(start.elapsed() < Duration::from_millis(100));
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            !index.matches(&rows, 0, "large 汉"),
            "completion cannot change an event target before painting"
        );
        let end = Instant::now() + Duration::from_secs(3);
        while !index.ready_for(&rows, "large 汉") {
            index.update(&rows, true, "large 汉");
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(index.matches(&rows, 0, "large 汉"));
        let pointer = index.ready.as_ref().unwrap().fields.as_ptr();
        for _ in 0..10 {
            index.update(&rows, true, "large 汉");
        }
        assert_eq!(pointer, index.ready.as_ref().unwrap().fields.as_ptr());
        let fields = index.ready.as_ref().unwrap().fields.clone();
        index.update(&rows, true, "missing");
        assert!(!index.matches(&rows, 0, "missing"));
        let end = Instant::now() + Duration::from_secs(3);
        while !index.ready_for(&rows, "missing") {
            index.update(&rows, true, "missing");
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            Arc::ptr_eq(&fields, &index.ready.as_ref().unwrap().fields),
            "query changes must reuse normalized source fields"
        );
        assert!(!index.matches(&rows, 0, "missing"));
        let replacement = Arc::new(vec![Row {
            key: super::super::page::RowKey::FlowField("value"),
            title: "Replacement".into(),
            detail: "small".into(),
            action: Action::None,
        }]);
        assert!(!index.matches(&replacement, 0, "large"));
        index.update(&replacement, true, "large");
        assert!(!index.matches(&replacement, 0, "large"));
        index.update(&replacement, false, "");
        assert!(!index.is_pending());
    }
}
