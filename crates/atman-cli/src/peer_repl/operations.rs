//! Explicit context operations retain their source and receipt independently of view selection.

use super::*;
use atman_runtime::{
    flow_store::FlowRef,
    form::{FormAnswer, FormSubmission},
    rendezvous::{PromptId, await_prompt_with_payload},
    tool::ToolCtx,
};
use std::collections::HashSet;
use tokio::task::{Id, JoinSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Operation {
    Suggest,
    Compact,
}

impl Operation {
    pub fn parse(command: &str) -> Option<Self> {
        match command.trim() {
            ":suggest" => Some(Self::Suggest),
            ":compact" => Some(Self::Compact),
            _ => None,
        }
    }

    pub fn notice(self) -> &'static str {
        match self {
            Self::Suggest => "Examining recent flow messages…",
            Self::Compact => "Compacting context…",
        }
    }
}

pub(super) struct Operations<T> {
    tasks: JoinSet<Result<String>>,
    receipts: HashMap<Id, ((FlowRef, Operation), T)>,
    active: HashSet<(FlowRef, Operation)>,
}

impl<T: Send + 'static> Operations<T> {
    pub fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
            receipts: HashMap::new(),
            active: HashSet::new(),
        }
    }

    pub fn start(
        &mut self,
        scope: &Attachment,
        worker: &FlowWorker,
        operation: Operation,
        request: Uuid,
        receipt: T,
    ) -> Result<()> {
        let address = worker.address().clone();
        let key = (address.clone(), operation);
        ensure!(
            !self.active.contains(&key),
            "this operation is already pending for this flow"
        );
        ensure!(
            address.session_id == scope.session.id().into(),
            "operation and Session scope differ"
        );
        let session = worker.context();
        if operation == Operation::Compact {
            let providers = scope.executor.providers.clone();
            self.enqueue(key, receipt, async move {
                crate::compact_context(&session, &providers).await
            });
            return Ok(());
        }
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
        self.enqueue(key, receipt, async move {
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
        });
        Ok(())
    }

    fn enqueue(
        &mut self,
        key: (FlowRef, Operation),
        receipt: T,
        future: impl Future<Output = Result<String>> + Send + 'static,
    ) {
        self.active.insert(key.clone());
        let task = self.tasks.spawn(future);
        self.receipts.insert(task.id(), (key, receipt));
    }

    pub async fn next(&mut self) -> (T, Result<String>) {
        let Some(result) = self.tasks.join_next_with_id().await else {
            return std::future::pending().await;
        };
        let (id, result) = match result {
            Ok(result) => result,
            Err(error) => (
                error.id(),
                Err(anyhow::anyhow!("context operation failed: {error}")),
            ),
        };
        let (key, receipt) = self
            .receipts
            .remove(&id)
            .expect("operation receipt exists until completion");
        self.active.remove(&key);
        (receipt, result)
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
            let mut tasks = Operations::new();
            let requests = [Uuid::now_v7(), Uuid::now_v7()];
            for (index, worker) in workers.iter().enumerate() {
                tasks
                    .start(&attachment, worker, Operation::Suggest, requests[index], index)
                    .unwrap();
            }
            assert!(
                tasks
                    .start(&attachment, &workers[0], Operation::Suggest, Uuid::now_v7(), 99)
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
            // Context work has independent slots per operation and flow. Waiting for one
            // flow's context lock must not prevent another flow from completing.
            let first_context = workers[0].context();
            let held_context = first_context.acquire_compact_lock().await;
            for (index, worker) in workers.iter().enumerate() {
                tasks.start(&attachment, worker, Operation::Compact, Uuid::now_v7(), 100 + index).unwrap();
            }
            assert!(tasks.start(&attachment, &workers[0], Operation::Compact, Uuid::now_v7(), 999).is_err());
            let (receipt, result) = tasks.next().await;
            assert_eq!(receipt, 101);
            assert!(result.unwrap().starts_with("Context unchanged"));
            assert_eq!(forms.list_pending().len(), 2);
            assert!(attachment.command(&workers[0], ":status").await.is_ok());
            drop(held_context);
            let (receipt, result) = tasks.next().await;
            assert_eq!(receipt, 100);
            assert!(result.unwrap().starts_with("Context unchanged"));
            // Answer in reverse order; receipts still identify their original caller.
            forms
                .submit(&requests[1].to_string(), FormSubmission::Rejected)
                .await
                .unwrap();
            let (receipt, result) = tasks.next().await;
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
            let (receipt, result) = tasks.next().await;
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
                .start(&attachment, &workers[0], Operation::Suggest, Uuid::now_v7(), 2)
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
            let mut failed = Operations::new();
            failed.start(&attachment, &workers[0], Operation::Suggest, Uuid::now_v7(), 3).unwrap();
            let (receipt, result) = failed.next().await;
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

    #[tokio::test]
    async fn failed_operations_return_the_original_receipt_and_release_their_slot() {
        let key = (
            FlowRef {
                session_id: Uuid::now_v7().into(),
                instance_id: FlowInstanceId::now(),
            },
            Operation::Compact,
        );
        let mut operations = Operations::new();
        operations.enqueue(key.clone(), 1, async { panic!("operation failure") });
        let (receipt, result) = operations.next().await;
        assert_eq!(receipt, 1);
        assert!(result.is_err());
        assert!(operations.active.is_empty());
        assert!(operations.receipts.is_empty());
        operations.enqueue(key, 2, async { Ok("completed".into()) });
        let (receipt, result) = operations.next().await;
        assert_eq!(receipt, 2);
        assert_eq!(result.unwrap(), "completed");
    }
}
