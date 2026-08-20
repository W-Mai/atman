use std::sync::Arc;

use crate::{Executor, Session, Value};

pub async fn maybe_generate_session_name(
    executor: &Executor,
    session: &Arc<Session>,
    user_text: &str,
) -> anyhow::Result<bool> {
    let meta = crate::session_meta::SessionMeta::load(session.dir()).unwrap_or_default();
    if meta.name_source == crate::session_meta::NameSource::User
        || meta.title.as_deref().is_some_and(|title| !title.is_empty())
    {
        return Ok(false);
    }
    let flow = atman_dsl::parse::parse_file(crate::templates::SESSION_NAME_AT)
        .map_err(|error| anyhow::anyhow!("parsing built-in session name flow: {error}"))?;
    let goal = session.goal().unwrap_or_default();
    let input = format!("User prompt:\n{user_text}\n\nGoal:\n{goal}");
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
    Ok(crate::session_meta::SessionMeta::set_auto_title(
        session.dir(),
        title,
    )?)
}
