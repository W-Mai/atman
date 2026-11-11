//! Sparse view-local disclosure metadata; retained branches never cache document payloads.

use std::collections::{HashMap, HashSet};

use atman_runtime::projection::flow::FlowRecordId;

use super::{FlowViewModel, OutputMutation};
use crate::app::{Disclosure, OutputItem};

#[derive(Clone)]
pub(super) enum ItemPresentation {
    Thinking(Disclosure),
    Compaction(Disclosure),
    Tools(HashMap<String, Disclosure>),
    Workflow {
        presentation: crate::workflow::Presentation,
        expanded: HashSet<String>,
        panel: bool,
    },
    Diff(bool),
    File(bool),
}

impl ItemPresentation {
    pub(super) fn capture(item: &OutputItem) -> Option<Self> {
        Some(match item {
            OutputItem::Thinking { disclosure, .. } => Self::Thinking(*disclosure),
            OutputItem::CompactionSummary { disclosure, .. } => Self::Compaction(*disclosure),
            OutputItem::ToolDispatch { calls } => Self::Tools(
                calls
                    .iter()
                    .map(|call| (call.id.clone(), call.disclosure))
                    .collect(),
            ),
            OutputItem::WorkflowPanel {
                presentation,
                expanded_nodes,
                panel_expanded,
                ..
            } => Self::Workflow {
                presentation: presentation.clone(),
                expanded: expanded_nodes.clone(),
                panel: *panel_expanded,
            },
            OutputItem::DiffPreview { expanded, .. } => Self::Diff(*expanded),
            OutputItem::FsDetail { expanded, .. } => Self::File(*expanded),
            _ => return None,
        })
    }

    fn apply(self, item: &mut OutputItem) -> bool {
        match (self, item) {
            (Self::Thinking(saved), OutputItem::Thinking { disclosure, .. })
            | (Self::Compaction(saved), OutputItem::CompactionSummary { disclosure, .. }) => {
                *disclosure = saved
            }
            (Self::Tools(saved), OutputItem::ToolDispatch { calls }) => {
                for call in calls {
                    if let Some(disclosure) = saved.get(&call.id) {
                        call.disclosure = *disclosure;
                    }
                }
            }
            (
                Self::Workflow {
                    presentation: saved,
                    expanded,
                    panel,
                },
                OutputItem::WorkflowPanel {
                    presentation,
                    expanded_nodes,
                    panel_expanded,
                    ..
                },
            ) => {
                *presentation = saved;
                *expanded_nodes = expanded;
                *panel_expanded = panel;
            }
            (Self::Diff(saved), OutputItem::DiffPreview { expanded, .. })
            | (Self::File(saved), OutputItem::FsDetail { expanded, .. }) => *expanded = saved,
            _ => return false,
        }
        true
    }
}

#[derive(Default)]
pub(super) struct BranchPresentation {
    items: HashMap<usize, (FlowRecordId, ItemPresentation)>,
    folds: HashMap<FlowRecordId, bool>,
}

impl FlowViewModel {
    pub(super) fn retain_branch_presentation(&mut self, destination: &mut Self) {
        let Some(previous) = self.instance.head.as_ref().map(|head| head.branch) else {
            return;
        };
        let Some(next) = destination.instance.head.as_ref().map(|head| head.branch) else {
            return;
        };
        let items = std::mem::take(&mut self.document.presentations)
            .into_iter()
            .filter_map(|(index, (id, presentation))| {
                (self.document.revisions().get(index)?.id == id).then_some(())?;
                Some((index, (self.source_record(index)?, presentation)))
            })
            .collect();
        let folds = std::mem::take(&mut self.work_disclosures)
            .into_iter()
            .filter_map(|(id, expanded)| Some((*self.instance.records.get(&id)?, expanded)))
            .collect();
        self.history_presentations
            .insert(previous, BranchPresentation { items, folds });
        destination.history_presentations = std::mem::take(&mut self.history_presentations);
        let Some(saved) = destination.history_presentations.remove(&next) else {
            return;
        };
        for (index, (record, presentation)) in saved.items {
            if destination.source_record(index) == Some(record) {
                destination
                    .document
                    .mutate(index, OutputMutation::Interaction, |item| {
                        presentation.apply(item)
                    });
            }
        }
        for fold in &mut destination.work_folds {
            if let Some(expanded) = destination
                .instance
                .records
                .get(&fold.start_id)
                .and_then(|record| saved.folds.get(record))
            {
                fold.target_visible = if *expanded {
                    fold.member_count as f32
                } else {
                    0.0
                };
                fold.from_visible = fold.target_visible;
                fold.duration = std::time::Duration::ZERO;
                destination
                    .work_disclosures
                    .insert(fold.start_id, *expanded);
            }
        }
    }
}
