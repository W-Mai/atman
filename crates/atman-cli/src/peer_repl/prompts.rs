//! Invocation-bound terminal prompts, independent of the selected flow.

use std::{collections::BTreeMap, sync::Mutex};

use anyhow::{Result, bail};
use atman_runtime::rendezvous::{PromptContext, PromptId, PromptResolver};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

struct Pending {
    label: String,
    reply: oneshot::Sender<Value>,
}

pub(super) struct Prompts {
    pending: Mutex<BTreeMap<Uuid, Pending>>,
    notices: mpsc::UnboundedSender<String>,
    disconnected: std::sync::atomic::AtomicBool,
}

impl Prompts {
    pub fn new(notices: mpsc::UnboundedSender<String>) -> Self {
        Self {
            pending: Mutex::new(BTreeMap::new()),
            notices,
            disconnected: Default::default(),
        }
    }

    pub fn pending(&self) -> Vec<String> {
        self.pending
            .lock()
            .unwrap()
            .values()
            .map(|pending| pending.label.clone())
            .collect()
    }

    pub fn disconnect(&self) {
        let mut pending = self.pending.lock().unwrap();
        self.disconnected
            .store(true, std::sync::atomic::Ordering::Release);
        pending.clear();
    }

    pub fn reply(&self, id: Uuid, value: Value) -> Result<()> {
        let Some(pending) = self.pending.lock().unwrap().remove(&id) else {
            bail!("prompt is no longer pending");
        };
        pending
            .reply
            .send(value)
            .map_err(|_| anyhow::anyhow!("prompt caller has ended"))
    }

    fn request(
        &self,
        id: PromptId,
        kind: &str,
        payload: Value,
        context: Option<&PromptContext>,
    ) -> oneshot::Receiver<Value> {
        let (reply, receiver) = oneshot::channel();
        let source = context
            .and_then(|ctx| ctx.run_id.as_ref())
            .map_or_else(|| "host".into(), ToString::to_string);
        let flow = context
            .and_then(|ctx| ctx.session.as_ref())
            .and_then(|session| session.flow_instance())
            .map_or_else(|| "host".into(), |flow| flow.instance_id.to_string());
        let label = format!(
            "Prompt {id} · Flow {flow} · run {source} · {kind}\n{payload}\n:reply {id} <JSON answer>"
        );
        let mut pending = self.pending.lock().unwrap();
        if self.disconnected.load(std::sync::atomic::Ordering::Acquire)
            || self.notices.is_closed()
            || pending.contains_key(&id.0)
        {
            return receiver;
        }
        pending.insert(
            id.0,
            Pending {
                label: label.clone(),
                reply,
            },
        );
        if self.notices.send(label).is_err() {
            pending.remove(&id.0);
        }
        receiver
    }
}

impl PromptResolver for Prompts {
    fn register(&self, id: PromptId) -> oneshot::Receiver<Value> {
        self.request(id, "prompt", Value::Null, None)
    }

    fn register_with_payload(
        &self,
        id: PromptId,
        kind: &str,
        payload: Value,
    ) -> oneshot::Receiver<Value> {
        self.request(id, kind, payload, None)
    }

    fn register_in_context(
        &self,
        id: PromptId,
        kind: &str,
        payload: Value,
        context: &PromptContext,
    ) -> oneshot::Receiver<Value> {
        self.request(id, kind, payload, Some(context))
    }

    fn drop_pending(&self, id: &PromptId) {
        self.pending.lock().unwrap().remove(&id.0);
    }

    fn expire_pending(&self, id: &PromptId) -> bool {
        self.pending.lock().unwrap().remove(&id.0).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replies_expiry_and_disconnect_have_one_owner() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let prompts = Prompts::new(tx);
        let id = PromptId::now();
        let answer = prompts.register(id);
        assert!(prompts.register(id).await.is_err());
        prompts.reply(id.0, Value::Bool(true)).unwrap();
        assert!(!prompts.expire_pending(&id));
        assert_eq!(answer.await.unwrap(), Value::Bool(true));
        assert!(prompts.reply(id.0, Value::Bool(false)).is_err());
        let id = PromptId::now();
        let expired = prompts.register(id);
        assert!(prompts.expire_pending(&id));
        assert!(expired.await.is_err());
        let pending = prompts.register(PromptId::now());
        prompts.disconnect();
        assert!(pending.await.is_err());
        assert!(prompts.register(PromptId::now()).await.is_err());
        assert!(prompts.pending().is_empty());
    }
}
