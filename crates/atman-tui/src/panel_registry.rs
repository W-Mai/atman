//! Shared registration policy for optional TUI navigation surfaces.

pub(crate) const EXTENDED: bool = cfg!(feature = "extended-panels");

pub(crate) fn command_enabled(command: &str) -> bool {
    EXTENDED
        || !matches!(
            command.split_whitespace().next().unwrap_or_default(),
            "flows" | "links" | "watchers" | "inbox" | "worktrees" | "workspace" | "atman"
        )
}

/// Session operations retain their transport without registering the directories.
pub(crate) fn navigation(action: crate::flow_hub::Action) -> crate::flow_hub::Action {
    use crate::flow_hub::{Action, HubRequest};
    if EXTENDED {
        return action;
    }
    match action {
        Action::Return | Action::Load(HubRequest::Sessions { .. }) => Action::SwitchSessions,
        Action::Load(
            HubRequest::Flows { .. }
            | HubRequest::IndexedHistory { .. }
            | HubRequest::LegacyFlows { .. }
            | HubRequest::SessionProject { .. }
            | HubRequest::ContinuationRemoval { .. }
            | HubRequest::SessionRemoval { .. },
        )
        | Action::Focus(_)
        | Action::Float(_)
        | Action::NewSession
        | Action::OpenLegacySession(_)
        | Action::OpenSession(_)
        | Action::RecoverIndexedHistory(_)
        | Action::SwitchSessions
        | Action::ChangeSessionProject(_)
        | Action::RemoveSession(_)
        | Action::None
        | Action::Close => action,
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_commands_and_direct_dispatch_share_the_surface_policy() {
        let builtins = crate::completion::builtins();
        for name in [
            "flows",
            "links",
            "watchers",
            "inbox",
            "worktrees",
            "workspace",
            "atman",
        ] {
            assert_eq!(
                builtins.iter().any(|(command, _)| *command == name),
                EXTENDED
            );
            assert_eq!(
                crate::palette::PaletteEntryId::from_command(name).is_some(),
                EXTENDED
            );
            if !EXTENDED {
                for _surface in ["main", "floating"] {
                    let mut ui = crate::UiState::new(crate::app::AppState::default());
                    let (tx, mut rx) = crate::ControlSender::channel();
                    for suffix in ["", " extra"] {
                        assert!(ui.wm.dispatch_meta_command(
                            &format!(":{name}{suffix}"),
                            &mut ui.app,
                            Some(&tx)
                        ));
                        assert!(ui.app.open_flow_hub.is_none());
                        assert!(rx.try_recv().is_err());
                        assert!(!ui.wm.modals.any_open());
                    }
                }
            }
        }
        for name in [
            "sessions",
            "projects",
            "goal",
            "models",
            "providers",
            "aliases",
            "mcp",
            "memory",
            "activity",
        ] {
            assert!(
                crate::palette::PaletteEntryId::from_command(name).is_some(),
                "{name}"
            );
        }
    }

    #[test]
    fn default_navigation_keeps_session_operations_without_registering_directories() {
        use crate::flow_hub::{Action, HubRequest};
        if EXTENDED {
            return;
        }
        assert!(matches!(navigation(Action::OpenManagement), Action::None));
        assert!(matches!(
            navigation(Action::Load(HubRequest::Workspaces { after: None })),
            Action::None
        ));
        assert!(matches!(navigation(Action::Return), Action::SwitchSessions));
        assert!(matches!(
            navigation(Action::Load(HubRequest::Sessions { before: None })),
            Action::SwitchSessions
        ));
        assert!(matches!(navigation(Action::NewSession), Action::NewSession));
        assert!(matches!(
            navigation(Action::Load(HubRequest::SessionProject {
                session: uuid::Uuid::now_v7()
            })),
            Action::Load(HubRequest::SessionProject { .. })
        ));
    }
}
