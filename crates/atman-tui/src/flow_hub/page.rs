use atman_runtime::event_log::legacy::LegacyFlowKey;
use atman_runtime::flow_runtime::{
    access::InstancePage,
    catalog::{CatalogStatus, SessionPage},
};
use atman_runtime::flow_store::FlowInstanceId;
use uuid::Uuid;

use super::{Action, execution_label};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubRequest {
    Sessions {
        before: Option<Uuid>,
    },
    Flows {
        session: Uuid,
        before: Option<FlowInstanceId>,
    },
    LegacyFlows {
        session: Uuid,
        after: Option<LegacyFlowKey>,
    },
}

pub enum FlowHubPage {
    Sessions(SessionPage),
    Flows {
        session: Uuid,
        title: String,
        default_flow: Option<FlowInstanceId>,
        page: InstancePage,
    },
    LegacyFlows {
        session: Uuid,
        title: String,
        flows: Vec<LegacyFlowKey>,
        next: Option<LegacyFlowKey>,
        total: usize,
    },
}

pub(super) struct Row {
    pub title: String,
    pub detail: String,
    pub action: Action,
}

impl FlowHubPage {
    pub fn title(&self) -> &str {
        match self {
            Self::Sessions(_) => "Sessions",
            Self::Flows { title, .. } | Self::LegacyFlows { title, .. } => title,
        }
    }
    pub fn unit(&self) -> &str {
        match self {
            Self::Sessions(_) => "sessions",
            Self::Flows { .. } | Self::LegacyFlows { .. } => "flows",
        }
    }
    pub fn first(&self) -> HubRequest {
        match self {
            Self::Sessions(_) => HubRequest::Sessions { before: None },
            Self::Flows { session, .. } => HubRequest::Flows {
                session: *session,
                before: None,
            },
            Self::LegacyFlows { session, .. } => HubRequest::LegacyFlows {
                session: *session,
                after: None,
            },
        }
    }
    pub fn next(&self) -> Option<HubRequest> {
        match self {
            Self::Sessions(page) => page.next.map(|before| HubRequest::Sessions {
                before: Some(before),
            }),
            Self::Flows { session, page, .. } => {
                page.next.clone().map(|before| HubRequest::Flows {
                    session: *session,
                    before: Some(before),
                })
            }
            Self::LegacyFlows { session, next, .. } => {
                next.clone().map(|after| HubRequest::LegacyFlows {
                    session: *session,
                    after: Some(after),
                })
            }
        }
    }
    pub fn total(&self) -> usize {
        match self {
            Self::Sessions(page) => page.total,
            Self::Flows { page, .. } => page.total,
            Self::LegacyFlows { total, .. } => *total,
        }
    }
    pub(super) fn rows(&self, focused: Option<&crate::control::ViewAddress>) -> Vec<Row> {
        match self {
            Self::LegacyFlows { session, flows, .. } => flows
                .iter()
                .map(|owner| {
                    let address = crate::control::ViewAddress::Legacy {
                        session: *session,
                        owner: owner.clone(),
                    };
                    Row {
                        title: format!(
                            "{}{}",
                            address.label(),
                            if focused == Some(&address) {
                                " · focused"
                            } else {
                                ""
                            }
                        ),
                        detail: "Original history · read-only snapshot".into(),
                        action: Action::Focus(address),
                    }
                })
                .collect(),
            Self::Sessions(page) => page
                .sessions
                .iter()
                .map(|row| {
                    let status = match row.status {
                        CatalogStatus::Running => "Running",
                        CatalogStatus::Idle => "Idle",
                        CatalogStatus::Closed => "Closed",
                        CatalogStatus::Detached => "Detached",
                        CatalogStatus::Unavailable => "Unavailable",
                    };
                    let detail = row.unavailable.clone().unwrap_or_else(|| {
                        format!(
                            "{} · {status} · {} flows · {}",
                            row.session_id,
                            row.flow_count
                                .map_or_else(|| "?".into(), |count| count.to_string()),
                            row.project_root.as_ref().map_or_else(
                                || "No project".into(),
                                |path| path.display().to_string()
                            )
                        )
                    });
                    Row {
                        title: row
                            .title
                            .clone()
                            .unwrap_or_else(|| "Unavailable session".into()),
                        detail,
                        action: Action::Load(HubRequest::Flows {
                            session: row.session_id,
                            before: None,
                        }),
                    }
                })
                .collect(),
            Self::Flows {
                default_flow, page, ..
            } => page
                .instances
                .iter()
                .map(|row| {
                    let marker =
                        if Some(&row.address) == focused.and_then(|address| address.instance()) {
                            " · focused"
                        } else if default_flow.as_ref() == Some(&row.address.instance_id) {
                            " · default"
                        } else {
                            ""
                        };
                    let status = row.state.as_ref().map_or("Unavailable", |state| {
                        if row.loaded {
                            execution_label(state.state())
                        } else {
                            "Detached"
                        }
                    });
                    let paused = row
                        .state
                        .as_ref()
                        .is_some_and(|state| !state.pauses().is_empty());
                    Row {
                        title: format!(
                            "{}{marker}",
                            row.entry.as_deref().unwrap_or("Unavailable flow")
                        ),
                        detail: row.unavailable.clone().unwrap_or_else(|| {
                            format!(
                                "{} · {status}{}",
                                row.address.instance_id,
                                if paused { " · execution paused" } else { "" }
                            )
                        }),
                        action: Action::Focus(row.address.clone().into()),
                    }
                })
                .collect(),
        }
    }
}
