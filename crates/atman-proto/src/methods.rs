//! Implemented JSON-RPC methods and their public wire contracts.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::capabilities::MethodCapability;

macro_rules! methods {
    ($( $variant:ident, $constant:ident, $name:literal, $request:expr, $response:expr; )+) => {
        $(pub const $constant: &str = $name;)+

        /// Methods accepted by the daemon dispatcher. Unknown names are not capabilities.
        #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
        pub enum Method {
            $(#[serde(rename = $name)] $variant,)+
        }

        impl Method {
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            pub fn from_name(name: &str) -> Option<Self> {
                match name { $($name => Some(Self::$variant),)+ _ => None }
            }

            pub const fn name(self) -> &'static str {
                match self { $(Self::$variant => $name,)+ }
            }

            pub fn capability(self) -> MethodCapability {
                let (request, response): (Option<&str>, Option<&str>) = match self {
                    $(Self::$variant => ($request, $response),)+
                };
                MethodCapability {
                    revision: self.revision(),
                    request_schema: request.map(str::to_owned),
                    response_schema: response.map(str::to_owned),
                }
            }
        }
    };
}

impl Method {
    const fn revision(self) -> u32 {
        match self {
            Self::FlowGet
            | Self::FlowInvoke
            | Self::GoalGet
            | Self::GoalSet
            | Self::GoalPause
            | Self::GoalResume
            | Self::GoalClear => 2,
            _ => 1,
        }
    }
}

methods! {
    Capabilities, CAPABILITIES, "capabilities", Some("CapabilitiesRequest"), Some("CapabilitiesResponse");
    Ping, PING, "ping", None, Some("PingResponse");
    ManagementGet, MANAGEMENT_GET, "management.get", None, None;
    ManagementOpen, MANAGEMENT_OPEN, "management.open", None, None;
    ManagementSelect, MANAGEMENT_SELECT, "management.select", Some("SelectManagementRequest"), None;
    SessionCreate, SESSION_CREATE, "session.create", Some("CreateSessionRequest"), Some("SessionView");
    SessionOpen, SESSION_OPEN, "session.open", Some("SessionRequest"), Some("SessionView");
    SessionFlows, SESSION_FLOWS, "session.flows", Some("ListFlowsRequest"), None;
    SessionSelectFlow, SESSION_SELECT_FLOW, "session.select_flow", Some("FlowRequest"), Some("SessionView");
    FlowGet, FLOW_GET, "flow.get", Some("FlowRequest"), None;
    FlowControl, FLOW_CONTROL, "flow.control", Some("ControlFlowRequest"), Some("ControlFlowResponse");
    FlowControlBatch, FLOW_CONTROL_BATCH, "flow.control_batch", Some("ControlFlowBatchRequest"), None;
    FlowControlStatus, FLOW_CONTROL_STATUS, "flow.control_status", Some("ControlFlowStatusRequest"), None;
    FlowLaunch, FLOW_LAUNCH, "flow.launch", Some("LaunchFlowRequest"), Some("LaunchFlowResponse");
    FlowBind, FLOW_BIND, "flow.bind", Some("FlowRequest"), Some("BindFlowResponse");
    FlowInvoke, FLOW_INVOKE, "flow.invoke", Some("InvokeFlowRequest"), Some("InvokeFlowResponse");
    FlowInputStatus, FLOW_INPUT_STATUS, "flow.input_status", Some("InputStatusRequest"), Some("InputStatusResponse");
    FlowHistoryBranches, FLOW_HISTORY_BRANCHES, "flow.history_branches", Some("HistoryBranchesRequest"), Some("HistoryBranchesResponse");
    FlowHistoryReview, FLOW_HISTORY_REVIEW, "flow.history_review", Some("HistoryReviewRequest"), Some("HistoryReviewResponse");
    FlowHistoryBegin, FLOW_HISTORY_BEGIN, "flow.history_begin", Some("HistoryOperationRequest"), Some("HistoryOperationRecord");
    FlowHistoryStop, FLOW_HISTORY_STOP, "flow.history_stop", Some("HistoryOperationQuery"), Some("HistoryOperationRecord");
    FlowHistoryApply, FLOW_HISTORY_APPLY, "flow.history_apply", Some("HistoryOperationQuery"), Some("HistoryOperationRecord");
    FlowHistoryAbort, FLOW_HISTORY_ABORT, "flow.history_abort", Some("HistoryOperationQuery"), Some("HistoryOperationRecord");
    FlowHistoryStatus, FLOW_HISTORY_STATUS, "flow.history_status", Some("HistoryOperationQuery"), Some("HistoryStatusResponse");
    FlowOutput, FLOW_OUTPUT, "flow.output", Some("ReadFlowOutputRequest"), None;
    FlowEvents, FLOW_EVENTS, "flow.events", Some("ReadFlowEventsRequest"), None;
    GoalRun, GOAL_RUN, "goal.run", Some("RunFlowRequest"), None;
    GoalGet, GOAL_GET, "goal.get", Some("GoalRequest"), None;
    GoalSet, GOAL_SET, "goal.set", Some("GoalRequest"), None;
    GoalPause, GOAL_PAUSE, "goal.pause", Some("GoalRequest"), None;
    GoalResume, GOAL_RESUME, "goal.resume", Some("GoalRequest"), None;
    GoalClear, GOAL_CLEAR, "goal.clear", Some("GoalRequest"), None;
    RunFlow, RUN_FLOW, "run_flow", Some("RunFlowRequest"), Some("RunFlowResponse");
    CancelRun, CANCEL_RUN, "cancel_run", Some("CancelRunRequest"), Some("CancelRunResponse");
    ListSessions, LIST_SESSIONS, "list_sessions", Some("ListSessionsRequest"), Some("ListSessionsResponse");
    RenameSession, RENAME_SESSION, "rename_session", None, Some("SessionSummary");
    ResolvePrompt, RESOLVE_PROMPT, "resolve_prompt", Some("ResolvePromptRequest"), Some("ResolvePromptResponse");
    ListPermissionRequests, LIST_PERMISSION_REQUESTS, "list_permission_requests", Some("ListPermissionRequestsRequest"), Some("ListPermissionRequestsResponse");
    CreatePermissionGroup, CREATE_PERMISSION_GROUP, "create_permission_group", Some("CreatePermissionGroupRequest"), Some("PermissionGroupView");
    ResolvePermissionRequests, RESOLVE_PERMISSION_REQUESTS, "resolve_permission_requests", Some("ResolvePermissionRequestsRequest"), Some("ResolvePermissionRequestsResponse");
}

/// Historical constant; the daemon does not dispatch this method.
pub const GET_EVENTS: &str = "get_events";
