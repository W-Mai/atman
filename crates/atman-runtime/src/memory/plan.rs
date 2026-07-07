use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::RuntimeError;
use crate::memory::{append_jsonl, read_jsonl};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanStep {
    pub index: usize,
    pub text: String,
    #[serde(default)]
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plan {
    pub id: String,
    pub title: String,
    pub steps: Vec<PlanStep>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Plan {
    pub fn new(id: impl Into<String>, title: impl Into<String>, steps: Vec<String>) -> Self {
        let now = Utc::now();
        Self {
            id: id.into(),
            title: title.into(),
            steps: steps
                .into_iter()
                .enumerate()
                .map(|(i, text)| PlanStep {
                    index: i,
                    text,
                    done: false,
                    done_at: None,
                })
                .collect(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn progress(&self) -> (usize, usize) {
        let done = self.steps.iter().filter(|s| s.done).count();
        (done, self.steps.len())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum PlanEntry {
    Upsert(Plan),
    Tick {
        plan_id: String,
        step_index: usize,
        at: DateTime<Utc>,
    },
}

pub struct PlanStore {
    path: PathBuf,
}

impl PlanStore {
    pub fn at(session_dir: impl AsRef<Path>) -> Self {
        Self {
            path: session_dir.as_ref().join("plans.jsonl"),
        }
    }

    pub async fn upsert(&self, plan: Plan) -> Result<(), RuntimeError> {
        let mut plan = plan;
        plan.updated_at = Utc::now();
        append_jsonl(&self.path, &PlanEntry::Upsert(plan)).await
    }

    pub async fn tick(&self, plan_id: &str, step_index: usize) -> Result<(), RuntimeError> {
        append_jsonl(
            &self.path,
            &PlanEntry::Tick {
                plan_id: plan_id.into(),
                step_index,
                at: Utc::now(),
            },
        )
        .await
    }

    pub async fn list(&self) -> Result<Vec<Plan>, RuntimeError> {
        let entries: Vec<PlanEntry> = read_jsonl(&self.path).await?;
        let mut plans: Vec<Plan> = Vec::new();
        for entry in entries {
            match entry {
                PlanEntry::Upsert(mut p) => {
                    if let Some(idx) = plans.iter().position(|x| x.id == p.id) {
                        let old = &plans[idx];
                        p.created_at = old.created_at;
                        plans[idx] = p;
                    } else {
                        plans.push(p);
                    }
                }
                PlanEntry::Tick {
                    plan_id,
                    step_index,
                    at,
                } => {
                    if let Some(p) = plans.iter_mut().find(|p| p.id == plan_id)
                        && let Some(step) = p.steps.iter_mut().find(|s| s.index == step_index)
                    {
                        step.done = true;
                        step.done_at = Some(at);
                        p.updated_at = at;
                    }
                }
            }
        }
        Ok(plans)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Plan>, RuntimeError> {
        Ok(self.list().await?.into_iter().find(|p| p.id == id))
    }

    pub async fn latest(&self) -> Result<Option<Plan>, RuntimeError> {
        Ok(self.list().await?.into_iter().max_by_key(|p| p.updated_at))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample(id: &str, title: &str, steps: &[&str]) -> Plan {
        Plan::new(id, title, steps.iter().map(|s| s.to_string()).collect())
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips() {
        let dir = TempDir::new().unwrap();
        let store = PlanStore::at(dir.path());
        let plan = sample("p1", "ship endurance", &["design", "implement", "ship"]);
        store.upsert(plan.clone()).await.unwrap();
        let fetched = store.get("p1").await.unwrap().unwrap();
        assert_eq!(fetched.title, "ship endurance");
        assert_eq!(fetched.steps.len(), 3);
    }

    #[tokio::test]
    async fn latest_returns_most_recent_by_updated_at() {
        let dir = TempDir::new().unwrap();
        let store = PlanStore::at(dir.path());
        store.upsert(sample("p1", "first", &["a"])).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        store.upsert(sample("p2", "second", &["b"])).await.unwrap();
        assert_eq!(store.latest().await.unwrap().unwrap().id, "p2");
    }

    #[tokio::test]
    async fn tick_flips_step_done() {
        let dir = TempDir::new().unwrap();
        let store = PlanStore::at(dir.path());
        store
            .upsert(sample("p1", "t", &["a", "b", "c"]))
            .await
            .unwrap();
        store.tick("p1", 1).await.unwrap();
        let plan = store.get("p1").await.unwrap().unwrap();
        assert!(!plan.steps[0].done);
        assert!(plan.steps[1].done);
        assert!(!plan.steps[2].done);
        assert_eq!(plan.progress(), (1, 3));
    }

    #[tokio::test]
    async fn upsert_replaces_by_id_preserving_created_at() {
        let dir = TempDir::new().unwrap();
        let store = PlanStore::at(dir.path());
        let mut plan = sample("p1", "v1", &["a"]);
        store.upsert(plan.clone()).await.unwrap();
        let original_created = store.get("p1").await.unwrap().unwrap().created_at;
        plan.title = "v2".into();
        plan.steps.push(PlanStep {
            index: 1,
            text: "b".into(),
            done: false,
            done_at: None,
        });
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        store.upsert(plan).await.unwrap();
        let updated = store.get("p1").await.unwrap().unwrap();
        assert_eq!(updated.title, "v2");
        assert_eq!(updated.steps.len(), 2);
        assert_eq!(updated.created_at, original_created);
    }
}
