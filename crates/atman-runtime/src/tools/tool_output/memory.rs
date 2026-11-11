//! Bounded writable output retention sharing the Flow's host-local allocation account.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::{Arc, Mutex},
};

use super::OutputStore;
use crate::flow_store::transient::{TransientAllocation, TransientRetention};

/// Immutable memory output identities and allocation ownership captured for retention.
pub struct MemoryOutputSnapshot(BTreeMap<String, MemoryValue>);

impl MemoryOutputSnapshot {
    /// Copy the captured outputs without executing tools or replacing conflicting durable data.
    pub fn persist_to(&self, destination: &std::path::Path) -> io::Result<()> {
        for (id, value) in &self.0 {
            crate::retained_file::publish(
                &super::output_dir(destination).join(format!("{id}.txt")),
                value.content().as_bytes(),
            )?;
        }
        Ok(())
    }
}

pub(super) struct MemoryOutputs {
    retention: TransientRetention,
    values: Mutex<HashMap<String, MemoryValue>>,
}

struct Seed {
    contents: Arc<BTreeMap<String, String>>,
    _allocation: TransientAllocation,
}

struct Output {
    content: String,
    _allocation: TransientAllocation,
}

#[derive(Clone)]
pub(super) struct MemoryValue {
    data: MemoryData,
    digest: blake3::Hash,
}

#[derive(Clone)]
enum MemoryData {
    Seed { seed: Arc<Seed>, id: String },
    Output(Arc<Output>),
}

impl MemoryValue {
    pub(super) fn content(&self) -> &str {
        match &self.data {
            MemoryData::Seed { seed, id } => &seed.contents[id],
            MemoryData::Output(output) => &output.content,
        }
    }
}

impl OutputStore {
    pub fn memory_snapshot(&self) -> io::Result<MemoryOutputSnapshot> {
        let memory = self.memory.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "output store is not writable memory",
            )
        })?;
        Ok(MemoryOutputSnapshot(
            memory
                .values
                .lock()
                .unwrap()
                .iter()
                .map(|(id, value)| (id.clone(), value.clone()))
                .collect(),
        ))
    }

    /// Create a writable memory store without a filesystem publication directory.
    pub fn memory(retention: TransientRetention, seed: OutputStore) -> io::Result<Self> {
        let store = Self {
            session_dir: None,
            retained: Arc::default(),
            memory: Some(Arc::new(MemoryOutputs {
                retention,
                values: Mutex::default(),
            })),
        };
        store.import_retained(&seed)?;
        Ok(store)
    }

    /// Atomically import an immutable snapshot. Conflicting IDs and insufficient capacity publish nothing.
    pub fn import_retained(&self, seed: &OutputStore) -> io::Result<()> {
        if seed.session_dir.is_some() || seed.memory.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output seed must be an immutable captured snapshot",
            ));
        }
        let memory = self.memory.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "output store is not writable memory",
            )
        })?;
        let prepared = seed
            .retained
            .iter()
            .map(|(id, text)| (id.clone(), blake3::hash(text.as_bytes())))
            .collect::<Vec<_>>();
        let bytes = seed
            .retained
            .iter()
            .try_fold(0usize, |total, (id, text)| {
                total.checked_add(id.len())?.checked_add(text.len())
            })
            .ok_or_else(|| io::Error::other("retained output size overflow"))?;
        let mut values = memory.values.lock().unwrap();
        for (id, digest) in &prepared {
            if values.get(id).is_some_and(|value| value.digest != *digest) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "captured output ID has conflicting content",
                ));
            }
        }
        if prepared.iter().all(|(id, _)| values.contains_key(id)) {
            return Ok(());
        }
        // The entire immutable map is retained; charge it even when some entries were already present.
        let retained = Arc::new(Seed {
            contents: seed.retained.clone(),
            _allocation: memory.retention.reserve(bytes)?,
        });
        for (id, digest) in prepared {
            values.entry(id.clone()).or_insert_with(|| MemoryValue {
                data: MemoryData::Seed {
                    seed: retained.clone(),
                    id,
                },
                digest,
            });
        }
        Ok(())
    }
}

impl MemoryOutputs {
    pub(super) fn is_empty(&self) -> bool {
        self.values.lock().unwrap().is_empty()
    }

    pub(super) fn get(&self, id: &str) -> Option<MemoryValue> {
        self.values.lock().unwrap().get(id).cloned()
    }

    pub(super) fn register(&self, content: &str) -> io::Result<String> {
        let id = format!("out_{}", uuid::Uuid::now_v7().simple());
        let bytes = id
            .len()
            .checked_add(content.len())
            .ok_or_else(|| io::Error::other("output size overflow"))?;
        let allocation = self.retention.reserve(bytes)?;
        let value = MemoryValue {
            digest: blake3::hash(content.as_bytes()),
            data: MemoryData::Output(Arc::new(Output {
                content: content.to_owned(),
                _allocation: allocation,
            })),
        };
        self.values.lock().unwrap().insert(id.clone(), value);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_store::transient::{TransientBudget, TransientLimits};
    use crate::tools::tool_output::{ToolOutputBudget, truncate_tool_result_content_with_budget};

    fn retention(maximum_bytes: usize) -> (TransientBudget, TransientRetention) {
        let host = TransientBudget::new(maximum_bytes * 2).unwrap();
        let retention = TransientRetention::new(
            host.clone(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            TransientLimits {
                maximum_bytes,
                terminal_reserve_bytes: 16 * 1024,
            },
        )
        .unwrap();
        (host, retention)
    }

    fn reference(id: &str, text: &str) -> String {
        serde_json::json!({ "content": text, "output_id": id, "total_bytes": text.len(), "next": {"mode":"bytes","offset":text.len(),"has_more":false} }).to_string()
    }

    #[test]
    fn memory_outputs_share_captured_assets_and_keep_allocation_until_retention_handoff() {
        let (host, retention) = retention(64 * 1024);
        let id = "out_00000000000000000000000000000001";
        let seed =
            OutputStore::retained(BTreeMap::from([(id.into(), "seed output".into())])).unwrap();
        let store = OutputStore::memory(retention.clone(), seed.clone()).unwrap();
        let initial = host.retained_bytes();
        store.import_retained(&seed).unwrap();
        assert_eq!(host.retained_bytes(), initial);
        assert_eq!(
            store
                .retained_content(&reference(id, "seed output"))
                .as_deref(),
            Some("seed output")
        );
        let text = "new output\n".repeat(30);
        let notice = truncate_tool_result_content_with_budget(
            &text,
            "new",
            Some(&store),
            ToolOutputBudget {
                max_lines: 1,
                max_bytes: 8,
                max_line_bytes: 8,
            },
        );
        assert_eq!(
            store.retained_content(&notice).as_deref(),
            Some(text.as_str())
        );
        assert!(host.retained_bytes() > initial);
        let copy = store.snapshot_references([notice.as_str()]);
        assert_eq!(
            copy.retained_content(&notice).as_deref(),
            Some(text.as_str())
        );
        let snapshot = store.memory_snapshot().unwrap();
        let held = host.retained_bytes();
        drop((store, retention, seed));
        assert_eq!(host.retained_bytes(), held);
        let dir = tempfile::tempdir().unwrap();
        snapshot.persist_to(dir.path()).unwrap();
        let durable = OutputStore::at(dir.path());
        assert_eq!(
            durable
                .read_bytes(id, 0, 100, ToolOutputBudget::default())
                .unwrap()
                .content,
            "seed output"
        );
        assert_eq!(
            durable
                .snapshot_references([notice.as_str()])
                .retained_content(&notice)
                .as_deref(),
            Some(text.as_str())
        );
        drop(snapshot);
        assert_eq!(host.retained_bytes(), 0);
    }

    #[test]
    fn memory_output_import_and_registration_reject_capacity_or_conflicts_atomically() {
        let (host, retention) = retention(64 * 1024);
        let store = OutputStore::memory(retention.clone(), OutputStore::default()).unwrap();
        let id = store.register("first", "kept").unwrap();
        let before = host.retained_bytes();
        let conflicting = OutputStore::retained(BTreeMap::from([
            (id.clone(), "changed".into()),
            (
                "out_00000000000000000000000000000002".into(),
                "not imported".into(),
            ),
        ]))
        .unwrap();
        assert!(store.import_retained(&conflicting).is_err());
        assert_eq!(host.retained_bytes(), before);
        assert!(
            store
                .read_bytes(
                    "out_00000000000000000000000000000002",
                    0,
                    100,
                    ToolOutputBudget::default()
                )
                .is_err()
        );
        assert!(
            store
                .register("oversized", &"x".repeat(64 * 1024))
                .is_none()
        );
        assert_eq!(host.retained_bytes(), before);
        assert!(retention.usage().rejections > 0);
        assert_eq!(
            store
                .read_bytes(&id, 0, 100, ToolOutputBudget::default())
                .unwrap()
                .content,
            "kept"
        );
        let directory = tempfile::tempdir().unwrap();
        assert!(
            store
                .import_retained(&OutputStore::at(directory.path()))
                .is_err()
        );
        assert!(
            store
                .read_bytes("../../outside", 0, 100, ToolOutputBudget::default())
                .is_err()
        );
    }
}
