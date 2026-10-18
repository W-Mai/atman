//! Read-only CLI views of recorded persistent session catalogs.

use std::path::Path;

use anyhow::{Context, Result};
use atman_runtime::flow_store::RuntimeStore;
use uuid::Uuid;

pub(crate) fn inspect(
    root: &Path,
    sid: &str,
) -> Result<Option<atman_runtime::flow_store::SessionInspection>> {
    let Ok(id) = Uuid::parse_str(sid) else {
        return Ok(None);
    };
    if !RuntimeStore::session_ids(root)?.contains(&id) {
        return Ok(None);
    }
    Ok(Some(RuntimeStore::inspect_session(root, id)?))
}

pub(crate) fn tail(root: &Path, sid: &str, count: usize) -> Result<bool> {
    let Some(lines) = events(root, sid, count)? else {
        return Ok(false);
    };
    for line in lines {
        println!("{line}");
    }
    Ok(true)
}

pub(crate) fn events(
    root: &Path,
    sid: &str,
    count: usize,
) -> Result<Option<Vec<serde_json::Value>>> {
    let Some(session) = inspect(root, sid)? else {
        return Ok(None);
    };
    let id = session
        .projection
        .default_flow
        .as_ref()
        .context("session has no default flow")?;
    let mut lines = std::collections::VecDeque::new();
    let read = session.inspect_flow_updates(id, |cursor, updates| {
        for update in updates {
            if count == 0 {
                continue;
            }
            if lines.len() == count {
                lines.pop_front();
            }
            lines.push_back(serde_json::json!({"flow": id, "cursor": cursor, "update": update}));
        }
        Ok(())
    })?;
    if read.trailing_bytes != 0 {
        eprintln!(
            "flow {id}: {} incomplete trailing bytes left unchanged",
            read.trailing_bytes
        );
    }
    Ok(Some(lines.into_iter().collect()))
}

pub(crate) fn cost(root: &Path, sid: &str) -> Result<Option<crate::CostSummary>> {
    let Some(session) = inspect(root, sid)? else {
        return Ok(None);
    };
    let mut summary = crate::CostSummary::default();
    for flow in &session.projection.flows {
        let read = session.inspect_flow_updates(&flow.id, |_, updates| {
            for update in updates {
                if let atman_runtime::flow_store::view::FlowUpdate::ExecutionEvents(events) = update
                {
                    for event in events {
                        if let atman_runtime::event::Event::LlmCall {
                            model,
                            usage,
                            wallclock_ms,
                            ..
                        } = event.event
                        {
                            summary.record(model, usage, wallclock_ms);
                        }
                    }
                }
            }
            Ok(())
        })?;
        if read.trailing_bytes != 0 {
            eprintln!(
                "flow {}: {} incomplete trailing bytes left unchanged",
                flow.id, read.trailing_bytes
            );
        }
    }
    Ok(Some(summary))
}

pub(crate) fn list(root: &Path, project: Option<&Path>) -> Result<usize> {
    let mut rows = Vec::new();
    let mut errors = 0;
    for id in RuntimeStore::session_ids(root)? {
        let read = match RuntimeStore::inspect_session(root, id) {
            Ok(read) => read,
            Err(error) => {
                eprintln!("cannot inspect persistent session {id}: {error}");
                errors += 1;
                continue;
            }
        };
        let descriptor = &read.projection.descriptor;
        if project.is_some_and(|wanted| {
            descriptor
                .project
                .as_ref()
                .is_none_or(|binding| binding.root() != wanted)
        }) {
            continue;
        }
        if read.journal.trailing_bytes != 0 {
            eprintln!(
                "persistent session {id}: {} incomplete trailing bytes left unchanged",
                read.journal.trailing_bytes
            );
        }
        rows.push(read);
    }
    rows.sort_by_cached_key(|read| {
        std::cmp::Reverse(
            read.dir
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH),
        )
    });
    if !rows.is_empty() {
        println!("Persistent sessions (recorded catalog)");
        println!("{:<38} {:>5}  {:<28} where", "session_id", "flows", "title");
        for read in &rows {
            let descriptor = &read.projection.descriptor;
            let location = descriptor
                .project
                .as_ref()
                .map(|project| crate::short_project_path(project.root()))
                .unwrap_or_else(|| "-".into());
            println!(
                "{:<38} {:>5}  {:<28} {}",
                descriptor.id,
                read.projection.flows.len(),
                single_line(read.projection.title.as_deref().unwrap_or("-")),
                single_line(&location)
            );
        }
    }
    Ok(rows.len() + errors)
}

pub(crate) fn show(root: &Path, sid: &str) -> Result<bool> {
    let Ok(id) = Uuid::parse_str(sid) else {
        return Ok(false);
    };
    if !RuntimeStore::session_ids(root)?.contains(&id) {
        return Ok(false);
    }
    let read = RuntimeStore::inspect_session(root, id)
        .with_context(|| format!("inspect persistent session {id}"))?;
    let descriptor = &read.projection.descriptor;
    println!("session_id: {id}");
    println!("layout:     persistent (recorded catalog)");
    println!(
        "title:      {}",
        single_line(read.projection.title.as_deref().unwrap_or("-"))
    );
    println!("dir:        {}", read.dir.display());
    println!(
        "project:    {}",
        descriptor
            .project
            .as_ref()
            .map(|project| project.root().display().to_string())
            .unwrap_or_else(|| "-".into())
    );
    println!("flows:      {}", read.projection.flows.len());
    println!(
        "default:    {}",
        read.projection
            .default_flow
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".into())
    );
    println!(
        "catalog:    {} validated bytes; {} incomplete trailing bytes",
        read.journal.valid_bytes, read.journal.trailing_bytes
    );
    if let Some(cursor) = read.journal.cursor {
        println!(
            "cursor:     {}:{}:{}",
            cursor.journal_id, cursor.sequence, cursor.digest
        );
    }
    for flow in &read.projection.flows {
        let marker = if read.projection.default_flow.as_ref() == Some(&flow.id) {
            "*"
        } else {
            " "
        };
        println!("  {marker} {}", flow.id);
    }
    Ok(true)
}

fn single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}
