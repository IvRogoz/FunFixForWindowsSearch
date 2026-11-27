use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
use tray_icon::menu::{Menu, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

type TrayInit = (Option<TrayIcon>, Option<MenuId>, Option<MenuId>);

use crate::commands::{
    apply_command_choice, command_menu_items, format_latest_window, is_exact_directive_token,
    parse_scope_directive,
};
use crate::indexing;
use crate::platform::{is_process_elevated, open_path, request_self_elevation, reveal_path};
use crate::search::query_has_incomplete_boolean_logic;
use crate::search_worker::{SearchEvent, SearchWorkerMessage};
use crate::storage::{
    load_persisted_scope, load_quick_help_dismissed, persist_quick_help_dismissed, persist_scope,
};
use crate::{
    debug_log, estimate_index_memory_bytes, IndexBackend, IndexEvent, RendererModeRequest,
    SearchItem, SearchScope, WindowModeRequest, DEFAULT_LATEST_WINDOW_SECS, DEFAULT_RESULT_ROWS,
    DELTA_REFRESH_COOLDOWN, KEYBOARD_PAGE_JUMP, MAX_INDEX_EVENTS_PER_TICK, MAX_RESULT_ROWS,
    MAX_SEARCH_EVENTS_PER_TICK, MIN_RESULT_ROWS, QUERY_DEBOUNCE_DELAY, UNKNOWN_TS,
    VISIBLE_RESULTS_LIMIT,
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
    item_positions: HashMap<Box<str>, usize>,
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
    index_cancel: Option<Arc<AtomicBool>>,
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
    pub(crate) show_privilege_overlay: bool,
    pub(crate) show_quick_help_overlay: bool,
    pub(crate) show_about_overlay: bool,
    pub(crate) show_startup_about: bool,
    startup_about_deadline: Option<Instant>,
    pub(crate) quick_help_selected_action: usize,
    pub(crate) pending_query: Option<(String, Instant, u64)>,
    pub(crate) query_edit_counter: u64,
    pub(crate) search_tx: mpsc::Sender<SearchWorkerMessage>,
    pub(crate) search_rx: mpsc::Receiver<SearchEvent>,
    pub(crate) search_generation: u64,
    pub(crate) active_search_job: Option<u64>,
    pub(crate) active_search_query: Option<String>,
    pub(crate) active_search_cursor: usize,
    pub(crate) needs_search_refresh: bool,
    pub(crate) next_search_refresh_at: Instant,
    pub(crate) latest_only_mode: bool,
    pub(crate) latest_window_secs: i64,
    pub(crate) tracking_enabled: bool,
    pub(crate) result_rows: usize,
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
            item_positions: HashMap::new(),
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
            index_cancel: None,
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
            show_privilege_overlay: !is_elevated,
            show_quick_help_overlay: is_elevated && !load_quick_help_dismissed(),
            show_about_overlay: false,
            show_startup_about: true,
            startup_about_deadline: None,
            quick_help_selected_action: 0,
            pending_query: None,
            query_edit_counter: 0,
            search_tx,
            search_rx,
            search_generation: 0,
            active_search_job: None,
            active_search_query: None,
            active_search_cursor: 0,
            needs_search_refresh: false,
            next_search_refresh_at: Instant::now(),
            latest_only_mode: false,
            latest_window_secs: DEFAULT_LATEST_WINDOW_SECS,
            tracking_enabled: true,
            result_rows: DEFAULT_RESULT_ROWS,
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

    pub(crate) fn update_startup_about(&mut self, now: Instant) -> Option<Duration> {
        if !self.show_startup_about || !self.panel_visible {
            return None;
        }
        // Start the two seconds on the first visible frame, after initialization.
        let deadline = *self
            .startup_about_deadline
            .get_or_insert(now + Duration::from_secs(2));
        if now >= deadline {
            self.show_startup_about = false;
            return None;
        }
        Some(deadline.duration_since(now))
    }

    pub(crate) fn about_lines(&self) -> Vec<&'static str> {
        let mut lines = crate::ABOUT_LINES.to_vec();
        if self.show_startup_about {
            *lines.last_mut().unwrap() =
                "Closes after 2 seconds | Indexing continues in the background";
        }
        lines
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
        self.items.clear();
        self.selected = 0;
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
        self.activate_selected_with(open_path);
    }

    fn activate_selected_with(&mut self, open: impl FnOnce(&str) -> Result<(), String>) {
        if self.show_quick_help_overlay && !self.show_startup_about {
            if self.quick_help_selected_action == 0 {
                self.show_quick_help_overlay = false;
            } else {
                self.show_quick_help_overlay = false;
                persist_quick_help_dismissed(true);
            }
            return;
        }

        let suggestions = command_menu_items(&self.raw_query, self.tracking_enabled);
        let first_token = self.raw_query.split_whitespace().next().unwrap_or("");

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
        } else if self.results_ready() {
            if let Some(item) = self.items.get(self.selected) {
                self.last_action = path_action_status("Open", &item.path, open(&item.path));
            }
        }
    }

    pub(crate) fn on_escape(&mut self) {
        if self.show_privilege_overlay && !self.show_startup_about {
            self.show_privilege_overlay = false;
            return;
        }
        if self.show_about_overlay && !self.show_startup_about {
            self.show_about_overlay = false;
            return;
        }
        if self.show_quick_help_overlay && !self.show_startup_about {
            self.show_quick_help_overlay = false;
            return;
        }
        self.panel_visible = false;
    }

    pub(crate) fn on_move_down(&mut self) {
        if self.show_quick_help_overlay && !self.show_startup_about {
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
        if self.show_quick_help_overlay && !self.show_startup_about {
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
        self.reveal_selected_with(reveal_path);
    }

    fn reveal_selected_with(&mut self, reveal: impl FnOnce(&str) -> Result<(), String>) {
        if (self.show_quick_help_overlay && !self.show_startup_about) || !self.results_ready() {
            return;
        }
        if let Some(item) = self.items.get(self.selected) {
            self.last_action = path_action_status("Reveal", &item.path, reveal(&item.path));
        }
    }

    fn results_ready(&self) -> bool {
        self.pending_query.is_none()
            && self.active_search_job.is_none()
            && !self.needs_search_refresh
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

            match request_self_elevation(&self.scope) {
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
            let _ = self.search_tx.send(SearchWorkerMessage::ClearRecentEvents);
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

        if parsed.result_rows_directive {
            if let Some(rows) = parsed.result_rows {
                self.result_rows = rows.clamp(MIN_RESULT_ROWS, MAX_RESULT_ROWS);
                self.pending_window_mode_request =
                    Some(WindowModeRequest::SetResultRows(self.result_rows));
                self.last_action = format!("Showing {} result rows", self.result_rows);
            } else {
                self.last_action =
                    format!("Usage: /rows N ({}-{})", MIN_RESULT_ROWS, MAX_RESULT_ROWS);
            }
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
            self.item_positions.clear();
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
        if !self.should_exit
            && self.active_search_job.is_none()
            && (!self.indexing_in_progress || self.indexing_is_refresh)
        {
            self.schedule_search_from_current_query();
        }
    }

    fn begin_index(&mut self, scope: SearchScope) {
        let cancel = self.replace_index_cancellation();
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
        self.cancel_active_search();
        self.items.clear();
        self.selected = 0;
        let _ = self.search_tx.send(SearchWorkerMessage::Clear);
        self.needs_search_refresh = false;
        self.recent_event_by_path.clear();
        self.changes_added_since_index = 0;
        self.changes_updated_since_index = 0;
        self.changes_deleted_since_index = 0;

        let (tx, rx) = mpsc::channel::<IndexEvent>();
        self.index_rx = Some(rx);

        thread::spawn(move || {
            indexing::run_index_job(scope, job_id, tx, cancel);
        });
    }

    fn replace_index_cancellation(&mut self) -> Arc<AtomicBool> {
        if let Some(previous) = self.index_cancel.take() {
            previous.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.index_cancel = Some(Arc::clone(&cancel));
        cancel
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

        self.process_index_events(&mut out);

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
                if self.needs_search_refresh || self.items.is_empty() {
                    self.schedule_search_from_current_query();
                }
                out.focus_search = true;
            }
            out.visibility_changed = true;
        }

        if self.should_exit {
            out.should_quit = true;
        }

        out
    }

    fn process_index_events(&mut self, out: &mut TickOutcome) {
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
                            self.item_positions = crate::corpus::item_positions(&self.all_items);
                            self.indexing_is_refresh = true;
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
                            self.item_positions = crate::corpus::item_positions(&self.all_items);
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
            self.needs_search_refresh = true;
            return;
        }

        if self.pending_query.is_some() {
            self.needs_search_refresh = true;
            return;
        }
        self.needs_search_refresh = false;

        let q = self.query.trim().to_ascii_lowercase();

        if query_has_incomplete_boolean_logic(&q) && !self.latest_only_mode {
            self.items.clear();
            self.cancel_active_search();
            self.clamp_selected();
            return;
        }

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
            self.items.clear();
            self.selected = 0;
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

        let deleted_paths: Vec<Box<str>> = deleted_paths
            .into_iter()
            .map(String::into_boxed_str)
            .collect();
        for path in &deleted_paths {
            self.recent_event_by_path.remove(path.as_ref());
        }
        for upsert in &upserts {
            if self.tracking_enabled {
                let event_ts = if upsert.modified_unix_secs == UNKNOWN_TS {
                    now_unix
                } else {
                    upsert.modified_unix_secs
                };
                self.recent_event_by_path
                    .insert(upsert.path.clone(), event_ts);
            }
        }

        self.cancel_active_search();
        let _ = self.search_tx.send(SearchWorkerMessage::ApplyDelta {
            upserts: upserts.clone(),
            deleted_paths: deleted_paths.clone(),
            tracking_enabled: self.tracking_enabled,
            now_unix,
        });
        let stats = crate::corpus::apply_delta(
            &mut self.all_items,
            &mut self.item_positions,
            upserts,
            &deleted_paths,
        );
        self.index_memory_bytes = self
            .index_memory_bytes
            .saturating_add(stats.bytes_added)
            .saturating_sub(stats.bytes_removed);
        self.needs_search_refresh = true;
        if self.tracking_enabled {
            (stats.added, stats.updated, stats.deleted)
        } else {
            (0, 0, 0)
        }
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        if let Some(cancel) = &self.index_cancel {
            cancel.store(true, Ordering::Release);
        }
    }
}

fn path_action_status(action: &str, path: &str, result: Result<(), String>) -> String {
    match result {
        Ok(()) => format!("{action}: {path}"),
        Err(error) => format!("{action} failed: {path}: {error}"),
    }
}

#[cfg(test)]
impl AppState {
    // No hotkeys, tray, indexing, filesystem settings, or native windows in tests.
    pub(crate) fn test_state() -> (Self, mpsc::Receiver<SearchWorkerMessage>) {
        let (search_tx, requests) = mpsc::channel();
        let (_, search_rx) = mpsc::channel();
        (
            Self {
                raw_query: String::new(),
                query: String::new(),
                all_items: Vec::new(),
                item_positions: HashMap::new(),
                items: Vec::new(),
                selected: 0,
                last_action: String::new(),
                panel_visible: true,
                _hotkey_manager: None,
                _hotkey: None,
                _tray_icon: None,
                menu_toggle_id: None,
                menu_quit_id: None,
                last_toggle_at: None,
                scope: SearchScope::CurrentFolder,
                command_selected: 0,
                index_rx: None,
                index_cancel: None,
                index_job_counter: 0,
                active_index_job: None,
                indexing_in_progress: false,
                indexing_progress: 0.0,
                indexing_phase: "index",
                index_backend: IndexBackend::Detecting,
                index_memory_bytes: 0,
                visual_progress_test_active: false,
                indexing_is_refresh: false,
                is_elevated: false,
                show_privilege_overlay: false,
                show_quick_help_overlay: false,
                show_about_overlay: false,
                show_startup_about: false,
                startup_about_deadline: None,
                quick_help_selected_action: 0,
                pending_query: None,
                query_edit_counter: 0,
                search_tx,
                search_rx,
                search_generation: 0,
                active_search_job: None,
                active_search_query: None,
                active_search_cursor: 0,
                needs_search_refresh: false,
                next_search_refresh_at: Instant::now(),
                latest_only_mode: false,
                latest_window_secs: DEFAULT_LATEST_WINDOW_SECS,
                tracking_enabled: true,
                result_rows: DEFAULT_RESULT_ROWS,
                recent_event_by_path: HashMap::new(),
                changes_added_since_index: 0,
                changes_updated_since_index: 0,
                changes_deleted_since_index: 0,
                hotkey_retry_after: None,
                skip_scope_persist_once: true,
                should_exit: false,
                pending_window_mode_request: None,
                pending_renderer_mode_request: None,
            },
            requests,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SearchItemKind;

    fn item(path: &str) -> SearchItem {
        SearchItem {
            path: path.into(),
            modified_unix_secs: UNKNOWN_TS,
            kind: SearchItemKind::File,
        }
    }

    #[test]
    fn startup_about_expires_after_two_seconds_while_indexing_continues() {
        let (mut app, _) = AppState::test_state();
        app.show_startup_about = true;
        app.show_privilege_overlay = true;
        app.active_index_job = Some(1);
        let start = Instant::now();
        assert_eq!(
            app.update_startup_about(start),
            Some(Duration::from_secs(2))
        );

        let (tx, rx) = mpsc::channel();
        app.index_rx = Some(rx);
        let mut outcome = TickOutcome {
            visibility_changed: false,
            focus_search: false,
            should_quit: false,
            window_mode_request: None,
            renderer_mode_request: None,
        };
        tx.send(IndexEvent::Progress {
            job_id: 1,
            current: 5,
            total: 10,
            phase: "index",
        })
        .unwrap();
        app.process_index_events(&mut outcome);
        assert_eq!(app.indexing_progress, 0.5);
        assert!(app.indexing_in_progress);
        assert_eq!(
            app.update_startup_about(start + Duration::from_millis(1999)),
            Some(Duration::from_millis(1))
        );
        assert!(app.show_startup_about);

        tx.send(IndexEvent::Done {
            job_id: 1,
            items: vec![item("indexed-during-splash")],
            backend: IndexBackend::WalkDir,
        })
        .unwrap();
        app.process_index_events(&mut outcome);
        assert!(!app.indexing_in_progress);
        assert_eq!(app.all_items[0].path.as_ref(), "indexed-during-splash");
        assert!(app.show_startup_about);
        assert_eq!(
            app.update_startup_about(start + Duration::from_secs(2)),
            None
        );
        assert!(!app.show_startup_about);
        assert!(app.show_privilege_overlay);

        app.show_privilege_overlay = false;
        app.apply_raw_query("/about".into(), true);
        app.update_startup_about(start + Duration::from_secs(10));
        assert!(app.show_about_overlay);
        assert!(!app.show_startup_about);
    }

    #[test]
    fn hidden_start_waits_for_first_visible_frame_before_timing_about() {
        let (mut app, _) = AppState::test_state();
        app.show_startup_about = true;
        app.show_quick_help_overlay = true;
        app.panel_visible = false;
        let start = Instant::now();
        assert_eq!(app.update_startup_about(start), None);
        assert!(app.startup_about_deadline.is_none());
        app.panel_visible = true;
        let shown_at = start + Duration::from_secs(10);
        assert_eq!(
            app.update_startup_about(shown_at),
            Some(Duration::from_secs(2))
        );
        app.update_startup_about(shown_at + Duration::from_secs(2));
        assert!(!app.show_startup_about);
        assert!(app.show_quick_help_overlay);
        app.panel_visible = false;
        app.panel_visible = true;
        assert_eq!(
            app.update_startup_about(shown_at + Duration::from_secs(3)),
            None
        );
        assert!(!app.show_startup_about);
    }

    #[test]
    fn hidden_completed_reindex_refreshes_existing_results_on_reopen() {
        let (mut app, _) = AppState::test_state();
        app.panel_visible = false;
        app.items = vec![item("old")];
        app.active_index_job = Some(2);
        let (tx, rx) = mpsc::channel();
        app.index_rx = Some(rx);
        tx.send(IndexEvent::Done {
            job_id: 2,
            items: vec![item("new")],
            backend: IndexBackend::WalkDir,
        })
        .unwrap();
        let mut outcome = TickOutcome {
            visibility_changed: false,
            focus_search: false,
            should_quit: false,
            window_mode_request: None,
            renderer_mode_request: None,
        };
        app.process_index_events(&mut outcome);
        assert!(app.needs_search_refresh);
        assert!(!app.results_ready());
        app.panel_visible = true;
        app.schedule_search_from_current_query();
        assert_eq!(app.items[0].path.as_ref(), "new");
        assert!(!app.needs_search_refresh);
    }

    #[test]
    fn query_edit_and_active_search_cannot_activate_stale_results() {
        let (mut app, _) = AppState::test_state();
        app.items = vec![item("old")];
        app.on_query_changed("new".into());
        assert!(app.items.is_empty());
        // Even a retained result cannot bypass the pending-query guard.
        app.items = vec![item("old")];
        app.activate_selected_with(|_| panic!("opened during debounce"));
        app.reveal_selected_with(|_| panic!("revealed during debounce"));
        app.apply_raw_query("new".into(), false);
        assert!(app.items.is_empty());
        assert!(app.active_search_job.is_some());
        app.items = vec![item("old")];
        app.activate_selected_with(|_| panic!("opened during search"));
        app.reveal_selected_with(|_| panic!("revealed during search"));
    }

    #[test]
    fn commands_still_execute_during_query_debounce() {
        let (mut app, _) = AppState::test_state();
        app.all_items = vec![item("available.txt")];
        app.on_query_changed("/rows 8".into());
        app.activate_selected_with(|_| panic!("command opened a result"));
        assert_eq!(app.result_rows, 8);
        assert!(matches!(
            app.pending_window_mode_request,
            Some(WindowModeRequest::SetResultRows(8))
        ));
        assert_eq!(app.items[0].path.as_ref(), "available.txt");
    }

    #[test]
    fn failed_open_and_reveal_are_reported_in_status() {
        let (mut app, _) = AppState::test_state();
        app.items = vec![item("missing.txt")];
        app.activate_selected_with(|_| Err("file not found".into()));
        assert_eq!(app.last_action, "Open failed: missing.txt: file not found");
        app.reveal_selected_with(|_| Err("Explorer unavailable".into()));
        assert_eq!(
            app.last_action,
            "Reveal failed: missing.txt: Explorer unavailable"
        );
        app.activate_selected_with(|_| Ok(()));
        assert_eq!(app.last_action, "Open: missing.txt");
    }

    #[test]
    fn replacing_or_dropping_app_cancels_index_jobs() {
        let (mut app, _) = AppState::test_state();
        let first = app.replace_index_cancellation();
        let second = app.replace_index_cancellation();
        assert!(first.load(Ordering::Acquire));
        assert!(!second.load(Ordering::Acquire));
        drop(app);
        assert!(second.load(Ordering::Acquire));
    }

    #[test]
    fn live_delta_sends_only_changes_and_updates_memory_and_counts() {
        let (mut app, requests) = AppState::test_state();
        app.all_items = vec![item("a"), item("b"), item("c")];
        app.item_positions = crate::corpus::item_positions(&app.all_items);
        app.recompute_index_memory_bytes();
        assert_eq!(
            app.apply_index_delta(
                vec![item("b"), item("new")],
                vec!["a".into(), "missing".into()]
            ),
            (1, 1, 1)
        );
        assert_eq!(
            app.index_memory_bytes,
            estimate_index_memory_bytes(&app.all_items)
        );
        let mut deltas = 0;
        for request in requests.try_iter() {
            match request {
                SearchWorkerMessage::ApplyDelta {
                    upserts,
                    deleted_paths,
                    ..
                } => {
                    assert_eq!(upserts.len(), 2);
                    assert_eq!(deleted_paths.len(), 2);
                    deltas += 1;
                }
                SearchWorkerMessage::SetCorpus { .. } => panic!("delta copied entire corpus"),
                _ => {}
            }
        }
        assert_eq!(deltas, 1);
        assert!(app.needs_search_refresh);
        app.activate_selected_with(|_| panic!("activated stale live result"));
    }
}

fn init_hotkey() -> Result<(Option<GlobalHotKeyManager>, Option<HotKey>), String> {
    let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
    let hotkey = HotKey::new(Some(Modifiers::empty()), Code::Backquote);

    manager.register(hotkey).map_err(|e| e.to_string())?;

    Ok((Some(manager), Some(hotkey)))
}

fn init_tray() -> Result<TrayInit, String> {
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
