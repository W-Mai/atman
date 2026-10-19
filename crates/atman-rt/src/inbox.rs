//! Single-consumer batch ownership for host-managed inboxes.
//!
//! Hosts serialize mutations, authenticate message sources, enforce execution gates, and persist input commits. Reservations do not remove messages. The publication callback runs before consumption and must not reenter the owning queue.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};

/// FIFO messages with at most one outstanding reservation, including filtered batches.
pub struct PendingBatch<T> {
    pending: VecDeque<T>,
    reserved: Option<Weak<()>>,
}

impl<T> Default for PendingBatch<T> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            reserved: None,
        }
    }
}

/// Opaque reservation identity; items can be inspected but not replaced or forged.
pub struct Batch<T> {
    token: Arc<()>,
    positions: Vec<usize>,
    items: Vec<T>,
}

impl<T> Batch<T> {
    pub fn items(&self) -> &[T] {
        &self.items
    }
}

impl<T> PendingBatch<T> {
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
    pub fn len(&self) -> usize {
        self.pending.len()
    }
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.pending.iter()
    }
    pub fn is_ready(&self) -> bool {
        !self.has_reservation() && !self.pending.is_empty()
    }

    pub fn is_ready_matching(&self, predicate: impl FnMut(&T) -> bool) -> bool {
        !self.has_reservation() && self.pending.iter().any(predicate)
    }

    pub fn push_back(&mut self, item: T) {
        self.pending.push_back(item);
    }

    /// Discard pending items only after the host has committed their explicit cancellation.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.reserved = None;
    }

    pub fn pop_front(&mut self) -> Option<T> {
        if self.has_reservation() {
            None
        } else {
            self.pending.pop_front()
        }
    }

    pub fn owns(&self, batch: &Batch<T>) -> bool {
        self.reserved
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|token| Arc::ptr_eq(&token, &batch.token))
    }

    /// Whether an outstanding receipt owns a batch in this queue.
    pub fn has_reservation(&self) -> bool {
        self.reserved.as_ref().and_then(Weak::upgrade).is_some()
    }

    /// Exchange pending positions; reject changes while a receipt owns a reservation.
    pub fn swap(&mut self, first: usize, second: usize) -> bool {
        if self.has_reservation() || first >= self.pending.len() || second >= self.pending.len() {
            return false;
        }
        self.pending.swap(first, second);
        true
    }

    pub fn release(&mut self, batch: &Batch<T>) -> bool {
        if !self.owns(batch) {
            return false;
        }
        self.reserved = None;
        true
    }

    /// Revoke ownership while retaining every pending message in its original position.
    pub fn invalidate_reservation(&mut self) {
        self.reserved = None;
    }

    /// Publish before consumption. Failure keeps ownership and all messages intact.
    pub fn commit<E>(
        &mut self,
        batch: &Batch<T>,
        publish: impl FnOnce() -> Result<(), E>,
    ) -> Result<bool, E> {
        if !self.owns(batch) {
            return Ok(false);
        }
        publish()?;
        let mut positions = batch.positions.iter().peekable();
        let mut index = 0;
        self.pending.retain(|_| {
            let selected = positions.peek().is_some_and(|&&position| position == index);
            if selected {
                positions.next();
            }
            index += 1;
            !selected
        });
        self.reserved = None;
        Ok(true)
    }
}

impl<T: Clone> PendingBatch<T> {
    pub fn reserve(&mut self) -> Option<Batch<T>> {
        self.reserve_matching(|_| true)
    }

    /// Reserve matching entries in FIFO order; unmatched entries and later arrivals remain queued.
    pub fn reserve_matching(&mut self, mut predicate: impl FnMut(&T) -> bool) -> Option<Batch<T>> {
        if !self.is_ready() {
            return None;
        }
        let mut positions = Vec::new();
        let mut items = Vec::new();
        for (position, item) in self.pending.iter().enumerate() {
            if predicate(item) {
                positions.push(position);
                items.push(item.clone());
            }
        }
        if positions.is_empty() {
            return None;
        }
        // A live receipt keeps this allocation alive, preventing cross-queue or reused-token ABA.
        let token = Arc::new(());
        self.reserved = Some(Arc::downgrade(&token));
        Some(Batch {
            token,
            positions,
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_receipts_release_ownership_without_consuming_inputs() {
        let mut queue = PendingBatch::default();
        queue.push_back(1);
        let batch = queue.reserve().unwrap();
        assert!(!queue.is_ready());
        drop(batch);
        assert!(queue.is_ready_matching(|item| *item == 1));
        let next = queue.reserve().unwrap();
        assert_eq!(next.items(), &[1]);
        assert!(queue.owns(&next));
    }

    #[test]
    fn publication_failure_keeps_fifo_and_excludes_other_consumers() {
        let mut queue = PendingBatch::default();
        queue.push_back(1);
        queue.push_back(2);
        let batch = queue.reserve().unwrap();
        assert_eq!(batch.items(), &[1, 2]);
        assert!(!queue.swap(0, 1));
        queue.push_back(3);
        assert!(queue.reserve().is_none());
        assert!(queue.pop_front().is_none());
        assert!(!queue.is_ready_matching(|_| true));
        assert_eq!(
            queue.commit(&batch, || Err("disk failed")),
            Err("disk failed")
        );
        assert_eq!(queue.len(), 3);
        assert!(queue.owns(&batch));
        assert_eq!(queue.commit(&batch, || Ok::<_, ()>(())), Ok(true));
        assert_eq!(queue.pop_front(), Some(3));
        assert!(!queue.swap(0, 1));
        assert!(
            !queue
                .commit(&batch, || -> Result<(), ()> {
                    panic!("duplicate publication")
                })
                .unwrap()
        );
    }

    #[test]
    fn filtered_batches_preserve_unselected_messages_and_late_arrivals() {
        let mut queue = PendingBatch::default();
        for value in 0..6 {
            queue.push_back(value);
        }
        assert!(queue.reserve_matching(|_| false).is_none());
        assert!(queue.is_ready());
        let batch = queue.reserve_matching(|value| value % 2 == 0).unwrap();
        assert_eq!(batch.items(), &[0, 2, 4]);
        queue.push_back(6);
        queue.commit(&batch, || Ok::<_, ()>(())).unwrap();
        assert_eq!(queue.iter().copied().collect::<Vec<_>>(), [1, 3, 5, 6]);
        assert!(!queue.is_ready_matching(|value| *value == 2));
        assert!(queue.is_ready_matching(|value| *value == 6));
    }

    #[test]
    fn receipts_cannot_cross_queues_or_survive_release_invalidation_and_clear() {
        let mut a = PendingBatch::default();
        let mut b = PendingBatch::default();
        a.push_back(1);
        b.push_back(1);
        let foreign = b.reserve().unwrap();
        let old = a.reserve().unwrap();
        assert!(!a.release(&foreign));
        assert!(
            !a.commit(&foreign, || -> Result<(), ()> {
                panic!("foreign publication")
            })
            .unwrap()
        );
        assert!(a.release(&old));
        let next = a.reserve().unwrap();
        assert!(!a.release(&old));
        a.invalidate_reservation();
        assert_eq!(a.len(), 1);
        let newest = a.reserve().unwrap();
        assert!(!a.release(&next));
        a.clear();
        a.push_back(2);
        let after_clear = a.reserve().unwrap();
        assert!(!a.release(&newest));
        assert!(a.owns(&after_clear));
    }

    #[test]
    fn queue_reordering_preserves_batch_positions_and_rejects_active_ownership() {
        let mut queue = PendingBatch::default();
        for value in 0..3 {
            queue.push_back(value);
        }
        assert!(queue.swap(0, 2));
        let batch = queue.reserve_matching(|value| *value != 1).unwrap();
        assert_eq!(batch.items(), &[2, 0]);
        assert!(!queue.swap(0, 1));
        queue.commit(&batch, || Ok::<_, ()>(())).unwrap();
        assert_eq!(queue.pop_front(), Some(1));
    }
}
