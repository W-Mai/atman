//! Ordered clipboard writes with view-owned observation and workspace-owned completion.

use std::{collections::VecDeque, sync::Arc};
use tokio::sync::{mpsc, oneshot};

use crate::{
    app::{AppState, NoteLevel},
    control::{Completion, ControlWork},
    selection::{CopyPayload, SelectionState},
};

pub(crate) enum Payload {
    Text(String),
    Rich(CopyPayload),
}

struct Request {
    payload: Payload,
    reply: oneshot::Sender<Result<(), String>>,
    _completion: Arc<Completion>,
}

#[derive(Clone)]
pub(crate) struct CopyService {
    tx: mpsc::UnboundedSender<Request>,
    pub work: ControlWork,
}

pub(super) fn write(payload: Payload) -> Result<(), String> {
    match payload {
        Payload::Text(text) => super::write_text(&text).map_err(|error| error.to_string()),
        Payload::Rich(payload) => {
            let (html, plain) = crate::selection_menu::rich_copy_payload(&payload);
            super::write_html(&html, &plain).map_err(|error| error.to_string())
        }
    }
}

impl CopyService {
    pub fn start() -> std::io::Result<Self> {
        Self::start_with(write)
    }

    fn start_with(
        mut write: impl FnMut(Payload) -> Result<(), String> + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, mut requests) = mpsc::unbounded_channel::<Request>();
        std::thread::Builder::new()
            .name("atman-clipboard".into())
            .spawn(move || {
                while let Some(request) = requests.blocking_recv() {
                    let result = write(request.payload);
                    let _ = request.reply.send(result);
                    // The completion guard stays with the actual write after observation ends.
                }
            })?;
        Ok(Self {
            tx,
            work: ControlWork::default(),
        })
    }
}

struct Receipt {
    reply: oneshot::Receiver<Result<(), String>>,
    success: Option<String>,
    selection: Option<SelectionState>,
}

pub(crate) struct Outcome {
    result: Result<(), String>,
    success: Option<String>,
    selection: Option<SelectionState>,
}

#[derive(Default)]
pub(crate) struct CopyJobs {
    pub service: Option<CopyService>,
    requests: VecDeque<Receipt>,
}

impl CopyJobs {
    pub fn is_waiting(&self) -> bool {
        !self.requests.is_empty()
    }

    fn start(
        &mut self,
        payload: Payload,
        success: Option<String>,
        selection: Option<SelectionState>,
    ) -> Result<(), String> {
        let service = self.service.as_ref().ok_or("Clipboard is unavailable")?;
        let (reply, receiver) = oneshot::channel();
        service
            .tx
            .send(Request {
                payload,
                reply,
                _completion: service.work.begin(),
            })
            .map_err(|_| "Clipboard worker stopped")?;
        self.requests.push_back(Receipt {
            reply: receiver,
            success,
            selection,
        });
        Ok(())
    }

    pub async fn next(&mut self) -> Outcome {
        let Some(receipt) = self.requests.front_mut() else {
            return std::future::pending().await;
        };
        let result = (&mut receipt.reply)
            .await
            .unwrap_or_else(|_| Err("Clipboard worker stopped".into()));
        let receipt = self.requests.pop_front().unwrap();
        Outcome {
            result,
            success: receipt.success,
            selection: receipt.selection,
        }
    }
}

impl AppState {
    pub(crate) fn copy_to_clipboard(
        &mut self,
        payload: Payload,
        success: Option<String>,
        selection: Option<SelectionState>,
    ) {
        if let Err(error) = self.clipboard_copies.start(payload, success, selection) {
            self.push_note(format!("clipboard write failed: {error}"), NoteLevel::Error);
        }
        self.refresh_clipboard_status(false);
    }

    pub(crate) fn clipboard_copy_ready(&mut self, outcome: Outcome) {
        match outcome.result {
            Ok(()) => {
                if let Some(selection) = outcome.selection
                    && self.selection.as_ref() == Some(&selection)
                {
                    self.selection = Some(crate::selection::selection_retain_copied(selection));
                }
                if let Some(message) = outcome.success {
                    self.push_note(message, NoteLevel::Info);
                }
            }
            Err(error) => {
                self.push_note(format!("clipboard write failed: {error}"), NoteLevel::Error);
            }
        }
        self.refresh_clipboard_status(false);
    }

    pub(crate) fn refresh_clipboard_status(&mut self, finishing: bool) {
        if self.clipboard_copies.is_waiting() || finishing {
            self.status_notes.insert(
                "clipboard".into(),
                format!(
                    "{} {}",
                    crate::loading::glyph(self.animation_frame),
                    if finishing {
                        "Finishing copy"
                    } else {
                        "Copying"
                    }
                ),
            );
        } else {
            self.status_notes.remove("clipboard");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Mutex, time::Duration};

    #[tokio::test]
    async fn writes_stay_ordered_and_drain_after_cancelled_or_dropped_observation() {
        let (started, entered) = oneshot::channel();
        let mut started = Some(started);
        let (release, wait) = std::sync::mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let observed = written.clone();
        let service = CopyService::start_with(move |payload| {
            let Payload::Text(text) = payload else {
                unreachable!()
            };
            if let Some(started) = started.take() {
                started.send(()).unwrap();
                wait.recv().unwrap();
            }
            observed.lock().unwrap().push(text.clone());
            if text == "B" {
                Err("controlled write failure".into())
            } else {
                Ok(())
            }
        })
        .unwrap();
        let mut a = CopyJobs {
            service: Some(service.clone()),
            ..Default::default()
        };
        let mut b = CopyJobs {
            service: Some(service.clone()),
            ..Default::default()
        };
        a.start(Payload::Text("A".into()), None, None).unwrap();
        entered.await.unwrap();
        b.start(Payload::Text("B".into()), Some("B copied".into()), None)
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), a.next())
                .await
                .is_err()
        );
        assert!(a.is_waiting());
        drop(a);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), service.work.settled())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        let result = b.next().await;
        assert_eq!(result.result, Err("controlled write failure".into()));
        assert_eq!(result.success.as_deref(), Some("B copied"));
        service.work.settled().await;
        assert_eq!(*written.lock().unwrap(), ["A", "B"]);
        assert!(!b.is_waiting());
    }

    #[tokio::test]
    async fn terminal_gate_never_blocks_ui_and_late_copy_cannot_replace_a_new_selection() {
        let (started, entered) = oneshot::channel();
        let mut started = Some(started);
        let (release, wait) = std::sync::mpsc::channel();
        let service = CopyService::start_with(move |_| {
            let _output = crate::clipboard::OUTPUT.lock().unwrap();
            started.take().unwrap().send(()).unwrap();
            wait.recv().unwrap();
            Ok(())
        })
        .unwrap();
        let point = crate::selection::SemanticPoint {
            domain: crate::selection::SelectionDomain::TranscriptProse,
            ordinal: 0,
            grapheme: 0,
            affinity: Default::default(),
        };
        let selection = crate::selection::selection_begin(point, Default::default(), 0);
        let mut app = AppState::new("copy-origin".into(), None);
        app.clipboard_copies.service = Some(service.clone());
        app.selection = Some(selection.clone());
        app.copy_to_clipboard(Payload::Text("original".into()), None, Some(selection));
        entered.await.unwrap();
        assert!(crate::clipboard::try_output().is_none());
        assert!(app.status_notes.contains_key("clipboard"));
        app.selection = None;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), app.clipboard_copies.next())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        let outcome = app.clipboard_copies.next().await;
        app.clipboard_copy_ready(outcome);
        assert!(app.selection.is_none());
        assert!(!app.status_notes.contains_key("clipboard"));
        service.work.settled().await;
        assert!(crate::clipboard::try_output().is_some());
    }
}
