//! FIFO reservation state embedded under the owner's existing mutex.

use std::collections::VecDeque;

pub(crate) struct PendingBatch<T> {
    pending: VecDeque<T>,
    token: u64,
    reserved: bool,
}

impl<T> Default for PendingBatch<T> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            token: 0,
            reserved: false,
        }
    }
}

pub(crate) struct Batch<T> {
    token: u64,
    pub items: Vec<T>,
}

impl<T> PendingBatch<T> {
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn is_ready(&self) -> bool {
        !self.reserved && !self.pending.is_empty()
    }

    pub fn push_back(&mut self, item: T) {
        self.pending.push_back(item);
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.reserved = false;
        self.token = self.token.wrapping_add(1);
    }

    pub fn pop_front(&mut self) -> Option<T> {
        if self.reserved {
            None
        } else {
            self.pending.pop_front()
        }
    }

    pub fn release(&mut self, batch: &Batch<T>) -> bool {
        if !self.reserved || self.token != batch.token {
            return false;
        }
        self.reserved = false;
        true
    }

    /// Publish before consumption. A failed publication leaves the batch reserved.
    pub fn commit<E>(
        &mut self,
        batch: &Batch<T>,
        publish: impl FnOnce() -> Result<(), E>,
    ) -> Result<bool, E> {
        if !self.reserved || self.token != batch.token {
            return Ok(false);
        }
        publish()?;
        self.pending.drain(..batch.items.len());
        self.reserved = false;
        Ok(true)
    }
}

impl<T: Clone> PendingBatch<T> {
    pub fn reserve(&mut self) -> Option<Batch<T>> {
        if !self.is_ready() {
            return None;
        }
        self.token = self.token.wrapping_add(1);
        self.reserved = true;
        Some(Batch {
            token: self.token,
            items: self.pending.iter().cloned().collect(),
        })
    }
}
