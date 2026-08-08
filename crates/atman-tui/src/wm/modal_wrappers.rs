use crate::app::AppState;
use crate::wm::component::{EventCtx, RenderCtx, WmEvent, WmEventResult};
use crate::wm::ModalComponent;
use ratatui::Frame;
use ratatui::layout::Rect;

macro_rules! modal_wrapper {
    ($name:ident, $render:expr) => {
        pub struct $name<'a> {
            pub app: &'a mut AppState,
        }

        impl ModalComponent for $name<'_> {
            fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &RenderCtx) {
                ($render)(self.app, frame, area);
            }

            fn handle_event(
                &mut self,
                _event: &WmEvent,
                _ctx: &mut EventCtx,
            ) -> WmEventResult {
                WmEventResult::Consumed(Vec::new())
            }

            fn preferred_rect(&self, viewport: Rect) -> Rect {
                viewport
            }
        }
    };
}

modal_wrapper!(FormModalWrapper, |app: &mut AppState,
                                  frame: &mut Frame,
                                  area: Rect| {
    crate::form_modal::render(frame, area, &app.form_modal);
});

modal_wrapper!(CompactReviewWrapper, |app: &mut AppState,
                                      frame: &mut Frame,
                                      area: Rect| {
    if let Some(modal) = &app.compact_review {
        crate::compact_review_modal::render(frame, area, modal);
    }
});

modal_wrapper!(SessionSwitcherWrapper, |app: &mut AppState,
                                       frame: &mut Frame,
                                       area: Rect| {
    crate::session_switcher::render(frame, area, &app.session_switcher);
});

modal_wrapper!(HistorySearchWrapper, |app: &mut AppState,
                                    frame: &mut Frame,
                                    area: Rect| {
    crate::history_search_modal::render(frame, area, &mut app.history_search);
});

modal_wrapper!(ProviderManagerWrapper, |app: &mut AppState,
                                      frame: &mut Frame,
                                      area: Rect| {
    crate::provider_manager::render(frame, area, &app.provider_manager);
});

modal_wrapper!(AliasManagerWrapper, |app: &mut AppState,
                                   frame: &mut Frame,
                                   area: Rect| {
    crate::alias_manager::render(frame, area, &app.alias_manager);
});

modal_wrapper!(ModelPickerWrapper, |app: &mut AppState,
                                  frame: &mut Frame,
                                  area: Rect| {
    crate::model_picker::render(frame, area, &app.model_picker, &app.context.model);
});

modal_wrapper!(OnboardingWrapper, |app: &mut AppState,
                                frame: &mut Frame,
                                area: Rect| {
    crate::onboarding::render(frame, area, &app.onboarding);
});

modal_wrapper!(PaletteWrapper, |app: &mut AppState,
                             frame: &mut Frame,
                             area: Rect| {
    crate::palette::render(frame, area, &app.palette);
});

modal_wrapper!(ThemePickerWrapper, |app: &mut AppState,
                                 frame: &mut Frame,
                                 area: Rect| {
    crate::render_theme_picker(frame, area, app);
});
