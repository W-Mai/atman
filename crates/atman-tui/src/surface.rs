//! Shared filled chrome. Content diagrams retain their own strokes.
use ratatui::{
    style::{Color, Style},
    widgets::Block,
};

#[derive(Clone, Copy)]
pub(crate) enum Role {
    Composer,
    Context,
    Overlay,
    Notice,
    ProjectRail,
}

impl Role {
    pub fn background(self) -> Color {
        let t = crate::theme::theme();
        match self {
            Self::Composer => t.panel_bg.into(),
            Self::Context => t.work_detail_bg.into(),
            Self::Overlay => crate::wm::shell::surface_bg(&t),
            Self::Notice => t.note_info_bg.into(),
            Self::ProjectRail => t.work_bg.into(),
        }
    }
}

pub(crate) fn block<'a>(role: Role) -> Block<'a> {
    Block::default().style(
        Style::default()
            .bg(role.background())
            .fg(crate::theme::theme().tinted_fg.into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_surfaces_fill_cells_without_decorative_strokes() {
        for role in [
            Role::Composer,
            Role::Context,
            Role::Overlay,
            Role::Notice,
            Role::ProjectRail,
        ] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 5)).unwrap();
            terminal
                .draw(|frame| frame.render_widget(block(role), frame.area()))
                .unwrap();
            for cell in &terminal.backend().buffer().content {
                assert_eq!(cell.bg, role.background());
                assert_eq!(cell.symbol(), " ");
            }
        }
    }
}
