use std::time::{Duration, Instant};

use global_hotkey::GlobalHotKeyEvent;
use iced::keyboard::{key, Event as KeyboardEvent, Key};
use iced::widget::operation;
use iced::Task;
use tray_icon::menu::MenuEvent;

use crate::commands::{apply_command_choice, command_menu_items, is_exact_directive_token};
use crate::search_worker::SearchEvent;
use crate::storage::persist_quick_help_dismissed;
use crate::{
    debug_log, init_hotkey, open_path, reveal_path, windowing, App, IndexBackend, IndexEvent,
    Message, DELTA_REFRESH_COOLDOWN, KEYBOARD_PAGE_JUMP, MAX_INDEX_EVENTS_PER_TICK,
    MAX_SEARCH_EVENTS_PER_TICK, PANEL_ANIMATION_DURATION, QUERY_DEBOUNCE_DELAY,
};

pub(crate) fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::QueryChanged(query) => {
            if app.show_privilege_overlay {
                app.show_privilege_overlay = false;
            }
            if app.show_quick_help_overlay {
                app.show_quick_help_overlay = false;
            }

            app.raw_query = query;
            app.query_edit_counter = app.query_edit_counter.wrapping_add(1);
            app.cancel_active_search();
            app.needs_search_refresh = false;
            app.pending_query = Some((
                app.raw_query.clone(),
                Instant::now() + QUERY_DEBOUNCE_DELAY,
                app.query_edit_counter,
            ));

            let suggestions = command_menu_items(&app.raw_query, app.tracking_enabled);
            if suggestions.is_empty() {
                app.command_selected = 0;
            } else {
                app.command_selected = app.command_selected.min(suggestions.len() - 1);
            }

            return Task::none();
        }
        Message::ActivateSelected => {
            if app.show_quick_help_overlay {
                if app.quick_help_selected_action == 0 {
                    app.show_quick_help_overlay = false;
                } else {
                    app.show_quick_help_overlay = false;
                    persist_quick_help_dismissed(true);
                }

                return windowing::keep_search_input_focus(app.search_input_id.clone());
            }

            let suggestions = command_menu_items(&app.raw_query, app.tracking_enabled);
            let first_token = app
                .raw_query
                .trim_start()
                .split_whitespace()
                .next()
                .unwrap_or("");

            debug_log(&format!(
                "ActivateSelected raw='{}' token='{}' suggestions={} selected={} items={}",
                app.raw_query,
                first_token,
                suggestions.len(),
                app.command_selected,
                app.items.len()
            ));

            if is_exact_directive_token(first_token, app.tracking_enabled) {
                debug_log("ActivateSelected executing exact directive token");
                let task = app.apply_raw_query(app.raw_query.clone(), true);
                return Task::batch(vec![
                    task,
                    operation::focus(app.search_input_id.clone()),
                    operation::move_cursor_to_end(app.search_input_id.clone()),
                ]);
            }

            if !suggestions.is_empty() {
                if let Some(choice) = suggestions.get(app.command_selected) {
                    debug_log(&format!(
                        "ActivateSelected applying suggestion command='{}'",
                        choice.command
                    ));
                    let new_raw = apply_command_choice(&app.raw_query, choice.command);
                    let task = app.apply_raw_query(new_raw, true);

                    return Task::batch(vec![
                        task,
                        operation::focus(app.search_input_id.clone()),
                        operation::move_cursor_to_end(app.search_input_id.clone()),
                    ]);
                }
            } else if app.raw_query.trim_start().starts_with('/') {
                app.last_action = format!("Unknown command: {}", first_token);
            } else if let Some(item) = app.items.get(app.selected) {
                app.last_action = format!("Open: {}", item.path);
                let _ = open_path(item.path.as_ref());
            }
        }
        Message::AnimateFrame => {
            let target = if app.panel_visible { 1.0 } else { 0.0 };

            let now = Instant::now();
            let dt = app
                .panel_anim_last_tick
                .map(|last| now.saturating_duration_since(last))
                .unwrap_or(Duration::from_millis(16));
            app.panel_anim_last_tick = Some(now);

            let step =
                (dt.as_secs_f32() / PANEL_ANIMATION_DURATION.as_secs_f32()).clamp(0.01, 0.25);

            if app.panel_progress < target {
                app.panel_progress = (app.panel_progress + step).min(1.0);
            } else if app.panel_progress > target {
                app.panel_progress = (app.panel_progress - step).max(0.0);
            }

            let move_task = windowing::sync_window_to_progress(app.panel_progress);

            let reached_target = (app.panel_progress - target).abs() <= f32::EPSILON;
            if reached_target {
                app.panel_anim_last_tick = None;

                if !app.panel_visible {
                    return Task::batch(vec![move_task, windowing::finalize_panel_hidden_mode()]);
                }
            }

            return move_task;
        }
        Message::PollExternal => {
            if app.visual_progress_test_active {
                app.indexing_in_progress = true;
                app.indexing_progress = (app.indexing_progress + 0.03).min(1.0);

                if app.indexing_progress >= 1.0 {
                    app.visual_progress_test_active = false;
                    app.indexing_in_progress = false;
                    app.last_action = "Visual progress test complete".to_string();
                }
            }

            if app.panel_visible {
                if let Some((pending_query, due_at, edit_id)) = app.pending_query.clone() {
                    if Instant::now() >= due_at && edit_id == app.query_edit_counter {
                        app.pending_query = None;
                        let _ = app.apply_raw_query(pending_query, false);
                    }
                }

                if app.pending_query.is_some() {
                    app.cancel_active_search();
                    return Task::none();
                }

                if app.needs_search_refresh
                    && app.pending_query.is_none()
                    && Instant::now() >= app.next_search_refresh_at
                {
                    app.needs_search_refresh = false;
                    app.next_search_refresh_at = Instant::now() + DELTA_REFRESH_COOLDOWN;
                    app.schedule_search_from_current_query();
                }

                if app.pending_query.is_none() {
                    app.process_filename_index_build_step();
                }
            }

            for _ in 0..MAX_SEARCH_EVENTS_PER_TICK {
                let Ok(event) = app.search_rx.try_recv() else {
                    break;
                };

                match event {
                    SearchEvent::Progress {
                        generation,
                        scanned,
                        total,
                    } => {
                        if app.active_search_job == Some(generation) {
                            app.active_search_cursor = scanned.min(total);
                        }
                    }
                    SearchEvent::Done { generation, items } => {
                        if app.active_search_job == Some(generation) {
                            app.items = items;
                            app.active_search_job = None;
                            app.active_search_query = None;
                            app.active_search_cursor = 0;
                            app.clamp_selected();
                        }
                    }
                }
            }

            let mut focus_search_after_index_done = false;

            if let Some(rx) = &app.index_rx {
                let mut pending = Vec::new();
                for _ in 0..MAX_INDEX_EVENTS_PER_TICK {
                    match rx.try_recv() {
                        Ok(event) => pending.push(event),
                        Err(_) => break,
                    }
                }

                if !pending.is_empty() {
                    debug_log(&format!(
                        "PollExternal received {} index events",
                        pending.len()
                    ));
                }

                for event in pending {
                    match event {
                        IndexEvent::SnapshotLoaded { job_id, items } => {
                            if app.active_index_job == Some(job_id) {
                                app.all_items = items;
                                app.indexing_is_refresh = true;
                                app.filename_index_dirty = true;
                                app.filename_index_building = false;
                                app.filename_index_build_cursor = 0;
                                app.recompute_index_memory_bytes();
                                app.push_corpus_to_search_worker();
                                app.schedule_search_from_current_query();
                                app.last_action = format!(
                                    "Loaded snapshot: {} items [{}]",
                                    app.all_items.len(),
                                    app.scope.label()
                                );
                            }
                        }
                        IndexEvent::Progress {
                            job_id,
                            current,
                            total,
                            phase,
                        } => {
                            if app.active_index_job == Some(job_id) {
                                app.indexing_in_progress = true;
                                app.indexing_phase = phase;
                                app.indexing_progress = if total == 0 {
                                    0.0
                                } else {
                                    (current as f32 / total as f32).clamp(0.0, 1.0)
                                };
                            }
                        }
                        IndexEvent::Done {
                            job_id,
                            items,
                            backend,
                        } => {
                            if app.active_index_job == Some(job_id) {
                                app.indexing_in_progress = false;
                                app.indexing_progress = 1.0;
                                app.indexing_phase = "done";
                                app.index_backend = backend;
                                app.all_items = items;
                                app.filename_index_dirty = true;
                                app.filename_index_building = false;
                                app.filename_index_build_cursor = 0;
                                app.recompute_index_memory_bytes();
                                app.recent_event_by_path.clear();
                                app.changes_added_since_index = 0;
                                app.changes_updated_since_index = 0;
                                app.changes_deleted_since_index = 0;
                                app.push_corpus_to_search_worker();
                                if app.all_items.is_empty() && backend == IndexBackend::Detecting {
                                    app.last_action = "NTFS indexing unavailable (run elevated and ensure USN journal is available)".to_string();
                                } else {
                                    app.last_action = format!(
                                        "Indexed {} files [{}]",
                                        app.all_items.len(),
                                        app.scope.label()
                                    );
                                }
                                app.schedule_search_from_current_query();
                                if app.panel_visible {
                                    focus_search_after_index_done = true;
                                }
                            }
                        }
                        IndexEvent::Delta {
                            job_id,
                            upserts,
                            deleted_paths,
                        } => {
                            if app.active_index_job == Some(job_id) {
                                let (added, updated, deleted) =
                                    app.apply_index_delta(upserts, deleted_paths);
                                app.changes_added_since_index += added;
                                app.changes_updated_since_index += updated;
                                app.changes_deleted_since_index += deleted;
                                app.recompute_index_memory_bytes();
                                app.indexing_in_progress = false;
                                app.indexing_progress = 1.0;
                                app.indexing_phase = "live";
                                app.last_action = format!(
                                    "Live index update: {} items [{}]",
                                    app.all_items.len(),
                                    app.scope.label()
                                );
                            }
                        }
                    }
                }
            }

            if focus_search_after_index_done {
                return windowing::keep_search_input_focus(app.search_input_id.clone());
            }

            if app._hotkey_manager.is_none() || app._hotkey.is_none() {
                let should_retry = app
                    .hotkey_retry_after
                    .is_none_or(|due| Instant::now() >= due);
                if should_retry {
                    match init_hotkey() {
                        Ok((manager, hotkey)) => {
                            app._hotkey_manager = manager;
                            app._hotkey = hotkey;
                            app.hotkey_retry_after = None;
                            app.last_action = "Global hotkey ready".to_string();
                        }
                        Err(err) => {
                            debug_log(&format!("hotkey retry failed: {}", err));
                            app.hotkey_retry_after =
                                Some(Instant::now() + Duration::from_millis(1200));
                        }
                    }
                }
            }

            let mut toggled = false;

            while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                if let Some(hotkey) = &app._hotkey {
                    if event.id == hotkey.id() {
                        toggled = true;
                    }
                }
            }

            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if app
                    .menu_toggle_id
                    .as_ref()
                    .is_some_and(|id| event.id == *id)
                {
                    toggled = true;
                }
                if app.menu_quit_id.as_ref().is_some_and(|id| event.id == *id) {
                    return iced::exit();
                }
            }

            if toggled {
                if let Some(last) = app.last_toggle_at {
                    if last.elapsed() < Duration::from_millis(220) {
                        return Task::none();
                    }
                }
                app.last_toggle_at = Some(Instant::now());

                app.panel_visible = !app.panel_visible;
                app.panel_anim_last_tick = None;

                let window_task = if app.panel_visible {
                    windowing::prepare_panel_for_show_mode()
                } else {
                    Task::none()
                };

                if app.panel_visible {
                    app.schedule_search_from_current_query();
                    return Task::batch(vec![
                        window_task,
                        operation::focus(app.search_input_id.clone()),
                        operation::move_cursor_to_end(app.search_input_id.clone()),
                    ]);
                }

                return window_task;
            }
        }
        Message::Keyboard(event) => {
            if !app.panel_visible {
                return Task::none();
            }

            if let KeyboardEvent::KeyPressed { key, modifiers, .. } = event {
                if app.show_privilege_overlay {
                    app.show_privilege_overlay = false;
                }

                let suggestions = command_menu_items(&app.raw_query, app.tracking_enabled);
                let command_mode = !suggestions.is_empty();

                match key.as_ref() {
                    Key::Named(key::Named::Escape) => {
                        if app.show_quick_help_overlay {
                            app.show_quick_help_overlay = false;
                            return Task::none();
                        }
                        app.panel_visible = false;
                        app.panel_anim_last_tick = None;
                        return Task::none();
                    }
                    Key::Named(key::Named::Tab)
                    | Key::Named(key::Named::ArrowLeft)
                    | Key::Named(key::Named::ArrowRight) => {
                        if app.show_quick_help_overlay {
                            app.quick_help_selected_action =
                                (app.quick_help_selected_action + 1) % 2;
                            return Task::none();
                        }
                    }
                    Key::Character("d") | Key::Character("D") => {
                        if app.show_quick_help_overlay {
                            app.show_quick_help_overlay = false;
                            persist_quick_help_dismissed(true);
                            return Task::none();
                        }
                    }
                    Key::Named(key::Named::ArrowDown) => {
                        if app.show_quick_help_overlay {
                            app.quick_help_selected_action = 1;
                            return Task::none();
                        }
                        if command_mode {
                            app.command_selected =
                                (app.command_selected + 1).min(suggestions.len() - 1);
                            return windowing::keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected = (app.selected + 1).min(app.items.len() - 1);
                            return Task::batch(vec![
                                windowing::sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                windowing::keep_search_input_focus(app.search_input_id.clone()),
                            ]);
                        }
                    }
                    Key::Named(key::Named::ArrowUp) => {
                        if app.show_quick_help_overlay {
                            app.quick_help_selected_action = 0;
                            return Task::none();
                        }
                        if command_mode {
                            app.command_selected = app.command_selected.saturating_sub(1);
                            return windowing::keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected = app.selected.saturating_sub(1);
                            return Task::batch(vec![
                                windowing::sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                windowing::keep_search_input_focus(app.search_input_id.clone()),
                            ]);
                        }
                    }
                    Key::Named(key::Named::PageDown) => {
                        if command_mode {
                            app.command_selected = (app.command_selected + KEYBOARD_PAGE_JUMP)
                                .min(suggestions.len() - 1);
                            return windowing::keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected =
                                (app.selected + KEYBOARD_PAGE_JUMP).min(app.items.len() - 1);
                            return Task::batch(vec![
                                windowing::sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                windowing::keep_search_input_focus(app.search_input_id.clone()),
                            ]);
                        }
                    }
                    Key::Named(key::Named::PageUp) => {
                        if command_mode {
                            app.command_selected =
                                app.command_selected.saturating_sub(KEYBOARD_PAGE_JUMP);
                            return windowing::keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected = app.selected.saturating_sub(KEYBOARD_PAGE_JUMP);
                            return Task::batch(vec![
                                windowing::sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                windowing::keep_search_input_focus(app.search_input_id.clone()),
                            ]);
                        }
                    }
                    Key::Named(key::Named::Home) => {
                        if command_mode {
                            app.command_selected = 0;
                            return windowing::keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected = 0;
                            return Task::batch(vec![
                                windowing::sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                windowing::keep_search_input_focus(app.search_input_id.clone()),
                            ]);
                        }
                    }
                    Key::Named(key::Named::End) => {
                        if command_mode {
                            app.command_selected = suggestions.len() - 1;
                            return windowing::keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected = app.items.len() - 1;
                            return Task::batch(vec![
                                windowing::sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                windowing::keep_search_input_focus(app.search_input_id.clone()),
                            ]);
                        }
                    }
                    Key::Named(key::Named::Enter) if modifiers.alt() && !command_mode => {
                        if app.show_quick_help_overlay {
                            return Task::none();
                        }
                        if let Some(item) = app.items.get(app.selected) {
                            app.last_action = format!("Reveal: {}", item.path);
                            let _ = reveal_path(item.path.as_ref());
                        }
                    }
                    Key::Named(key::Named::Enter) if command_mode => {
                        if app.show_quick_help_overlay {
                            if app.quick_help_selected_action == 0 {
                                app.show_quick_help_overlay = false;
                            } else {
                                app.show_quick_help_overlay = false;
                                persist_quick_help_dismissed(true);
                            }
                            return Task::none();
                        }
                        return Task::done(Message::ActivateSelected);
                    }
                    Key::Named(key::Named::Enter) => {
                        if app.show_quick_help_overlay {
                            if app.quick_help_selected_action == 0 {
                                app.show_quick_help_overlay = false;
                            } else {
                                app.show_quick_help_overlay = false;
                                persist_quick_help_dismissed(true);
                            }
                            return Task::none();
                        }
                    }
                    _ => {}
                }
            }
        }
        Message::StartIndex(scope) => {
            app.begin_index(scope);
        }
        Message::CloseQuickHelp => {
            app.show_quick_help_overlay = false;
            return windowing::keep_search_input_focus(app.search_input_id.clone());
        }
        Message::CloseQuickHelpForever => {
            app.show_quick_help_overlay = false;
            persist_quick_help_dismissed(true);
            return windowing::keep_search_input_focus(app.search_input_id.clone());
        }
    }

    Task::none()
}
