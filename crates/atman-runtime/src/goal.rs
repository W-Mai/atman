//! Goal state and cooperative execution control shared by managed agents.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::memory::goal::{GoalActor, GoalPauseReason, GoalRecord, GoalStatus, GoalStore};

/// Product contract for managed agents; ordinary language flows remain single runs.
pub fn is_managed(flow: &atman_rt::ast::FlowDecl) -> bool {
    flow.contract.as_ref().is_some_and(|contract| {
        contract.blocks.iter().any(|block| {
            block.name.name == "invocation"
                && block.kwargs.iter().any(|(name, value)| {
                    name.name == "goal"
                        && matches!(
                            value,
                            atman_rt::ast::Expr::Literal(atman_rt::ast::Literal::Bool(true))
                        )
                })
        })
    })
}

pub fn continuation_prompt(control: &GoalController) -> String {
    format!(
        "Goal continuation control, not a new user request. Continue the existing objective from the recorded context. Do not repeat completed effects. Explicitly call memory.goal.pause with a result or blocker when no autonomous work remains. Current goal data: {}",
        serde_json::to_string(&control.record()).expect("goal serialization"),
    )
}

/// Inspect readiness without claiming notifications or starting a turn.
pub fn has_next_turn(control: &GoalController, watchers: &crate::watch::WatchHub) -> bool {
    control.allows_background_turn()
        && (watchers.has_pending_events()
            || control.child_results.has_pending()
            || control.should_continue())
}

/// Begin a background turn only after context access is available. Cancellation while waiting leaves notifications queued; pause and readiness are checked again before consuming anything.
pub async fn begin_background_turn(
    session: &crate::session::Session,
) -> Option<crate::message::Message> {
    let _compact = session.acquire_compact_lock().await;
    let control = &session.goal_control;
    let _transition = control.transition.lock().unwrap();
    if !control.allows_background_turn() || session.current_turn().is_some() {
        return None;
    }
    if let Some(receipt) = control.child_results.reserve() {
        let mut message =
            crate::message::Message::user_text(crate::event::TurnId::now(), receipt.prompt());
        message.origin = crate::message::MessageOrigin::ChildResult;
        let committed = receipt
            .commit(|| {
                session.begin_turn(message.clone());
                Ok::<_, std::convert::Infallible>(())
            })
            .unwrap();
        return committed.then_some(message);
    }
    let (prompt, origin) = if let Some(prompt) = session.watch_hub.take_pending_prompt() {
        (prompt, crate::message::MessageOrigin::Watcher)
    } else if control.should_continue() {
        (
            continuation_prompt(control),
            crate::message::MessageOrigin::Internal,
        )
    } else {
        return None;
    };
    let mut message = crate::message::Message::user_text(crate::event::TurnId::now(), prompt);
    message.origin = origin;
    // No await separates notification consumption from its insertion into history.
    session.begin_turn(message.clone());
    Some(message)
}

/// Invocation recipe for subsequent turns; model history stays in its owning session.
#[derive(Clone)]
pub struct GoalTurn {
    pub program: crate::source_program::LinkedProgram,
    pub flow_name: String,
    pub args: Vec<(String, crate::Value)>,
    pub env: crate::InvocationEnv,
}

impl GoalTurn {
    pub fn continuation_args(&self, prompt: &str) -> Vec<(String, crate::Value)> {
        let mut args = self.args.clone();
        let parameter = self
            .program
            .entry_flow(&self.flow_name)
            .and_then(|id| self.program.flow(&id))
            .and_then(|flow| flow.contract.as_ref())
            .and_then(|contract| contract.blocks.iter().find(|b| b.name.name == "invocation"))
            .and_then(|block| {
                block
                    .kwargs
                    .iter()
                    .find(|(name, _)| name.name == "user_message")
            })
            .and_then(|(_, value)| match value {
                atman_rt::ast::Expr::Ident(name) => Some(name.name.as_str()),
                _ => None,
            });
        if let Some(parameter) = parameter {
            args.retain(|(name, _)| name != parameter);
            args.push((parameter.into(), crate::Value::Str(prompt.into())));
        }
        args
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GoalSnapshot {
    pub goal: Option<GoalRecord>,
    pub execution: GoalExecution,
    pub paused_by: Option<String>,
    pub running_operations: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalExecution {
    Idle,
    Running,
    Pausing,
    Paused,
    Finishing,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GoalAction {
    Pause,
    Resume,
    Set { text: String },
    Create { text: String },
    Clear,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExecutionPause {
    Open,
    // An agent acknowledging a user pause can still deliver this turn's answer.
    Finishing,
    Paused,
}

impl ExecutionPause {
    fn for_pause(actor: GoalActor, reason: GoalPauseReason) -> Self {
        match (actor, reason) {
            (GoalActor::Agent, GoalPauseReason::UserRequested) => Self::Finishing,
            (GoalActor::Agent, _) => Self::Open,
            _ => Self::Paused,
        }
    }
}

struct State {
    record: Option<GoalRecord>,
    execution_pause: ExecutionPause,
}

/// One stable agent owner, independent of the identifiers of its individual runs.
pub struct GoalController {
    pub child_results: Arc<crate::agent_results::ResultInbox>,
    owner: String,
    parent: Option<Arc<GoalController>>,
    store: Option<GoalStore>,
    state: Mutex<State>,
    changes: Arc<watch::Sender<u64>>,
    transition: Arc<Mutex<()>>,
    operations: AtomicUsize,
    turns: AtomicUsize,
    descendant_updates: Mutex<std::collections::BTreeMap<String, GoalRecord>>,
    events: std::sync::OnceLock<crate::event::EventSink>,
}

impl GoalController {
    pub fn open(store: GoalStore, owner: String) -> io::Result<Arc<Self>> {
        let record = if store.load()?.is_some() {
            store.update(None, |record| {
                if let Some(goal) = record {
                    goal.owner = owner.clone();
                    if goal.status == GoalStatus::Active {
                        goal.pause(
                            GoalActor::Runtime,
                            GoalPauseReason::Interrupted,
                            "Execution interrupted; resume explicitly to continue.".into(),
                        );
                    }
                }
                Ok(())
            })?
        } else {
            None
        };
        Ok(Self::build(Some(store), owner, None, record))
    }

    pub fn ephemeral(owner: String) -> Arc<Self> {
        Self::build(None, owner, None, None)
    }

    pub fn child(
        self: &Arc<Self>,
        owner: String,
        objective: String,
        session_dir: Option<&std::path::Path>,
    ) -> io::Result<Arc<Self>> {
        let parent_id = self.record().map(|g| g.id);
        let record = GoalRecord::new(objective, owner.clone(), parent_id);
        let store = session_dir
            .map(|dir| GoalStore::for_child(dir, &record.id))
            .transpose()?;
        if let Some(store) = &store {
            store.update(None, |saved| {
                *saved = Some(record.clone());
                Ok(())
            })?;
        }
        let child = Self::build(store, owner, Some(self.clone()), Some(record));
        if let Some(sink) = self.events.get() {
            child.bind_events(sink.clone());
        }
        Ok(child)
    }

    fn build(
        store: Option<GoalStore>,
        owner: String,
        parent: Option<Arc<Self>>,
        record: Option<GoalRecord>,
    ) -> Arc<Self> {
        let execution_pause = record
            .as_ref()
            .filter(|g| g.status == GoalStatus::Paused)
            .and_then(|g| g.pause.as_ref())
            .map_or(ExecutionPause::Open, |p| {
                ExecutionPause::for_pause(p.actor, p.reason)
            });
        let changes = parent
            .as_ref()
            .map(|p| p.changes.clone())
            .unwrap_or_else(|| Arc::new(watch::channel(0).0));
        let transition = parent
            .as_ref()
            .map(|p| p.transition.clone())
            .unwrap_or_default();
        Arc::new(Self {
            child_results: crate::agent_results::ResultInbox::new(changes.clone()),
            owner,
            parent,
            store,
            state: Mutex::new(State {
                record,
                execution_pause,
            }),
            changes,
            transition,
            operations: AtomicUsize::new(0),
            turns: AtomicUsize::new(0),
            descendant_updates: Mutex::new(Default::default()),
            events: Default::default(),
        })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn bind_events(&self, sink: crate::event::EventSink) {
        if self.events.set(sink).is_ok() && self.record().is_some() {
            self.emit_state();
        }
    }

    fn emit_state(&self) {
        if let Some(sink) = self.events.get() {
            sink.emit(crate::event::Event::GoalChanged {
                owner: self.owner.clone(),
                goal: self.record(),
            });
        }
    }

    pub fn turn_running(&self) -> bool {
        self.turns.load(Ordering::Acquire) != 0
    }

    pub fn begin_user_turn(&self) {
        let _transition = self.transition.lock().unwrap();
        if !self.turn_running() {
            self.state.lock().unwrap().execution_pause = ExecutionPause::Open;
            self.changed();
        }
    }

    pub fn replace_completed(&self, text: &str, expected: Option<(&str, u64)>) -> io::Result<()> {
        let _transition = self.transition.lock().unwrap();
        if text.trim().is_empty() {
            return Err(io::Error::other("goal objective must not be empty"));
        }
        self.mutate(expected, |record| {
            let previous = record
                .as_ref()
                .ok_or_else(|| io::Error::other("no goal set"))?;
            if !previous.pause.as_ref().is_some_and(|p| {
                p.actor == GoalActor::Agent && p.reason == GoalPauseReason::Completed
            }) {
                return Err(io::Error::other(
                    "only an agent-completed goal can be replaced by new input",
                ));
            }
            *record = Some(GoalRecord::new(
                text.trim().into(),
                self.owner.clone(),
                previous.parent_goal_id.clone(),
            ));
            Ok(())
        })?;
        self.state.lock().unwrap().execution_pause = ExecutionPause::Open;
        self.changed();
        Ok(())
    }

    pub fn start_turn(self: &Arc<Self>) -> GoalTurnGuard {
        self.turns.fetch_add(1, Ordering::AcqRel);
        self.changed();
        GoalTurnGuard(self.clone())
    }

    pub fn record_turn(
        &self,
        turn_id: Option<crate::event::TurnId>,
        run_id: crate::event::FlowRunId,
    ) {
        if let Some(sink) = self.events.get() {
            sink.emit(crate::event::Event::GoalTurnStarted {
                owner: self.owner.clone(),
                goal_id: self.record().map(|g| g.id),
                turn_id,
                run_id,
            });
        }
    }

    pub fn is_ancestor_of(&self, other: &Self) -> bool {
        let mut next = other.parent.as_deref();
        while let Some(parent) = next {
            if std::ptr::eq(self, parent) {
                return true;
            }
            next = parent.parent.as_deref();
        }
        false
    }

    pub fn record(&self) -> Option<GoalRecord> {
        self.state.lock().unwrap().record.clone()
    }

    pub fn descendant_updates(&self) -> Vec<GoalRecord> {
        self.descendant_updates
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }

    pub async fn wait_active(&self, cancel: &CancellationToken) -> Result<(), RuntimeError> {
        let mut changes = self.subscribe();
        loop {
            if cancel.is_cancelled() {
                return Err(RuntimeError::Cancelled("flow cancelled by user".into()));
            }
            if self.allows_background_turn()
                && (self.should_continue() || self.child_results.has_pending())
            {
                return Ok(());
            }
            tokio::select! {
                _ = cancel.cancelled() => {},
                _ = changes.changed() => {},
            }
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    fn blocked_by(&self) -> Option<String> {
        let execution_pause = self.state.lock().unwrap().execution_pause;
        if execution_pause == ExecutionPause::Paused
            || (execution_pause == ExecutionPause::Finishing && !self.turn_running())
        {
            return Some(self.owner.clone());
        }
        let mut next = self.parent.as_deref();
        let mut parent_goal_id = self.record().and_then(|g| g.parent_goal_id);
        while let Some(parent) = next {
            let state = parent.state.lock().unwrap();
            if state.execution_pause != ExecutionPause::Open
                || parent_goal_id
                    .as_ref()
                    .is_some_and(|id| state.record.as_ref().is_none_or(|g| &g.id != id))
            {
                return Some(parent.owner.clone());
            }
            parent_goal_id = state.record.as_ref().and_then(|g| g.parent_goal_id.clone());
            next = parent.parent.as_deref();
        }
        None
    }

    pub fn snapshot(&self) -> GoalSnapshot {
        let goal = self.record();
        let paused_by = self.blocked_by();
        let running_operations = self.operations.load(Ordering::Acquire);
        let execution = if paused_by.is_some() {
            if running_operations == 0 {
                GoalExecution::Paused
            } else {
                GoalExecution::Pausing
            }
        } else if goal
            .as_ref()
            .is_some_and(|g| g.status == GoalStatus::Paused)
        {
            if !self.turn_running() {
                GoalExecution::Paused
            } else {
                GoalExecution::Finishing
            }
        } else if running_operations != 0 {
            GoalExecution::Running
        } else {
            GoalExecution::Idle
        };
        GoalSnapshot {
            goal,
            execution,
            paused_by,
            running_operations,
        }
    }

    pub fn should_continue(&self) -> bool {
        self.allows_background_turn()
            && self
                .record()
                .is_some_and(|g| g.status == GoalStatus::Active)
    }

    /// Agent completion allows event handling; user, parent, and failure gates do not.
    pub fn allows_background_turn(&self) -> bool {
        let open = self.state.lock().unwrap().execution_pause == ExecutionPause::Open;
        open && self.blocked_by().is_none()
    }

    /// Serialize message pickup with execution pauses and task replacement.
    pub(crate) fn with_background_gate<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let _transition = self.transition.lock().unwrap();
        self.allows_background_turn().then(action)
    }

    fn mutate(
        &self,
        expected: Option<(&str, u64)>,
        change: impl FnOnce(&mut Option<GoalRecord>) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some((id, revision)) = expected
            && !state
                .record
                .as_ref()
                .is_some_and(|g| g.id == id && g.revision == revision)
        {
            return Err(io::Error::other(
                "stale goal: read its current state before updating",
            ));
        }
        let mut record = state.record.clone();
        change(&mut record)?;
        if let Some(store) = &self.store {
            record = store.update(expected, |saved| {
                *saved = record.clone();
                Ok(())
            })?;
        } else if let Some(goal) = &mut record
            && state.record.as_ref() != Some(goal)
        {
            if let Some(old) = state.record.as_ref().filter(|old| old.id == goal.id) {
                goal.revision = old
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("goal revision exhausted"))?;
            }
            goal.updated_at = chrono::Utc::now();
        }
        if state.record.is_some()
            && state.record.as_ref().map(|g| &g.id) != record.as_ref().map(|g| &g.id)
        {
            self.child_results.invalidate();
        }
        state.record = record.clone();
        drop(state);
        self.emit_state();
        if let Some(parent) = &self.parent
            && let Some(record) = record
        {
            parent
                .descendant_updates
                .lock()
                .unwrap()
                .insert(self.owner.clone(), record);
        }
        self.changed();
        Ok(())
    }

    /// Create a fresh objective for an owner, replacing only the acknowledged record.
    pub fn create(&self, text: &str, expected: Option<(&str, u64)>) -> io::Result<()> {
        let _transition = self.transition.lock().unwrap();
        let text = text.trim();
        if text.is_empty() {
            return Err(io::Error::other("goal objective must not be empty"));
        }
        self.mutate(expected, |record| {
            if record.is_some() && expected.is_none() {
                return Err(io::Error::other(
                    "a goal already exists; read it before replacing",
                ));
            }
            *record = Some(GoalRecord::new(
                text.into(),
                self.owner.clone(),
                self.parent.as_ref().and_then(|p| p.record().map(|g| g.id)),
            ));
            Ok(())
        })?;
        self.state.lock().unwrap().execution_pause = ExecutionPause::Open;
        self.changed();
        Ok(())
    }

    pub fn set(&self, text: &str, expected: Option<(&str, u64)>) -> io::Result<()> {
        let _transition = self.transition.lock().unwrap();
        let text = text.trim();
        if text.is_empty() {
            return Err(io::Error::other("goal objective must not be empty"));
        }
        let creating = self.record().is_none();
        self.mutate(expected, |record| {
            if let Some(goal) = record {
                goal.objective = text.into();
            } else {
                *record = Some(GoalRecord::new(
                    text.into(),
                    self.owner.clone(),
                    self.parent.as_ref().and_then(|p| p.record().map(|g| g.id)),
                ));
            }
            Ok(())
        })?;
        if creating {
            self.state.lock().unwrap().execution_pause = ExecutionPause::Open;
            self.changed();
        }
        Ok(())
    }

    pub fn pause(
        &self,
        actor: GoalActor,
        reason: GoalPauseReason,
        summary: String,
        expected: Option<(&str, u64)>,
    ) -> io::Result<()> {
        let _transition = self.transition.lock().unwrap();
        if let Some((id, revision)) = expected
            && !self
                .record()
                .is_some_and(|g| g.id == id && g.revision == revision)
        {
            return Err(io::Error::other(
                "stale goal: read its current state before updating",
            ));
        }
        // External control stops execution even without a goal or when persistence fails.
        if actor != GoalActor::Agent {
            self.state.lock().unwrap().execution_pause = ExecutionPause::Paused;
            self.changed();
        }
        self.mutate(expected, |record| {
            let goal = record
                .as_mut()
                .ok_or_else(|| io::Error::other("no goal set"))?;
            if actor != GoalActor::User
                && goal
                    .pause
                    .as_ref()
                    .is_some_and(|p| p.actor == GoalActor::User)
            {
                return Err(io::Error::other("goal is paused by the user"));
            }
            goal.pause(actor, reason, summary);
            Ok(())
        })?;
        if ExecutionPause::for_pause(actor, reason) == ExecutionPause::Finishing {
            let mut state = self.state.lock().unwrap();
            if state.execution_pause == ExecutionPause::Open {
                state.execution_pause = ExecutionPause::Finishing;
            }
            drop(state);
            self.changed();
        }
        Ok(())
    }

    pub fn resume(&self, actor: GoalActor, expected: Option<(&str, u64)>) -> io::Result<()> {
        let _transition = self.transition.lock().unwrap();
        self.mutate(expected, |record| {
            let goal = record
                .as_mut()
                .ok_or_else(|| io::Error::other("no goal set"))?;
            if actor != GoalActor::User
                && goal
                    .pause
                    .as_ref()
                    .is_some_and(|p| p.actor == GoalActor::User)
            {
                return Err(io::Error::other("only the user can resume this goal"));
            }
            goal.status = GoalStatus::Active;
            goal.pause = None;
            Ok(())
        })?;
        self.state.lock().unwrap().execution_pause = ExecutionPause::Open;
        self.changed();
        Ok(())
    }

    pub fn clear(&self) -> io::Result<()> {
        let _transition = self.transition.lock().unwrap();
        // Removing an objective must not release an existing execution pause.
        self.mutate(None, |record| {
            *record = None;
            Ok(())
        })?;
        self.child_results.invalidate();
        Ok(())
    }

    pub async fn wait(&self, cancel: &CancellationToken) -> Result<(), RuntimeError> {
        let mut changes = self.subscribe();
        loop {
            if cancel.is_cancelled() {
                return Err(RuntimeError::Cancelled("flow cancelled by user".into()));
            }
            if self.blocked_by().is_none() {
                return Ok(());
            }
            tokio::select! {
                _ = cancel.cancelled() => {},
                _ = changes.changed() => {},
            }
        }
    }

    pub async fn enter(
        self: &Arc<Self>,
        cancel: &CancellationToken,
    ) -> Result<GoalOperation, RuntimeError> {
        loop {
            self.wait(cancel).await?;
            let _transition = self.transition.lock().unwrap();
            if self.blocked_by().is_none() {
                self.adjust_operations(true);
                return Ok(GoalOperation(self.clone()));
            }
        }
    }

    pub fn try_enter(self: &Arc<Self>) -> Option<GoalOperation> {
        let _transition = self.transition.lock().unwrap();
        if self.blocked_by().is_some() {
            return None;
        }
        self.adjust_operations(true);
        Some(GoalOperation(self.clone()))
    }

    fn adjust_operations(&self, started: bool) {
        let mut next = Some(self);
        while let Some(control) = next {
            if started {
                control.operations.fetch_add(1, Ordering::AcqRel);
            } else {
                control.operations.fetch_sub(1, Ordering::AcqRel);
            }
            next = control.parent.as_deref();
        }
        self.changed();
    }
}

pub struct GoalTurnGuard(Arc<GoalController>);

impl Drop for GoalTurnGuard {
    fn drop(&mut self) {
        self.0.turns.fetch_sub(1, Ordering::AcqRel);
        self.0.changed();
    }
}

pub struct GoalOperation(Arc<GoalController>);

impl Drop for GoalOperation {
    fn drop(&mut self) {
        self.0.adjust_operations(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn child_results_survive_turn_end_and_pause_without_duplicate_background_turns() {
        use crate::event::{FlowRunId, TurnId};
        use crate::message::{Message, MessageOrigin};
        let session = crate::session::Session::open_ephemeral();
        session.goal_control.set("parent", None).unwrap();
        session
            .goal_control
            .pause(
                GoalActor::Agent,
                GoalPauseReason::Completed,
                "done".into(),
                None,
            )
            .unwrap();
        let turn = session.begin_turn(Message::user_text(TurnId::now(), "original"));
        let route = session
            .goal_control
            .child_results
            .route("child".into(), true);
        route.publish(FlowRunId::now(), TurnId::now(), "first".into());
        route.publish(FlowRunId::now(), TurnId::now(), "second".into());
        assert!(begin_background_turn(&session).await.is_none());
        let reserved = session.goal_control.child_results.reserve().unwrap();
        let mut message = Message::user_text(turn, reserved.prompt());
        message.origin = MessageOrigin::ChildResult;
        session.end_turn();
        assert!(session.commit_child_results(&reserved, message).is_err());
        drop(reserved);

        let lock = session.acquire_compact_lock().await;
        let start = begin_background_turn(&session);
        tokio::pin!(start);
        tokio::select! {
            biased;
            _ = &mut start => panic!("must wait for context"),
            _ = std::future::ready(()) => {},
        }
        session
            .goal_control
            .pause(
                GoalActor::User,
                GoalPauseReason::UserRequested,
                "pause".into(),
                None,
            )
            .unwrap();
        drop(lock);
        assert!(start.await.is_none());
        assert!(session.goal_control.child_results.has_pending());
        session.goal_control.resume(GoalActor::User, None).unwrap();
        session
            .goal_control
            .pause(
                GoalActor::Agent,
                GoalPauseReason::Completed,
                "done".into(),
                None,
            )
            .unwrap();
        let (a, b) = tokio::join!(
            begin_background_turn(&session),
            begin_background_turn(&session)
        );
        assert_eq!(usize::from(a.is_some()) + usize::from(b.is_some()), 1);
        let message = a.or(b).unwrap();
        assert_eq!(message.origin, MessageOrigin::ChildResult);
        assert!(
            message.text_concat().find("first").unwrap()
                < message.text_concat().find("second").unwrap()
        );
        session.end_turn();
        assert!(!has_next_turn(&session.goal_control, &session.watch_hub));
        assert_eq!(
            session
                .messages_full()
                .iter()
                .filter(|m| m.origin == MessageOrigin::ChildResult)
                .count(),
            1
        );
        route.publish(FlowRunId::now(), TurnId::now(), "late".into());
        session.goal_control.clear().unwrap();
        route.publish(FlowRunId::now(), TurnId::now(), "obsolete".into());
        assert!(!has_next_turn(&session.goal_control, &session.watch_hub));
    }

    #[tokio::test]
    async fn background_start_preserves_events_until_context_and_execution_are_ready() {
        use crate::watch::{WatchEvent, WatchMode, WatchSource};
        use std::time::Duration;
        let session = crate::session::Session::open_ephemeral();
        let source = WatchSource::Bash {
            handle: "job".into(),
        };
        let id = session.watch_hub.register(
            source.clone(),
            "done".into(),
            WatchMode::Once,
            Duration::from_secs(1),
        );
        session.watch_hub.enqueue_event(WatchEvent {
            watcher_id: id,
            source,
            pattern: "done".into(),
            row: None,
            col: None,
            text: "finished".into(),
            timestamp: chrono::Utc::now(),
            timed_out: false,
            exited: false,
            timeout: Duration::from_secs(1),
        });
        let compact = session.acquire_compact_lock().await;
        {
            let start = begin_background_turn(&session);
            tokio::pin!(start);
            tokio::select! {
                biased;
                _ = &mut start => panic!("context is locked"),
                _ = std::future::ready(()) => {},
            }
        } // Dropping a pending start must not consume the notification.
        assert!(session.watch_hub.has_pending_events());
        assert!(session.current_turn().is_none());
        let start = begin_background_turn(&session);
        tokio::pin!(start);
        tokio::select! {
            biased;
            _ = &mut start => panic!("context is locked"),
            _ = std::future::ready(()) => {},
        }
        // Cancel without a goal must also stop background wakeups.
        session.cancel_flow();
        drop(compact);
        assert!(start.await.is_none());
        assert!(session.watch_hub.has_pending_events());
        assert!(!has_next_turn(&session.goal_control, &session.watch_hub));
        session.begin_turn(crate::message::Message::user_text(
            crate::event::TurnId::now(),
            "continue",
        ));
        assert!(begin_background_turn(&session).await.is_none());
        session.end_turn();
        let (first, second) = tokio::join!(
            begin_background_turn(&session),
            begin_background_turn(&session)
        );
        assert_eq!(
            usize::from(first.is_some()) + usize::from(second.is_some()),
            1
        );
        assert!(!session.watch_hub.has_pending_events());
        assert_eq!(
            session
                .messages()
                .iter()
                .filter(|m| m.origin == crate::message::MessageOrigin::Watcher)
                .count(),
            1
        );
        session.end_turn();
        assert!(begin_background_turn(&session).await.is_none());
        assert!(
            session
                .goal_control
                .pause(
                    GoalActor::Runtime,
                    GoalPauseReason::RunFailed,
                    "failed".into(),
                    None
                )
                .is_err()
        );
        assert!(!session.goal_control.allows_background_turn());
        session.goal_control.set("new task", None).unwrap();
        session.control_goal(None, GoalAction::Clear, None).unwrap();
        assert!(session.goal_control.record().is_none());
        assert!(!session.goal_control.allows_background_turn());
        session.goal_control.begin_user_turn();
        session.goal_control.clear().unwrap();
        assert!(session.goal_control.allows_background_turn());
    }

    #[tokio::test]
    async fn pause_keeps_work_and_parent_resume_preserves_child_pause() {
        let parent = GoalController::ephemeral("root".into());
        parent.set("root task", None).unwrap();
        let child = parent
            .child("child".into(), "child task".into(), None)
            .unwrap();
        let cancel = CancellationToken::new();
        let operation = child.enter(&cancel).await.unwrap();
        parent
            .pause(
                GoalActor::User,
                GoalPauseReason::UserRequested,
                "pause".into(),
                None,
            )
            .unwrap();
        assert_eq!(parent.snapshot().execution, GoalExecution::Pausing);
        assert!(!child.should_continue());
        drop(operation);
        assert_eq!(parent.snapshot().execution, GoalExecution::Paused);
        child
            .pause(
                GoalActor::Agent,
                GoalPauseReason::Completed,
                "done".into(),
                None,
            )
            .unwrap();
        parent.resume(GoalActor::User, None).unwrap();
        assert!(!child.should_continue());
        // The child's own final response remains possible after its pause tool.
        child.wait(&cancel).await.unwrap();
    }

    #[test]
    fn goal_suspension_is_local_but_execution_pauses_reach_descendants() {
        for (actor, reason, permits_children) in [
            (GoalActor::Agent, GoalPauseReason::Completed, true),
            (GoalActor::User, GoalPauseReason::Completed, false),
            (GoalActor::Parent, GoalPauseReason::Completed, false),
            (GoalActor::Runtime, GoalPauseReason::Completed, false),
            (GoalActor::Agent, GoalPauseReason::NeedsInput, true),
            (GoalActor::Agent, GoalPauseReason::Blocked, true),
            (GoalActor::Agent, GoalPauseReason::UserRequested, false),
            (GoalActor::Parent, GoalPauseReason::ParentRequested, false),
            (GoalActor::Runtime, GoalPauseReason::Cancelled, false),
            (GoalActor::Runtime, GoalPauseReason::RunFailed, false),
        ] {
            let root = GoalController::ephemeral("root".into());
            root.set("parent work", None).unwrap();
            let child = root
                .child("child".into(), "child work".into(), None)
                .unwrap();
            let grandchild = child
                .child("grandchild".into(), "delayed work".into(), None)
                .unwrap();
            root.pause(actor, reason, "parent state".into(), None)
                .unwrap();
            assert!(!root.should_continue());
            assert_eq!(
                child.should_continue(),
                permits_children,
                "{actor:?}/{reason:?}"
            );
            assert_eq!(
                grandchild.should_continue(),
                permits_children,
                "{actor:?}/{reason:?}"
            );
            assert_eq!(child.snapshot().paused_by.is_none(), permits_children);

            child
                .pause(
                    GoalActor::Agent,
                    GoalPauseReason::Completed,
                    "child done".into(),
                    None,
                )
                .unwrap();
            assert!(!child.should_continue());
            assert_eq!(grandchild.should_continue(), permits_children);
        }
    }

    #[tokio::test]
    async fn user_request_runs_descendants_without_resuming_the_old_goal() {
        for reason in [
            GoalPauseReason::NeedsInput,
            GoalPauseReason::Blocked,
            GoalPauseReason::Completed,
        ] {
            let root = GoalController::ephemeral("root".into());
            root.set("old objective", None).unwrap();
            root.pause(GoalActor::Agent, reason, "old turn ended".into(), None)
                .unwrap();
            let old_goal = root.record().unwrap();
            root.begin_user_turn();
            let child = root
                .child("child".into(), "new request".into(), None)
                .unwrap();
            let grandchild = child
                .child("grandchild".into(), "nested work".into(), None)
                .unwrap();
            assert!(!root.should_continue());
            assert_eq!(root.record().unwrap(), old_goal);
            assert!(child.should_continue());
            assert!(grandchild.try_enter().is_some());
            root.pause(
                GoalActor::User,
                GoalPauseReason::UserRequested,
                "stop".into(),
                None,
            )
            .unwrap();
            assert!(grandchild.try_enter().is_none());
            let cancel = CancellationToken::new();
            let pending = grandchild.enter(&cancel);
            tokio::pin!(pending);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), &mut pending)
                    .await
                    .is_err()
            );
            root.begin_user_turn();
            tokio::time::timeout(std::time::Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap();
            assert!(!root.should_continue());
        }
    }

    #[test]
    fn agent_acknowledged_user_pause_allows_only_the_current_turn_to_finish() {
        let root = GoalController::ephemeral("root".into());
        root.set("work", None).unwrap();
        let child = root.child("child".into(), "work".into(), None).unwrap();
        let turn = root.start_turn();
        root.pause(
            GoalActor::Agent,
            GoalPauseReason::UserRequested,
            "user paused".into(),
            None,
        )
        .unwrap();
        assert!(root.try_enter().is_some());
        assert!(child.try_enter().is_none());
        assert!(!root.allows_background_turn());
        drop(turn);
        assert!(root.try_enter().is_none());
        root.begin_user_turn();
        assert!(child.try_enter().is_some());
        assert!(!root.should_continue());
    }

    #[tokio::test]
    async fn delayed_child_can_finish_after_parent_completion_but_still_obeys_user_pause() {
        let root = GoalController::ephemeral("root".into());
        root.set("parent work", None).unwrap();
        let child = root
            .child("child".into(), "delayed work".into(), None)
            .unwrap();
        let cancel = CancellationToken::new();
        let pending_cancel = cancel.clone();
        let pending_child = child.clone();
        let (release, delayed) = tokio::sync::oneshot::channel();
        let pending = tokio::spawn(async move {
            delayed.await.unwrap();
            pending_child.wait_active(&pending_cancel).await.unwrap();
            pending_child.enter(&pending_cancel).await.unwrap()
        });
        root.pause(
            GoalActor::Agent,
            GoalPauseReason::Completed,
            "parent done".into(),
            None,
        )
        .unwrap();
        release.send(()).unwrap();
        let operation = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap();
        drop(operation);
        root.pause(
            GoalActor::User,
            GoalPauseReason::UserRequested,
            "stop children".into(),
            None,
        )
        .unwrap();
        assert!(!child.should_continue());
        cancel.cancel();
        assert!(child.wait(&cancel).await.is_err());
    }

    #[tokio::test]
    async fn cancelled_gate_unblocks_and_user_pause_cannot_be_overwritten() {
        let goal = GoalController::ephemeral("root".into());
        goal.set("work", None).unwrap();
        goal.pause(
            GoalActor::User,
            GoalPauseReason::UserRequested,
            "stop".into(),
            None,
        )
        .unwrap();
        assert!(goal.resume(GoalActor::Parent, None).is_err());
        assert!(
            goal.pause(
                GoalActor::Agent,
                GoalPauseReason::Completed,
                "done".into(),
                None
            )
            .is_err()
        );
        goal.set("edited", None).unwrap();
        assert!(!goal.should_continue());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(goal.wait(&cancel).await.is_err());
    }

    #[test]
    fn reopening_active_goal_requires_explicit_resume() {
        let dir = tempfile::tempdir().unwrap();
        let control = GoalController::open(GoalStore::at(dir.path()), "root".into()).unwrap();
        control.set("work", None).unwrap();
        assert!(control.should_continue());
        drop(control);
        let restored = GoalController::open(GoalStore::at(dir.path()), "root".into()).unwrap();
        assert!(!restored.should_continue());
        restored.resume(GoalActor::User, None).unwrap();
        assert!(restored.should_continue());
    }
    #[test]
    fn creating_a_goal_requires_the_current_identity_and_preserves_parent_gates() {
        let root = GoalController::ephemeral("root".into());
        root.create("original", None).unwrap();
        let original = root.record().unwrap();
        let child = root
            .child("child".into(), "child objective".into(), None)
            .unwrap();
        assert!(root.create("unacknowledged", None).is_err());
        assert!(
            root.create("stale", Some((&original.id, original.revision + 1)))
                .is_err()
        );
        assert_eq!(root.record().unwrap(), original);
        root.pause(
            GoalActor::User,
            GoalPauseReason::UserRequested,
            "pause".into(),
            None,
        )
        .unwrap();
        let paused = root.record().unwrap();
        root.create("replacement", Some((&paused.id, paused.revision)))
            .unwrap();
        let replacement = root.record().unwrap();
        assert_ne!(replacement.id, original.id);
        assert_eq!(replacement.revision, 1);
        assert!(root.should_continue());
        assert!(
            !child.should_continue(),
            "old descendants must not follow a replacement root goal"
        );
        let old_child = child.record().unwrap();
        child
            .create(
                "new child objective",
                Some((&old_child.id, old_child.revision)),
            )
            .unwrap();
        assert_eq!(child.record().unwrap().parent_goal_id, Some(replacement.id));
        root.pause(
            GoalActor::User,
            GoalPauseReason::UserRequested,
            "pause".into(),
            None,
        )
        .unwrap();
        assert!(
            !child.should_continue(),
            "new child goals still respect the parent's pause"
        );
        assert!(
            root.set("stale edit", Some((&original.id, original.revision)))
                .is_err()
        );
    }
}
