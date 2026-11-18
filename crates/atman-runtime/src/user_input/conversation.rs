//! Initial conversation source preparation for native and network hosts.

use std::path::Path;

use anyhow::{Context, Result, ensure};

use crate::{
    config_hub::ConfigHub,
    flow_authority::{EffectiveAuthority, contract_allows_shell},
    flow_store::{EntrySnapshot, SessionDescriptor, launch::EntryStart},
    source_program::load_program_from_bundle,
    storage::ProjectContext,
    trust::TrustConfig,
};

pub struct PreparedConversationEntry {
    pub entry: EntrySnapshot,
    pub start: EntryStart,
    pub trust: TrustConfig,
    pub authority: EffectiveAuthority,
}

/// Capture the initial agent or retained source, startup hook and execution authority. Managed-command installation requires the caller's original host effect boundary.
pub fn prepare_conversation_entry(
    config: &ConfigHub,
    descriptor: &SessionDescriptor,
    project: Option<&ProjectContext>,
    retained: Option<&EntrySnapshot>,
    session_start: bool,
    directory: Option<&Path>,
) -> Result<PreparedConversationEntry> {
    let roots = descriptor.source_roots_for(project);
    let (program, name) = match retained {
        Some(entry) => (
            load_program_from_bundle(&entry.bundle, &entry.path, &roots)?,
            entry.name.clone(),
        ),
        None => {
            let (program, name, _, _) = super::resolve_slash_command(
                "/agent",
                config.config_dir(),
                project.map(|project| project.root()),
            )?;
            (program, name)
        }
    };
    let flow = program
        .entry_flow(&name)
        .and_then(|id| program.flow(&id))
        .context("default agent entry is unavailable")?;
    ensure!(
        crate::validate::invocation_message_parameter(flow)?.is_some(),
        "default agent entry requires an invocation user_message parameter"
    );
    let trust = config.trust_config()?;
    let authority = EffectiveAuthority::root(
        &trust,
        contract_allows_shell(flow.contract.as_ref()),
        directory
            .map(Path::to_path_buf)
            .or_else(|| project.map(|project| project.root().to_path_buf())),
    );
    let entry = EntrySnapshot {
        name,
        path: program
            .entry_path()
            .context("default source path is unavailable")?
            .to_owned(),
        bundle: program.revision_bundle(),
    };
    let startup = config.config_dir().join("on_session_start.at");
    let start = if session_start && startup.try_exists()? {
        let program = crate::source_program::load_program(&startup, &roots)?;
        match program.entry_file().flows.first() {
            Some(flow) => EntryStart::Command {
                entry: Box::new(EntrySnapshot {
                    name: flow.name.name.clone(),
                    path: startup,
                    bundle: program.revision_bundle(),
                }),
                arguments: Default::default(),
                invocation: Default::default(),
            },
            None => EntryStart::Waiting,
        }
    } else {
        EntryStart::Waiting
    };
    Ok(PreparedConversationEntry {
        entry,
        start,
        trust,
        authority,
    })
}
