//! Per-view control ordering without awaiting services in the UI controller.

use super::*;

struct Request {
    id: Uuid,
    command: TuiControl,
    replies: ControlReplies,
}

pub(super) struct Controls {
    tx: mpsc::UnboundedSender<Request>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl Controls {
    pub fn start(
        scope: Attachment,
        worker: FlowWorker,
        mut panels: crate::tui_panels::PanelServices,
        forms: Arc<atman_runtime::session::FormRegistry>,
        commands: mpsc::UnboundedSender<TuiCommand>,
    ) -> Self {
        let (tx, mut incoming) = mpsc::unbounded_channel::<Request>();
        let task = tokio::spawn(async move {
            loop {
                if incoming.is_empty() && commands.is_closed() && !panels.has_pending_mutations() {
                    break;
                }
                let request = tokio::select! {
                    result = panels.next_completion(), if panels.has_pending_work() => { result?; continue; }
                    request = incoming.recv() => match request { Some(request) => request, None => break },
                    _ = commands.closed(), if !commands.is_closed() => { continue; }
                };
                let Request {
                    id,
                    command,
                    replies,
                } = request;
                let (scope, worker, forms, response) = (
                    scope.clone(),
                    worker.clone(),
                    forms.clone(),
                    replies.clone(),
                );
                let (next, result) = crate::tui_panels::background(async move {
                    let result = match panels.dispatch(command, Some(response.clone())).await {
                        Some(command) => {
                            scope
                                .tui_control(&worker, id, command, &response, &forms)
                                .await
                        }
                        None => Ok(()),
                    };
                    (panels, result)
                })
                .await;
                panels = next;
                if let Err(error) = result {
                    let _ = replies.send(TuiCommand::Toast {
                        message: format!("{error:#}"),
                        level: atman_tui::app::NoteLevel::Error,
                    });
                }
                drop(replies);
            }
            while panels.has_pending_mutations() {
                panels.next_completion().await?;
            }
            Ok(())
        });
        Self {
            tx,
            task: Some(task),
        }
    }

    pub fn send(&self, id: Uuid, command: TuiControl, replies: ControlReplies) -> Result<()> {
        self.tx
            .send(Request {
                id,
                command,
                replies,
            })
            .map_err(|_| anyhow::anyhow!("Flow control queue is closed"))
    }

    pub fn can_detach(&self) -> bool {
        self.task.is_none()
    }

    pub async fn next_completion(&mut self) -> Result<()> {
        let Some(task) = &mut self.task else {
            return std::future::pending().await;
        };
        let result = task.await;
        self.task = None;
        result?
    }
}

pub(super) fn spawn_navigation<T: Send + 'static>(
    tasks: &mut tokio::task::JoinSet<T>,
    work: impl std::future::Future<Output = T> + Send + 'static,
) {
    tasks.spawn(crate::tui_panels::background(work));
}
