use super::*;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

fn fixture() -> ModelManager {
    let mut manager = ModelManager::default();
    for provider in ["gateway", "catalog-account"] {
        manager
            .groups
            .push(atman_runtime::model_registry::ProviderGroup {
                provider_name: provider.into(),
                models: (0..9)
                    .map(|index| atman_runtime::model_registry::ModelRow {
                        slug: format!("model-{index}"),
                        provider_name: provider.into(),
                        context_budget: 32768,
                        max_output_tokens: None,
                        thinking: false,
                        reasoning: Default::default(),
                        capabilities: Default::default(),
                        image_detail: Default::default(),
                    })
                    .collect(),
            });
    }
    manager
        .configured
        .insert(("gateway".into(), "model-0".into()));
    manager.model_idx = vec![0, 0];
    manager.sync_browser();
    manager
}

fn render(manager: &mut ModelManager, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let app = crate::app::AppState::new("fixture-session".into(), None);
    let theme = crate::theme::theme();
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            <ModelManager as crate::wm::modal::ModalOverlay>::render_content(
                manager,
                frame,
                frame.area(),
                &app,
                &theme,
            )
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

fn click(rect: Rect) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x,
        row: rect.y,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn provider_groups_survive_search_scope_paging_and_empty_catalogs() {
    let mut manager = fixture();
    manager
        .groups
        .push(atman_runtime::model_registry::ProviderGroup {
            provider_name: "empty-provider".into(),
            models: Vec::new(),
        });
    manager.sync_browser();
    assert_eq!(manager.filtered_model_count(), 18);
    assert_eq!(
        manager
            .browser
            .rows()
            .iter()
            .filter(|row| row.kind == BrowserRowKind::Provider)
            .count(),
        3
    );
    for (width, height) in [(40, 14), (80, 30), (108, 42)] {
        manager.page = Page::List;
        manager.focus = Focus::List;
        manager.search.replace_with("");
        manager.sync_browser();
        manager.navigate(&KeyAction::Home);
        render(&mut manager, width, height);
        let pages = manager.list_pages().len();
        for page in 0..pages {
            let buffer = render(&mut manager, width, height);
            assert_eq!(manager.list_page, page);
            let rect = manager.list_rect.unwrap();
            let header = &manager.browser.rows()[manager.list_pages()[page][0]];
            assert_eq!(header.kind, BrowserRowKind::Provider);
            assert!(buffer[(rect.x + 2, rect.y + 1)].symbol() != " ");
            assert!(
                manager
                    .record_hits
                    .iter()
                    .all(|(_, hit)| hit.height == 4 && hit.bottom() <= rect.bottom())
            );
            manager.handle_key(&KeyAction::PageDown, None);
        }
        manager.search.replace_with("empty-provider");
        manager.sync_browser();
        assert_eq!(manager.current_provider(), "empty-provider");
        assert!(manager.current_model().is_none());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        manager.handle_key(&KeyAction::Delete, Some(&tx));
        manager.handle_key(&KeyAction::Char('a'), Some(&tx));
        assert!(rx.try_recv().is_err());
        assert!(manager.open_alias_model.is_none());
    }
    manager.search.replace_with("model-8");
    manager.sync_browser();
    assert_eq!(manager.filtered_model_count(), 2);
    assert_eq!(
        manager
            .browser
            .rows()
            .iter()
            .filter(|row| row.kind == BrowserRowKind::Provider)
            .count(),
        2
    );
    manager.search.replace_with("does-not-exist");
    manager.sync_browser();
    assert!(manager.browser.rows().is_empty());
    assert!(manager.current_model().is_none());
    manager.search.replace_with("");
    manager.locked_provider = Some("catalog-account".into());
    manager.sync_browser();
    assert_eq!(manager.filtered_model_count(), 9);
    assert_eq!(
        manager
            .browser
            .rows()
            .iter()
            .filter(|row| row.kind == BrowserRowKind::Provider)
            .count(),
        1
    );
}

#[test]
fn empty_provider_pages_are_reachable_by_mouse_without_selecting_old_models() {
    let mut manager = fixture();
    manager.groups = (0..7)
        .map(|index| atman_runtime::model_registry::ProviderGroup {
            provider_name: format!("empty-{index}"),
            models: Vec::new(),
        })
        .collect();
    manager.sync_browser();
    render(&mut manager, 40, 14);
    let pages = manager.list_pages().len();
    for expected in 1..pages {
        let rect = manager.list_rect.unwrap();
        manager.handle_mouse(
            &MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: rect.x,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            },
            None,
        );
        render(&mut manager, 40, 14);
        assert_eq!(manager.list_page, expected);
        assert!(manager.current_model().is_none());
    }
    let last = manager.browser.selected_index();
    manager.navigate(&KeyAction::HistoryDown);
    assert_eq!(manager.browser.selected_index(), last);
}

#[test]
fn identity_controls_scope_search_and_write_capabilities() {
    let mut manager = fixture();
    assert!(manager.current_editable());
    manager.locked_provider = Some("catalog-account".into());
    manager.sync_browser();
    assert_eq!(manager.filtered_model_count(), 9);
    assert!(!manager.current_editable());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    manager.handle_key(&KeyAction::Char('e'), Some(&tx));
    manager.handle_key(&KeyAction::Delete, Some(&tx));
    assert!(!manager.show_form);
    assert!(manager.confirm_delete.is_none());
    assert!(rx.try_recv().is_err());
    manager.handle_key(&KeyAction::Char('/'), None);
    manager.paste("model-8");
    assert_eq!(manager.filtered_model_count(), 1);
    assert_eq!(manager.current_model().unwrap().slug, "model-8");
    manager.handle_key(&KeyAction::Escape, None);
    manager.dispatch(Action::Add, None);
    assert_eq!(manager.selected_provider, "catalog-account");
    assert!(!manager.form_fields().contains(&2));
}

#[test]
fn record_geometry_drives_pagination_and_mouse_identity() {
    for (width, height) in [(40, 14), (80, 30), (108, 42)] {
        let mut manager = fixture();
        render(&mut manager, width, height);
        let first_key = manager.record_hits[0].0.clone();
        manager.handle_key(&KeyAction::PageDown, None);
        assert!(manager.record_hits.is_empty());
        let buffer = render(&mut manager, width, height);
        assert_ne!(first_key, manager.record_hits[0].0);
        let (key, rect) = manager.record_hits[0].clone();
        assert_eq!(rect.height, 4);
        assert!(rect.bottom() <= manager.list_rect.unwrap().bottom());
        assert_eq!(buffer[(rect.x + 1, rect.y)].symbol(), " ");
        assert_eq!(buffer[(rect.x + 1, rect.bottom() - 1)].symbol(), " ");
        manager.handle_mouse(&click(rect), None);
        assert_eq!(manager.browser.selected().unwrap().value, key);
        assert!(manager.page == Page::Details);
        assert!(!manager.show_form);
        render(&mut manager, width, height);
        assert!(
            manager
                .action_hits
                .iter()
                .filter(|(_, rect)| rect.height > 1)
                .all(|(_, rect)| rect.height == 3)
        );
    }
}

#[test]
fn alias_mouse_and_keyboard_return_the_same_modal_action() {
    let mut manager = fixture();
    let mut app = crate::app::AppState::new("fixture-session".into(), None);
    let keyboard = <ModelManager as crate::wm::modal::ModalOverlay>::handle_key(
        &mut manager,
        &KeyAction::Char('a'),
        &mut app,
        None,
    );
    assert!(matches!(keyboard, Some(ModalAction::OpenAliasForModel(model)) if model == "model-0"));
    render(&mut manager, 108, 42);
    let rect = manager
        .action_hits
        .iter()
        .find(|(a, _)| *a == Action::Alias)
        .unwrap()
        .1;
    assert!(
        matches!(manager.handle_mouse(&click(rect), None), Some(ModalAction::OpenAliasForModel(model)) if model == "model-0")
    );
}

#[test]
fn escape_never_saves_and_dirty_confirmation_defaults_to_keep_editing() {
    let mut manager = fixture();
    manager.open_form();
    manager.name_editor.replace_with("fixture-model");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    manager.handle_key(&KeyAction::Submit, Some(&tx));
    assert!(rx.try_recv().is_err());
    manager.handle_key(&KeyAction::Escape, Some(&tx));
    assert!(manager.confirm_discard);
    manager.handle_key(&KeyAction::Submit, Some(&tx));
    assert!(!manager.confirm_discard);
    assert!(manager.show_form);
    assert!(rx.try_recv().is_err());
    manager.handle_key(&KeyAction::Save, Some(&tx));
    let crate::TuiControl::MutateModel(request) = rx.try_recv().unwrap() else {
        panic!("expected save");
    };
    manager.handle_key(&KeyAction::Escape, Some(&tx));
    assert!(!manager.open);
    manager.open_with_provider("different-provider");
    assert_eq!(manager.selected_provider, "gateway");
    assert!(manager.resolve_mutation(&request, &Err("write failed".into())));
    assert_eq!(manager.name_editor.buf(), "fixture-model");
    assert!(manager.show_form);
    assert!(
        manager
            .test_result
            .as_ref()
            .is_some_and(|(error, ok)| !ok && error == "write failed")
    );
}

#[test]
fn only_explicit_stop_cancels_a_test_and_results_belong_to_the_draft() {
    let mut manager = fixture();
    manager.open_form();
    manager.name_editor.replace_with("fixture-model");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    manager.test_model(Some(&tx));
    let crate::TuiControl::TestModel(request) = rx.try_recv().unwrap() else {
        panic!("expected model test");
    };
    manager.handle_key(&KeyAction::Escape, Some(&tx));
    assert!(!request.cancel.is_cancelled());
    manager.open();
    assert!(manager.show_form);
    assert!(!manager.resolve_test(request.request_id + 1, &Ok("late".into())));
    manager.dispatch(Action::Stop, Some(&tx));
    assert!(request.cancel.is_cancelled());
    assert!(manager.resolve_test(request.request_id, &Err("cancelled".into())));
    manager.form_field = 0;
    manager.handle_key(&KeyAction::CursorLeft, Some(&tx));
    assert!(manager.test_result.is_some());
    manager.handle_key(&KeyAction::Char('x'), Some(&tx));
    assert!(manager.test_result.is_none());
}

#[test]
fn delete_confirmation_is_safe_by_default_and_blocks_alias_references() {
    let mut manager = fixture();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    manager.handle_key(&KeyAction::Delete, Some(&tx));
    assert!(manager.confirm_delete.is_some());
    manager.handle_key(&KeyAction::Submit, Some(&tx));
    assert!(manager.confirm_delete.is_none());
    assert!(rx.try_recv().is_err());
    manager.confirm_delete = Some("model-0".into());
    manager.delete_aliases = vec!["smart".into()];
    render(&mut manager, 40, 18);
    assert!(
        !manager
            .action_hits
            .iter()
            .any(|(action, _)| *action == Action::Confirm)
    );
    let rect = manager
        .action_hits
        .iter()
        .find(|(action, _)| *action == Action::Aliases)
        .unwrap()
        .1;
    assert!(matches!(
        manager.handle_mouse(&click(rect), Some(&tx)),
        Some(ModalAction::OpenAliasManager)
    ));
    assert!(rx.try_recv().is_err());
}

#[test]
fn form_cursor_actions_and_selector_hits_use_rendered_geometry() {
    for width in [40, 96] {
        let mut manager = fixture();
        manager.open_form();
        manager.name_editor.replace_with("fixture-model");
        let fields = manager.form_fields();
        for field in fields.iter().filter(|field| **field < 7) {
            manager.form_field = *field;
            render(&mut manager, width, 24);
            let hit = manager
                .form_hits
                .iter()
                .find(|(f, _)| f == field)
                .unwrap()
                .1;
            let previous = manager.form_editor_mut().map(|e| e.buf().to_owned());
            manager.handle_mouse(&click(hit.record), None);
            assert_eq!(
                previous,
                manager.form_editor_mut().map(|e| e.buf().to_owned())
            );
            assert_eq!(hit.record.height, if width < 70 { 4 } else { 3 });
            assert!(hit.value.y > hit.record.y && hit.value.bottom() < hit.record.bottom());
        }
        manager.form_field = 4;
        manager.thinking_editor.replace_with("default");
        render(&mut manager, width, 24);
        let hit = manager.form_hits.iter().find(|(f, _)| *f == 4).unwrap().1;
        if let Some((_, right)) = hit.arrows {
            manager.handle_mouse(&click(right), None);
            assert_eq!(manager.thinking_editor.buf(), "off");
        }
        manager.thinking_editor.replace_with("budget:");
        manager.paste("8192");
        assert_eq!(manager.thinking_editor.buf(), "budget:8192");
        assert!(manager.has_text_focus());
        render(&mut manager, width, 24);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        manager.handle_mouse(&click(manager.save_rect.unwrap()), Some(&tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::TuiControl::MutateModel(_))
        ));
    }
}

#[test]
fn form_status_expands_and_scrolls_without_losing_the_draft() {
    let mut manager = fixture();
    manager.open_form();
    manager.name_editor.replace_with("fixture-model");
    manager.test_result = Some((
        format!("{} END-OF-DETAILS", "Diagnostic context ".repeat(70)),
        false,
    ));
    manager.dispatch(Action::Details, None);
    render(&mut manager, 40, 18);
    assert!(manager.detail_max_scroll > 0);
    manager.handle_key(&KeyAction::CursorEnd, None);
    assert_eq!(manager.detail_scroll, manager.detail_max_scroll);
    let buffer = render(&mut manager, 40, 18);
    let rect = manager.detail_rect.unwrap();
    let text: String = (rect.y..rect.bottom())
        .map(|y| {
            (rect.x..rect.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .collect();
    assert!(text.contains("END-OF-DETAILS"));
    manager.handle_key(&KeyAction::Escape, None);
    assert!(manager.show_form);
    assert_eq!(manager.name_editor.buf(), "fixture-model");
}

#[test]
fn hidden_enabled_and_api_values_survive_save() {
    let mut manager = fixture();
    manager.open_form();
    manager.name_editor.replace_with("fixture-model");
    manager.form_enabled = false;
    manager.form_api = Some(atman_runtime::provider::OpenAiApi::Responses);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    manager.handle_key(&KeyAction::Save, Some(&tx));
    let crate::TuiControl::MutateModel(request) = rx.try_recv().unwrap() else {
        panic!("expected save");
    };
    assert!(matches!(
        request.action,
        crate::ModelMutation::Upsert {
            enabled: false,
            api: Some(atman_runtime::provider::OpenAiApi::Responses),
            ..
        }
    ));
}

#[test]
fn disabled_model_editor_uses_configuration_budget_not_runtime_availability() {
    let mut manager = fixture();
    let mut model = manager.current_model().unwrap().clone();
    model.context_budget = 0;
    let entry = atman_runtime::model_registry::ModelEntry {
        context_budget: Some(128000),
        enabled: Some(false),
        ..Default::default()
    };
    manager.load_edit_entry(&model, "gateway".into(), &entry);
    assert_eq!(manager.context_budget_editor.buf(), "128000");
    assert!(!manager.form_enabled);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    manager.handle_key(&KeyAction::Save, Some(&tx));
    let crate::TuiControl::MutateModel(request) = rx.try_recv().unwrap() else {
        panic!("expected save");
    };
    assert!(matches!(
        request.action,
        crate::ModelMutation::Upsert {
            context_budget: 128000,
            enabled: false,
            ..
        }
    ));
}

#[test]
fn returned_alias_updates_clear_stale_delete_blockers() {
    let mut manager = fixture();
    manager.confirm_delete = Some("fixture-removed-alias-model".into());
    manager.delete_aliases = vec!["removed-alias".into()];
    manager.confirm_focus = 1;
    manager.refresh_delete_references();
    assert!(manager.delete_aliases.is_empty());
    assert_eq!(manager.confirm_focus, 0);
    render(&mut manager, 40, 18);
    assert!(
        manager
            .action_hits
            .iter()
            .any(|(action, _)| *action == Action::Confirm)
    );
}
