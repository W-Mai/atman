use crate::app::AppState;
use crate::wm::component::{EventCtx, RenderCtx, WmEvent, WmEventResult};
use crate::wm::{ModalComponent, ModalManager};
use ratatui::Frame;
use ratatui::layout::Rect;

macro_rules! modal_wrapper {
    ($name:ident, $render:expr) => {
        pub struct $name<'a> {
            pub app: &'a mut AppState,
            pub modals: &'a mut ModalManager,
        }

        impl ModalComponent for $name<'_> {
            fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &RenderCtx) {
                ($render)(self.app, self.modals, frame, area);
            }

            fn handle_event(&mut self, _event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
                WmEventResult::Consumed(Vec::new())
            }

            fn preferred_rect(&self, viewport: Rect) -> Rect {
                viewport
            }
        }
    };
}

modal_wrapper!(FormModalWrapper, |_app: &mut AppState,
                                 modals: &mut ModalManager,
                                 frame: &mut Frame,
                                 area: Rect| {
    crate::form_modal::render(frame, area, &modals.form_modal);
});

modal_wrapper!(CompactReviewWrapper, |_app: &mut AppState,
                                    modals: &mut ModalManager,
                                    frame: &mut Frame,
                                    area: Rect| {
    if let Some(modal) = &mut modals.compact_review {
        crate::compact_review_modal::render(frame, area, modal);
    }
});

modal_wrapper!(SessionSwitcherWrapper, |_app: &mut AppState,
                                     modals: &mut ModalManager,
                                     frame: &mut Frame,
                                     area: Rect| {
    crate::session_switcher::render(frame, area, &modals.session_switcher);
});

modal_wrapper!(HistorySearchWrapper, |_app: &mut AppState,
                                   modals: &mut ModalManager,
                                   frame: &mut Frame,
                                   area: Rect| {
    crate::history_search_modal::render(frame, area, &mut modals.history_search);
});

modal_wrapper!(ProviderManagerWrapper, |_app: &mut AppState,
                                    modals: &mut ModalManager,
                                    frame: &mut Frame,
                                    area: Rect| {
    crate::provider_manager::render(frame, area, &mut modals.provider_manager);
});

modal_wrapper!(AliasManagerWrapper, |_app: &mut AppState,
                                 modals: &mut ModalManager,
                                 frame: &mut Frame,
                                 area: Rect| {
    crate::alias_manager::render(frame, area, &modals.alias_manager);
});

modal_wrapper!(ModelPickerWrapper, |app: &mut AppState,
                                modals: &mut ModalManager,
                                frame: &mut Frame,
                                area: Rect| {
    crate::model_picker::render(frame, area, &modals.model_picker, &app.context.model);
});

modal_wrapper!(OnboardingWrapper, |_app: &mut AppState,
                               modals: &mut ModalManager,
                               frame: &mut Frame,
                               area: Rect| {
    crate::onboarding::render(frame, area, &modals.onboarding);
});

modal_wrapper!(PaletteWrapper, |_app: &mut AppState,
                            modals: &mut ModalManager,
                            frame: &mut Frame,
                            area: Rect| {
    crate::palette::render(frame, area, &modals.palette);
});

modal_wrapper!(ThemePickerWrapper, |app: &mut AppState,
                                _modals: &mut ModalManager,
                                frame: &mut Frame,
                                area: Rect| {
    crate::render_theme_picker(frame, area, app);
});

modal_wrapper!(TrustModePickerWrapper, |app: &mut AppState,
                                    _modals: &mut ModalManager,
                                    frame: &mut Frame,
                                    area: Rect| {
    crate::render_trust_mode_picker(frame, area, app);
});
