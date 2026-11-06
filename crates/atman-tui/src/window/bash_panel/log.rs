//! File fallback and width-specific indexing owned by one task panel.

use std::{
    io::{self, BufRead},
    path::PathBuf,
    sync::{Arc, OnceLock, mpsc},
};

use tokio::{sync::Semaphore, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::wrapped_text::{WrappedLineMode, WrappedRowIndex};

static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub(super) struct PreparedLog {
    output: Arc<str>,
    pub index: WrappedRowIndex,
}

struct Pending {
    result: mpsc::Receiver<io::Result<PreparedLog>>,
    worker: JoinHandle<()>,
    cancellation: CancellationToken,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.worker.abort();
    }
}

#[derive(Default)]
pub(super) struct LogSource {
    path: Option<PathBuf>,
    output: Option<Arc<str>>,
    width: Option<usize>,
    pending: Option<Pending>,
    pub error: Option<String>,
}

impl LogSource {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            ..Self::default()
        }
    }

    pub fn is_waiting(&self) -> bool {
        self.pending.is_some()
    }

    pub fn output(&self) -> Option<&str> {
        (self.error.is_none() && self.pending.is_none())
            .then_some(self.output.as_deref())
            .flatten()
    }

    pub fn retry(&mut self) {
        self.width = None;
        self.error = None;
    }

    pub fn suspend(&mut self) {
        if self.pending.take().is_some() {
            self.width = None;
        }
    }

    pub fn prepare(&mut self, width: usize) -> Option<WrappedRowIndex> {
        self.prepare_with_limit(
            width,
            LIMIT.get_or_init(|| Arc::new(Semaphore::new(2))).clone(),
        )
    }

    fn prepare_with_limit(
        &mut self,
        width: usize,
        limit: Arc<Semaphore>,
    ) -> Option<WrappedRowIndex> {
        let path = self.path.as_ref()?;
        if self.width != Some(width) {
            self.pending = None;
            self.width = Some(width);
            self.error = None;
            let output = self.output.clone();
            let path = path.clone();
            let (sender, receiver) = mpsc::channel();
            let cancellation = CancellationToken::new();
            let cancelled = cancellation.clone();
            let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                return None;
            };
            let worker = runtime.spawn(async move {
                let permit = tokio::select! {
                    _ = cancelled.cancelled() => return,
                    permit = limit.acquire_owned() => match permit { Ok(permit) => permit, Err(_) => return },
                };
                let observation = cancelled.clone();
                let result = crate::read_work::run(&observation, move || {
                    let _permit = permit;
                    let output = match output { Some(output) => output, None => read_log(&path, &cancelled)? };
                    if cancelled.is_cancelled() { return Err(io::Error::new(io::ErrorKind::Interrupted, "log loading cancelled")); }
                    let mut index = WrappedRowIndex::default();
                    index.update(&output, 0, width.max(1), WrappedLineMode::SplitNewline);
                    Ok(PreparedLog { output, index })
                }).await.unwrap_or_else(|| Err("Log loading cancelled".into()))
                    .unwrap_or_else(|error| Err(io::Error::other(error)));
                let _ = sender.send(result);
            });
            self.pending = Some(Pending {
                result: receiver,
                worker,
                cancellation,
            });
        }
        let pending = self.pending.as_ref()?;
        let result = match pending.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(io::Error::other("background log preparation stopped"))
            }
        };
        self.pending = None;
        match result {
            Ok(prepared) => {
                self.output = Some(prepared.output);
                Some(prepared.index)
            }
            Err(error) => {
                self.error = Some(error.to_string());
                None
            }
        }
    }
}

fn read_log(path: &std::path::Path, cancelled: &CancellationToken) -> io::Result<Arc<str>> {
    if cancelled.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "log loading cancelled",
        ));
    }
    let file = std::fs::File::open(path)?;
    if cancelled.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "log loading cancelled",
        ));
    }
    let reader = io::BufReader::new(file);
    let mut output = String::new();
    let mut first = true;
    for line in reader.lines() {
        if cancelled.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "log loading cancelled",
            ));
        }
        let line = line?;
        if !first {
            output.push('\n');
        }
        first = false;
        output.push_str(
            line.strip_prefix("[out] ")
                .or_else(|| line.strip_prefix("[err] "))
                .unwrap_or(&line),
        );
    }
    Ok(output.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fallback_retains_text_across_resize_and_rejects_obsolete_work() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bg_task.log");
        std::fs::write(&path, "[out] 你好🚀\n[err] error\n").unwrap();
        let gate = Arc::new(Semaphore::new(0));
        let mut source = LogSource::new(Some(path.clone()));
        assert!(source.prepare_with_limit(80, gate.clone()).is_none());
        assert!(source.is_waiting());
        source.prepare_with_limit(6, gate.clone());
        gate.add_permits(1);
        let index = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(index) = source.prepare_with_limit(6, gate.clone()) {
                    break index;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(source.output(), Some("你好🚀\nerror"));
        assert_eq!(index.len(), 2);
        std::fs::remove_file(&path).unwrap();
        source.prepare_with_limit(3, gate.clone());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while source.is_waiting() {
                source.prepare_with_limit(3, gate.clone());
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            source.error.is_none(),
            "resizing must reuse loaded text after the source file is removed"
        );
        let mut missing = LogSource::new(Some(path));
        missing.prepare_with_limit(3, gate.clone());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while missing.is_waiting() {
                missing.prepare_with_limit(3, gate.clone());
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(missing.error.is_some());
        std::fs::write(directory.path().join("bg_task.log"), "[out] recovered").unwrap();
        missing.retry();
        missing.prepare_with_limit(3, gate.clone());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while missing.is_waiting() {
                missing.prepare_with_limit(3, gate.clone());
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(missing.output(), Some("recovered"));
    }
}
