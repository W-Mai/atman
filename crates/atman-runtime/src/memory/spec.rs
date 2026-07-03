use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::MemoryId;
use crate::error::RuntimeError;

const PHASES: &[&str] = &[
    "research",
    "design",
    "implementation",
    "testing",
    "retrospective",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecEntry {
    pub id: MemoryId,
    pub feature: String,
    pub phase: String,
    pub content: String,
    pub ts: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecDeviation {
    pub id: MemoryId,
    pub feature: String,
    pub section: String,
    pub delta: String,
    pub reason: String,
    pub ts: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone)]
pub struct SpecStore {
    root: PathBuf,
}

impl SpecStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn feature_dir(&self, feature: &str) -> PathBuf {
        self.root.join(feature)
    }

    fn entries_path(&self, feature: &str) -> PathBuf {
        self.feature_dir(feature).join("entries.jsonl")
    }

    fn deviations_path(&self, feature: &str) -> PathBuf {
        self.feature_dir(feature).join("deviations.jsonl")
    }

    pub async fn status(&self, feature: &str) -> Result<SpecStatus, RuntimeError> {
        let entries: Vec<SpecEntry> = super::read_jsonl(&self.entries_path(feature)).await?;
        if entries.is_empty() {
            return Ok(SpecStatus {
                feature: feature.into(),
                phase: "not_started".into(),
                entry_count: 0,
                deviation_count: 0,
            });
        }
        let latest = latest_phase(&entries);
        let dev_count = super::read_jsonl::<SpecDeviation>(&self.deviations_path(feature))
            .await?
            .len();
        Ok(SpecStatus {
            feature: feature.into(),
            phase: latest,
            entry_count: entries.len(),
            deviation_count: dev_count,
        })
    }

    pub async fn update(
        &self,
        feature: &str,
        phase: &str,
        content: String,
    ) -> Result<SpecEntry, RuntimeError> {
        if !PHASES.contains(&phase) {
            return Err(RuntimeError::ToolFailed(format!(
                "spec.update: unknown phase `{phase}` (want one of {})",
                PHASES.join(", ")
            )));
        }
        let current = self.status(feature).await?;
        if let Err(msg) = check_phase_transition(&current.phase, phase) {
            return Err(RuntimeError::ToolFailed(format!("spec.update: {msg}")));
        }
        let entry = SpecEntry {
            id: MemoryId::now(),
            feature: feature.into(),
            phase: phase.into(),
            content,
            ts: chrono::Utc::now(),
        };
        super::append_jsonl(&self.entries_path(feature), &entry).await?;
        Ok(entry)
    }

    pub async fn deviate(
        &self,
        feature: &str,
        section: String,
        delta: String,
        reason: String,
    ) -> Result<SpecDeviation, RuntimeError> {
        let current = self.status(feature).await?;
        if current.phase == "not_started" {
            return Err(RuntimeError::ToolFailed(
                "spec.deviate: feature has no entries yet, run spec.update first".into(),
            ));
        }
        let dev = SpecDeviation {
            id: MemoryId::now(),
            feature: feature.into(),
            section,
            delta,
            reason,
            ts: chrono::Utc::now(),
        };
        super::append_jsonl(&self.deviations_path(feature), &dev).await?;
        Ok(dev)
    }

    pub async fn deviations(&self, feature: &str) -> Result<Vec<SpecDeviation>, RuntimeError> {
        super::read_jsonl(&self.deviations_path(feature)).await
    }

    pub async fn entries(&self, feature: &str) -> Result<Vec<SpecEntry>, RuntimeError> {
        super::read_jsonl(&self.entries_path(feature)).await
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecStatus {
    pub feature: String,
    pub phase: String,
    pub entry_count: usize,
    pub deviation_count: usize,
}

fn latest_phase(entries: &[SpecEntry]) -> String {
    let mut best = 0usize;
    for e in entries {
        if let Some(idx) = PHASES.iter().position(|p| *p == e.phase.as_str())
            && idx + 1 > best
        {
            best = idx + 1;
        }
    }
    if best == 0 {
        "not_started".into()
    } else {
        PHASES[best - 1].into()
    }
}

fn check_phase_transition(current: &str, next: &str) -> Result<(), String> {
    let cur_idx = PHASES.iter().position(|p| *p == current).unwrap_or(0);
    let next_idx = PHASES
        .iter()
        .position(|p| *p == next)
        .ok_or_else(|| format!("unknown phase `{next}`"))?;
    let is_first = current == "not_started";
    if is_first && next != PHASES[0] {
        return Err(format!(
            "phase gate: must start with `{}`, not `{next}`",
            PHASES[0]
        ));
    }
    if !is_first && next_idx > cur_idx + 1 {
        return Err(format!(
            "phase gate: cannot skip from `{current}` to `{next}` (must go through {})",
            PHASES[cur_idx + 1]
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (SpecStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SpecStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    #[tokio::test]
    async fn new_feature_status_is_not_started() {
        let (s, _dir) = store().await;
        let st = s.status("x").await.unwrap();
        assert_eq!(st.phase, "not_started");
        assert_eq!(st.entry_count, 0);
    }

    #[tokio::test]
    async fn update_advances_phase() {
        let (s, _dir) = store().await;
        s.update("x", "research", "notes".into()).await.unwrap();
        assert_eq!(s.status("x").await.unwrap().phase, "research");
        s.update("x", "design", "spec".into()).await.unwrap();
        assert_eq!(s.status("x").await.unwrap().phase, "design");
    }

    #[tokio::test]
    async fn phase_gate_rejects_skip() {
        let (s, _dir) = store().await;
        let err = s
            .update("x", "implementation", "premature".into())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("must start with `research`"));
    }

    #[tokio::test]
    async fn phase_gate_rejects_backwards() {
        let (s, _dir) = store().await;
        s.update("x", "research", "r".into()).await.unwrap();
        s.update("x", "design", "d".into()).await.unwrap();
        let err = s
            .update("x", "testing", "premature".into())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("cannot skip"), "err: {err}");
    }

    #[tokio::test]
    async fn deviate_requires_prior_entry() {
        let (s, _dir) = store().await;
        let err = s
            .deviate("x", "sec".into(), "delta".into(), "why".into())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no entries"));
    }

    #[tokio::test]
    async fn deviate_appends_to_deviations_file() {
        let (s, _dir) = store().await;
        s.update("x", "research", "r".into()).await.unwrap();
        s.update("x", "design", "d".into()).await.unwrap();
        s.deviate(
            "x",
            "data".into(),
            "added field".into(),
            "need array".into(),
        )
        .await
        .unwrap();
        s.deviate("x", "algo".into(), "changed loop".into(), "perf".into())
            .await
            .unwrap();
        let devs = s.deviations("x").await.unwrap();
        assert_eq!(devs.len(), 2);
        assert_eq!(s.status("x").await.unwrap().deviation_count, 2);
    }

    #[tokio::test]
    async fn unknown_phase_rejected() {
        let (s, _dir) = store().await;
        let err = s.update("x", "brainstorm", "n".into()).await.unwrap_err();
        assert!(format!("{err}").contains("unknown phase"));
    }
}
