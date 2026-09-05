#![cfg(unix)]

use std::process::{Child, Command, Stdio};

use std::os::unix::fs::FileTypeExt;

use atman_client::{Client, ClientIdentity, HttpTransport, UnixTransport};
use atman_proto::{FlowRunId, FormAnswer, FormSubmission, RunLifecycle, StartRunResponse};
use futures::StreamExt;

const AUTH_TOKEN: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

struct DaemonChild(Child);

impl DaemonChild {
    fn terminate(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        unsafe {
            libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
        }
        for _ in 0..100 {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

async fn connect(base_url: &str) -> Client {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            let transport = HttpTransport::new(base_url, AUTH_TOKEN).unwrap();
            match Client::connect(transport, ClientIdentity::new("process-test", "1")).await {
                Ok(client) => return client,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
            }
        }
    })
    .await
    .expect("daemon did not accept HTTP connections")
}

async fn wait_for_form(
    session: &atman_client::SessionClient,
) -> atman_proto::PendingFormProjection {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            session.refresh_until_current().await.unwrap();
            if let Some(form) = session.current().projection().interactions.forms.first() {
                return form.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("form did not enter the public session projection")
}

async fn wait_for_terminal_run(
    session: &atman_client::SessionClient,
    run_id: &FlowRunId,
) -> RunLifecycle {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            session.refresh_until_current().await.unwrap();
            if let Some(state) = session
                .current()
                .projection()
                .runs
                .iter()
                .find(|run| &run.id == run_id)
                .map(|run| run.state)
                && matches!(
                    state,
                    RunLifecycle::Cancelled
                        | RunLifecycle::Succeeded
                        | RunLifecycle::Failed
                        | RunLifecycle::Lost
                )
            {
                return state;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("run did not reach a terminal projection state")
}

async fn wait_for_no_forms(session: &atman_client::SessionClient) {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            session.refresh_until_current().await.unwrap();
            if session.current().projection().interactions.forms.is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("resolved forms did not leave the public session projection");
}

async fn start_run(
    session: &atman_client::SessionClient,
    flow_path: &std::path::Path,
) -> StartRunResponse {
    session
        .start_run(
            flow_path.to_string_lossy().into_owned(),
            None,
            serde_json::Map::new(),
            None,
            Vec::new(),
        )
        .await
        .expect("failed to start flow")
}

#[tokio::test]
async fn client_round_trip_survives_a_real_daemon_reconnect() {
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    let config_dir = temp.path().join("config");
    let project_dir = temp.path().join("project");
    let commands_dir = config_dir.join("commands");
    std::fs::create_dir_all(&commands_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(
        config_dir.join("daemon.toml"),
        format!("auth_token = \"{AUTH_TOKEN}\"\n"),
    )
    .unwrap();
    std::fs::write(
        config_dir.join("routes.at"),
        "default_route { flow: echo }\n",
    )
    .unwrap();
    std::fs::write(
        commands_dir.join("echo.at"),
        "flow echo(message: string) -> string {\n    return message\n}\n",
    )
    .unwrap();
    let wait_flow = commands_dir.join("wait.at");
    std::fs::write(
        &wait_flow,
        "flow wait() -> bool {\n    answer = user_confirm(\"Proceed?\")\n    session.push(message.assistant(to_json_string(answer)))\n    return answer\n}\n",
    )
    .unwrap();

    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let socket_path = data_dir.join("run/atman.sock");
    let mut daemon = DaemonChild(
        Command::new(env!("CARGO_BIN_EXE_atman-daemon"))
            .current_dir(&project_dir)
            .env("ATMAN_TEST_DATA_DIR", &data_dir)
            .env("ATMAN_TEST_CONFIG_DIR", &config_dir)
            .env("ATMAN_DAEMON_CONFIG_PATH", config_dir.join("daemon.toml"))
            .env("ATMAN_DAEMON_PORT", port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );

    let base_url = format!("http://127.0.0.1:{port}");
    let client = connect(&base_url).await;
    assert!(
        std::fs::metadata(&socket_path)
            .unwrap()
            .file_type()
            .is_socket()
    );
    let session = client
        .create_session(
            Some(project_dir.to_string_lossy().into_owned()),
            Some("Black-box session".into()),
        )
        .await
        .unwrap();
    let session_id = session.session_id().clone();
    let initial_cursor = session.current().cursor();
    let mut events = client
        .session_events(session_id.clone(), initial_cursor)
        .await
        .unwrap()
        .expect("HTTP transport must provide the session stream");

    let sent = session
        .send_message("hello through the daemon", None, Vec::new())
        .await
        .unwrap();
    let streamed = tokio::time::timeout(WAIT_TIMEOUT, events.next())
        .await
        .expect("session stream did not publish the sent turn")
        .expect("session stream ended")
        .unwrap();
    assert_eq!(streamed.session_id, session_id);
    assert!(streamed.cursor > initial_cursor);
    assert_eq!(
        wait_for_terminal_run(&session, &sent.run_id).await,
        RunLifecycle::Succeeded
    );

    let prompted = start_run(&session, &wait_flow).await;
    let form = wait_for_form(&session).await;
    assert_eq!(form.run_id, prompted.run_id);
    let unix_client = Client::connect(
        UnixTransport::new(&socket_path),
        ClientIdentity::new("local-process-test", "1"),
    )
    .await
    .unwrap();
    let unix_session = unix_client
        .attach_session(session_id.clone())
        .await
        .unwrap();
    assert_eq!(wait_for_form(&unix_session).await, form);
    let resolved = unix_session
        .submit_form(
            form.id,
            FormSubmission::Submitted {
                answers: vec![FormAnswer::Confirmed { value: true }],
            },
        )
        .await
        .unwrap();
    assert!(resolved.resolved);
    assert_eq!(
        wait_for_terminal_run(&session, &prompted.run_id).await,
        RunLifecycle::Succeeded
    );

    assert!(
        session
            .current()
            .projection()
            .transcript
            .iter()
            .any(|item| {
                matches!(item, atman_proto::TranscriptItem::Message {
                    run_id: Some(run_id), message, ..
                } if run_id == &prompted.run_id
                    && message.role == atman_proto::MessageRole::Assistant
                    && message.parts.iter().any(|part| {
                        matches!(part, atman_proto::MessagePart::Text { text } if text == "true")
                    }))
            })
    );

    let cancellable = start_run(&session, &wait_flow).await;
    wait_for_form(&session).await;
    let cancelled = session
        .cancel_run(cancellable.run_id.clone())
        .await
        .unwrap();
    assert!(cancelled.cancelled);
    assert_eq!(
        wait_for_terminal_run(&session, &cancellable.run_id).await,
        RunLifecycle::Cancelled
    );
    wait_for_no_forms(&session).await;
    unix_session.refresh_until_current().await.unwrap();
    session.refresh_until_current().await.unwrap();
    assert_eq!(
        unix_session.current().projection(),
        session.current().projection()
    );

    drop(events);
    let before_reconnect = session.current();
    drop(session);
    drop(client);
    let reconnected = connect(&base_url)
        .await
        .attach_session(session_id)
        .await
        .unwrap();
    assert_eq!(
        reconnected.current().projection(),
        before_reconnect.projection()
    );
    assert_eq!(reconnected.current().cursor(), before_reconnect.cursor());

    daemon.terminate();
}
