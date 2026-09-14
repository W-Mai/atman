use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::MemoryId;
use crate::error::RuntimeError;
use crate::index::AnchorIndex;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecReview {
    pub feature: String,
    pub design_revision: String,
    pub approved: bool,
    pub ts: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone)]
pub struct SpecStore {
    root: PathBuf,
    anchor_index: Option<Arc<AnchorIndex>>,
}

impl SpecStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            anchor_index: None,
        }
    }

    pub fn with_index(mut self, index: Arc<AnchorIndex>) -> Self {
        self.anchor_index = Some(index);
        self
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

    fn reviews_path(&self, feature: &str) -> PathBuf {
        self.feature_dir(feature).join("reviews.jsonl")
    }

    pub async fn design_revision(&self, feature: &str) -> Result<Option<String>, RuntimeError> {
        let entries = self.entries(feature).await?;
        if !entries.iter().any(|entry| entry.phase == "design") {
            return Ok(None);
        }
        Ok(Some(revision_for(&render_phase_markdown(
            feature, "design", &entries,
        ))))
    }

    pub async fn phase_markdown(&self, feature: &str, phase: &str) -> Result<String, RuntimeError> {
        validate_feature(feature)?;
        if !PHASES.contains(&phase) {
            return Err(RuntimeError::ToolFailed(format!(
                "spec.read: unknown phase `{phase}`"
            )));
        }
        let entries = self.entries(feature).await?;
        Ok(render_phase_markdown(feature, phase, &entries))
    }

    pub async fn phase_revision(
        &self,
        feature: &str,
        phase: &str,
    ) -> Result<Option<String>, RuntimeError> {
        let entries = self.entries(feature).await?;
        if !entries.iter().any(|entry| entry.phase == phase) {
            return Ok(None);
        }
        Ok(Some(revision_for(
            &self.phase_markdown(feature, phase).await?,
        )))
    }

    pub async fn materialized_revision(
        &self,
        feature: &str,
        phase: &str,
    ) -> Result<String, RuntimeError> {
        validate_feature(feature)?;
        if !PHASES.contains(&phase) {
            return Err(RuntimeError::ToolFailed(format!(
                "spec.read: unknown phase `{phase}`"
            )));
        }
        let filename = if phase == "implementation" {
            "IMPLEMENTATION.md".to_owned()
        } else {
            format!("{phase}.md")
        };
        let path = self.feature_dir(feature).join(filename);
        match tokio::fs::read_to_string(path).await {
            Ok(content) => Ok(revision_for(&content)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(RuntimeError::ToolFailed(format!(
                "spec.read: materialized file: {error}"
            ))),
        }
    }

    pub async fn review(
        &self,
        feature: &str,
        design_revision: &str,
        approved: bool,
    ) -> Result<SpecReview, RuntimeError> {
        if self.design_revision(feature).await?.as_deref() != Some(design_revision) {
            return Err(RuntimeError::ToolFailed(
                "spec.review: design revision is missing or stale".into(),
            ));
        }
        let file_revision = self.materialized_revision(feature, "design").await?;
        if file_revision != design_revision {
            return Err(RuntimeError::ToolFailed(
                "spec.review: materialized design differs from the current revision".into(),
            ));
        }
        let review = SpecReview {
            feature: feature.into(),
            design_revision: design_revision.into(),
            approved,
            ts: chrono::Utc::now(),
        };
        super::append_jsonl(&self.reviews_path(feature), &review).await?;
        Ok(review)
    }

    pub async fn status(&self, feature: &str) -> Result<SpecStatus, RuntimeError> {
        validate_feature(feature)?;
        let entries: Vec<SpecEntry> = super::read_jsonl(&self.entries_path(feature)).await?;
        if entries.is_empty() {
            return Ok(SpecStatus {
                feature: feature.into(),
                phase: "not_started".into(),
                entry_count: 0,
                deviation_count: 0,
                design_revision: None,
                approved_design_revision: None,
            });
        }
        let latest = latest_phase(&entries);
        let dev_count = super::read_jsonl::<SpecDeviation>(&self.deviations_path(feature))
            .await?
            .len();
        let design_revision = entries
            .iter()
            .any(|entry| entry.phase == "design")
            .then(|| revision_for(&render_phase_markdown(feature, "design", &entries)));
        let reviews: Vec<SpecReview> = super::read_jsonl(&self.reviews_path(feature)).await?;
        let materialized_revision = self.materialized_revision(feature, "design").await?;
        let approved_design_revision =
            reviews
                .last()
                .filter(|review| review.approved)
                .and_then(|review| {
                    (design_revision.as_deref() == Some(review.design_revision.as_str())
                        && materialized_revision == review.design_revision)
                        .then(|| review.design_revision.clone())
                });
        Ok(SpecStatus {
            feature: feature.into(),
            phase: latest,
            entry_count: entries.len(),
            deviation_count: dev_count,
            design_revision,
            approved_design_revision,
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
        if let Some(idx) = &self.anchor_index
            && let Err(e) = insert_entry(idx, &entry)
        {
            let key = format!("spec.index.entry:{}", entry.id);
            crate::notify!(
                warn,
                location = Inline,
                stack = dedupe(key, 60_000),
                "spec entry index insert failed (id={}): {e}",
                entry.id
            );
        }
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
        if let Some(idx) = &self.anchor_index
            && let Err(e) = insert_deviation(idx, &dev)
        {
            let key = format!("spec.index.deviation:{}", dev.id);
            crate::notify!(
                warn,
                location = Inline,
                stack = dedupe(key, 60_000),
                "spec deviation index insert failed (id={}): {e}",
                dev.id
            );
        }
        Ok(dev)
    }

    pub async fn deviations(&self, feature: &str) -> Result<Vec<SpecDeviation>, RuntimeError> {
        validate_feature(feature)?;
        super::read_jsonl(&self.deviations_path(feature)).await
    }

    pub async fn entries(&self, feature: &str) -> Result<Vec<SpecEntry>, RuntimeError> {
        validate_feature(feature)?;
        super::read_jsonl(&self.entries_path(feature)).await
    }
}

fn insert_entry(index: &AnchorIndex, entry: &SpecEntry) -> rusqlite::Result<()> {
    let conn = index.conn();
    conn.execute(
        "INSERT OR REPLACE INTO spec_entries (id, feature, phase, content, ts) VALUES (?, ?, ?, ?, ?)",
        rusqlite::params![
            entry.id.to_string(),
            entry.feature,
            entry.phase,
            entry.content,
            entry.ts.to_rfc3339(),
        ],
    )?;
    let rowid = conn.last_insert_rowid();
    conn.execute(
        "INSERT OR REPLACE INTO spec_entries_fts (rowid, content) VALUES (?, ?)",
        rusqlite::params![rowid, entry.content],
    )?;
    Ok(())
}

fn insert_deviation(index: &AnchorIndex, dev: &SpecDeviation) -> rusqlite::Result<()> {
    let conn = index.conn();
    conn.execute(
        "INSERT OR REPLACE INTO spec_deviations (id, feature, section, delta, reason, ts) VALUES (?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            dev.id.to_string(),
            dev.feature,
            dev.section,
            dev.delta,
            dev.reason,
            dev.ts.to_rfc3339(),
        ],
    )?;
    let rowid = conn.last_insert_rowid();
    conn.execute(
        "INSERT OR REPLACE INTO spec_deviations_fts (rowid, delta, reason) VALUES (?, ?, ?)",
        rusqlite::params![rowid, dev.delta, dev.reason],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecStatus {
    pub feature: String,
    pub phase: String,
    pub entry_count: usize,
    pub deviation_count: usize,
    pub design_revision: Option<String>,
    pub approved_design_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecMaterializeResult {
    pub path: PathBuf,
    pub revision: String,
    pub changed: bool,
}

impl SpecStore {
    pub async fn materialize(
        &self,
        feature: &str,
        expected_revision: Option<&str>,
    ) -> Result<SpecMaterializeResult, RuntimeError> {
        self.materialize_phase(feature, None, expected_revision)
            .await
    }

    pub async fn materialize_phase(
        &self,
        feature: &str,
        phase: Option<&str>,
        expected_revision: Option<&str>,
    ) -> Result<SpecMaterializeResult, RuntimeError> {
        validate_feature(feature)?;
        if let Some(phase) = phase
            && !PHASES.contains(&phase)
        {
            return Err(RuntimeError::ToolFailed(format!(
                "spec.materialize: unknown phase `{phase}`"
            )));
        }
        let entries: Vec<SpecEntry> = super::read_jsonl(&self.entries_path(feature)).await?;
        let deviations: Vec<SpecDeviation> =
            super::read_jsonl(&self.deviations_path(feature)).await?;
        let markdown = match phase {
            Some(phase) if phase != "implementation" => {
                render_phase_markdown(feature, phase, &entries)
            }
            _ => render_materialized_markdown(feature, &entries, &deviations),
        };
        let revision = revision_for(&markdown);
        let filename = match phase {
            Some(phase) if phase != "implementation" => format!("{phase}.md"),
            _ => "IMPLEMENTATION.md".into(),
        };
        let path = self.feature_dir(feature).join(filename);
        let existing = tokio::fs::read_to_string(&path).await.ok();
        if let Some(expected) = expected_revision
            && existing
                .as_deref()
                .is_some_and(|text| revision_for(text) != expected)
        {
            return Err(RuntimeError::ToolFailed(format!(
                "spec.materialize: revision conflict (expected {expected})"
            )));
        }
        if existing.as_deref() == Some(markdown.as_str()) {
            return Ok(SpecMaterializeResult {
                path,
                revision,
                changed: false,
            });
        }
        tokio::fs::create_dir_all(self.feature_dir(feature))
            .await
            .map_err(|e| {
                RuntimeError::ToolFailed(format!("spec.materialize: create feature dir: {e}"))
            })?;
        let tmp = path.with_extension("md.tmp");
        tokio::fs::write(&tmp, markdown.as_bytes())
            .await
            .map_err(|e| {
                RuntimeError::ToolFailed(format!("spec.materialize: write temp file: {e}"))
            })?;
        tokio::fs::rename(&tmp, &path).await.map_err(|e| {
            RuntimeError::ToolFailed(format!("spec.materialize: replace file: {e}"))
        })?;
        Ok(SpecMaterializeResult {
            path,
            revision,
            changed: true,
        })
    }
}

fn validate_feature(feature: &str) -> Result<(), RuntimeError> {
    if feature.is_empty()
        || feature == "."
        || feature == ".."
        || feature.contains('/')
        || feature.contains('\\')
        || feature.chars().any(char::is_control)
    {
        return Err(RuntimeError::ToolFailed(
            "spec feature must be a single directory name".into(),
        ));
    }
    Ok(())
}

fn render_phase_markdown(feature: &str, phase: &str, entries: &[SpecEntry]) -> String {
    let mut out = format!("# {phase} — {feature}\n\n");
    for entry in entries.iter().filter(|entry| entry.phase == phase) {
        out.push_str(&format!("## {}\n\n{}\n\n", entry.id.0, entry.content));
    }
    out
}

fn revision_for(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn render_materialized_markdown(
    feature: &str,
    entries: &[SpecEntry],
    deviations: &[SpecDeviation],
) -> String {
    let mut out = format!("# Implementation — {feature}\n\n");
    for entry in entries {
        out.push_str(&format!(
            "## {} — {}\n\n{}\n\n",
            entry.phase, entry.id.0, entry.content
        ));
    }
    if !deviations.is_empty() {
        out.push_str("## Deviations\n\n");
        for deviation in deviations {
            out.push_str(&format!(
                "- **{}**: {} — {}\n",
                deviation.section, deviation.delta, deviation.reason
            ));
        }
    }
    out
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
    async fn materialize_is_idempotent_and_detects_revision_conflicts() {
        let (s, dir) = store().await;
        s.update("x", "research", "notes".into()).await.unwrap();
        let first = s.materialize("x", None).await.unwrap();
        assert!(first.changed);
        assert!(first.path.exists());
        let second = s.materialize("x", Some(&first.revision)).await.unwrap();
        assert!(!second.changed);
        assert_eq!(first.revision, second.revision);
        tokio::fs::write(&first.path, "user edit\n").await.unwrap();
        let error = s.materialize("x", Some(&first.revision)).await.unwrap_err();
        assert!(error.to_string().contains("revision conflict"));
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("x/IMPLEMENTATION.md"))
                .await
                .unwrap(),
            "user edit\n"
        );
    }

    #[tokio::test]
    async fn materialize_phase_keeps_phase_documents_in_the_store() {
        let (s, dir) = store().await;
        s.update("x", "research", "Observed behavior".into())
            .await
            .unwrap();
        s.update("x", "design", "Chosen design".into())
            .await
            .unwrap();
        let research = s
            .materialize_phase("x", Some("research"), None)
            .await
            .unwrap();
        let design = s
            .materialize_phase("x", Some("design"), None)
            .await
            .unwrap();
        assert_eq!(research.path, dir.path().join("x/research.md"));
        assert_eq!(design.path, dir.path().join("x/design.md"));
        assert!(
            tokio::fs::read_to_string(&research.path)
                .await
                .unwrap()
                .contains("Observed behavior")
        );
        assert!(
            !tokio::fs::read_to_string(&research.path)
                .await
                .unwrap()
                .contains("Chosen design")
        );
        assert!(
            tokio::fs::read_to_string(&design.path)
                .await
                .unwrap()
                .contains("Chosen design")
        );
        assert_eq!(
            s.materialize_phase("x", Some("implementation"), None)
                .await
                .unwrap()
                .path,
            dir.path().join("x/IMPLEMENTATION.md")
        );
    }

    #[tokio::test]
    async fn feature_cannot_escape_spec_root() {
        let (s, _dir) = store().await;
        assert!(s.status("../outside").await.is_err());
        assert!(s.materialize("..", None).await.is_err());
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
    async fn approval_tracks_only_the_current_design_revision() {
        let (s, dir) = store().await;
        s.update("x", "research", "observations".into())
            .await
            .unwrap();
        s.update("x", "design", "first design".into())
            .await
            .unwrap();
        s.materialize_phase("x", Some("design"), Some(""))
            .await
            .unwrap();
        let first = s.status("x").await.unwrap().design_revision.unwrap();
        assert!(s.review("x", "stale", true).await.is_err());
        s.review("x", &first, true).await.unwrap();
        assert_eq!(
            s.status("x").await.unwrap().approved_design_revision,
            Some(first.clone())
        );
        s.update("x", "design", "changed design".into())
            .await
            .unwrap();
        assert_eq!(s.status("x").await.unwrap().approved_design_revision, None);
        s.materialize_phase("x", Some("design"), Some(&first))
            .await
            .unwrap();
        let second = s.status("x").await.unwrap().design_revision.unwrap();
        s.review("x", &second, false).await.unwrap();
        assert_eq!(s.status("x").await.unwrap().approved_design_revision, None);
        s.review("x", &second, true).await.unwrap();
        assert_eq!(
            s.status("x").await.unwrap().approved_design_revision,
            Some(second)
        );
        tokio::fs::write(dir.path().join("x/design.md"), "external edit")
            .await
            .unwrap();
        assert_eq!(s.status("x").await.unwrap().approved_design_revision, None);
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
    async fn update_and_deviate_dual_write_to_index() {
        let dir = tempfile::tempdir().unwrap();
        let index = std::sync::Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let s = SpecStore::new(dir.path().to_path_buf()).with_index(index.clone());
        s.update(
            "feat_x",
            "research",
            "supercalifragilistic research notes".into(),
        )
        .await
        .unwrap();
        s.update(
            "feat_x",
            "design",
            "midordermetamorphosis design notes".into(),
        )
        .await
        .unwrap();
        s.deviate(
            "feat_x",
            "sec".into(),
            "hyperloquacious delta text".into(),
            "quintessentialpolyphony reason text".into(),
        )
        .await
        .unwrap();

        let conn = index.conn();
        let entry_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM spec_entries",
                rusqlite::params![],
                |r| r.get(0),
            )
            .unwrap();
        let dev_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM spec_deviations",
                rusqlite::params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(entry_count, 2);
        assert_eq!(dev_count, 1);

        let entry_fts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM spec_entries_fts WHERE spec_entries_fts MATCH ?",
                rusqlite::params!["supercalifragilistic"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(entry_fts, 1);

        let dev_fts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM spec_deviations_fts WHERE spec_deviations_fts MATCH ?",
                rusqlite::params!["quintessentialpolyphony"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dev_fts, 1);
    }

    #[tokio::test]
    async fn unknown_phase_rejected() {
        let (s, _dir) = store().await;
        let err = s.update("x", "brainstorm", "n".into()).await.unwrap_err();
        assert!(format!("{err}").contains("unknown phase"));
    }
}
