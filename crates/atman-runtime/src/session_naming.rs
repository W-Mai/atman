use std::sync::Arc;

use crate::{Executor, Session, Value};

const MAX_MESSAGES: usize = 24;
const MAX_MESSAGE_CHARS: usize = 600;

pub async fn maybe_generate_session_name(
    executor: &Executor,
    session: &Arc<Session>,
) -> anyhow::Result<bool> {
    generate_session_name(executor, session, false).await
}

pub async fn force_generate_session_name(
    executor: &Executor,
    session: &Arc<Session>,
) -> anyhow::Result<bool> {
    generate_session_name(executor, session, true).await
}

async fn generate_session_name(
    executor: &Executor,
    session: &Arc<Session>,
    force: bool,
) -> anyhow::Result<bool> {
    let meta = crate::session_meta::SessionMeta::load(session.dir()).unwrap_or_default();
    if !force && meta.name_source == crate::session_meta::NameSource::User {
        return Ok(false);
    }
    let flow = atman_dsl::parse::parse_file(crate::templates::SESSION_NAME_AT)
        .map_err(|error| anyhow::anyhow!("parsing built-in session name flow: {error}"))?;
    let input = naming_input(session);
    let mut naming_executor = executor.clone();
    naming_executor.events = crate::event::EventSink::new();
    naming_executor.tool_ctx.events = None;
    naming_executor.tool_ctx.session_runtime = None;
    naming_executor.tool_ctx.session_messages = None;
    naming_executor.tool_ctx.session_messages_handle = None;
    naming_executor.tool_ctx.stdout_broadcast = None;
    let value = naming_executor
        .run(
            &flow,
            "session_name",
            vec![("input".into(), Value::Str(input))],
        )
        .await
        .map_err(|error| anyhow::anyhow!("running built-in session name flow: {error}"))?;
    let Value::Str(title) = value else {
        anyhow::bail!("session name flow did not return a string");
    };
    Ok(crate::session_meta::SessionMeta::set_auto_title_with_force(
        session.dir(),
        title,
        force,
    )?)
}

fn naming_input(session: &Session) -> String {
    let messages = session.messages();
    let start = messages.len().saturating_sub(MAX_MESSAGES);
    let mut input = format!(
        "Goal:\n{}\n\nRecent conversation:\n",
        session.goal().unwrap_or_default()
    );
    for message in &messages[start..] {
        let text: String = message
            .text_concat()
            .chars()
            .take(MAX_MESSAGE_CHARS)
            .collect();
        if !text.trim().is_empty() {
            input.push_str(message.role.as_str());
            input.push_str(": ");
            input.push_str(&text);
            input.push('\n');
        }
    }
    input
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    #[test]
    fn naming_input_uses_goal_and_recent_messages() {
        let session = Session::open_ephemeral();
        session.set_goal(Some("Ship session naming".into()));
        session.append_message(
            Message::user_text(crate::event::TurnId::now(), "Fix switcher"),
            None,
        );
        let input = naming_input(&session);
        assert!(input.contains("Ship session naming"));
        assert!(input.contains("user: Fix switcher"));
    }
}
