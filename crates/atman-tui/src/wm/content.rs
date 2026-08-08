use ratatui::Frame;
use ratatui::layout::Rect;

use atman_runtime::TaskSnapshot;

use crate::app::OutputItem;
use crate::task_panel::ActivityNode;

use crate::wm::WindowId;

use super::{PanelBtn, WindowInstance, WmHitmap};

#[allow(clippy::too_many_arguments)]
pub fn render_panel_content(
    f: &mut Frame,
    area: Rect,
    panel: &mut WindowInstance,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
    activity_nodes: &[ActivityNode],
    hovered_btn: &Option<(WindowId, PanelBtn)>,
    hovered_history_row: &Option<String>,
    hitmap_out: &mut WmHitmap,
    animation_frame: u32,
    is_focused: bool,
    modal_open: bool,
    mcp_servers: &[atman_runtime::mcp::McpServerStatus],
    expanded_mcp_servers: &std::collections::HashSet<String>,
    mcp_selected: usize,
    hovered_mcp_row: &Option<String>,
    mcp_browser: &crate::mcp_manager::McpBrowserState<'_>,
    items_version: u64,
    expanded_version: u64,
) {
    let _hovered_btn = hovered_btn;
    if area.height == 0 || area.width == 0 {
        return;
    }

    if let Some(ref mut content) = panel.content {
        if !is_focused && modal_open && !content.wants_background_updates() {
            return;
        }
        panel.content_version = content.content_version();
        content.sync_state(panel.scroll, panel.h_scroll, panel.split);
        let regions = content.render_content(
            area,
            f,
            &crate::wm::RenderCtx {
                window_id: panel.id,
                snapshots,
                items,
                animation_frame,
                expanded_tools: &panel.expanded_tools,
                activity_nodes,
                items_version,
                expanded_version,
                mcp_servers,
                expanded_mcp_servers,
                mcp_selected,
                hovered_mcp_row,
                mcp_browser,
                hovered_history_row,
            },
        );
        let (s, hs, sp) = content.extract_state();
        panel.scroll = s;
        panel.h_scroll = hs;
        panel.split = sp;
        for region in regions {
            match region.target {
                crate::wm::component::HitTarget::HistoryRow(s) => hitmap_out
                    .history_row_rects
                    .push((panel.id, s, region.rect)),
                crate::wm::component::HitTarget::WorkflowNode(i, s) => hitmap_out
                    .workflow_node_rects
                    .push((panel.id, i, s, region.rect)),
                crate::wm::component::HitTarget::McpRow(s) => {
                    hitmap_out.mcp_row_rects.push((panel.id, s, region.rect))
                }
                crate::wm::component::HitTarget::ToolHeader(s) => hitmap_out
                    .tool_header_rects
                    .push((panel.id, s, region.rect)),
                _ => {}
            }
        }
    }
}
