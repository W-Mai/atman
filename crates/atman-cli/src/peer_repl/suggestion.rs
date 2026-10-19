//! User-requested suggestions owned by the attachment, not by a selected view or turn.

use super::*;
use atman_runtime::{
    flow_store::FlowRef,
    form::{FormAnswer, FormSubmission},
    rendezvous::{PromptId, await_prompt_with_payload},
    tool::ToolCtx,
};
use std::collections::HashSet;
use tokio::task::JoinSet;

pub(super) struct Suggestions<T> {
    tasks: JoinSet<(FlowRef, T, Result<String>)>,
    active: HashSet<FlowRef>,
}

impl<T: Send + 'static> Suggestions<T> {
    pub fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
            active: HashSet::new(),
        }
    }

    pub fn start(
        &mut self,
        scope: &Attachment,
        worker: &FlowWorker,
        request: Uuid,
        receipt: T,
    ) -> Result<()> {
        let address = worker.address().clone();
        ensure!(
            !self.active.contains(&address),
            "a suggestion is already pending for this flow"
        );
        ensure!(
            address.session_id == scope.session.id().into(),
            "suggestion and Session scope differ"
        );
        let session = worker.context();
        let messages = session.messages_full();
        let mut context = ToolCtx::new();
        context.prompt_resolver = scope.executor.tool_ctx.prompt_resolver.clone();
        context.forms = Some(session.forms());
        context.session_runtime = Some(session);
        context.tool_use_id = Some(format!("suggest_{request}"));
        let resolver = context
            .form_resolver()
            .context("suggestion confirmation unavailable")?;
        let executor = scope.executor.clone();
        let config = scope.config.clone();
        self.active.insert(address.clone());
        self.tasks.spawn(async move {
            let result = async {
                let transcript = crate::suggest::recent_messages(&messages, crate::suggest::recent_turns_limit());
                let Some(proposal) = crate::suggest::prepare(&executor, &transcript).await? else {
                    return Ok("No reusable pattern in recent flow messages".into());
                };
                let reply = await_prompt_with_payload(
                    &resolver,
                    PromptId(request),
                    "form_ask",
                    serde_json::json!({"kind":"confirm", "prompt":proposal.confirmation(config.config_dir())}),
                    std::time::Duration::from_secs(300),
                ).await?;
                let accepted = match serde_json::from_value::<FormSubmission>(reply)? {
                    FormSubmission::Submitted { answers } => {
                        matches!(answers.as_slice(), [FormAnswer::Confirmed { value: true }])
                    }
                    FormSubmission::Rejected => false,
                };
                if !accepted { return Ok("Suggestion discarded".into()); }
                let (name, path) = crate::install_suggested_flow(config.config_dir(), &proposal.name, &proposal.source)?;
                Ok(format!("Installed /{name} at {}", path.display()))
            }.await;
            (address, receipt, result)
        });
        Ok(())
    }

    pub async fn next(&mut self) -> Result<(T, Result<String>)> {
        let Some(result) = self.tasks.join_next().await else {
            return std::future::pending().await;
        };
        let (address, receipt, result) = result?;
        self.active.remove(&address);
        Ok((receipt, result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::{
        flow_store::{EntrySnapshot, FlowDescriptor},
        providers::mock::MockProvider,
        source_program::RevisionBundle,
    };

    #[tokio::test]
    async fn suggestions_keep_flow_prompt_and_receipt_ownership_and_require_acceptance() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let config = ConfigHub::from_config_dir(dir.path().join("config"));
            std::fs::create_dir_all(config.config_dir()).unwrap();
            std::fs::write(
                config.config_dir().join("config.toml"),
                "[suggest]\nmodel = 'mini'\n",
            )
            .unwrap();
            let descriptor = SessionDescriptor {
                id: Uuid::now_v7(),
                project: None,
                configuration: SessionConfiguration::Atman(config.source()),
                title: "Suggestions".into(),
            };
            let root = RuntimeStore::open(dir.path()).unwrap();
            let host = RuntimeHost::default();
            let session = host
                .attach_session(root.create_session(descriptor.clone()).unwrap())
                .unwrap();
            let mut workers = Vec::new();
            for name in ["first", "second"] {
                let address = FlowRef {
                    session_id: descriptor.id.into(),
                    instance_id: FlowInstanceId::now(),
                };
                let worker = session
                    .create_flow(FlowDescriptor {
                        address,
                        creation_request: Uuid::now_v7(),
                        created_by: None,
                        content: EntrySnapshot {
                            name: name.into(),
                            path: dir.path().join("main.at"),
                            bundle: RevisionBundle {
                                entry: "main.at".into(),
                                sources: [("main.at".into(), "flow main() {}".into())].into(),
                                edges: Default::default(),
                            },
                        }
                        .into(),
                    })
                    .await
                    .unwrap();
                worker.context().append_message(
                    atman_runtime::message::Message::user_text(atman_rt::TurnId::now(), name),
                    None,
                );
                workers.push(worker);
            }
            let forms = Arc::new(atman_runtime::session::FormRegistry::default());
            let mut changes = forms.subscribe();
            let mut executor = Executor::new();
            executor.providers =
                atman_runtime::provider::ProviderRegistry::with_models(config.models().clone());
            executor.attach_provider_lifecycle(config.clone()).unwrap();
            let mut provider = MockProvider::new("mini");
            for worker in &workers {
                let transcript = crate::suggest::recent_messages(&worker.context().messages_full(), 30);
                assert!(!transcript.is_empty());
                provider = provider.with_prefix("mini", crate::suggest::build_meta_prompt(&transcript),
                    atman_runtime::Value::Str("```at\nflow repeat_task() { return 1 }\n```".into()));
            }
            executor.providers.register(Arc::new(provider));
            executor.tool_ctx.prompt_resolver = Some(Arc::new(
                atman_runtime::rendezvous::FormPromptResolver::new(forms.clone()),
            ));
            let (notices, _rx) = mpsc::unbounded_channel();
            let attachment = Attachment {
                host: host.clone(),
                session,
                descriptor,
                executor,
                config: config.clone(),
                prompts: Arc::new(prompts::Prompts::new(notices)),
                permissions: None,
            };
            let mut tasks = Suggestions::new();
            let requests = [Uuid::now_v7(), Uuid::now_v7()];
            for (index, worker) in workers.iter().enumerate() {
                tasks
                    .start(&attachment, worker, requests[index], index)
                    .unwrap();
            }
            assert!(
                tasks
                    .start(&attachment, &workers[0], Uuid::now_v7(), 99)
                    .is_err()
            );
            while changes.borrow_and_update().len() != 2 {
                tokio::select! {
                    result = tasks.next() => panic!("suggestion ended before confirmation: {result:?}"),
                    result = changes.changed() => result.unwrap(),
                }
            }
            for (index, worker) in workers.iter().enumerate() {
                let mut bound = forms.subscribe_for(worker.context().deferred_form_inbox());
                while bound.borrow_and_update().is_empty() {
                    bound.changed().await.unwrap();
                }
                let own = bound.borrow().clone();
                assert_eq!(own.len(), 1);
                assert_eq!(own[0].form_id, requests[index].to_string());
                assert!(own[0].kind.prompt().contains("flow repeat_task()"));
                assert!(
                    own[0]
                        .kind
                        .prompt()
                        .contains(&config.config_dir().display().to_string())
                );
            }
            assert!(!config.config_dir().join("commands").exists());
            // Answer in reverse order; receipts still identify their original caller.
            forms
                .submit(&requests[1].to_string(), FormSubmission::Rejected)
                .await
                .unwrap();
            let (receipt, result) = tasks.next().await.unwrap();
            assert_eq!(receipt, 1);
            assert_eq!(result.unwrap(), "Suggestion discarded");
            assert!(!config.config_dir().join("commands").exists());
            forms
                .submit(
                    &requests[0].to_string(),
                    FormSubmission::Submitted {
                        answers: vec![FormAnswer::Confirmed { value: true }],
                    },
                )
                .await
                .unwrap();
            let (receipt, result) = tasks.next().await.unwrap();
            assert_eq!(receipt, 0);
            assert!(result.unwrap().starts_with("Installed /repeat_task"));
            assert!(
                config
                    .config_dir()
                    .join("commands/repeat_task.at")
                    .is_file()
            );
            assert!(forms.list_pending().is_empty());
            // Detaching the entire owner releases pending confirmation without installing again.
            tasks
                .start(&attachment, &workers[0], Uuid::now_v7(), 2)
                .unwrap();
            while changes.borrow_and_update().is_empty() {
                changes.changed().await.unwrap();
            }
            drop(tasks);
            while !changes.borrow_and_update().is_empty() {
                changes.changed().await.unwrap();
            }
            assert!(
                !config
                    .config_dir()
                    .join("commands/repeat_task_v2.at")
                    .exists()
            );
            attachment.executor.providers.remove("mini");
            let mut failed = Suggestions::new();
            failed.start(&attachment, &workers[0], Uuid::now_v7(), 3).unwrap();
            let (receipt, result) = failed.next().await.unwrap();
            assert_eq!(receipt, 3);
            assert!(result.unwrap_err().to_string().contains("no provider"));
            assert!(forms.list_pending().is_empty());
            assert!(attachment.command(&workers[0], ":status").await.is_ok());
            assert!(workers[0].snapshot().await.unwrap().pending.is_empty());
            host.shutdown().await.unwrap();
        })
        .await
        .unwrap();
    }
}
