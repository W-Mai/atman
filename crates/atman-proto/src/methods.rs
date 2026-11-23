//! Implemented JSON-RPC methods and their public wire contracts.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::capabilities::MethodCapability;

/// Client outcome classification when a request's response is unavailable. This does not control dispatcher task ownership or authorize automatic retries.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientOperation {
    Query,
    Command,
}

macro_rules! methods {
    ($( $variant:ident, $constant:ident, $name:literal, $operation:ident, $request:expr, $response:expr; )+) => {
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

            pub const fn client_operation(self) -> ClientOperation {
                match self { $(Self::$variant => ClientOperation::$operation,)+ }
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
    Capabilities, CAPABILITIES, "capabilities", Query, Some("CapabilitiesRequest"), Some("CapabilitiesResponse");
    Ping, PING, "ping", Query, None, Some("PingResponse");
    HostDescribe, HOST_DESCRIBE, "host.describe", Query, None, Some("HostDescription");
    HostHandoff, HOST_HANDOFF, "host.handoff", Command, Some("HandoffSessionRequest"), Some("SessionHandoffOutcome");
    DaemonShutdown, DAEMON_SHUTDOWN, "daemon.shutdown", Command, None, None;
    ManagementGet, MANAGEMENT_GET, "management.get", Command, None, None;
    ManagementOpen, MANAGEMENT_OPEN, "management.open", Command, None, None;
    ManagementSelect, MANAGEMENT_SELECT, "management.select", Command, Some("SelectManagementRequest"), None;
    ProjectList, PROJECT_LIST, "project.list", Query, Some("ListProjectsRequest"), Some("ProjectListPage");
    ProjectSetPinned, PROJECT_SET_PINNED, "project.set_pinned", Command, Some("SetProjectPinnedRequest"), Some("SetProjectPinnedResponse");
    SettingsGitCoAuthor, SETTINGS_GIT_CO_AUTHOR, "settings.git_co_author", Query, Some("ReadGitCoAuthorRequest"), Some("GitCoAuthorSetting");
    SettingsGitCoAuthorSet, SETTINGS_GIT_CO_AUTHOR_SET, "settings.git_co_author.set", Command, Some("GitCoAuthorSetting"), Some("GitCoAuthorSetting");
    SessionCreate, SESSION_CREATE, "session.create", Command, Some("CreateSessionRequest"), Some("SessionView");
    SessionOpen, SESSION_OPEN, "session.open", Command, Some("SessionRequest"), Some("SessionView");
    SessionBootstrap, SESSION_BOOTSTRAP, "session.bootstrap", Command, Some("BootstrapSessionRequest"), Some("LaunchFlowResponse");
    SessionFlows, SESSION_FLOWS, "session.flows", Query, Some("ListFlowsRequest"), Some("FlowListPage");
    SessionSelectFlow, SESSION_SELECT_FLOW, "session.select_flow", Command, Some("FlowRequest"), Some("SessionView");
    FlowGet, FLOW_GET, "flow.get", Command, Some("FlowRequest"), None;
    FlowExecution, FLOW_EXECUTION, "flow.execution", Query, Some("FlowExecutionRequest"), Some("FlowExecutionResponse");
    FlowOverview, FLOW_OVERVIEW, "flow.overview", Query, Some("FlowRequest"), Some("FlowOverviewResponse");
    FlowComposer, FLOW_COMPOSER, "flow.composer", Query, Some("FlowComposerRequest"), Some("FlowComposerResponse");
    FlowComposerPrepare, FLOW_COMPOSER_PREPARE, "flow.composer.prepare", Command, Some("FlowRequest"), Some("FlowComposerResponse");
    FlowComposerPreferenceSet, FLOW_COMPOSER_PREFERENCE_SET, "flow.composer.preference.set", Command, Some("SetComposerPreferenceRequest"), Some("ComposerPreferenceResponse");
    FlowComposerPreferenceStatus, FLOW_COMPOSER_PREFERENCE_STATUS, "flow.composer.preference.status", Query, Some("ComposerPreferenceStatusRequest"), Some("ComposerPreferenceResponse");
    FlowControl, FLOW_CONTROL, "flow.control", Command, Some("ControlFlowRequest"), Some("ControlFlowResponse");
    FlowControlBatch, FLOW_CONTROL_BATCH, "flow.control_batch", Command, Some("ControlFlowBatchRequest"), Some("FlowControlBatchRecord");
    FlowControlStatus, FLOW_CONTROL_STATUS, "flow.control_status", Command, Some("ControlFlowStatusRequest"), Some("FlowControlBatchRecord");
    FlowLaunch, FLOW_LAUNCH, "flow.launch", Command, Some("LaunchFlowRequest"), Some("LaunchFlowResponse");
    FlowBind, FLOW_BIND, "flow.bind", Command, Some("FlowRequest"), Some("BindFlowResponse");
    FlowInvoke, FLOW_INVOKE, "flow.invoke", Command, Some("InvokeFlowRequest"), Some("InvokeFlowResponse");
    FlowSubmit, FLOW_SUBMIT, "flow.submit", Command, Some("SubmitFlowRequest"), Some("SubmitFlowResponse");
    QuestionStart, QUESTION_START, "question.start", Command, Some("StartQuestionRequest"), Some("StartQuestionResponse");
    QuestionStatus, QUESTION_STATUS, "question.status", Query, Some("QuestionStatusRequest"), Some("QuestionStatusResponse");
    QuestionTranscript, QUESTION_TRANSCRIPT, "question.transcript", Query, Some("ReadQuestionTranscriptRequest"), Some("ReadQuestionTranscriptResponse");
    FlowInbox, FLOW_INBOX, "flow.inbox", Query, Some("ReadFlowInboxRequest"), Some("ReadFlowInboxResponse");
    FlowInboxEdit, FLOW_INBOX_EDIT, "flow.inbox_edit", Command, Some("EditFlowInboxRequest"), Some("EditFlowInboxResponse");
    FlowInputStatus, FLOW_INPUT_STATUS, "flow.input_status", Query, Some("InputStatusRequest"), Some("InputStatusResponse");
    FlowConversation, FLOW_CONVERSATION, "flow.conversation", Query, Some("ReadConversationRequest"), Some("ReadConversationResponse");
    FlowApprovalQueue, FLOW_APPROVAL_QUEUE, "flow.approval_queue", Query, Some("ReadFlowApprovalQueueRequest"), Some("FlowApprovalQueueResponse");
    FlowDrafts, FLOW_DRAFTS, "flow.drafts", Query, Some("ReadFlowDraftsRequest"), Some("ReadFlowDraftsResponse");
    FlowHistoryBranches, FLOW_HISTORY_BRANCHES, "flow.history_branches", Command, Some("HistoryBranchesRequest"), Some("HistoryBranchesResponse");
    FlowHistoryReview, FLOW_HISTORY_REVIEW, "flow.history_review", Command, Some("HistoryReviewRequest"), Some("HistoryReviewResponse");
    FlowHistoryBegin, FLOW_HISTORY_BEGIN, "flow.history_begin", Command, Some("HistoryOperationRequest"), Some("HistoryOperationRecord");
    FlowHistoryStop, FLOW_HISTORY_STOP, "flow.history_stop", Command, Some("HistoryOperationQuery"), Some("HistoryOperationRecord");
    FlowHistoryApply, FLOW_HISTORY_APPLY, "flow.history_apply", Command, Some("HistoryOperationQuery"), Some("HistoryOperationRecord");
    FlowHistoryAbort, FLOW_HISTORY_ABORT, "flow.history_abort", Command, Some("HistoryOperationQuery"), Some("HistoryOperationRecord");
    FlowHistoryStatus, FLOW_HISTORY_STATUS, "flow.history_status", Query, Some("HistoryOperationQuery"), Some("HistoryStatusResponse");
    FlowHistoryRecoverInput, FLOW_HISTORY_RECOVER_INPUT, "flow.history_recover_input", Query, Some("HistoryRecoverInputRequest"), Some("HistoryRecoveredInput");
    FlowOutput, FLOW_OUTPUT, "flow.output", Query, Some("ReadFlowOutputRequest"), Some("FlowOutputPage");
    FlowOutputDetail, FLOW_OUTPUT_DETAIL, "flow.output_detail", Query, Some("ReadFlowOutputDetailRequest"), Some("FlowOutputDetail");
    FlowEvents, FLOW_EVENTS, "flow.events", Query, Some("ReadFlowEventsRequest"), Some("FlowEventPage");
    GoalRun, GOAL_RUN, "goal.run", Command, Some("RunFlowRequest"), None;
    GoalGet, GOAL_GET, "goal.get", Command, Some("GoalRequest"), None;
    GoalSet, GOAL_SET, "goal.set", Command, Some("GoalRequest"), None;
    GoalPause, GOAL_PAUSE, "goal.pause", Command, Some("GoalRequest"), None;
    GoalResume, GOAL_RESUME, "goal.resume", Command, Some("GoalRequest"), None;
    GoalClear, GOAL_CLEAR, "goal.clear", Command, Some("GoalRequest"), None;
    RunFlow, RUN_FLOW, "run_flow", Command, Some("RunFlowRequest"), Some("RunFlowResponse");
    CancelRun, CANCEL_RUN, "cancel_run", Command, Some("CancelRunRequest"), Some("CancelRunResponse");
    ListSessions, LIST_SESSIONS, "list_sessions", Query, Some("ListSessionsRequest"), Some("ListSessionsResponse");
    RenameSession, RENAME_SESSION, "rename_session", Command, None, Some("SessionSummary");
    ResolvePrompt, RESOLVE_PROMPT, "resolve_prompt", Command, Some("ResolvePromptRequest"), Some("ResolvePromptResponse");
    ListPermissionRequests, LIST_PERMISSION_REQUESTS, "list_permission_requests", Command, Some("ListPermissionRequestsRequest"), Some("ListPermissionRequestsResponse");
    CreatePermissionGroup, CREATE_PERMISSION_GROUP, "create_permission_group", Command, Some("CreatePermissionGroupRequest"), Some("PermissionGroupView");
    ResolvePermissionRequests, RESOLVE_PERMISSION_REQUESTS, "resolve_permission_requests", Command, Some("ResolvePermissionRequestsRequest"), Some("ResolvePermissionRequestsResponse");
}

/// Historical constant; the daemon does not dispatch this method.
pub const GET_EVENTS: &str = "get_events";

#[cfg(test)]
mod tests {
    use super::{ClientOperation, Method};

    #[test]
    fn client_outcomes_keep_owner_opening_separate_from_read_only_observation() {
        for method in [
            Method::SessionOpen,
            Method::FlowGet,
            Method::GoalGet,
            Method::ManagementGet,
            Method::FlowControlStatus,
            Method::FlowHistoryReview,
            Method::FlowInboxEdit,
            Method::FlowComposerPreferenceSet,
            Method::FlowComposerPrepare,
            Method::ListPermissionRequests,
        ] {
            assert_eq!(method.client_operation(), ClientOperation::Command);
        }
        for method in [
            Method::Capabilities,
            Method::SessionFlows,
            Method::FlowComposer,
            Method::FlowComposerPreferenceStatus,
            Method::FlowApprovalQueue,
            Method::FlowExecution,
            Method::FlowInputStatus,
            Method::FlowInbox,
            Method::FlowHistoryStatus,
            Method::FlowOutput,
            Method::FlowEvents,
        ] {
            assert_eq!(method.client_operation(), ClientOperation::Query);
        }
        assert_eq!(
            serde_json::to_value(ClientOperation::Query).unwrap(),
            "query"
        );
        assert_eq!(
            serde_json::to_value(ClientOperation::Command).unwrap(),
            "command"
        );
    }
}
