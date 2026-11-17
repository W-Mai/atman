use super::*;
use atman_runtime::{
    flow_store::{EntrySnapshot, FlowDescriptor, FlowRef},
    rendezvous::{FormPromptResolver, PromptContext, PromptId, PromptResolver},
    source_program::RevisionBundle,
};

async fn worker(host: &RuntimeHost, data: &Path) -> FlowWorker {
    let id = Uuid::now_v7();
    let session = host
        .create_session(
            data,
            SessionDescriptor {
                id,
                configuration: Default::default(),
                project: None,
                title: "Native interaction owner".into(),
            },
        )
        .await
        .unwrap();
    session
        .create_flow(FlowDescriptor {
            address: FlowRef {
                session_id: id.into(),
                instance_id: FlowInstanceId::now(),
            },
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: data.join("main.at"),
                bundle: RevisionBundle {
                    entry: "entry:main.at".into(),
                    sources: [("entry:main.at".into(), "flow main() {}".into())].into(),
                    edges: Default::default(),
                },
            }
            .into(),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn retired_observations_preserve_shared_form_inbox_and_original_compact_decisions() {
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let first = worker(&host, data.path()).await;
    let second = worker(&host, data.path()).await;
    let (notices, _notices) = mpsc::unbounded_channel();
    let prompts = prompts::Prompts::new(notices);
    let forms = first.context().forms();
    let native_form = forms.subscribe();
    let context = second.context();
    let native_compact = context.compact_reviews().subscribe();
    let mut observations = Observations::new(&prompts, &forms);
    observations.observe(&second);
    drop(native_form);
    drop(native_compact);

    let prompt = PromptId::now();
    let answer = FormPromptResolver::new(forms.clone()).register_in_context(
        prompt,
        "form_ask",
        serde_json::json!({"kind":"text", "prompt":"Reply to the original owner"}),
        &PromptContext {
            events: context.sink().clone(),
            session: Some(context.clone()),
            run_id: Some(atman_rt::RunId::now()),
            tool_use_id: Some("original-form".into()),
        },
    );
    assert!(forms.owns_form(&prompt.to_string(), &context.deferred_form_inbox()));
    assert!(!Arc::ptr_eq(&forms, &context.forms()));
    assert!(context.sink().snapshot().iter().any(|event| matches!(event,
        atman_runtime::event::Event::PendingPrompt { prompt_id, .. } if *prompt_id == prompt.0)));
    assert!(
        !first
            .context()
            .sink()
            .snapshot()
            .iter()
            .any(|event| matches!(event,
        atman_runtime::event::Event::PendingPrompt { prompt_id, .. } if *prompt_id == prompt.0))
    );
    let submission = serde_json::to_value(atman_runtime::form::FormSubmission::Submitted {
        answers: vec![atman_runtime::form::FormAnswer::TextEntered {
            text: "kept answer".into(),
        }],
    })
    .unwrap();
    let reply = |id, value, session| {
        crate::peer_repl::rpc::answer_native_interaction(&host, &prompts, id, value, session)
    };
    assert!(
        !reply(
            prompt.0,
            submission.clone(),
            Some(first.address().session_id.0)
        )
        .await
        .unwrap()
    );
    assert!(
        reply(
            prompt.0,
            submission.clone(),
            Some(second.address().session_id.0)
        )
        .await
        .unwrap()
    );
    assert_eq!(answer.await.unwrap(), submission);

    for (decision, expected) in [
        (serde_json::json!({"decision":"accept_as_is"}), None),
        (
            serde_json::json!({"decision":"accept_edited", "summary":"edited summary"}),
            Some("edited summary"),
        ),
        (serde_json::json!({"decision":"reject"}), Some("rejected")),
    ] {
        let id = Uuid::now_v7();
        let reviews = context.compact_reviews();
        let mut original = reviews.request(atman_runtime::session::PendingCompactReview {
            review_id: id.to_string(),
            summary: "original summary".into(),
            slice_preview: "original slice".into(),
            slice_count: 2,
            range_start: 3,
            range_end: 4,
            tokens_before: 50,
            emitted_at: chrono::Utc::now(),
        });
        assert!(matches!(
            original.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        observations.update();
        assert!(context.sink().snapshot().iter().any(|event| matches!(event,
            atman_runtime::event::Event::PendingPrompt { prompt_id, kind, payload }
                if *prompt_id == id && kind == "compact_review"
                    && payload["summary"] == "original summary"
                    && payload["range_start"] == 3)));
        assert!(
            !first
                .context()
                .sink()
                .snapshot()
                .iter()
                .any(|event| matches!(event,
            atman_runtime::event::Event::PendingPrompt { prompt_id, .. } if *prompt_id == id))
        );
        assert!(
            !reply(id, decision.clone(), Some(first.address().session_id.0))
                .await
                .unwrap()
        );
        assert!(
            reply(
                id,
                serde_json::json!({"decision":"unknown"}),
                Some(second.address().session_id.0)
            )
            .await
            .is_err()
        );
        assert_eq!(reviews.list_pending().unwrap().review_id, id.to_string());
        assert!(
            reply(id, decision, Some(second.address().session_id.0))
                .await
                .unwrap()
        );
        match (original.await.unwrap(), expected) {
            (atman_runtime::CompactReviewDecision::AcceptAsIs, None)
            | (atman_runtime::CompactReviewDecision::Reject, Some("rejected")) => {}
            (atman_runtime::CompactReviewDecision::AcceptEdited { summary }, Some(expected)) => {
                assert_eq!(summary, expected);
            }
            other => panic!("original decision changed: {other:?}"),
        }
        assert!(
            !reply(
                id,
                serde_json::json!({"decision":"reject"}),
                Some(second.address().session_id.0)
            )
            .await
            .unwrap()
        );
        observations.update();
        assert!(context.sink().snapshot().iter().any(|event| matches!(event,
            atman_runtime::event::Event::PromptExpired { prompt_id } if *prompt_id == id)));
    }
    host.shutdown().await.unwrap();
}
