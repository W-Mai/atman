//! Large-session production-path baselines.
//!
//! Run in release mode with output enabled:
//! `cargo test -p atman-tui --release --test large_session_performance -- --ignored --nocapture`

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use atman_runtime::event::{FlowRunId, TurnId};
use atman_runtime::permission::PermissionRequestId;
use atman_runtime::permission_audit::{
    PermissionAuditTarget, PermissionPolicyReference, PermissionRequestAudit,
};
use atman_runtime::workflow::{
    NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
    WorkflowPermissionIdentity, WorkflowPermissionRequest, WorkflowPermissionState,
};
use atman_tui::app::{NoteLevel, OutputItem, OutputStore};
use atman_tui::output::{LayoutCache, LayoutKey, LayoutRequest, RenderCtx};

struct LayoutBaseline {
    cold: Duration,
    warm: Duration,
    animated: Duration,
    total_rows: u32,
}

fn measure_layout(items: &[OutputItem]) -> LayoutBaseline {
    let items = OutputStore::from(items.to_vec());
    let expanded_tools = HashSet::new();
    let mut cache = LayoutCache::default();
    let mut ctx = RenderCtx {
        expanded_tools: &expanded_tools,
        messages: &[],
        animation_frame: 0,
        panel_width: 120,
        hovered_thinking_idx: None,
        hovered_output_node: None,
    };
    let cold_key = LayoutKey {
        width: 120,
        theme: atman_tui::theme::current_mode(),
    };
    let request = LayoutRequest {
        scroll_offset: 0,
        viewport_rows: 40,
        follow_tail_rows: None,
    };
    let started = Instant::now();
    let metrics = cache.update_dirty(cold_key, &items, &ctx, request);
    let _ = cache.visible_slice(metrics.scroll_offset, request.viewport_rows, 0);
    let cold = started.elapsed();

    let started = Instant::now();
    let metrics = cache.update_dirty(cold_key, &items, &ctx, request);
    let _ = cache.visible_slice(metrics.scroll_offset, request.viewport_rows, 0);
    let warm = started.elapsed();

    ctx.animation_frame = 1;
    let started = Instant::now();
    let metrics = cache.update_dirty(cold_key, &items, &ctx, request);
    let _ = cache.visible_slice(metrics.scroll_offset, request.viewport_rows, 1);
    let animated = started.elapsed();
    LayoutBaseline {
        cold,
        warm,
        animated,
        total_rows: metrics.total_rows,
    }
}

fn tool_node(run_id: &FlowRunId, idx: usize, at: chrono::DateTime<chrono::Utc>) -> WorkflowNode {
    WorkflowNode {
        id: format!("tool:{run_id}:tool-{idx}"),
        kind: WorkflowNodeKind::ToolCall {
            tool_use_id: format!("tool-{idx}"),
            tool: "fs.read".into(),
            args_preview: format!("{{\"path\":\"src/{idx}.rs\"}}"),
            call_intent: None,
            result_preview: None,
        },
        label: "fs.read".into(),
        status: if idx == 0 {
            NodeStatus::Running
        } else {
            NodeStatus::Ok
        },
        started_at: Some(at),
        ended_at: (idx != 0).then_some(at),
        output_preview: None,
        children: Vec::new(),
        parallelism: Parallelism::Serial,
        approval: None,
        llm_stats: None,
    }
}

fn permission_request(
    request_id: PermissionRequestId,
    run_id: &FlowRunId,
    idx: usize,
    at: chrono::DateTime<chrono::Utc>,
) -> PermissionRequestAudit {
    PermissionRequestAudit {
        request_id: Some(request_id),
        revision: 1,
        session_id: "large-session-baseline".into(),
        requesting_run_id: run_id.clone(),
        parent_run_id: None,
        root_run_id: run_id.clone(),
        tool_use_id: format!("tool-{idx}"),
        tool: "fs.read".into(),
        call_intent: None,
        tier: atman_runtime::Tier::Two,
        execution_boundary: None,
        provenance: Default::default(),
        target: PermissionAuditTarget::User,
        group_ids: Vec::new(),
        policy: PermissionPolicyReference {
            snapshot_id: "baseline".into(),
            rule_id: "baseline".into(),
        },
        escalation_path: Vec::new(),
        decision_id: None,
        actor: None,
        scope: None,
        reason: None,
        at,
    }
}

fn large_workflow(node_count: usize, permission_count: usize) -> OutputItem {
    let run_id = FlowRunId(uuid::Uuid::from_u128(1));
    let at = chrono::Utc::now();
    let root = WorkflowNode {
        id: run_id.to_string(),
        kind: WorkflowNodeKind::Flow {
            run_id: run_id.to_string(),
            flow_name: "large-session-baseline".into(),
        },
        label: "large-session-baseline".into(),
        status: NodeStatus::Running,
        started_at: Some(at),
        ended_at: None,
        output_preview: None,
        children: (0..node_count)
            .map(|idx| tool_node(&run_id, idx, at))
            .collect(),
        parallelism: Parallelism::Serial,
        approval: None,
        llm_stats: None,
    };
    let permission_requests = (0..permission_count)
        .map(|idx| {
            let request_id = PermissionRequestId(uuid::Uuid::from_u128(idx as u128 + 2));
            (
                WorkflowPermissionIdentity::Canonical {
                    request_id: request_id.clone(),
                },
                WorkflowPermissionRequest {
                    payload: permission_request(request_id, &run_id, idx, at),
                    state: WorkflowPermissionState::Pending,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    OutputItem::WorkflowPanel {
        turn_index: 0,
        graph: WorkflowGraph {
            turn_id: TurnId::now(),
            root: vec![root],
            permission_requests,
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        }
        .into(),
        expanded_nodes: HashSet::new(),
        panel_expanded: false,
        started_at: Instant::now(),
        ended_at: None,
        cancelled: false,
    }
}

#[test]
#[ignore = "large release-mode layout baseline"]
fn baseline_one_hundred_thousand_output_entries() {
    let items = (0..100_000)
        .map(|idx| OutputItem::SystemNote {
            text: format!("synthetic event {idx}"),
            level: NoteLevel::Info,
        })
        .collect::<Vec<_>>();
    let baseline = measure_layout(&items);
    assert!(baseline.total_rows >= items.len() as u32);
    eprintln!(
        "output baseline: items={} rows={} cold_ms={} warm_ms={} animated_ms={}",
        items.len(),
        baseline.total_rows,
        baseline.cold.as_millis(),
        baseline.warm.as_millis(),
        baseline.animated.as_millis()
    );
}

#[test]
#[ignore = "large release-mode workflow and permission baseline"]
fn baseline_fifty_thousand_nodes_and_one_hundred_thousand_permissions() {
    let items = vec![large_workflow(50_000, 100_000)];
    let baseline = measure_layout(&items);
    assert!(baseline.total_rows > 0);
    eprintln!(
        "workflow baseline: nodes=50000 permissions=100000 rows={} cold_ms={} warm_ms={} animated_ms={}",
        baseline.total_rows,
        baseline.cold.as_millis(),
        baseline.warm.as_millis(),
        baseline.animated.as_millis()
    );
}

#[test]
#[ignore = "release-mode incident session baseline; requires ATMAN_INCIDENT_SESSION"]
fn incident_session_animation_stays_within_frame_budget() {
    let path = PathBuf::from(
        std::env::var_os("ATMAN_INCIDENT_SESSION")
            .expect("ATMAN_INCIDENT_SESSION must point to an events.jsonl file"),
    );
    let started = Instant::now();
    let entries = atman_runtime::replay_transcript_from(&path).expect("replay incident session");
    let replay = started.elapsed();
    assert!(!entries.is_empty());

    let started = Instant::now();
    let output = atman_tui::history::flatten_transcript(&entries);
    let flatten = started.elapsed();
    let item_count = output.len();
    assert!(item_count > 0);

    let items = OutputStore::from(output);
    let expanded_tools = HashSet::new();
    let mut cache = LayoutCache::default();
    let mut ctx = RenderCtx {
        expanded_tools: &expanded_tools,
        messages: &[],
        animation_frame: 0,
        panel_width: 120,
        hovered_thinking_idx: None,
        hovered_output_node: None,
    };
    let key = LayoutKey {
        width: 120,
        theme: atman_tui::theme::current_mode(),
    };
    let request = LayoutRequest {
        scroll_offset: 0,
        viewport_rows: 40,
        follow_tail_rows: Some(40),
    };
    let started = Instant::now();
    let metrics = cache.update_dirty(key, &items, &ctx, request);
    let _ = cache.visible_slice(metrics.scroll_offset, request.viewport_rows, 0);
    let cold_layout = started.elapsed();

    const TICKS: u32 = 600;
    let started = Instant::now();
    for frame in 1..=TICKS {
        ctx.animation_frame = frame;
        let metrics = cache.update_dirty(key, &items, &ctx, request);
        let _ = cache.visible_slice(
            metrics.scroll_offset,
            request.viewport_rows,
            ctx.animation_frame,
        );
    }
    let animation = started.elapsed();
    assert!(
        animation < Duration::from_millis(25).saturating_mul(TICKS),
        "animation exceeded the 25 ms per-frame CPU budget: {animation:?} for {TICKS} frames"
    );
    eprintln!(
        "incident session: entries={} items={} rows={} replay_ms={} flatten_ms={} cold_layout_ms={} animation_frames={} animation_ms={} animation_us_per_frame={}",
        entries.len(),
        item_count,
        metrics.total_rows,
        replay.as_millis(),
        flatten.as_millis(),
        cold_layout.as_millis(),
        TICKS,
        animation.as_millis(),
        animation.as_micros() / u128::from(TICKS),
    );
}
