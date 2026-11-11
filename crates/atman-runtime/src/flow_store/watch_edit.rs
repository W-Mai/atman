//! Atomic replacement of user subscriptions, retaining earlier registrations and input.

use std::{collections::BTreeMap, io};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    FlowFact, FlowPosition, FlowStore, Inbox, invalid,
    process_watch::{ProcessWatch, ProcessWatchRecord, ProcessWatchStatus, ProcessWatches},
    subscription::{
        FlowSubscription, ObservationAnchor, SubscriptionPurpose, SubscriptionStatus, Subscriptions,
    },
    watch_view::WatchRegistration,
};

/// Immutable user-visible registration content; observation progress is excluded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "definition", rename_all = "snake_case")]
pub enum WatchDefinition {
    Flow(FlowSubscription),
    Process(ProcessWatch),
}

impl WatchDefinition {
    pub fn id(&self) -> Uuid {
        match self {
            Self::Flow(watch) => watch.id,
            Self::Process(watch) => watch.id,
        }
    }

    fn validate_user(&self) -> io::Result<()> {
        if let Self::Flow(watch) = self
            && (watch.purpose != SubscriptionPurpose::Watcher || watch.reply.is_some())
        {
            return Err(invalid("internal result routes are not editable watchers"));
        }
        Ok(())
    }
}

impl From<&WatchRegistration> for WatchDefinition {
    fn from(record: &WatchRegistration) -> Self {
        match record {
            WatchRegistration::Flow(record) => Self::Flow(record.subscription.clone()),
            WatchRegistration::Process(record) => Self::Process(record.watch.clone()),
        }
    }
}

/// One journal fact retires the expected registration and installs its replacement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WatcherReplacement {
    pub expected: WatchDefinition,
    pub replacement: WatchRegistration,
}

#[derive(Default)]
pub(super) struct WatchReplacements(BTreeMap<Uuid, (WatcherReplacement, FlowPosition)>);

impl WatchReplacements {
    pub(super) fn retry(
        &self,
        expected: &WatchDefinition,
        replacement: &WatchDefinition,
    ) -> io::Result<Option<FlowPosition>> {
        match self.0.get(&replacement.id()) {
            Some((old, receipt))
                if &old.expected == expected
                    && WatchDefinition::from(&old.replacement) == *replacement =>
            {
                Ok(Some(receipt.clone()))
            }
            Some(_) => Err(invalid(
                "watcher replacement identity conflicts with recorded edit",
            )),
            None => Ok(None),
        }
    }

    pub(super) fn validate(
        &self,
        edit: &WatcherReplacement,
        flows: &Subscriptions,
        processes: &ProcessWatches,
        inbox: &Inbox,
    ) -> io::Result<()> {
        let new = WatchDefinition::from(&edit.replacement);
        edit.expected.validate_user()?;
        new.validate_user()?;
        if edit.expected.id() == new.id()
            || self.0.contains_key(&new.id())
            || flows.contains(new.id())
            || processes.contains(new.id())
        {
            return Err(invalid(
                "watcher replacement requires a new registration identity",
            ));
        }
        let registered_at = match &edit.replacement {
            WatchRegistration::Flow(record) => record.registered_at,
            WatchRegistration::Process(record) => record.registered_at,
        };
        let current = match &edit.expected {
            WatchDefinition::Flow(expected) => flows
                .values()
                .find(|record| record.subscription.id == expected.id)
                .is_some_and(|record| {
                    record.subscription == *expected
                        && record.status == SubscriptionStatus::Active
                        && expected
                            .expires_at
                            .is_none_or(|expiry| expiry > registered_at)
                }),
            WatchDefinition::Process(expected) => processes
                .values()
                .find(|record| record.watch.id == expected.id)
                .is_some_and(|record| {
                    record.watch == *expected
                        && record.status == ProcessWatchStatus::Active
                        && expected
                            .expires_at
                            .is_none_or(|expiry| expiry > registered_at)
                }),
        };
        if !current {
            return Err(invalid(
                "watcher changed or is no longer active; refresh before editing",
            ));
        }
        match &edit.replacement {
            WatchRegistration::Flow(record) => flows.validate_registration(record),
            WatchRegistration::Process(record) => processes.validate_registration(record, inbox),
        }
    }

    pub(super) fn apply(
        &mut self,
        edit: WatcherReplacement,
        flows: &mut Subscriptions,
        processes: &mut ProcessWatches,
        inbox: &mut Inbox,
        receipt: &FlowPosition,
    ) -> io::Result<()> {
        self.validate(&edit, flows, processes, inbox)?;
        match &edit.expected {
            WatchDefinition::Flow(watch) => flows.cancel(watch.id, inbox, receipt)?,
            WatchDefinition::Process(watch) => processes.cancel(watch.id)?,
        }
        match &edit.replacement {
            WatchRegistration::Flow(record) => flows.register(record.clone())?,
            WatchRegistration::Process(record) => processes.register(record.clone(), inbox)?,
        }
        self.0
            .insert(edit.replacement.id(), (edit, receipt.clone()));
        Ok(())
    }
}

impl FlowStore {
    /// Return the original receipt for an identical edit, including after cancellation or source loss.
    pub fn watcher_replacement(
        &self,
        expected: &WatchDefinition,
        replacement: &WatchDefinition,
    ) -> io::Result<Option<FlowPosition>> {
        self.watch_replacements.retry(expected, replacement)
    }

    /// Replace a user watcher using a host-authenticated source anchor and committed observation progress; previously accepted input remains intact.
    pub fn replace_flow_watch(
        &mut self,
        expected: WatchDefinition,
        subscription: FlowSubscription,
        anchor: &ObservationAnchor,
    ) -> io::Result<FlowPosition> {
        if let Some(receipt) =
            self.watcher_replacement(&expected, &WatchDefinition::Flow(subscription.clone()))?
        {
            return Ok(receipt);
        }
        if &subscription.source != anchor.source() || anchor.is_closed() {
            return Err(invalid(
                "replacement requires an open matching source anchor",
            ));
        }
        self.replace_watcher(WatcherReplacement {
            expected,
            replacement: WatchRegistration::Flow(
                self.subscriptions.registration(subscription, anchor),
            ),
        })
    }

    /// Replace a user watcher at the source position captured by its process producer.
    pub fn replace_process_watch(
        &mut self,
        expected: WatchDefinition,
        watch: ProcessWatch,
        started_after: u64,
    ) -> io::Result<FlowPosition> {
        if let Some(receipt) =
            self.watcher_replacement(&expected, &WatchDefinition::Process(watch.clone()))?
        {
            return Ok(receipt);
        }
        self.replace_watcher(WatcherReplacement {
            expected,
            replacement: WatchRegistration::Process(ProcessWatchRecord {
                history_head: Some(self.history_head()),
                watch,
                registered_at: Utc::now(),
                started_after,
                status: ProcessWatchStatus::Active,
            }),
        })
    }

    fn replace_watcher(&mut self, edit: WatcherReplacement) -> io::Result<FlowPosition> {
        if !self.state.accepts_input() {
            return Err(invalid("recipient cannot edit watchers"));
        }
        self.watch_replacements.validate(
            &edit,
            &self.subscriptions,
            &self.process_watches,
            &self.inbox,
        )?;
        let receipt = self
            .journal
            .commit(&[FlowFact::WatcherReplaced(Box::new(edit.clone()))])?;
        self.watch_replacements.apply(
            edit,
            &mut self.subscriptions,
            &mut self.process_watches,
            &mut self.inbox,
            &receipt,
        )?;
        self.cursor = receipt.clone();
        Ok(receipt)
    }
}
