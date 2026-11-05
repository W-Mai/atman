//! Latest-query filesystem completion with bounded worker ownership.

use std::sync::{Arc, OnceLock};

use tokio::{sync::Semaphore, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::CompletionItem;

static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Debug, Default)]
pub(super) struct PathCompletion {
    query: Option<String>,
    worker: Option<JoinHandle<Vec<CompletionItem>>>,
    cancellation: CancellationToken,
}

impl Drop for PathCompletion {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl PathCompletion {
    pub(super) fn is_waiting(&self) -> bool {
        self.worker.is_some()
    }

    pub(super) fn request(&mut self, prefix: &str) -> bool {
        self.request_with_limit(
            prefix,
            LIMIT.get_or_init(|| Arc::new(Semaphore::new(2))).clone(),
        )
    }

    fn request_with_limit(&mut self, prefix: &str, limit: Arc<Semaphore>) -> bool {
        if self.query.as_deref() == Some(prefix) {
            return false;
        }
        self.cancel();
        self.query = Some(prefix.into());
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return true;
        };
        let query = prefix.to_owned();
        let cancelled = self.cancellation.clone();
        self.worker = Some(runtime.spawn(async move {
            let permit = tokio::select! {
                _ = cancelled.cancelled() => return Vec::new(),
                permit = limit.acquire_owned() => match permit { Ok(permit) => permit, Err(_) => return Vec::new() },
            };
            tokio::task::spawn_blocking(move || {
                // Aborting the async consumer must not release a still-running worker's slot.
                let _permit = permit;
                scan(&query, &cancelled)
            }).await.unwrap_or_default()
        }));
        true
    }

    pub(super) async fn next(&mut self) -> Vec<CompletionItem> {
        let Some(worker) = self.worker.as_mut() else {
            return std::future::pending().await;
        };
        let items = worker.await.unwrap_or_default();
        self.worker = None;
        items
    }

    pub(super) fn cancel(&mut self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
        self.cancellation = CancellationToken::new();
        self.query = None;
    }
}

fn scan(prefix: &str, cancelled: &CancellationToken) -> Vec<CompletionItem> {
    if cancelled.is_cancelled() {
        return Vec::new();
    }
    let (dir, basename) = prefix.rfind('/').map_or(("./", prefix), |index| {
        (&prefix[..=index], &prefix[index + 1..])
    });
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let basename = basename.to_ascii_lowercase();
    let mut items = Vec::new();
    for entry in entries {
        if cancelled.is_cancelled() {
            return Vec::new();
        }
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.to_ascii_lowercase().starts_with(&basename) {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        items.push(CompletionItem {
            insert: if is_dir {
                format!("@{dir}{name}/")
            } else {
                format!("@{dir}{name} ")
            },
            hint: if is_dir { "dir".into() } else { "file".into() },
        });
        if items.len() == 32 {
            break;
        }
    }
    items.sort_by(|a, b| a.insert.cmp(&b.insert));
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_the_current_query_can_publish_and_cancelled_polls_retain_ownership() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("Beta")).unwrap();
        std::fs::write(directory.path().join("Alpha.txt"), "").unwrap();
        let gate = Arc::new(Semaphore::new(0));
        let mut completion = PathCompletion::default();
        let query = format!("{}/A", directory.path().display());
        assert!(completion.request_with_limit(&query, gate.clone()));
        assert!(!completion.request_with_limit(&query, gate.clone()));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), completion.next())
                .await
                .is_err()
        );
        completion.request_with_limit(&format!("{}/b", directory.path().display()), gate.clone());
        gate.add_permits(1);
        let items = completion.next().await;
        assert_eq!(items.len(), 1);
        assert!(items[0].insert.ends_with("/Beta/"));
        assert!(!completion.is_waiting());
        completion.request_with_limit(&query, gate.clone());
        completion.cancel();
        assert!(!completion.is_waiting());
        tokio::time::timeout(std::time::Duration::from_secs(1), gate.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }
}
