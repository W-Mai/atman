//! Explicit continuation of historical conversations without recovering old execution.

use super::*;
use atman_runtime::{
    event_log::legacy::LegacyFlowKey,
    flow_store::{FlowRef, history::LegacyArchive, view::FlowContentSummary},
    tools::tool_output::OutputSource,
};
use atman_tui::control::ViewAddress;

pub(super) struct HistorySeed {
    pub source: LegacyArchive,
    pub transcript: ArchiveTranscript,
    pub verify_source: bool,
}

impl HistorySeed {
    pub fn prepare(&self) -> Result<(tempfile::TempDir, Vec<OutputSource>)> {
        let staging = tempfile::Builder::new()
            .prefix("atman-history-outputs-")
            .tempdir()?;
        let outputs = self
            .transcript
            .outputs
            .capture_archived_references(&self.transcript.context, staging.path())?;
        // A source edit during asset capture cannot silently change the selected history.
        if self.verify_source {
            self.source.source.verify()?;
        }
        Ok((staging, outputs))
    }
}

impl scopes::Scopes {
    pub(super) async fn continue_history(
        self: &Arc<Self>,
        address: ViewAddress,
    ) -> Result<FlowRef> {
        let scopes = self.clone();
        tokio::spawn(async move { scopes.continue_with_config(address, None).await }).await?
    }

    pub(super) async fn continue_with_config(
        self: &Arc<Self>,
        address: ViewAddress,
        configuration: Option<ConfigHub>,
    ) -> Result<FlowRef> {
        // The original source need not remain readable once its continuation has committed.
        if let ViewAddress::Legacy { session, owner } = &address {
            if let Some(flow) = self.existing_continuation(*session, owner).await? {
                return Ok(flow);
            }
        }
        let (history, title, seed) = match address {
            ViewAddress::Legacy { session, owner } => {
                ensure!(
                    !matches!(owner, LegacyFlowKey::Unassigned),
                    "unassigned historical records cannot seed an executable conversation"
                );
                let (view, meta) = legacy_selection(&self.root, session, Some(owner)).await?;
                let config = configuration.map(Ok).unwrap_or_else(ConfigHub::global)?;
                let project = meta
                    .as_ref()
                    .and_then(|meta| meta.project_root.as_deref())
                    .map(|root| {
                        atman_runtime::storage::ProjectContext::resolve(&config, root, &self.root)
                    })
                    .transpose()?;
                let title = meta
                    .and_then(|meta| meta.title)
                    .unwrap_or_else(|| "Historical session".into());
                let source = view.continuation_source();
                let transcript = view.into_transcript();
                (
                    HistorySeed {
                        source,
                        transcript,
                        verify_source: true,
                    },
                    title,
                    scopes::SessionSeed {
                        config,
                        project,
                        directory: None,
                        prompt_resolver: None,
                    },
                )
            }
            ViewAddress::Instance(flow) => {
                let archive =
                    archived_selection(&self.root, flow.session_id.0, Some(flow.instance_id))
                        .await?
                        .context("only a historical flow can create a continuation")?;
                let Some(FlowContentSummary::History(source)) = archive.view.transcript().content()
                else {
                    bail!("selected flow has no historical source");
                };
                if let Some(flow) = self
                    .existing_continuation(source.session_id, &source.owner)
                    .await?
                {
                    return Ok(flow);
                }
                ensure!(
                    !matches!(source.owner, LegacyFlowKey::Unassigned),
                    "unassigned historical records cannot seed an executable conversation"
                );
                let config = configuration.map(Ok).unwrap_or_else(ConfigHub::global)?;
                (
                    HistorySeed {
                        source: source.clone(),
                        transcript: archive.view.transcript().archived_transcript()?,
                        verify_source: false,
                    },
                    archive.title.unwrap_or_else(|| "Historical session".into()),
                    scopes::SessionSeed {
                        config,
                        project: archive.project,
                        directory: None,
                        prompt_resolver: None,
                    },
                )
            }
        };
        ensure!(
            !history.transcript.context.is_empty(),
            "historical flow has no recoverable conversation context"
        );
        let id = LegacyArchive::continuation_id(history.source.session_id, &history.source.owner)?;
        let descriptor = SessionDescriptor {
            id,
            title: format!("{title} · continued"),
            project: seed.project.clone(),
            configuration: SessionConfiguration::Atman(seed.config.source()),
        };
        self.initialize_session_entry(
            seed,
            id,
            descriptor,
            scopes::CreationMode::Deferred,
            Some(history),
        )
        .await
    }

    async fn existing_continuation(
        &self,
        session: Uuid,
        owner: &LegacyFlowKey,
    ) -> Result<Option<FlowRef>> {
        let id = LegacyArchive::continuation_id(session, owner)?;
        let root = self.root.clone();
        let present = tokio::task::spawn_blocking(move || {
            RuntimeStore::session_ids(&root).map(|ids| ids.contains(&id))
        })
        .await??;
        if !present {
            return Ok(None);
        }
        let target = self.host.open_session(&self.root, id).await?;
        let address = FlowRef {
            session_id: id.into(),
            instance_id: atman_rt::FlowInstanceId(id.into()),
        };
        let catalog = target.snapshot().await?;
        if !catalog
            .catalog
            .flows
            .iter()
            .any(|entry| entry.id == address.instance_id)
        {
            return Ok(None);
        }
        let worker = target.open_flow(address.instance_id.clone()).await?;
        let Some(record) = worker.launch_record().await? else {
            return Ok(None);
        };
        ensure!(
            record
                .input
                .continued_from
                .as_ref()
                .is_some_and(|source| source.session_id == session && &source.owner == owner),
            "continuation identity belongs to another source"
        );
        Ok(Some(address))
    }
}

pub(super) async fn interact_history(
    scopes: Arc<scopes::Scopes>,
    address: ViewAddress,
    prompts: mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    let (input, mut received) = mpsc::unbounded_channel();
    let (printer, attached) = oneshot::channel();
    crate::spawn_stdin_reader(input, printer);
    let mut printer = attached.await.unwrap_or(None);
    crate::render_note(
        &mut printer,
        "Enter a message to continue in a new session; original history is preserved. :q exits."
            .into(),
    );
    while let Some(line) = received.recv().await {
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if matches!(text, ":quit" | ":q" | ":exit") {
            return Ok(());
        }
        if text.starts_with(':') {
            crate::render_note(&mut printer, "History controls cannot start execution. Enter a message to continue, or :q to exit.".into());
            continue;
        }
        let result: Result<Attachment> = async {
            let target = scopes.continue_history(address.clone()).await?;
            let scope = scopes.open_flow_scope(&target).await?;
            let worker = scope.session.open_flow(target.instance_id).await?;
            TerminalView::new(&worker)
                .await?
                .submit(&scope, &worker, text)
                .await?;
            Ok(scope)
        }
        .await;
        match result {
            Ok(scope) => return scope.interact_using(prompts, received, printer).await,
            Err(error) => crate::render_note(
                &mut printer,
                format!("Continuation failed; input was not accepted: {error:#}"),
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_rt::TurnId;
    use atman_runtime::{
        attachment_store::{AttachmentStore, image_bytes},
        event::{Event, EventEnvelope},
        message::{Message, MessageOrigin, MessagePart, MessageRole},
        tools::tool_output::{
            OutputStore, ToolOutputBudget, truncate_tool_result_content_with_budget,
        },
    };

    #[tokio::test]
    async fn history_continuation_owns_context_assets_and_accepts_new_input_without_replaying_work()
    {
        for (archived, source_present, foreign) in [
            (false, true, false),
            (true, false, false),
            (true, true, false),
            (true, true, true),
        ] {
            let data = tempfile::tempdir().unwrap();
            let config = ConfigHub::from_config_dir(data.path().join("config"));
            std::fs::create_dir_all(config.config_dir().join("commands")).unwrap();
            std::fs::write(config.config_dir().join("config.toml"), "").unwrap();
            std::fs::write(config.config_dir().join("commands/echo.at"),
                "flow agent(user_prompt: string) -> string { contract { invocation { user_message: user_prompt } } return final.answer(message: user_prompt) }").unwrap();
            std::fs::write(
                config.config_dir().join("routes.at"),
                "default_route { flow: echo }\n",
            )
            .unwrap();
            // Even an invalid startup program must remain entirely outside continuation.
            std::fs::write(
                config.config_dir().join("on_session_start.at"),
                "must not parse or execute this",
            )
            .unwrap();
            let session = Uuid::now_v7();
            let original = data
                .path()
                .join(if foreign {
                    "external/sessions"
                } else {
                    "sessions"
                })
                .join(session.to_string());
            std::fs::create_dir_all(&original).unwrap();
            let local = data.path().join("sessions").join(session.to_string());
            if foreign {
                std::fs::create_dir_all(&local).unwrap();
                std::fs::write(local.join("events.jsonl"), "Local session must remain").unwrap();
            }
            let image = AttachmentStore::at(&original).import_base64("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a7p8AAAAASUVORK5CYII=", None).unwrap();
            let bytes = image_bytes(&image).unwrap();
            let turn = TurnId::now();
            let mut user = Message::user_text(turn.clone(), "retained request");
            user.parts.push(MessagePart::Image { source: image });
            let mut call = Message::assistant_text(turn.clone(), "");
            call.parts.push(MessagePart::ToolUse {
                id: "historical-call".into(),
                name: "fs.read".into(),
                input: serde_json::json!({"path":"past.txt"}),
                intent: None,
            });
            let full = "first line\nsecond line\nretained complete result\n";
            let output = OutputStore::at(&original);
            let mut result = Message::user_text(turn.clone(), "");
            result.role = MessageRole::Tool;
            result.parts = vec![MessagePart::ToolResult {
                tool_use_id: "historical-call".into(),
                is_error: false,
                content: truncate_tool_result_content_with_budget(
                    full,
                    "fs.read",
                    Some(&output),
                    ToolOutputBudget {
                        max_lines: 1,
                        max_bytes: 16,
                        max_line_bytes: 16,
                    },
                ),
            }];
            let refs = output.capture_references(&[result.clone()]).unwrap();
            let output_id = serde_json::to_value(&refs).unwrap()[0]["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let mut answer = Message::assistant_text(turn.clone(), "historical answer");
            answer.origin = MessageOrigin::FinalAnswer;
            let events = vec![
                Event::UserMsg {
                    turn_id: turn.clone(),
                    flow_run_id: None,
                    message: Message::user_text(turn.clone(), "compacted away"),
                    presentation: None,
                    injection_id: None,
                },
                Event::SystemMsg {
                    turn_id: turn.clone(),
                    flow_run_id: None,
                    message: Message::system_compact_summary(
                        turn.clone(),
                        "retained summary",
                        1,
                        1,
                        1,
                    ),
                },
                Event::ContextCompact {
                    session_id: session.to_string(),
                    flow_run_id: None,
                    before_tokens: 100,
                    after_tokens: 10,
                    compacted_range_start: 0,
                    compacted_range_end: 0,
                    summary_text: Some("retained summary".into()),
                    replacement_msg_seq: Some(2),
                },
                Event::UserMsg {
                    turn_id: turn.clone(),
                    flow_run_id: None,
                    message: user,
                    presentation: None,
                    injection_id: None,
                },
                Event::AssistantMsg {
                    turn_id: turn.clone(),
                    flow_run_id: None,
                    message: call,
                },
                Event::ToolResultMsg {
                    turn_id: turn.clone(),
                    flow_run_id: None,
                    message: result,
                },
                Event::AssistantMsg {
                    turn_id: turn,
                    flow_run_id: None,
                    message: answer,
                },
            ];
            let raw = events
                .into_iter()
                .enumerate()
                .map(|(index, event)| {
                    format!(
                        "{}\n",
                        serde_json::to_string(&EventEnvelope::new(index as u64 + 1, event))
                            .unwrap()
                    )
                })
                .collect::<String>();
            let source = original.join("events.jsonl");
            std::fs::write(&source, &raw).unwrap();
            let history = LegacyHistory::inspect(&source, session).unwrap();
            assert_eq!(
                history
                    .read_flow(&LegacyFlowKey::Default)
                    .unwrap()
                    .transcript()
                    .context,
                history
                    .read_flow(&LegacyFlowKey::Default)
                    .unwrap()
                    .transcript()
                    .context
            );
            let address = if archived {
                let id =
                    atman_runtime::flow_store::migration::destination_id(&source, session).unwrap();
                let imported = RuntimeStore::open(data.path())
                    .unwrap()
                    .archive_legacy(
                        &source,
                        session,
                        SessionDescriptor {
                            id,
                            title: "Archive".into(),
                            project: None,
                            configuration: Default::default(),
                        },
                        true,
                    )
                    .unwrap();
                if !source_present {
                    std::fs::remove_dir_all(&original).unwrap();
                }
                FlowRef {
                    session_id: id.into(),
                    instance_id: imported.flows[0].instance.clone(),
                }
                .into()
            } else {
                ViewAddress::Legacy {
                    session,
                    owner: LegacyFlowKey::Default,
                }
            };
            let host = RuntimeHost::default();
            let (notices, _received) = mpsc::unbounded_channel();
            let scopes = Arc::new(scopes::Scopes::empty(
                data.path(),
                host.clone(),
                None,
                Arc::new(prompts::Prompts::new(notices)),
            ));
            if !archived {
                std::fs::rename(
                    original.join("tool_outputs"),
                    original.join("missing-outputs"),
                )
                .unwrap();
                assert!(
                    scopes
                        .continue_with_config(address.clone(), Some(config.clone()))
                        .await
                        .is_err()
                );
                assert_eq!(std::fs::read_to_string(&source).unwrap(), raw);
                std::fs::rename(
                    original.join("missing-outputs"),
                    original.join("tool_outputs"),
                )
                .unwrap();
            }
            let (left, right) = tokio::join!(
                scopes.continue_with_config(address.clone(), Some(config.clone())),
                scopes.continue_with_config(address.clone(), Some(config))
            );
            let target = left.unwrap();
            assert_eq!(right.unwrap(), target);
            let worker = host.flow(&target).await.unwrap();
            let record = worker.launch_record().await.unwrap().unwrap();
            assert!(record.acceptance.is_none());
            assert!(!record.input.execution.unwrap().session_start);
            assert!(worker.goal().await.unwrap().goal.is_none());
            assert!(worker.snapshot().await.unwrap().pending.is_empty());
            let context = worker.context().messages_full();
            assert_eq!(context.len(), 5);
            assert!(
                !context
                    .iter()
                    .any(|message| message.text_concat().contains("compacted away"))
            );
            assert!(
                context
                    .iter()
                    .any(|message| message.text_concat().contains("retained summary"))
            );
            if source_present && !foreign {
                use atman_tui::flow_hub::{FlowHubPage, HubRequest, SessionRemovalRequest};
                std::fs::write(&source, format!("{raw}\n")).unwrap();
                assert!(scopes.continuation_removal(target.clone()).await.is_err());
                assert!(source.is_file());
                std::fs::write(&source, &raw).unwrap();
                let review = scopes.continuation_removal(target.clone()).await.unwrap();
                assert_eq!(review.continuation.as_ref(), Some(&target));
                assert!(
                    matches!(&review.request, SessionRemovalRequest::Legacy(request) if request.session_id == session)
                );
                let request = review.request.clone();
                let current =
                    atman_tui::project_hub::SessionTarget::Persistent(target.session_id.0);
                assert_eq!(
                    FlowHubPage::SessionRemoval(review).first(),
                    HubRequest::ContinuationRemoval {
                        owner: target.clone()
                    }
                );
                std::fs::write(original.join("arrived-after-review"), "keep").unwrap();
                assert!(
                    scopes
                        .remove_session(request.clone(), current.clone())
                        .await
                        .is_err()
                );
                assert_eq!(std::fs::read_to_string(&source).unwrap(), raw);
                std::fs::remove_file(original.join("arrived-after-review")).unwrap();
                scopes
                    .remove_session(request.clone(), current.clone())
                    .await
                    .unwrap();
                scopes.remove_session(request, current).await.unwrap();
                assert!(!original.exists());
            }
            if foreign {
                let error = scopes
                    .continuation_removal(target.clone())
                    .await
                    .err()
                    .unwrap();
                assert!(error.to_string().contains("another storage location"));
                assert_eq!(std::fs::read_to_string(&source).unwrap(), raw);
                assert_eq!(
                    std::fs::read_to_string(local.join("events.jsonl")).unwrap(),
                    "Local session must remain"
                );
            }
            assert!(scopes.continuation_removal(target.clone()).await.is_err());
            assert_eq!(
                scopes.continue_history(address.clone()).await.unwrap(),
                target
            );
            let inherited_image = context
                .iter()
                .flat_map(|message| &message.parts)
                .find_map(|part| {
                    if let MessagePart::Image { source } = part {
                        Some(source)
                    } else {
                        None
                    }
                })
                .unwrap();
            assert_eq!(image_bytes(inherited_image).unwrap(), bytes);
            assert_eq!(
                OutputStore::at(worker.context().dir())
                    .read_lines(&output_id, 0, 100, ToolOutputBudget::default())
                    .unwrap()
                    .content,
                full
            );
            let mut reader = worker.subscribe_view(PAGE).await.unwrap();
            let mut model = atman_tui::flow_view::FlowViewModel::default();
            model
                .sync_view(
                    reader.refresh().await.unwrap(),
                    Some(&OutputStore::at(worker.context().dir())),
                )
                .unwrap();
            assert!(!model.is_archive());
            let scope = scopes.open_flow_scope(&target).await.unwrap();
            assert_eq!(
                scope.continued_session().await.unwrap(),
                Some(target.clone())
            );
            assert!(scope.restore_scheduling().await.unwrap().is_empty());
            let mut commits = worker.subscribe_commits();
            let request = Uuid::now_v7();
            let input = UserContent {
                text: "new request".into(),
                images: vec![],
                presentation: None,
            };
            scope
                .submit_user(&worker, request, input.clone(), Default::default())
                .await
                .unwrap();
            scope
                .submit_user(&worker, request, input, Default::default())
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    commits.borrow_and_update();
                    if worker.context().messages_full().iter().any(|message| {
                        message.is_visible_assistant_reply()
                            && message.text_concat() == "new request"
                    }) {
                        break;
                    }
                    commits.changed().await.unwrap();
                }
            })
            .await
            .unwrap_or_else(|error| panic!("new input did not produce a reply: {error}"));
            assert_eq!(
                worker
                    .context()
                    .messages_full()
                    .iter()
                    .filter(|message| message.is_visible_assistant_reply()
                        && message.text_concat() == "new request")
                    .count(),
                1
            );
            host.shutdown().await.unwrap();
            drop(scope);
            drop(worker);
            drop(scopes);
            drop(host);
            let reopened = RuntimeHost::default();
            let (notices, _received) = mpsc::unbounded_channel();
            let scopes = Arc::new(scopes::Scopes::empty(
                data.path(),
                reopened.clone(),
                None,
                Arc::new(prompts::Prompts::new(notices)),
            ));
            assert_eq!(scopes.continue_history(address).await.unwrap(), target);
            let worker = reopened.flow(&target).await.unwrap();
            assert!(worker.context().messages_full().iter().any(|message| {
                message.is_visible_assistant_reply() && message.text_concat() == "new request"
            }));
            assert_eq!(
                OutputStore::at(worker.context().dir())
                    .read_lines(&output_id, 0, 100, ToolOutputBudget::default())
                    .unwrap()
                    .content,
                full,
            );
            assert!(worker.snapshot().await.unwrap().pending.is_empty());
            reopened.shutdown().await.unwrap();
        }
    }
}
