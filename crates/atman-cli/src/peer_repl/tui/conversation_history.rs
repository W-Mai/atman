//! Confirmed history changes finish on the original worker independently of window visibility.

use super::*;
use atman_runtime::flow_store::history_operation::{HistoryOperationStatus, HistoryReviewRequest};
use atman_tui::conversation_history::{Command, Response};

pub(super) async fn dispatch(worker: &FlowWorker, command: Command, replies: &ControlReplies) {
    let result = execute(worker, command, replies)
        .await
        .map_err(|error| format!("{error:#}"));
    let _ = replies.send(TuiCommand::ConversationHistory {
        result,
        finished: true,
    });
}

async fn execute(
    worker: &FlowWorker,
    command: Command,
    replies: &ControlReplies,
) -> Result<Response> {
    let sender = SenderIdentity::User(USER);
    match command {
        Command::Pending => Ok(Response::Operation(
            worker
                .pending_history_operation(sender, |_| Ok(()))
                .await?
                .map(Box::new),
        )),
        Command::Recover { operation, input } => Ok(Response::Recovered(Box::new(
            atman_tui::conversation_history::PreparedRecovery::prepare(
                worker
                    .history_recovered_input(sender, operation, input, |_| Ok(()))
                    .await?,
            ),
        ))),
        Command::Branches { expected, offset } => {
            ensure!(
                replies.history_head() == Some(&expected),
                "History intent differs from the displayed head"
            );
            Ok(Response::Branches(Box::new(
                worker.history_branches(expected, offset, 32).await?,
            )))
        }
        Command::Review {
            id,
            expected,
            target,
        } => {
            ensure!(
                replies.history_head() == Some(&expected),
                "History intent differs from the displayed head"
            );
            let review = worker
                .review_history_operation(
                    HistoryReviewRequest {
                        id,
                        sender,
                        flow: worker.address().clone(),
                        expected_head: expected,
                        target,
                    },
                    |_| Ok(()),
                )
                .await?;
            Ok(Response::Review(Box::new(review)))
        }
        Command::Status(id) => Ok(Response::Operation(
            worker
                .history_operation(sender, id, |_| Ok(()))
                .await?
                .map(Box::new),
        )),
        Command::Abort(id) => Ok(Response::Operation(Some(Box::new(
            worker
                .abort_history_operation(sender, id, |_| Ok(()))
                .await?,
        )))),
        Command::Confirm(request) => {
            ensure!(
                request.sender == sender && &request.flow == worker.address(),
                "History review belongs to another operator or Flow"
            );
            // The reviewed head remains unchanged on retry, even after the view installs the result.
            let mut commits = worker.subscribe_commits();
            let mut record = worker.begin_history_operation(*request, |_| Ok(())).await?;
            let mut published = None;
            loop {
                if published.as_ref() != Some(&record.status) {
                    published = Some(record.status.clone());
                    let _ = replies.send(TuiCommand::ConversationHistory {
                        result: Ok(Response::Operation(Some(Box::new(record.clone())))),
                        finished: false,
                    });
                }
                record = match record.status {
                    HistoryOperationStatus::Applied { .. }
                    | HistoryOperationStatus::Aborted { .. } => {
                        return Ok(Response::Operation(Some(Box::new(record))));
                    }
                    HistoryOperationStatus::Preparing => {
                        worker
                            .stop_history_operation(sender.clone(), record.request.id, |_| Ok(()))
                            .await?
                    }
                    HistoryOperationStatus::Ready { .. } => {
                        worker
                            .apply_history_operation(sender.clone(), record.request.id, |_| Ok(()))
                            .await?
                    }
                    HistoryOperationStatus::Stopping => {
                        commits.changed().await.context(
                            "History owner stopped; query the retained operation before retrying",
                        )?;
                        worker
                            .history_operation(sender.clone(), record.request.id, |_| Ok(()))
                            .await?
                            .context("Accepted history operation is unavailable")?
                    }
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::flow_store::{
        EntrySnapshot, FlowDescriptor, FlowRef, FlowTransition,
        completion::InvocationResult,
        history_operation::{HistoryOperationRequest, HistoryOperationTarget},
        source_capture::{ConversationBranchId, ConversationCutoff, SourceSelection},
    };
    use atman_runtime::source_program::RevisionBundle;

    async fn fixture(active: bool) -> (tempfile::TempDir, FlowWorker, HistoryOperationRequest) {
        let directory = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(directory.path()).unwrap();
        let id = Uuid::now_v7();
        let mut session = root
            .create_session(SessionDescriptor {
                id,
                configuration: Default::default(),
                project: None,
                title: "History controls".into(),
            })
            .unwrap();
        let mut store = session
            .create_flow(FlowDescriptor {
                address: FlowRef {
                    session_id: id.into(),
                    instance_id: FlowInstanceId::now(),
                },
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: RevisionBundle {
                        entry: "main.at".into(),
                        sources: [("main.at".into(), "flow main() {}".into())].into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        store
            .transition(store.state().revision(), FlowTransition::Activated)
            .unwrap();
        store
            .transition(
                store.state().revision(),
                FlowTransition::TurnStarted {
                    turn: atman_rt::TurnId::now(),
                    run: atman_rt::RunId::now(),
                },
            )
            .unwrap();
        let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(turn)) =
            store.state().state()
        else {
            panic!("active turn")
        };
        let lease = turn.lease.clone();
        let done = store
            .finish_turn(
                lease.clone(),
                InvocationResult::Unobserved(atman_rt::TurnOutcome::Completed),
            )
            .unwrap();
        let expected = store.history_head();
        let target = HistoryOperationTarget::Revert {
            observed: SourceSelection {
                flow: store.state().address().clone(),
                branch: expected.branch,
                cursor: store.cursor().require_durable().unwrap().clone(),
            },
            cutoff: Box::new(ConversationCutoff::AfterCompletedTurn {
                lease,
                receipt: done.receipt.require_durable().unwrap().clone(),
            }),
            branch: ConversationBranchId::random(),
        };
        if active {
            store
                .transition(
                    store.state().revision(),
                    FlowTransition::TurnStarted {
                        turn: atman_rt::TurnId::now(),
                        run: atman_rt::RunId::now(),
                    },
                )
                .unwrap();
        }
        let review = store
            .review_history_operation(
                HistoryReviewRequest {
                    id: Uuid::now_v7(),
                    sender: SenderIdentity::User(USER),
                    flow: store.state().address().clone(),
                    expected_head: expected,
                    target,
                },
                |_| Ok(()),
            )
            .unwrap();
        (
            directory,
            FlowWorker::start(store).await.unwrap(),
            review.request,
        )
    }

    async fn request(
        worker: &FlowWorker,
        command: Command,
    ) -> (
        BoundControl,
        mpsc::UnboundedReceiver<atman_tui::control::ControlReply>,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (sender, replies) = atman_tui::ControlSender::for_flow(
            ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: worker.address().clone().into(),
            },
            tx,
        );
        sender
            .send(TuiControl::ConversationHistory(command))
            .unwrap();
        (rx.recv().await.unwrap(), replies)
    }

    #[tokio::test]
    async fn confirmed_history_survives_closed_view_and_retries_original_receipts() {
        let (_directory, worker, reviewed) = fixture(false).await;
        let command = Command::Confirm(Box::new(reviewed.clone()));
        let (bound, replies) = request(&worker, command.clone()).await;
        drop(replies);
        let mut forged = reviewed.clone();
        forged.sender = SenderIdentity::User(Uuid::now_v7());
        let before = worker.snapshot().await.unwrap().cursor;
        assert!(
            execute(&worker, Command::Confirm(Box::new(forged)), &bound.replies)
                .await
                .is_err()
        );
        assert_eq!(worker.snapshot().await.unwrap().cursor, before);
        let Response::Operation(Some(applied)) = execute(&worker, command.clone(), &bound.replies)
            .await
            .unwrap()
        else {
            panic!("operation")
        };
        assert!(matches!(
            applied.status,
            HistoryOperationStatus::Applied { .. }
        ));
        let cursor = worker.snapshot().await.unwrap().cursor;
        let Response::Operation(Some(retry)) =
            execute(&worker, command, &bound.replies).await.unwrap()
        else {
            panic!("retry")
        };
        assert_eq!(retry, applied);
        assert_eq!(worker.snapshot().await.unwrap().cursor, cursor);
        let restore = worker
            .review_history_operation(
                HistoryReviewRequest {
                    id: Uuid::now_v7(),
                    sender: SenderIdentity::User(USER),
                    flow: worker.address().clone(),
                    expected_head: worker.status().await.unwrap().history_head.unwrap(),
                    target: HistoryOperationTarget::Restore {
                        branch: reviewed.expected_head.branch,
                    },
                },
                |_| Ok(()),
            )
            .await
            .unwrap();
        execute(
            &worker,
            Command::Confirm(Box::new(restore.request)),
            &bound.replies,
        )
        .await
        .unwrap();
        assert_eq!(
            worker.status().await.unwrap().history_head.unwrap().branch,
            reviewed.expected_head.branch
        );
        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stopping_keeps_original_progress_and_abort_remains_responsive() {
        let (_directory, worker, reviewed) = fixture(true).await;
        let command = Command::Confirm(Box::new(reviewed.clone()));
        let (bound, mut replies) = request(&worker, command.clone()).await;
        let original = bound.ticket.clone();
        let owner = worker.clone();
        let task = tokio::spawn(async move { dispatch(&owner, command, &bound.replies).await });
        loop {
            let reply = tokio::time::timeout(std::time::Duration::from_secs(5), replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reply.ticket, original);
            if let TuiCommand::ConversationHistory {
                result: Ok(Response::Operation(Some(record))),
                finished: false,
            } = reply.command
                && record.status == HistoryOperationStatus::Stopping
            {
                break;
            }
        }
        assert!(!task.is_finished());
        assert_eq!(
            worker.status().await.unwrap().history_head,
            Some(reviewed.expected_head.clone())
        );
        let (abort, _results) = request(&worker, Command::Abort(reviewed.id)).await;
        let Response::Operation(Some(aborted)) =
            execute(&worker, Command::Abort(reviewed.id), &abort.replies)
                .await
                .unwrap()
        else {
            panic!("abort")
        };
        assert!(matches!(
            aborted.status,
            HistoryOperationStatus::Aborted { .. }
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            worker.status().await.unwrap().history_head,
            Some(reviewed.expected_head)
        );
        worker.shutdown().await.unwrap();
    }
}
