//! Immutable catalog capture and cancellable filtering outside panel events.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use super::{ConfessionView, KnowledgeState, OrganizationProposal, RuleView};

#[derive(Clone)]
pub(super) enum Indices {
    All(usize),
    Filtered(Arc<[usize]>),
}

impl Indices {
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        match self {
            Self::All(len) => *len,
            Self::Filtered(indices) => indices.len(),
        }
    }

    pub fn get(&self, index: usize) -> Option<usize> {
        match self {
            Self::All(len) => (index < *len).then_some(index),
            Self::Filtered(indices) => indices.get(index).copied(),
        }
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = usize> + '_ {
        (0..self.len()).map(|index| match self {
            Self::All(_) => index,
            Self::Filtered(indices) => indices[index],
        })
    }

    fn same_version(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::All(a), Self::All(b)) => a == b,
            (Self::Filtered(a), Self::Filtered(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

#[derive(Clone)]
pub(super) struct Snapshot {
    pub confessions: Arc<[ConfessionView]>,
    pub rules: Arc<[RuleView]>,
    pub proposals: Arc<[OrganizationProposal]>,
}

impl Snapshot {
    pub fn same_version(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.confessions, &other.confessions)
            && Arc::ptr_eq(&self.rules, &other.rules)
            && Arc::ptr_eq(&self.proposals, &other.proposals)
    }

    pub fn capture(state: &KnowledgeState) -> Self {
        Self {
            confessions: state.confessions.clone(),
            rules: state.rules.clone(),
            proposals: state.proposals.clone(),
        }
    }

    fn matches(&self, state: &KnowledgeState) -> bool {
        Arc::ptr_eq(&self.confessions, &state.confessions)
            && Arc::ptr_eq(&self.rules, &state.rules)
            && Arc::ptr_eq(&self.proposals, &state.proposals)
    }

    fn len(&self, tab: usize) -> usize {
        match tab {
            0 => self.confessions.len(),
            1 => self.rules.len(),
            _ => self.proposals.len(),
        }
    }
}

#[derive(Clone)]
struct Request {
    snapshot: Snapshot,
    tab: usize,
    query: Arc<str>,
}

impl Request {
    fn same_source(&self, state: &KnowledgeState, tab: usize) -> bool {
        self.tab == tab && self.snapshot.matches(state)
    }

    fn matches(&self, state: &KnowledgeState, tab: usize, query: &str) -> bool {
        self.same_source(state, tab) && self.query.as_ref() == query
    }

    fn filter(&self, cancelled: &AtomicBool) -> Option<Arc<[usize]>> {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let needle = self.query.to_lowercase();
        let contains = |value: &str| value.to_lowercase().contains(&needle);
        let mut triggers = std::collections::HashMap::new();
        if self.tab >= 2 {
            for view in self.snapshot.confessions.iter() {
                if cancelled.load(Ordering::Relaxed) {
                    return None;
                }
                triggers
                    .entry(&view.confession.id)
                    .or_insert(view.confession.trigger.as_str());
            }
        }
        let mut indices = Vec::new();
        for index in 0..self.snapshot.len(self.tab) {
            if cancelled.load(Ordering::Relaxed) {
                return None;
            }
            let matches = match self.tab {
                0 => {
                    let view = &self.snapshot.confessions[index];
                    let c = &view.confession;
                    [
                        c.trigger.as_str(),
                        c.rule_violated.as_str(),
                        c.mitigation.as_str(),
                        view.category.as_deref().unwrap_or(""),
                    ]
                    .into_iter()
                    .any(contains)
                        || c.created_at
                            .format("%Y-%m-%d")
                            .to_string()
                            .contains(&needle)
                }
                1 => {
                    let rule = &self.snapshot.rules[index];
                    [&rule.name, &rule.description, &rule.source_path]
                        .into_iter()
                        .any(|value| contains(value))
                }
                _ => {
                    let proposal = &self.snapshot.proposals[index];
                    [
                        proposal.category.as_str(),
                        proposal.reason.as_str(),
                        triggers.get(&proposal.id).copied().unwrap_or(""),
                    ]
                    .into_iter()
                    .any(contains)
                }
            };
            if matches {
                indices.push(index);
            }
        }
        Some(indices.into())
    }
}

type FilterResult = Result<Option<Arc<[usize]>>, String>;

struct Pending {
    request: Option<Request>,
    result: Option<mpsc::Receiver<FilterResult>>,
    cancelled: Arc<AtomicBool>,
    pool: futures::executor::ThreadPool,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        let retired = (self.request.take(), self.result.take());
        self.pool.spawn_ok(async move { drop(retired) });
    }
}

struct Ready {
    request: Request,
    indices: Arc<[usize]>,
}

struct Displayed {
    snapshot: Snapshot,
    tab: usize,
    indices: Indices,
}

pub(super) struct Filter {
    pending: Option<Pending>,
    ready: Option<Ready>,
    failed: Option<(Request, String)>,
    displayed: Option<Displayed>,
    pool: futures::executor::ThreadPool,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            pending: None,
            ready: None,
            failed: None,
            displayed: None,
            pool: crate::render_work::workers(),
        }
    }
}

impl Filter {
    pub fn indices(&self, state: &KnowledgeState, tab: usize, query: &str) -> Indices {
        match &self.displayed {
            Some(displayed) if displayed.tab == tab && displayed.snapshot.matches(state) => {
                displayed.indices.clone()
            }
            Some(_) => Indices::All(0),
            None if query.is_empty() => Indices::All(match tab {
                0 => state.confessions.len(),
                1 => state.rules.len(),
                _ => state.proposals.len(),
            }),
            None => Indices::All(0),
        }
    }

    fn present(&mut self, state: &KnowledgeState, tab: usize, indices: &Indices) {
        if self.displayed.as_ref().is_some_and(|displayed| {
            displayed.tab == tab
                && displayed.snapshot.matches(state)
                && displayed.indices.same_version(indices)
        }) {
            return;
        }
        let retired = self.displayed.replace(Displayed {
            snapshot: Snapshot::capture(state),
            tab,
            indices: indices.clone(),
        });
        self.retire(retired);
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn error(&self) -> Option<&str> {
        self.failed.as_ref().map(|(_, error)| error.as_str())
    }

    pub fn retry(&mut self) {
        let failed = self.failed.take();
        self.retire(failed);
    }

    fn retire(&self, value: impl Send + 'static) {
        self.pool.spawn_ok(async move { drop(value) });
    }

    pub fn clear(&mut self) {
        if self.pending.is_none() && self.ready.is_none() && self.failed.is_none() {
            return;
        }
        if let Some(pending) = &self.pending {
            pending.cancelled.store(true, Ordering::Relaxed);
        }
        let retired = (self.pending.take(), self.ready.take(), self.failed.take());
        self.retire(retired);
    }

    pub fn update(&mut self, state: &KnowledgeState, tab: usize, query: &str) -> Indices {
        let count = match tab {
            0 => state.confessions.len(),
            1 => state.rules.len(),
            _ => state.proposals.len(),
        };
        if query.is_empty() || count == 0 {
            self.clear();
            let indices = Indices::All(count);
            self.present(state, tab, &indices);
            return indices;
        }
        if self
            .ready
            .as_ref()
            .is_some_and(|ready| !ready.request.same_source(state, tab))
        {
            let retired = self.ready.take();
            self.retire(retired);
        }
        if self
            .failed
            .as_ref()
            .is_some_and(|(request, _)| !request.matches(state, tab, query))
        {
            self.retry();
        }
        if let Some(pending) = &mut self.pending {
            if !pending
                .request
                .as_ref()
                .expect("pending request")
                .matches(state, tab, query)
            {
                pending.cancelled.store(true, Ordering::Relaxed);
            }
            let result = match pending
                .result
                .as_ref()
                .expect("filter observation")
                .try_recv()
            {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Background filtering stopped".into()))
                }
            };
            if let Some(result) = result {
                let mut completed = self.pending.take().expect("completed filter");
                let request = completed.request.take().expect("captured filter request");
                if request.matches(state, tab, query) {
                    match result {
                        Ok(Some(indices)) => {
                            let retired = self.ready.replace(Ready { request, indices });
                            self.retire(retired);
                        }
                        Ok(None) => {}
                        Err(error) => self.failed = Some((request, error)),
                    }
                } else {
                    self.retire((request, result));
                }
            }
        }
        if self.pending.is_none()
            && self.failed.is_none()
            && self
                .ready
                .as_ref()
                .is_none_or(|ready| !ready.request.matches(state, tab, query))
        {
            let request = Request {
                snapshot: Snapshot::capture(state),
                tab,
                query: query.into(),
            };
            let captured = request.clone();
            let cancelled = Arc::new(AtomicBool::new(false));
            let stop = cancelled.clone();
            let (sender, receiver) = mpsc::channel();
            self.pool.spawn_ok(async move {
                let result = atman_runtime::capture_blocking_panic(|| captured.filter(&stop))
                    .ok_or_else(|| "Background filtering failed".to_owned());
                let _ = sender.send(result);
                crate::render_work::notify();
            });
            self.pending = Some(Pending {
                request: Some(request),
                result: Some(receiver),
                cancelled,
                pool: self.pool.clone(),
            });
        }
        let indices = self.ready.as_ref().map_or(Indices::All(0), |ready| {
            Indices::Filtered(ready.indices.clone())
        });
        self.present(state, tab, &indices);
        indices
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        self.clear();
        let displayed = self.displayed.take();
        self.retire(displayed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::task::SpawnExt;
    use std::time::{Duration, Instant};

    fn rule(name: &str) -> RuleView {
        RuleView {
            name: name.into(),
            description: "description".into(),
            scope: "project".into(),
            source: "file".into(),
            source_path: "rules.md".into(),
            content: "large rule 汉\n".repeat(80_000),
        }
    }

    fn settle(filter: &mut Filter, state: &KnowledgeState, query: &str) -> Vec<usize> {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let indices = filter.update(state, 1, query);
            assert!(filter.error().is_none());
            if !filter.is_pending() {
                return indices.iter().collect();
            }
            assert!(Instant::now() < until, "knowledge filter did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn filtering_captures_handles_coalesces_sources_and_closes_without_waiting() {
        let pool = futures::executor::ThreadPool::builder()
            .pool_size(1)
            .create()
            .unwrap();
        let (started, entered) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        pool.spawn_ok(async move {
            started.send(()).unwrap();
            wait.recv().unwrap();
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut state = KnowledgeState {
            rules: vec![rule("Needle"), rule("Other")].into(),
            ..Default::default()
        };
        let snapshot = Snapshot::capture(&state);
        assert!(Arc::ptr_eq(&snapshot.rules, &state.rules));
        assert_eq!(
            snapshot.rules[0].content.as_ptr(),
            state.rules[0].content.as_ptr()
        );
        drop(snapshot);
        let old = Arc::downgrade(&state.rules);
        let mut filter = Filter {
            pool: pool.clone(),
            pending: None,
            ready: None,
            failed: None,
            displayed: None,
        };
        let started = Instant::now();
        assert!(filter.update(&state, 1, "needle").is_empty());
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(filter.is_pending());
        let first = filter.pending.as_ref().unwrap().cancelled.clone();
        let mut closed = Filter {
            pool,
            pending: None,
            ready: None,
            failed: None,
            displayed: None,
        };
        closed.update(&state, 1, "needle");
        let closed_flag = closed.pending.as_ref().unwrap().cancelled.clone();
        let started = Instant::now();
        drop(closed);
        assert!(closed_flag.load(Ordering::Relaxed));
        assert!(started.elapsed() < Duration::from_millis(100));
        state.replace_catalog(
            Vec::new(),
            vec![rule("Latest"), rule("Needle from another source")],
        );
        assert!(filter.update(&state, 1, "LATEST").is_empty());
        assert!(first.load(Ordering::Relaxed));
        release.send(()).unwrap();
        assert_eq!(settle(&mut filter, &state, "LATEST"), [0]);
        let until = Instant::now() + Duration::from_secs(3);
        while old.upgrade().is_some() {
            assert!(
                Instant::now() < until,
                "obsolete catalog ownership was retained"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..10 {
            assert_eq!(
                filter
                    .update(&state, 1, "LATEST")
                    .iter()
                    .collect::<Vec<_>>(),
                [0]
            );
            assert!(!filter.is_pending());
        }
        let indices = filter.update(&state, 1, "");
        assert!(matches!(indices, Indices::All(2)));
        assert_eq!(indices.iter().collect::<Vec<_>>(), [0, 1]);
        assert!(filter.pending.is_none() && filter.ready.is_none());
    }

    #[test]
    fn row_actions_observe_the_last_painted_indices_and_reject_replaced_data() {
        let pool = futures::executor::ThreadPool::builder()
            .pool_size(1)
            .create()
            .unwrap();
        let mut filter = Filter {
            pending: None,
            ready: None,
            failed: None,
            displayed: None,
            pool: pool.clone(),
        };
        let mut state = KnowledgeState {
            rules: vec![rule("Needle"), rule("Latest")].into(),
            ..Default::default()
        };
        assert_eq!(settle(&mut filter, &state, "needle"), [0]);
        filter.update(&state, 1, "latest");
        futures::executor::block_on(pool.spawn_with_handle(async {}).unwrap());
        assert_eq!(
            filter
                .indices(&state, 1, "latest")
                .iter()
                .collect::<Vec<_>>(),
            [0]
        );
        assert_eq!(
            filter
                .update(&state, 1, "latest")
                .iter()
                .collect::<Vec<_>>(),
            [1]
        );
        state.replace_catalog(Vec::new(), vec![rule("Different target")]);
        assert!(filter.indices(&state, 1, "latest").is_empty());
    }

    #[test]
    fn failed_query_retains_displayed_indices_and_retries_the_actual_request() {
        let state = KnowledgeState {
            rules: vec![rule("Needle"), rule("Latest")].into(),
            ..Default::default()
        };
        let mut filter = Filter::default();
        assert_eq!(settle(&mut filter, &state, "needle"), [0]);
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        filter.pending = Some(Pending {
            request: Some(Request {
                snapshot: Snapshot::capture(&state),
                tab: 1,
                query: "latest".into(),
            }),
            result: Some(receiver),
            cancelled: Arc::new(AtomicBool::new(false)),
            pool: filter.pool.clone(),
        });
        let retained = filter.update(&state, 1, "latest");
        assert_eq!(retained.iter().collect::<Vec<_>>(), [0]);
        assert_eq!(filter.error(), Some("Background filtering stopped"));
        assert!(!filter.is_pending());
        filter.retry();
        assert_eq!(settle(&mut filter, &state, "latest"), [1]);
    }
}
