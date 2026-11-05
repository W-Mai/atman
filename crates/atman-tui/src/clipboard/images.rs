//! View-owned clipboard jobs and the workspace's attachment publication boundary.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use atman_runtime::{Session, message::ImageSource};
use tokio::{
    sync::{Semaphore, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Default)]
struct PublicationState {
    closing: bool,
    // False means cancellable preparation; true means filesystem publication started.
    requests: HashMap<Uuid, bool>,
}

struct Shared {
    state: Mutex<PublicationState>,
    cancelled: CancellationToken,
    changed: watch::Sender<()>,
}

#[derive(Clone)]
pub(crate) struct ClipboardWork(Arc<Shared>);

impl Default for ClipboardWork {
    fn default() -> Self {
        Self(Arc::new(Shared {
            state: Mutex::default(),
            cancelled: CancellationToken::new(),
            changed: watch::channel(()).0,
        }))
    }
}

impl ClipboardWork {
    fn register(&self, request: Uuid) -> Result<Publication, String> {
        let mut state = self.0.state.lock().unwrap();
        if state.closing {
            return Err("Image preparation stopped during shutdown".into());
        }
        state.requests.insert(request, false);
        Ok(Publication {
            work: self.clone(),
            request,
        })
    }

    fn cancel(&self, request: Uuid) {
        let mut state = self.0.state.lock().unwrap();
        if state.requests.get(&request) == Some(&false) {
            state.requests.remove(&request);
        }
    }

    pub(crate) fn close(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.closing = true;
        state.requests.retain(|_, publishing| *publishing);
        drop(state);
        self.0.cancelled.cancel();
    }

    pub(crate) fn is_publishing(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .requests
            .values()
            .any(|publishing| *publishing)
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<()> {
        self.0.changed.subscribe()
    }

    pub(crate) async fn shutdown(&self) {
        let mut changed = self.subscribe();
        self.close();
        loop {
            changed.borrow_and_update();
            if !self.is_publishing() {
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
}

struct Publication {
    work: ClipboardWork,
    request: Uuid,
}

impl Publication {
    fn begin(&self) -> bool {
        let mut state = self.work.0.state.lock().unwrap();
        let Some(publishing) = state.requests.get_mut(&self.request) else {
            return false;
        };
        *publishing = true;
        true
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        self.work
            .0
            .state
            .lock()
            .unwrap()
            .requests
            .remove(&self.request);
        self.work.0.changed.send_replace(());
    }
}

type ImageResult = Result<Option<ImageSource>, String>;

#[derive(Default)]
pub(crate) struct ImagePaste {
    pub(crate) work: ClipboardWork,
    jobs: HashMap<Uuid, JoinHandle<ImageResult>>,
}

impl Drop for ImagePaste {
    fn drop(&mut self) {
        for (request, job) in self.jobs.drain() {
            self.work.cancel(request);
            job.abort();
        }
    }
}

impl ImagePaste {
    pub(crate) fn is_waiting(&self) -> bool {
        !self.jobs.is_empty()
    }

    pub(crate) fn start(&mut self, request: Uuid, session: Arc<Session>) -> Result<(), String> {
        self.start_with(
            request,
            move || super::read_image_png().map_err(|error| error.to_string()),
            move |bytes| {
                session
                    .import_image_bytes(&bytes, Some("clipboard.png"))
                    .map_err(|error| error.to_string())
            },
            LIMIT.get_or_init(|| Arc::new(Semaphore::new(2))).clone(),
        )
    }

    fn start_with(
        &mut self,
        request: Uuid,
        read: impl FnOnce() -> Result<Vec<u8>, String> + Send + 'static,
        import: impl FnOnce(Vec<u8>) -> Result<ImageSource, String> + Send + 'static,
        limit: Arc<Semaphore>,
    ) -> Result<(), String> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| error.to_string())?;
        let publication = self.work.register(request)?;
        let cancelled = self.work.0.cancelled.clone();
        let job = runtime.spawn(async move {
            let permit = tokio::select! {
                _ = cancelled.cancelled() => return Ok(None),
                permit = limit.acquire_owned() => permit.map_err(|error| error.to_string())?,
            };
            tokio::task::spawn_blocking(move || {
                // Both guards belong to the actual blocking task, even if its consumer is aborted.
                let _permit = permit;
                if cancelled.is_cancelled() {
                    return Ok(None);
                }
                let bytes = read()?;
                if !publication.begin() {
                    return Ok(None);
                }
                import(bytes).map(Some)
            })
            .await
            .map_err(|error| error.to_string())?
        });
        self.jobs.insert(request, job);
        Ok(())
    }

    pub(crate) fn retain(&mut self, editor: &crate::input::InputEditor) {
        self.jobs.retain(|request, job| {
            let keep = editor.pending_images().iter().any(|image| {
                image.request() == Some(*request) && editor.buf().contains(&image.marker)
            });
            if !keep {
                self.work.cancel(*request);
                job.abort();
            }
            keep
        });
    }

    pub(crate) async fn next(&mut self) -> (Uuid, ImageResult) {
        if self.jobs.is_empty() {
            return std::future::pending().await;
        }
        let (request, result) = {
            let pending = self
                .jobs
                .iter_mut()
                .map(|(request, job)| {
                    Box::pin(async move {
                        (
                            *request,
                            job.await
                                .map_err(|error| error.to_string())
                                .and_then(|result| result),
                        )
                    })
                })
                .collect::<Vec<_>>();
            futures::future::select_all(pending).await.0
        };
        self.jobs.remove(&request);
        (request, result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::InputEditor;
    use std::{borrow::Cow, time::Duration};

    fn png() -> Vec<u8> {
        crate::clipboard::encode_image(arboard::ImageData {
            width: 1,
            height: 1,
            bytes: Cow::Owned(vec![0, 128, 255, 255]),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn deleting_a_reference_cancels_publication_but_keeps_the_worker_budget_until_exit() {
        let mut editor = InputEditor::default();
        let (request, _) = editor.reserve_image();
        let mut paste = ImagePaste::default();
        let limit = Arc::new(Semaphore::new(1));
        let (started_tx, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let directory = tempfile::tempdir().unwrap();
        let store = atman_runtime::attachment_store::AttachmentStore::at(directory.path());
        paste
            .start_with(
                request,
                move || {
                    started_tx.send(()).unwrap();
                    blocked.recv().unwrap();
                    Ok(png())
                },
                move |bytes| {
                    store
                        .import_bytes(&bytes, None)
                        .map_err(|error| error.to_string())
                },
                limit.clone(),
            )
            .unwrap();
        started.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), paste.next())
                .await
                .is_err()
        );
        assert!(paste.is_waiting());
        editor.replace_with("new draft");
        paste.retain(&editor);
        assert!(!paste.is_waiting());
        assert_eq!(limit.available_permits(), 0);
        release.send(()).unwrap();
        let _finished = limit.acquire().await.unwrap();
        assert!(!directory.path().join("attachments").exists());
        assert_eq!(editor.buf(), "new draft");
        paste.work.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_and_consumer_drop_wait_for_real_attachment_publication() {
        let mut editor = InputEditor::default();
        let (request, _) = editor.reserve_image();
        let mut paste = ImagePaste::default();
        let work = paste.work.clone();
        let (started_tx, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let directory = tempfile::tempdir().unwrap();
        let store = atman_runtime::attachment_store::AttachmentStore::at(directory.path());
        paste
            .start_with(
                request,
                || Ok(png()),
                move |bytes| {
                    started_tx.send(()).unwrap();
                    blocked.recv().unwrap();
                    store
                        .import_bytes(&bytes, None)
                        .map_err(|error| error.to_string())
                },
                Arc::new(Semaphore::new(1)),
            )
            .unwrap();
        started.await.unwrap();
        drop(paste);
        let shutdown = work.shutdown();
        tokio::pin!(shutdown);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut shutdown)
                .await
                .is_err()
        );
        assert!(work.is_publishing());
        release.send(()).unwrap();
        shutdown.await;
        assert!(!work.is_publishing());
        assert_eq!(
            std::fs::read_dir(directory.path().join("attachments"))
                .unwrap()
                .count(),
            1
        );
        assert!(work.register(Uuid::new_v4()).is_err());
    }

    #[tokio::test]
    async fn shutdown_revokes_unpublished_jobs_in_every_view_including_queued_workers() {
        let work = ClipboardWork::default();
        let mut active = ImagePaste {
            work: work.clone(),
            jobs: HashMap::new(),
        };
        let mut cached = ImagePaste {
            work: work.clone(),
            jobs: HashMap::new(),
        };
        let limit = Arc::new(Semaphore::new(1));
        let (started_tx, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        active
            .start_with(
                Uuid::now_v7(),
                move || {
                    started_tx.send(()).unwrap();
                    blocked.recv().unwrap();
                    Ok(png())
                },
                |_| panic!("cancelled preparation cannot publish"),
                limit.clone(),
            )
            .unwrap();
        started.await.unwrap();
        cached
            .start_with(
                Uuid::now_v7(),
                || panic!("queued read cannot start after shutdown"),
                |_| panic!("queued preparation cannot publish"),
                limit,
            )
            .unwrap();
        work.shutdown().await;
        release.send(()).unwrap();
        assert!(active.next().await.1.unwrap().is_none());
        assert!(cached.next().await.1.unwrap().is_none());
        assert!(!active.is_waiting() && !cached.is_waiting());
    }
}
