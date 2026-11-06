//! Work-fold state shared by every flow document view.

use super::FlowViewModel;
use crate::app::OutputItem;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub(crate) struct WorkFoldState {
    pub(crate) start_id: u64,
    pub(crate) end_id: u64,
    pub(crate) member_count: usize,
    pub(crate) completed_steps: usize,
    pub(crate) total_steps: usize,
    pub(crate) title: String,
    pub(crate) stats: String,
    pub(crate) from_visible: f32,
    pub(crate) target_visible: f32,
    pub(crate) started_at: Instant,
    pub(crate) duration: Duration,
}

impl WorkFoldState {
    fn animation_duration(member_count: usize) -> Duration {
        Duration::from_millis((400 + member_count as u64 * 40).clamp(400, 700))
    }

    pub(crate) fn visibility(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return self.target_visible;
        }
        let progress = (now.saturating_duration_since(self.started_at).as_secs_f32()
            / self.duration.as_secs_f32())
        .clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        (self.from_visible + (self.target_visible - self.from_visible) * eased)
            .clamp(0.0, self.member_count as f32)
    }

    pub(crate) fn is_expanded(&self) -> bool {
        self.target_visible > 0.0
    }

    pub(crate) fn is_animating(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) < self.duration
            && (self.from_visible - self.target_visible).abs() > f32::EPSILON
    }

    pub(crate) fn toggle(&mut self, now: Instant) {
        let current = self.visibility(now);
        self.from_visible = current;
        self.target_visible = if self.is_expanded() {
            0.0
        } else {
            self.member_count as f32
        };
        self.started_at = now;
        self.duration = Self::animation_duration(self.member_count);
    }
}

impl FlowViewModel {
    #[cfg(test)]
    pub(crate) fn restore_messages(
        &mut self,
        revision: crate::app::OutputRevision,
        messages: &[atman_runtime::message::Message],
        output_store: Option<&atman_runtime::tools::tool_output::OutputStore>,
    ) {
        if self.document.source_revision != Some((revision.id, revision.semantic)) {
            self.restore_prepared_messages(crate::history::PreparedMessages::capture(
                revision,
                messages,
                output_store,
            ));
        }
    }

    pub(crate) fn restore_prepared_messages(
        &mut self,
        prepared: crate::history::PreparedMessages,
    ) -> (Vec<OutputItem>, Vec<std::sync::Arc<OutputItem>>) {
        let crate::history::PreparedMessages {
            revision,
            items,
            final_keys,
        } = prepared;
        if self.document.source_revision == Some((revision.id, revision.semantic)) {
            return (items, Vec::new());
        }
        let fold_states = self
            .work_folds
            .iter()
            .map(|fold| (fold.start_id, fold.is_expanded()))
            .collect::<HashMap<_, _>>();
        let expanded = self
            .legacy_fold_bindings
            .iter()
            .filter_map(|(key, id)| fold_states.get(id).map(|expanded| (key.clone(), *expanded)))
            .collect::<HashMap<_, _>>();
        let retired = self.document.restore_items(revision, items);
        self.work_folds.clear();
        self.legacy_fold_bindings.clear();
        let mut keys = final_keys.into_iter();
        let mut start = 0;
        let mut markers = Vec::new();
        for (index, item) in self.document.iter().enumerate() {
            match item {
                OutputItem::UserTurn { .. } => start = index + 1,
                OutputItem::WorkFoldMarker {
                    start_index,
                    summary,
                } => {
                    if let Some(key) = keys.next() {
                        markers.push((key, start_index.unwrap_or(start), index, summary.clone()));
                    }
                    start = index + 2;
                }
                _ => {}
            }
        }
        for (key, start, end, summary) in markers {
            if let Some(id) = self.upsert_work_fold(start..end, summary.as_deref(), true) {
                if expanded.get(&key) == Some(&true)
                    && let Some(fold) = self.work_folds.last_mut()
                {
                    fold.toggle(Instant::now());
                }
                self.legacy_fold_bindings.insert(key, id);
            }
        }
        retired
    }

    pub(crate) fn upsert_work_fold(
        &mut self,
        work: std::ops::Range<usize>,
        summary: Option<&str>,
        restored: bool,
    ) -> Option<u64> {
        let start_index = work.start;
        let end_index = work.end.checked_sub(1)?;
        if start_index > end_index {
            return None;
        }
        let start_id = self.document.revisions().get(start_index)?.id;
        let end_id = self.document.revisions().get(end_index)?.id;
        let member_count = end_index - start_index + 1;
        let (completed_steps, total_steps, title, stats) =
            self.work_fold_metadata(start_index, end_index, summary);
        if let Some(fold) = self
            .work_folds
            .iter_mut()
            .find(|fold| fold.start_id == start_id)
        {
            // Publication can replace preview rows with a different member count.
            // Preserve the transition's progress and deadline across that handoff.
            let scale = member_count as f32 / fold.member_count as f32;
            fold.from_visible *= scale;
            fold.target_visible *= scale;
            fold.end_id = end_id;
            fold.member_count = member_count;
            fold.completed_steps = completed_steps;
            fold.total_steps = total_steps;
            fold.title = title;
            fold.stats = stats;
            return Some(start_id);
        }
        self.work_folds.push(WorkFoldState {
            start_id,
            end_id,
            member_count,
            completed_steps,
            total_steps,
            title,
            stats,
            from_visible: if restored { 0.0 } else { member_count as f32 },
            target_visible: 0.0,
            started_at: Instant::now(),
            duration: if restored {
                Duration::ZERO
            } else {
                WorkFoldState::animation_duration(member_count)
            },
        });
        Some(start_id)
    }

    fn work_fold_metadata(
        &self,
        start_index: usize,
        end_index: usize,
        summary: Option<&str>,
    ) -> (usize, usize, String, String) {
        let mut call_ids = HashSet::new();
        let mut intents = Vec::new();
        let mut files = HashSet::new();
        let mut insertions = 0usize;
        let mut deletions = 0usize;
        for (offset, item) in self
            .document
            .iter()
            .skip(start_index)
            .take(end_index - start_index + 1)
            .enumerate()
        {
            let OutputItem::ToolDispatch { calls } = item else {
                continue;
            };
            for call in calls {
                call_ids.insert((offset, call.id.clone()));
                if !call.intent.trim().is_empty()
                    && call.intent != call.tool
                    && !intents.iter().any(|intent| intent == &call.intent)
                    && intents.len() < 3
                {
                    intents.push(call.intent.clone());
                }
                if let Some((path, metrics)) = &call.applied_edit {
                    files.insert(path.clone());
                    insertions = insertions.saturating_add(metrics.insertions);
                    deletions = deletions.saturating_add(metrics.deletions);
                }
            }
        }
        let title = atman_runtime::message::ToolCallIntent::new(summary.unwrap_or_default())
            .map(|summary| summary.as_str().to_owned())
            .unwrap_or_else(|| {
                if intents.is_empty() {
                    "Completed internal work.".to_owned()
                } else {
                    intents.join(" · ")
                }
            });
        let total_steps = call_ids.len().max(1);
        let file_count = files.len();
        let mut stats = Vec::new();
        if file_count > 0 {
            stats.push(format!("{file_count} files"));
        }
        if insertions > 0 || deletions > 0 {
            stats.push(format!("+{insertions} −{deletions}"));
        }
        (total_steps, total_steps, title, stats.join(" · "))
    }

    pub fn toggle_work_fold(&mut self, group_key: &str) -> bool {
        let Some(id) = group_key
            .strip_prefix(crate::output::WORK_FOLD_REGION_PREFIX)
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return false;
        };
        let Some(fold) = self.work_folds.iter_mut().find(|fold| fold.start_id == id) else {
            return false;
        };
        fold.toggle(Instant::now());
        true
    }

    pub fn toggle_latest_work_fold(&mut self) -> bool {
        let Some(fold) = self.work_folds.last_mut() else {
            return false;
        };
        fold.toggle(Instant::now());
        true
    }

    pub(crate) fn collapse_work_fold(&mut self, start_id: Option<u64>) -> bool {
        let Some(start_id) = start_id else {
            return false;
        };
        let Some(fold) = self
            .work_folds
            .iter_mut()
            .find(|fold| fold.start_id == start_id)
        else {
            return false;
        };
        if !fold.is_expanded() {
            return false;
        }
        fold.toggle(Instant::now());
        true
    }

    pub(crate) fn work_fold_projections(
        &self,
        hovered: Option<&str>,
    ) -> Vec<crate::output::WorkFoldProjection> {
        let now = Instant::now();
        self.work_folds
            .iter()
            .filter_map(|fold| {
                let start_index = self.document.index_by_id(fold.start_id)?;
                let end_index = self.document.index_by_id(fold.end_id)?;
                let visibility = fold.visibility(now);
                let visible_members = visibility.ceil() as usize;
                let fraction = visibility.fract();
                Some(crate::output::WorkFoldProjection {
                    key: fold.start_id,
                    start_index,
                    end_index,
                    visible_members,
                    total_members: fold.member_count,
                    boundary_member: (fraction > f32::EPSILON)
                        .then(|| visible_members.saturating_sub(1)),
                    boundary_level: (fraction * 3.0).ceil().clamp(1.0, 3.0) as u8,
                    completed_steps: fold.completed_steps,
                    total_steps: fold.total_steps,
                    expanded: fold.is_expanded(),
                    animating: fold.is_animating(now),
                    hovered: hovered.is_some_and(|key| {
                        key == format!(
                            "{}{}",
                            crate::output::WORK_FOLD_REGION_PREFIX,
                            fold.start_id
                        )
                    }),
                    title: fold.title.clone(),
                    stats: fold.stats.clone(),
                })
            })
            .collect()
    }
}
