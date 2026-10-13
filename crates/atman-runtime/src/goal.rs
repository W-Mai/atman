//! Goal state and cooperative execution control shared by managed agents.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::memory::goal::{GoalActor, GoalPauseReason, GoalRecord, GoalStatus, GoalStore};

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

struct State {
    record: Option<GoalRecord>,
    execution_paused: bool,
}

/// One stable agent owner, independent of the identifiers of its individual runs.
pub struct GoalController {
    owner: String,
    parent: Option<Arc<GoalController>>,
    store: Option<GoalStore>,
    state: Mutex<State>,
    changes: Arc<watch::Sender<u64>>,
    transition: Arc<Mutex<()>>,
    operations: AtomicUsize,
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
        Ok(Self::build(store, owner, Some(self.clone()), Some(record)))
    }

    fn build(
        store: Option<GoalStore>,
        owner: String,
        parent: Option<Arc<Self>>,
        record: Option<GoalRecord>,
    ) -> Arc<Self> {
        let execution_paused = record.as_ref().is_some_and(|g| {
            g.status == GoalStatus::Paused
                && g.pause
                    .as_ref()
                    .is_some_and(|p| p.actor != GoalActor::Agent)
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
            owner,
            parent,
            store,
            state: Mutex::new(State {
                record,
                execution_paused,
            }),
            changes,
            transition,
            operations: AtomicUsize::new(0),
        })
    }

    pub fn owner(&self) -> &str {
        &self.owner
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

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    fn blocked_by(&self) -> Option<String> {
        if self.state.lock().unwrap().execution_paused {
            return Some(self.owner.clone());
        }
        let mut next = self.parent.as_deref();
        while let Some(parent) = next {
            let state = parent.state.lock().unwrap();
            if state.execution_paused
                || state
                    .record
                    .as_ref()
                    .is_some_and(|g| g.status == GoalStatus::Paused)
            {
                return Some(parent.owner.clone());
            }
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
            if running_operations == 0 {
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
        self.blocked_by().is_none()
            && self
                .record()
                .is_some_and(|g| g.status == GoalStatus::Active)
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
        state.record = record;
        drop(state);
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
            self.state.lock().unwrap().execution_paused = false;
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
        // User control stops local execution even if persistence fails.
        if actor == GoalActor::User {
            self.state.lock().unwrap().execution_paused = true;
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
        if actor != GoalActor::Agent {
            self.state.lock().unwrap().execution_paused = true;
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
        self.state.lock().unwrap().execution_paused = false;
        self.changed();
        Ok(())
    }

    pub fn clear(&self) -> io::Result<()> {
        let _transition = self.transition.lock().unwrap();
        self.state.lock().unwrap().execution_paused = true;
        self.changed();
        self.mutate(None, |record| {
            *record = None;
            Ok(())
        })
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
}
