//! CLI views and project association changes for recorded session catalogs.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use atman_runtime::flow_store::RuntimeStore;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum SessionSource {
    Persistent,
    Legacy,
}

fn move_target(
    root: &Path,
    sid: &str,
    source: Option<SessionSource>,
) -> Result<(String, SessionSource)> {
    let exact = Uuid::parse_str(sid).ok().map(|id| id.to_string());
    let matches = |name: &str| {
        exact
            .as_ref()
            .map_or_else(|| name.starts_with(sid), |id| name == id)
    };
    let mut targets = Vec::new();
    if source != Some(SessionSource::Legacy) {
        targets.extend(
            RuntimeStore::session_ids(root)?
                .into_iter()
                .map(|id| id.to_string())
                .filter(|id| matches(id))
                .map(|id| (id, SessionSource::Persistent)),
        );
    }
    if source != Some(SessionSource::Persistent) {
        match std::fs::read_dir(root.join("sessions")) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if entry.file_type()?.is_dir() && matches(&name) {
                        targets.push((name, SessionSource::Legacy));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    match targets.len() {
        0 => bail!("no session found matching `{sid}` in the selected storage source"),
        1 => Ok(targets.remove(0)),
        _ => bail!(
            "ambiguous session `{sid}`: {} matching records; use a full ID and --source persistent or --source legacy",
            targets.len()
        ),
    }
}

pub(crate) async fn move_project(
    root: &Path,
    sid: &str,
    cwd: &Path,
    source: Option<SessionSource>,
) -> Result<()> {
    let root = root.to_owned();
    let sid = sid.to_owned();
    let cwd = cwd.to_owned();
    tokio::task::spawn_blocking(move || {
        ensure!(cwd.is_dir(), "not a directory: {}", cwd.display());
        let cwd = cwd.canonicalize().with_context(|| format!("resolve path {}", cwd.display()))?;
        let (id, source) = move_target(&root, &sid, source)?;
        match source {
            SessionSource::Persistent => {
                let store = RuntimeStore::open(&root)?;
                let mut session = store.open_session(Uuid::parse_str(&id)?)
                    .context("open session catalog for editing; if another host owns it, use Move Session in that host")?;
                let atman_runtime::flow_store::SessionConfiguration::Atman(configuration) =
                    &session.projection().descriptor.configuration else {
                        bail!("session uses host-provided configuration; change its project through that host");
                    };
                let config = atman_runtime::config_hub::ConfigHub::from_source(configuration)?;
                let project = atman_runtime::storage::ProjectContext::resolve(&config, &cwd, &root)?;
                session.set_project(&session.cursor().clone(), Some(project))?;
                println!("persistent session {id} project: {}", cwd.display());
            }
            SessionSource::Legacy => {
                let dir = root.join("sessions").join(&id);
                let mut meta: atman_runtime::session_meta::SessionMeta =
                    match std::fs::read(dir.join("meta.json")) {
                        Ok(bytes) => serde_json::from_slice(&bytes)
                            .context("read legacy session metadata before changing its project")?,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
                        Err(error) => return Err(error.into()),
                    };
                meta.rebase(&cwd);
                meta.save(&dir)?;
                println!("legacy session {id} moved to {}", cwd.display());
                if let Some(project) = meta.project_root {
                    println!("  project_root: {}", project.display());
                }
            }
        }
        Ok(())
    }).await?
}

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
        if project.is_some_and(|wanted| {
            read.projection
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
            let location = read
                .projection
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
    println!("session_id: {id}");
    println!("layout:     persistent (recorded catalog)");
    println!(
        "title:      {}",
        single_line(read.projection.title.as_deref().unwrap_or("-"))
    );
    println!("dir:        {}", read.dir.display());
    println!(
        "project:    {}",
        read.projection
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
