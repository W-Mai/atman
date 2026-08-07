use ratatui::Frame;
use ratatui::layout::Rect;

use atman_runtime::TaskSnapshot;

use crate::app::OutputItem;
use crate::task_panel::ActivityNode;

use super::{PanelBtn, WindowInstance, WmHitmap};

#[allow(clippy::too_many_arguments)]
pub fn render_panel_content(
    f: &mut Frame,
    area: Rect,
    panel: &mut WindowInstance,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
    activity_nodes: &[ActivityNode],
    hovered_btn: &Option<(String, PanelBtn)>,
    _hovered_history_row: &Option<String>,
    hitmap_out: &mut WmHitmap,
    animation_frame: u32,
    _mcp_servers: &[atman_runtime::mcp::McpServerStatus],
    _expanded_mcp_servers: &std::collections::HashSet<String>,
    _mcp_selected: usize,
    _hovered_mcp_row: &Option<String>,
    _mcp_browser: &crate::mcp_manager::McpBrowserState<'_>,
    items_version: u64,
    expanded_version: u64,
) {
    let _hovered_btn = hovered_btn;
    if area.height == 0 || area.width == 0 {
        return;
    }

    if let Some(ref mut content) = panel.content {
        let regions = content.render_content(
            area,
            f,
            &crate::wm::RenderCtx {
                snapshots,
                items,
                animation_frame,
                panel_width: area.width,
                expanded_tools: &panel.expanded_tools,
                activity_nodes,
                items_version,
                expanded_version,
            },
        );
        for region in regions {
            match region.target {
                crate::wm::component::HitTarget::HistoryRow(s) => {
                    hitmap_out.history_row_rects.push((s, region.rect))
                }
                crate::wm::component::HitTarget::WorkflowNode(i, s) => {
                    hitmap_out.workflow_node_rects.push((i, s, region.rect))
                }
                crate::wm::component::HitTarget::McpRow(s) => {
                    hitmap_out.mcp_row_rects.push((s, region.rect))
                }
                _ => {}
            }
        }
    }
}
