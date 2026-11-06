//! Immutable message payloads with independently tracked buffer versions.

use std::{ops::Index, sync::Arc};

use atman_runtime::message::Message;

use super::DocumentIdentity;

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum MessageVersion {
    Revision(u64, u64),
    Shared(u64, DocumentIdentity),
}

/// Shared message storage with content versions independent of task metadata.
#[derive(Clone, Debug, Default)]
pub struct SharedMessages {
    values: Arc<Vec<Arc<Message>>>,
    version: DocumentIdentity,
}

impl From<Vec<Message>> for SharedMessages {
    fn from(messages: Vec<Message>) -> Self {
        Self {
            values: Arc::new(messages.into_iter().map(Arc::new).collect()),
            version: Default::default(),
        }
    }
}

impl SharedMessages {
    pub fn len(&self) -> usize {
        self.values.len()
    }
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    pub fn get(&self, index: usize) -> Option<&Message> {
        self.values.get(index).map(Arc::as_ref)
    }
    pub fn first(&self) -> Option<&Message> {
        self.values.first().map(Arc::as_ref)
    }
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Message> + ExactSizeIterator + Clone {
        self.values.iter().map(Arc::as_ref)
    }
    pub fn push(&mut self, message: Message) {
        Arc::make_mut(&mut self.values).push(Arc::new(message));
        self.version = Default::default();
    }
    pub(crate) fn retain_tail(&mut self, limit: usize) -> Vec<Arc<Message>> {
        let removed = self.len().saturating_sub(limit);
        if removed == 0 {
            return Vec::new();
        }
        self.version = Default::default();
        Arc::make_mut(&mut self.values).drain(..removed).collect()
    }
    pub(crate) fn identity(&self) -> DocumentIdentity {
        self.version.clone()
    }
}

impl Index<usize> for SharedMessages {
    type Output = Message;
    fn index(&self, index: usize) -> &Self::Output {
        &self.values[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_versions_share_payloads_and_isolate_mutations_without_body_copies() {
        let turn = atman_rt::TurnId::now();
        let mut original = SharedMessages::from(
            (0..100)
                .map(|_| Message::assistant_text(turn.clone(), "large payload 汉".repeat(4096)))
                .collect::<Vec<_>>(),
        );
        let mut fork = original.clone();
        let version = original.identity();
        let payload = original[0].parts.as_ptr();
        assert_eq!(version, fork.identity());
        fork.push(Message::assistant_text(turn.clone(), "new message"));
        assert_ne!(version, fork.identity());
        assert_eq!(original.len(), 100);
        assert_eq!(fork.len(), 101);
        assert_eq!(payload, fork[0].parts.as_ptr());
        assert!(
            original
                .iter()
                .zip(fork.iter())
                .all(|(left, right)| std::ptr::eq(left, right))
        );
        original.push(Message::assistant_text(turn, "different message"));
        assert_ne!(
            original.identity(),
            fork.identity(),
            "independent content mutations cannot alias by equal counters"
        );
        let before_tail = fork.identity();
        let retired = fork.retain_tail(100);
        assert_eq!(retired.len(), 1);
        assert_eq!(payload, retired[0].parts.as_ptr());
        assert_ne!(before_tail, fork.identity());
        let kept = fork.identity();
        assert!(fork.retain_tail(100).is_empty());
        assert_eq!(kept, fork.identity());
    }
}
