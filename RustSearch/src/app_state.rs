use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
use tray_icon::menu::{Menu, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::commands::{
    apply_command_choice, command_menu_items, format_latest_window, is_exact_directive_token,
    parse_scope_directive,
};
use crate::indexing;
use crate::platform::{is_process_elevated, open_path, request_self_elevation, reveal_path};
use crate::search::{contains_ascii_case_insensitive, file_name_from_path};
use crate::search_worker::{SearchEvent, SearchWorkerMessage};
use crate::storage::{
    load_persisted_scope, load_quick_help_dismissed, persist_quick_help_dismissed, persist_scope,
};
use crate::{
    debug_log, estimate_index_memory_bytes, IndexBackend, IndexEvent, RendererModeRequest,
    SearchItem, SearchScope, WindowModeRequest, DEFAULT_LATEST_WINDOW_SECS, DELTA_REFRESH_COOLDOWN,
    FILENAME_INDEX_BUILD_BATCH, KEYBOARD_PAGE_JUMP, MAX_INDEX_EVENTS_PER_TICK,
    MAX_SEARCH_EVENTS_PER_TICK, QUERY_DEBOUNCE_DELAY, UNKNOWN_TS, VISIBLE_RESULTS_LIMIT,
};

pub(crate) struct TickOutcome {
    pub(crate) visibility_changed: bool,
    pub(crate) focus_search: bool,
    pub(crate) should_quit: bool,
    pub(crate) window_mode_request: Option<WindowModeRequest>,
    pub(crate) renderer_mode_request: Option<RendererModeRequest>,
}

pub(crate) struct AppState {
    pub(crate) raw_query: String,
    pub(crate) query: String,
    pub(crate) all_items: Vec<SearchItem>,
    pub(crate) items: Vec<SearchItem>,
    pub(crate) selected: usize,
    pub(crate) last_action: String,
    pub(crate) panel_visible: bool,
    pub(crate) _hotkey_manager: Option<GlobalHotKeyManager>,
    pub(crate) _hotkey: Option<HotKey>,
    pub(crate) _tray_icon: Option<TrayIcon>,
    pub(crate) menu_toggle_id: Option<MenuId>,
    pub(crate) menu_quit_id: Option<MenuId>,
    pub(crate) last_toggle_at: Option<Instant>,
    pub(crate) scope: SearchScope,
    pub(crate) command_selected: usize,
    pub(crate) index_rx: Option<mpsc::Receiver<IndexEvent>>,
    pub(crate) index_job_counter: u64,
    pub(crate) active_index_job: Option<u64>,
    pub(crate) indexing_in_progress: bool,
    pub(crate) indexing_progress: f32,
    pub(crate) indexing_phase: &'static str,
    pub(crate) index_backend: IndexBackend,
    pub(crate) index_memory_bytes: usize,
    pub(crate) visual_progress_test_active: bool,
    pub(crate) indexing_is_refresh: bool,
    pub(crate) is_elevated: bool,
    pub(crate) use_dirwalk_fallback: bool,
    pub(crate) show_privilege_overlay: bool,
    pub(crate) show_quick_help_overlay: bool,
    pub(crate) show_about_overlay: bool,
    pub(crate) quick_help_selected_action: usize,
    pub(crate) pending_query: Option<(String, Instant, u64)>,
    pub(crate) query_edit_counter: u64,
    pub(crate) search_tx: mpsc::Sender<SearchWorkerMessage>,
    pub(crate) search_rx: mpsc::Receiver<SearchEvent>,
    pub(crate) search_generation: u64,
    pub(crate) active_search_job: Option<u64>,
    pub(crate) active_search_query: Option<String>,
    pub(crate) active_search_cursor: usize,
    pub(crate) filename_exact_index: HashMap<String, Vec<usize>>,
    pub(crate) filename_prefix_index: HashMap<String, Vec<usize>>,
    pub(crate) filename_index_dirty: bool,
    pub(crate) filename_index_building: bool,
    pub(crate) filename_index_build_cursor: usize,
    pub(crate) needs_search_refresh: bool,
    pub(crate) next_search_refresh_at: Instant,
    pub(crate) latest_only_mode: bool,
    pub(crate) latest_window_secs: i64,
    pub(crate) tracking_enabled: bool,
    pub(crate) recent_event_by_path: HashMap<Box<str>, i64>,
    pub(crate) changes_added_since_index: usize,
    pub(crate) changes_updated_since_index: usize,
    pub(crate) changes_deleted_since_index: usize,
    pub(crate) hotkey_retry_after: Option<Instant>,
    pub(crate) skip_scope_persist_once: bool,
    pub(crate) should_exit: bool,
    pub(crate) pending_window_mode_request: Option<WindowModeRequest>,
    pub(crate) pending_renderer_mode_request: Option<RendererModeRequest>,
}

impl AppState {
    pub(crate) fn new(start_visible: bool, startup_scope: Option<SearchScope>) -> Self {
        let (tray_icon, menu_toggle_id, menu_quit_id) = init_tray().unwrap_or((None, None, None));
        let (hotkey_manager, hotkey, hotkey_retry_after) = match init_hotkey() {
            Ok((manager, hotkey)) => (manager, hotkey, None),
            Err(err) => {
                debug_log(&format!("init_hotkey failed: {}", err));
                (
                    None,
                    None,
                    Some(Instant::now() + Duration::from_millis(1200)),
                )
            }
        };
        let persisted_scope = load_persisted_scope();
        let is_elevated = is_process_elevated();
        let arg_scope_override = startup_scope;
        let (search_tx, search_rx) = crate::search_worker::spawn_search_worker();
        let startup_scope = if let Some(scope) = arg_scope_override.clone() {
            scope
        } else if is_elevated {
            persisted_scope
        } else {
            SearchScope::CurrentFolder
        };

        let mut app = Self {
            raw_query: String::new(),
            query: String::new(),
            all_items: Vec::new(),
            items: Vec::new(),
            selected: 0,
            last_action: "Indexing files...".to_string(),
            panel_visible: start_visible,
            _hotkey_manager: hotkey_manager,
            _hotkey: hotkey,
            _tray_icon: tray_icon,
            menu_toggle_id,
            menu_quit_id,
            last_toggle_at: None,
            scope: startup_scope,
            command_selected: 0,
            index_rx: None,
            index_job_counter: 0,
            active_index_job: None,
            indexing_in_progress: false,
            indexing_progress: 0.0,
            indexing_phase: "index",
            index_backend: IndexBackend::Detecting,
            index_memory_bytes: 0,
            visual_progress_test_active: false,
            indexing_is_refresh: false,
            is_elevated,
            use_dirwalk_fallback: !is_elevated,
            show_privilege_overlay: !is_elevated,
            show_quick_help_overlay: is_elevated && !load_quick_help_dismissed(),
            show_about_overlay: false,
            quick_help_selected_action: 0,
            pending_query: None,
            query_edit_counter: 0,
            search_tx,
            search_rx,
            search_generation: 0,
            active_search_job: None,
            active_search_query: None,
            active_search_cursor: 0,
            filename_exact_index: HashMap::new(),
            filename_prefix_index: HashMap::new(),
            filename_index_dirty: true,
            filename_index_building: false,
            filename_index_build_cursor: 0,
            needs_search_refresh: false,
            next_search_refresh_at: Instant::now(),
            latest_only_mode: false,
            latest_window_secs: DEFAULT_LATEST_WINDOW_SECS,
            tracking_enabled: true,
            recent_event_by_path: HashMap::new(),
            changes_added_since_index: 0,
            changes_updated_since_index: 0,
            changes_deleted_since_index: 0,
            hotkey_retry_after,
            skip_scope_persist_once: !is_elevated && arg_scope_override.is_none(),
            should_exit: false,
            pending_window_mode_request: None,
            pending_renderer_mode_request: None,
        };

        app.begin_index(app.scope.clone());
        app
    }

    pub(crate) fn on_query_changed(&mut self, query: String) {
        if self.show_privilege_overlay {
            self.show_privilege_overlay = false;
        }
        if self.show_quick_help_overlay {
            self.show_quick_help_overlay = false;
        }
        if self.show_about_overlay {
            self.show_about_overlay = false;
        }

        self.raw_query = query;
        self.query_edit_counter = self.query_edit_counter.wrapping_add(1);
        self.cancel_active_search();
        self.needs_search_refresh = false;
        self.pending_query = Some((
            self.raw_query.clone(),
            Instant::now() + QUERY_DEBOUNCE_DELAY,
            self.query_edit_counter,
        ));

        let suggestions = command_menu_items(&self.raw_query, self.tracking_enabled);
        if suggestions.is_empty() {
            self.command_selected = 0;
        } else {
            self.command_selected = self.command_selected.min(suggestions.len() - 1);
        }
    }

    pub(crate) fn activate_selected(&mut self) {
        if self.show_quick_help_overlay {
            if self.quick_help_selected_action == 0 {
                self.show_quick_help_overlay = false;
            } else {
                self.show_quick_help_overlay = false;
                persist_quick_help_dismissed(true);
            }
            return;
        }

        let suggestions = command_menu_items(&self.raw_query, self.tracking_enabled);
        let first_token = self
            .raw_query
            .trim_start()
            .split_whitespace()
            .next()
            .unwrap_or("");

        if is_exact_directive_token(first_token, self.tracking_enabled) {
            self.apply_raw_query(self.raw_query.clone(), true);
            return;
        }

        if !suggestions.is_empty() {
            if let Some(choice) = suggestions.get(self.command_selected) {
                let new_raw = apply_command_choice(&self.raw_query, choice.command);
                self.apply_raw_query(new_raw, true);
            }
        } else if self.raw_query.trim_start().starts_with('/') {
            self.last_action = format!("Unknown command: {}", first_token);
        } else if let Some(item) = self.items.get(self.selected) {
            self.last_action = format!("Open: {}", item.path);
            let _ = open_path(item.path.as_ref());
        }
    }

    pub(crate) fn on_escape(&mut self) {
        if self.show_about_overlay {
            self.show_about_overlay = false;
            return;
        }
        if self.show_quick_help_overlay {
            self.show_quick_help_overlay = false;
            return;
        }
        self.panel_visible = false;
    }

    pub(crate) fn on_move_down(&mut self) {
        if self.show_quick_help_overlay {
            self.quick_help_selected_action = 1;
            return;
        }
        let suggestions = command_menu_items(&self.raw_query, self.tracking_enabled);
        let command_mode = !suggestions.is_empty();
        if command_mode {
            self.command_selected = (self.command_selected + 1).min(suggestions.len() - 1);
        } else if !self.items.is_empty() {
            self.selected = (self.selected + 1).min(self.items.len() - 1);
        }
    }

    pub(crate) fn on_move_up(&mut self) {
        if self.show_quick_help_overlay {
            self.quick_help_selected_action = 0;
            return;
        }
        let suggestions = command_menu_items(&self.raw_query, self.tracking_enabled);
        let command_mode = !suggestions.is_empty();
        if command_mode {
            self.command_selected = self.command_selected.saturating_sub(1);
        } else if !self.items.is_empty() {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    pub(crate) fn on_page_down(&mut self) {
        let suggestions = command_menu_items(&self.raw_query, self.tracking_enabled);
        let command_mode = !suggestions.is_empty();
        if command_mode {
            self.command_selected =
                (self.command_selected + KEYBOARD_PAGE_JUMP).min(suggestions.len() - 1);
        } else if !self.items.is_empty() {
            self.selected = (self.selected + KEYBOARD_PAGE_JUMP).min(self.items.len() - 1);
        }
    }

    pub(crate) fn on_page_up(&mut self) {
        let suggestions = command_menu_items(&self.raw_query, self.tracking_enabled);
        let command_mode = !suggestions.is_empty();
        if command_mode {
            self.command_selected = self.command_selected.saturating_sub(KEYBOARD_PAGE_JUMP);
        } else if !self.items.is_empty() {
            self.selected = self.selected.saturating_sub(KEYBOARD_PAGE_JUMP);
        }
    }

    pub(crate) fn on_home(&mut self) {
        let suggestions = command_menu_items(&self.raw_query, self.tracking_enabled);
        let command_mode = !suggestions.is_empty();
        if command_mode {
            self.command_selected = 0;
        } else if !self.items.is_empty() {
            self.selected = 0;
        }
    }

    pub(crate) fn on_end(&mut self) {
        let suggestions = command_menu_items(&self.raw_query, self.tracking_enabled);
        let command_mode = !suggestions.is_empty();
        if command_mode {
            self.command_selected = suggestions.len() - 1;
        } else if !self.items.is_empty() {
            self.selected = self.items.len() - 1;
        }
    }

    pub(crate) fn on_alt_enter(&mut self) {
        if self.show_quick_help_overlay {
            return;
        }
        if let Some(item) = self.items.get(self.selected) {
            self.last_action = format!("Reveal: {}", item.path);
            let _ = reveal_path(item.path.as_ref());
        }
    }

    fn apply_raw_query(&mut self, raw_query: String, execute_directives: bool) {
        self.pending_query = None;
        self.needs_search_refresh = false;
        self.raw_query = raw_query;
        let command_invocation = self.raw_query.trim_start().starts_with('/');

        let parsed = parse_scope_directive(&self.raw_query);
        self.query = parsed.clean_query;

        if !execute_directives {
            let cmd = self.raw_query.trim_start();
            if !cmd.starts_with("/latest") && !cmd.starts_with("/last") {
                self.latest_only_mode = false;
            }
            self.schedule_search_from_current_query();
            return;
        }

        if parsed.test_progress {
            self.visual_progress_test_active = true;
            self.indexing_in_progress = true;
            self.indexing_progress = 0.0;
            self.last_action = "Running visual progress test".to_string();
            if command_invocation {
                self.clear_command_input();
            }
            return;
        }

        if parsed.exit_app {
            self.should_exit = true;
            if command_invocation {
                self.clear_command_input();
            }
            return;
        }

        if parsed.elevate_app {
            if self.is_elevated {
                self.last_action = "Already elevated".to_string();
                return;
            }

            match request_self_elevation(&SearchScope::EntireCurrentDrive) {
                Ok(()) => {
                    self.should_exit = true;
                    if command_invocation {
                        self.clear_command_input();
                    }
                    return;
                }
                Err(err) => {
                    self.last_action = err;
                    if command_invocation {
                        self.clear_command_input();
                    }
                    return;
                }
            }
        }

        if parsed.latest_only {
            if !self.tracking_enabled {
                self.last_action = "Tracking is off (use /track to enable)".to_string();
                if command_invocation {
                    self.clear_command_input();
                }
                return;
            }

            self.latest_only_mode = true;
            if let Some(window_secs) = parsed.latest_window_secs {
                self.latest_window_secs = window_secs;
            }
            self.query.clear();
            self.last_action = format!(
                "Showing files changed in last {}",
                format_latest_window(self.latest_window_secs)
            );
            self.schedule_search_from_current_query();
            if command_invocation {
                self.clear_command_input();
            }
            return;
        }

        if parsed.toggle_tracking {
            self.tracking_enabled = !self.tracking_enabled;
            self.latest_only_mode = false;
            self.recent_event_by_path.clear();
            if self.tracking_enabled {
                self.last_action = "Tracking enabled".to_string();
            } else {
                self.last_action = "Tracking disabled".to_string();
                self.changes_added_since_index = 0;
                self.changes_updated_since_index = 0;
                self.changes_deleted_since_index = 0;
            }
            if command_invocation {
                self.clear_command_input();
            }
            return;
        }

        if parsed.toggle_fullscreen {
            self.pending_window_mode_request = Some(WindowModeRequest::ToggleFullscreen);
            self.last_action = "Toggling fullscreen".to_string();
            if command_invocation {
                self.clear_command_input();
            }
            return;
        }

        if parsed.toggle_fullheight {
            self.pending_window_mode_request = Some(WindowModeRequest::ToggleFullHeight);
            self.last_action = "Toggling full-height mode".to_string();
            if command_invocation {
                self.clear_command_input();
            }
            return;
        }

        if parsed.switch_renderer_gpu {
            self.pending_renderer_mode_request = Some(RendererModeRequest::Gpu);
            self.last_action = "Switching renderer to GPU".to_string();
            if command_invocation {
                self.clear_command_input();
            }
            return;
        }

        if parsed.switch_renderer_soft {
            self.pending_renderer_mode_request = Some(RendererModeRequest::Soft);
            self.last_action = "Switching renderer to soft".to_string();
            if command_invocation {
                self.clear_command_input();
            }
            return;
        }

        if parsed.show_about {
            self.show_about_overlay = true;
            self.last_action = "Showing about info".to_string();
            if command_invocation {
                self.clear_command_input();
            }
            return;
        }

        if parsed.reindex_current_scope {
            self.latest_only_mode = false;
            self.query.clear();
            self.last_action = format!("Reindexing scope: {}", self.scope.label());
            self.begin_index(self.scope.clone());
            if command_invocation {
                self.clear_command_input();
            }
            return;
        }

        let cmd = self.raw_query.trim_start();
        if !cmd.starts_with("/latest") && !cmd.starts_with("/last") {
            self.latest_only_mode = false;
        }

        if let Some(new_scope) = parsed.scope_override {
            if self.indexing_in_progress && self.scope == new_scope {
                self.last_action = format!("Already indexing scope: {}", self.scope.label());
                if command_invocation {
                    self.clear_command_input();
                }
                return;
            }

            self.scope = new_scope;
            self.all_items.clear();
            self.items.clear();
            self.selected = 0;
            self.last_action = format!("Indexing scope: {}", self.scope.label());
            self.begin_index(self.scope.clone());
            if command_invocation {
                self.clear_command_input();
            }
            return;
        }

        self.schedule_search_from_current_query();
    }

    fn clear_command_input(&mut self) {
        self.raw_query.clear();
        self.query.clear();
        self.pending_query = None;
        self.command_selected = 0;
    }

    fn begin_index(&mut self, scope: SearchScope) {
        self.index_job_counter += 1;
        let job_id = self.index_job_counter;
        self.active_index_job = Some(job_id);
        self.scope = scope.clone();
        if self.skip_scope_persist_once {
            self.skip_scope_persist_once = false;
        } else {
            persist_scope(&self.scope);
        }
        self.visual_progress_test_active = false;
        self.indexing_in_progress = true;
        self.indexing_progress = 0.0;
        self.indexing_phase = "index";
        self.indexing_is_refresh = false;
        self.index_backend = IndexBackend::Detecting;
        self.index_memory_bytes = 0;
        self.filename_index_dirty = true;
        self.filename_index_building = false;
        self.filename_index_build_cursor = 0;
        self.cancel_active_search();
        let _ = self.search_tx.send(SearchWorkerMessage::Clear);
        self.needs_search_refresh = false;
        self.recent_event_by_path.clear();
        self.changes_added_since_index = 0;
        self.changes_updated_since_index = 0;
        self.changes_deleted_since_index = 0;

        let (tx, rx) = mpsc::channel::<IndexEvent>();
        self.index_rx = Some(rx);

        let allow_dirwalk_fallback = self.use_dirwalk_fallback;
        thread::spawn(move || {
            indexing::run_index_job(scope, job_id, tx, allow_dirwalk_fallback);
        });
    }

    pub(crate) fn process_tick(&mut self) -> TickOutcome {
        let mut out = TickOutcome {
            visibility_changed: false,
            focus_search: false,
            should_quit: false,
            window_mode_request: None,
            renderer_mode_request: None,
        };

        out.window_mode_request = self.pending_window_mode_request.take();
        out.renderer_mode_request = self.pending_renderer_mode_request.take();

        if self.visual_progress_test_active {
            self.indexing_in_progress = true;
            self.indexing_progress = (self.indexing_progress + 0.03).min(1.0);

            if self.indexing_progress >= 1.0 {
                self.visual_progress_test_active = false;
                self.indexing_in_progress = false;
                self.last_action = "Visual progress test complete".to_string();
            }
        }

        if self.panel_visible {
            if let Some((pending_query, due_at, edit_id)) = self.pending_query.clone() {
                if Instant::now() >= due_at && edit_id == self.query_edit_counter {
                    self.pending_query = None;
                    self.apply_raw_query(pending_query, false);
                }
            }

            if self.pending_query.is_some() {
                self.cancel_active_search();
            }

            if self.needs_search_refresh
                && self.pending_query.is_none()
                && Instant::now() >= self.next_search_refresh_at
            {
                self.needs_search_refresh = false;
                self.next_search_refresh_at = Instant::now() + DELTA_REFRESH_COOLDOWN;
                self.schedule_search_from_current_query();
            }

            if self.pending_query.is_none() {
                self.process_filename_index_build_step();
            }
        }

        for _ in 0..MAX_SEARCH_EVENTS_PER_TICK {
            let Ok(event) = self.search_rx.try_recv() else {
                break;
            };

            match event {
                SearchEvent::Progress {
                    generation,
                    scanned,
                    total,
                } => {
                    if self.active_search_job == Some(generation) {
                        self.active_search_cursor = scanned.min(total);
                    }
                }
                SearchEvent::Done { generation, items } => {
                    if self.active_search_job == Some(generation) {
                        self.items = items;
                        self.active_search_job = None;
                        self.active_search_query = None;
                        self.active_search_cursor = 0;
                        self.clamp_selected();
                    }
                }
            }
        }

        if let Some(rx) = &self.index_rx {
            let mut pending = Vec::new();
            for _ in 0..MAX_INDEX_EVENTS_PER_TICK {
                match rx.try_recv() {
                    Ok(event) => pending.push(event),
                    Err(_) => break,
                }
            }

            for event in pending {
                match event {
                    IndexEvent::SnapshotLoaded { job_id, items } => {
                        if self.active_index_job == Some(job_id) {
                            self.all_items = items;
                            self.indexing_is_refresh = true;
                            self.filename_index_dirty = true;
                            self.filename_index_building = false;
                            self.filename_index_build_cursor = 0;
                            self.recompute_index_memory_bytes();
                            self.push_corpus_to_search_worker();
                            self.schedule_search_from_current_query();
                            self.last_action = format!(
                                "Loaded snapshot: {} items [{}]",
                                self.all_items.len(),
                                self.scope.label()
                            );
                        }
                    }
                    IndexEvent::Progress {
                        job_id,
                        current,
                        total,
                        phase,
                    } => {
                        if self.active_index_job == Some(job_id) {
                            self.indexing_in_progress = true;
                            self.indexing_phase = phase;
                            self.indexing_progress = if total == 0 {
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
                        if self.active_index_job == Some(job_id) {
                            self.indexing_in_progress = false;
                            self.indexing_progress = 1.0;
                            self.indexing_phase = "done";
                            self.index_backend = backend;
                            self.all_items = items;
                            self.filename_index_dirty = true;
                            self.filename_index_building = false;
                            self.filename_index_build_cursor = 0;
                            self.recompute_index_memory_bytes();
                            self.recent_event_by_path.clear();
                            self.changes_added_since_index = 0;
                            self.changes_updated_since_index = 0;
                            self.changes_deleted_since_index = 0;
                            self.push_corpus_to_search_worker();
                            if self.all_items.is_empty() && backend == IndexBackend::Detecting {
                                self.last_action = "NTFS indexing unavailable (run elevated and ensure USN journal is available)".to_string();
                            } else {
                                self.last_action = format!(
                                    "Indexed {} files [{}]",
                                    self.all_items.len(),
                                    self.scope.label()
                                );
                            }
                            self.schedule_search_from_current_query();
                            out.focus_search = true;
                        }
                    }
                    IndexEvent::Delta {
                        job_id,
                        upserts,
                        deleted_paths,
                    } => {
                        if self.active_index_job == Some(job_id) {
                            let (added, updated, deleted) =
                                self.apply_index_delta(upserts, deleted_paths);
                            self.changes_added_since_index += added;
                            self.changes_updated_since_index += updated;
                            self.changes_deleted_since_index += deleted;
                            self.recompute_index_memory_bytes();
                            self.indexing_in_progress = false;
                            self.indexing_progress = 1.0;
                            self.indexing_phase = "live";
                            self.last_action = format!(
                                "Live index update: {} items [{}]",
                                self.all_items.len(),
                                self.scope.label()
                            );
                        }
                    }
                }
            }
        }

        if self._hotkey_manager.is_none() || self._hotkey.is_none() {
            let should_retry = self
                .hotkey_retry_after
                .is_none_or(|due| Instant::now() >= due);
            if should_retry {
                match init_hotkey() {
                    Ok((manager, hotkey)) => {
                        self._hotkey_manager = manager;
                        self._hotkey = hotkey;
                        self.hotkey_retry_after = None;
                        self.last_action = "Global hotkey ready".to_string();
                    }
                    Err(err) => {
                        debug_log(&format!("hotkey retry failed: {}", err));
                        self.hotkey_retry_after =
                            Some(Instant::now() + Duration::from_millis(1200));
                    }
                }
            }
        }

        let mut toggled = false;
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if let Some(hotkey) = &self._hotkey {
                if event.id == hotkey.id() {
                    toggled = true;
                }
            }
        }

        while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            if self
                .menu_toggle_id
                .as_ref()
                .is_some_and(|id| event.id == *id)
            {
                toggled = true;
            }
            if self.menu_quit_id.as_ref().is_some_and(|id| event.id == *id) {
                out.should_quit = true;
            }
        }

        if toggled {
            if let Some(last) = self.last_toggle_at {
                if last.elapsed() < Duration::from_millis(220) {
                    return out;
                }
            }
            self.last_toggle_at = Some(Instant::now());
            self.panel_visible = !self.panel_visible;
            if self.panel_visible {
                self.schedule_search_from_current_query();
                out.focus_search = true;
            }
            out.visibility_changed = true;
        }

        if self.should_exit {
            out.should_quit = true;
        }

        out
    }

    fn recompute_index_memory_bytes(&mut self) {
        self.index_memory_bytes = estimate_index_memory_bytes(&self.all_items);
    }

    fn push_corpus_to_search_worker(&self) {
        let _ = self.search_tx.send(SearchWorkerMessage::SetCorpus {
            items: self.all_items.clone(),
            recent_event_by_path: self.recent_event_by_path.clone(),
        });
    }

    fn cancel_active_search(&mut self) {
        self.active_search_job = None;
        self.active_search_query = None;
        self.active_search_cursor = 0;
        let _ = self.search_tx.send(SearchWorkerMessage::Cancel);
    }

    fn schedule_search_from_current_query(&mut self) {
        if !self.panel_visible {
            self.cancel_active_search();
            return;
        }

        let q = self.query.trim().to_ascii_lowercase();

        if q.is_empty() && !self.latest_only_mode {
            self.items = self
                .all_items
                .iter()
                .take(VISIBLE_RESULTS_LIMIT)
                .cloned()
                .collect();
            self.cancel_active_search();
            self.clamp_selected();
        } else {
            if !self.latest_only_mode {
                if let Some(results) = self.try_fast_filename_search(&q) {
                    self.items = results;
                    self.cancel_active_search();
                    self.clamp_selected();
                    return;
                }
            }

            self.search_generation = self.search_generation.wrapping_add(1);
            let generation = self.search_generation;
            self.active_search_job = Some(generation);
            self.active_search_query = Some(q);
            self.active_search_cursor = 0;
            let _ = self.search_tx.send(SearchWorkerMessage::Run {
                generation,
                query: self.query.trim().to_ascii_lowercase(),
                latest_only_mode: self.latest_only_mode,
                latest_window_secs: self.latest_window_secs,
            });
        }
    }

    fn process_filename_index_build_step(&mut self) {
        if !self.filename_index_dirty {
            return;
        }

        if !self.filename_index_building {
            self.filename_exact_index.clear();
            self.filename_prefix_index.clear();
            self.filename_index_build_cursor = 0;
            self.filename_index_building = true;
        }

        let end = (self.filename_index_build_cursor + FILENAME_INDEX_BUILD_BATCH)
            .min(self.all_items.len());
        for index in self.filename_index_build_cursor..end {
            let item = &self.all_items[index];
            let name_lower = file_name_from_path(item.path.as_ref()).to_ascii_lowercase();
            self.filename_exact_index
                .entry(name_lower.clone())
                .or_default()
                .push(index);

            let mut prefix = String::new();
            for ch in name_lower.chars().take(3) {
                prefix.push(ch);
                self.filename_prefix_index
                    .entry(prefix.clone())
                    .or_default()
                    .push(index);
            }
        }

        self.filename_index_build_cursor = end;
        if self.filename_index_build_cursor >= self.all_items.len() {
            self.filename_index_dirty = false;
            self.filename_index_building = false;
            self.filename_index_build_cursor = 0;
        }
    }

    fn try_fast_filename_search(&mut self, query_lower: &str) -> Option<Vec<SearchItem>> {
        if query_lower.is_empty()
            || query_lower.contains('*')
            || query_lower.contains('?')
            || query_lower.contains('\\')
            || query_lower.contains('/')
            || query_lower.contains(':')
        {
            return None;
        }

        if self.filename_index_dirty || self.filename_index_building {
            return None;
        }

        let mut out = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();

        if let Some(exact) = self.filename_exact_index.get(query_lower) {
            for &idx in exact {
                if seen.insert(idx) {
                    out.push(self.all_items[idx].clone());
                    if out.len() >= VISIBLE_RESULTS_LIMIT {
                        return Some(out);
                    }
                }
            }
        }

        let mut prefix_key = String::new();
        for ch in query_lower.chars().take(3) {
            prefix_key.push(ch);
        }

        if let Some(candidates) = self.filename_prefix_index.get(&prefix_key) {
            for &idx in candidates {
                if seen.contains(&idx) {
                    continue;
                }

                let name = file_name_from_path(self.all_items[idx].path.as_ref());
                if contains_ascii_case_insensitive(name, query_lower) {
                    seen.insert(idx);
                    out.push(self.all_items[idx].clone());
                    if out.len() >= VISIBLE_RESULTS_LIMIT {
                        break;
                    }
                }
            }
        }

        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn clamp_selected(&mut self) {
        if self.items.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.items.len() - 1);
        }
    }

    fn apply_index_delta(
        &mut self,
        upserts: Vec<SearchItem>,
        deleted_paths: Vec<String>,
    ) -> (usize, usize, usize) {
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let mut deleted_count = 0usize;
        if !deleted_paths.is_empty() {
            let delete_set: HashSet<String> = deleted_paths.into_iter().collect();
            if self.tracking_enabled {
                deleted_count = delete_set.len();
                for path in &delete_set {
                    self.recent_event_by_path.remove(path.as_str());
                }
            }
            self.all_items
                .retain(|item| !delete_set.contains(item.path.as_ref()));
        }

        let mut added_count = 0usize;
        let mut updated_count = 0usize;
        for upsert in upserts {
            if self.tracking_enabled {
                let event_ts = if upsert.modified_unix_secs == UNKNOWN_TS {
                    now_unix
                } else {
                    upsert.modified_unix_secs
                };
                self.recent_event_by_path
                    .insert(upsert.path.clone(), event_ts);
            }
            if let Some(existing) = self
                .all_items
                .iter_mut()
                .find(|item| item.path == upsert.path)
            {
                *existing = upsert;
                if self.tracking_enabled {
                    updated_count += 1;
                }
            } else {
                self.all_items.push(upsert);
                if self.tracking_enabled {
                    added_count += 1;
                }
            }
        }

        self.needs_search_refresh = self.latest_only_mode || self.query.trim().is_empty();
        self.filename_index_dirty = true;
        self.filename_index_building = false;
        self.filename_index_build_cursor = 0;
        (added_count, updated_count, deleted_count)
    }
}

fn init_hotkey() -> Result<(Option<GlobalHotKeyManager>, Option<HotKey>), String> {
    let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
    let hotkey = HotKey::new(Some(Modifiers::empty()), Code::Backquote);

    manager.register(hotkey).map_err(|e| e.to_string())?;

    Ok((Some(manager), Some(hotkey)))
}

fn init_tray() -> Result<(Option<TrayIcon>, Option<MenuId>, Option<MenuId>), String> {
    let icon = build_tray_icon()?;
    let menu = Menu::new();
    let toggle = MenuItem::new("Show/Hide", true, None);
    let quit = MenuItem::new("Quit", true, None);

    menu.append(&toggle).map_err(|e| e.to_string())?;
    menu.append(&quit).map_err(|e| e.to_string())?;

    let tray = TrayIconBuilder::new()
        .with_tooltip("RustSearch")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()
        .map_err(|e| e.to_string())?;

    Ok((
        Some(tray),
        Some(toggle.id().clone()),
        Some(quit.id().clone()),
    ))
}

fn build_tray_icon() -> Result<Icon, String> {
    let width = 16;
    let height = 16;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);

    for y in 0..height {
        for x in 0..width {
            let edge = x == 0 || x == width - 1 || y == 0 || y == height - 1;
            let body = x == y || (x + y) == (width - 1);

            let (r, g, b, a) = if edge {
                (26, 35, 46, 255)
            } else if body {
                (125, 207, 255, 255)
            } else {
                (15, 19, 24, 255)
            };

            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    Icon::from_rgba(rgba, width, height).map_err(|e| e.to_string())
}
