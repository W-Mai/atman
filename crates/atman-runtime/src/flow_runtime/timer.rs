//! Timer controls serialized by the instance owner. RuntimeHost observation drives deadlines.

use std::io;

use uuid::Uuid;

use super::FlowWorker;
use crate::flow_store::position::FlowPosition;
use crate::flow_store::{
    FlowRef,
    timer::{Timer, TimerRecord},
};

impl FlowWorker {
    /// Schedule owned input without claiming a turn. The host authenticates the caller, including retries.
    pub async fn schedule_timer(
        &self,
        timer: Timer,
        authorize: impl FnOnce(&FlowRef) -> io::Result<()> + Send + 'static,
    ) -> io::Result<TimerRecord> {
        self.request(move |store| {
            authorize(store.state().address())?;
            store.schedule_timer(timer)
        })
        .await
    }

    pub async fn cancel_timer(
        &self,
        id: Uuid,
        authorize: impl FnOnce(&FlowRef) -> io::Result<()> + Send + 'static,
    ) -> io::Result<FlowPosition> {
        self.request(move |store| {
            authorize(store.state().address())?;
            store.cancel_timer(id)
        })
        .await
    }

    pub async fn timers(
        &self,
        authorize: impl FnOnce(&FlowRef) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Vec<TimerRecord>> {
        self.request(move |store| {
            authorize(store.state().address())?;
            Ok(store.timers().cloned().collect())
        })
        .await
    }
}
