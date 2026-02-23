#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod commands;
mod search;

use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;
use std::{env, process::Command};
use std::{io::Write, sync::OnceLock};
use std::{sync::mpsc, thread};

use serde::{Deserialize, Serialize};
#[cfg(target_os = "windows")]
use std::collections::HashSet;
#[cfg(target_os = "windows")]
use std::ffi::c_void;
#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
use iced::alignment::Horizontal;
use iced::keyboard::{key, Event as KeyboardEvent, Key};
use iced::widget::operation;
use iced::widget::{
    self, button, column, container, progress_bar, row, scrollable, stack, text, text_input,
};
use iced::window;
use iced::{
    Alignment, Color, Element, Fill, Font, Length, Padding, Point, Size, Subscription, Task, Theme,
};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use walkdir::WalkDir;

use commands::{
    apply_command_choice, command_menu_items, format_latest_window, is_exact_directive_token,
    parse_scope_directive, scope_arg_value,
};
use search::{
    contains_ascii_case_insensitive, file_name_from_path, file_type_color, query_matches_item,
    truncate_middle,
};

#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_HANDLE_EOF, ERROR_INVALID_FUNCTION, HANDLE,
    INVALID_HANDLE_VALUE,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0,
    READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_NEW_NAME, USN_RECORD_V2,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::IO::DeviceIoControl;
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWDEFAULT;

const PANEL_WIDTH: f32 = 980.0;
const PANEL_HEIGHT: f32 = 560.0;
const PANEL_WIDTH_RATIO: f32 = 0.5;
const VISIBLE_RESULTS_LIMIT: usize = 600;
const QUERY_DEBOUNCE_DELAY: Duration = Duration::from_millis(140);
const SEARCH_BATCH_SIZE: usize = 40_000;
const SEARCH_TIME_BUDGET_PER_TICK: Duration = Duration::from_millis(22);
const FILENAME_INDEX_BUILD_BATCH: usize = 20_000;
const DEFAULT_LATEST_WINDOW_SECS: i64 = 5 * 60;
const DELTA_REFRESH_COOLDOWN: Duration = Duration::from_millis(300);
const PANEL_ANIMATION_DURATION: Duration = Duration::from_millis(180);
const FILE_NAME_FONT_SIZE: u32 = 14;
const FILE_PATH_FONT_SIZE: u32 = 12;
const FILE_PATH_MAX_CHARS: usize = 86;
const MAX_INDEX_EVENTS_PER_TICK: usize = 16;
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const UNKNOWN_TS: i64 = i64::MIN;
const KEYBOARD_PAGE_JUMP: usize = 12;
const CONSOLAS_REGULAR: &[u8] = include_bytes!("../assets/fonts/consola.ttf");
const CONSOLAS_BOLD: &[u8] = include_bytes!("../assets/fonts/consolab.ttf");

static DEBUG_LOG_FILES: OnceLock<std::sync::Mutex<Vec<std::fs::File>>> = OnceLock::new();
static DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();

fn main() -> iced::Result {
    let _ = DEBUG_ENABLED.set(env::var("WIZMINI_DEBUG").ok().as_deref() == Some("1"));
    let _ = init_debug_log_file();
    std::panic::set_hook(Box::new(|info| {
        debug_log(&format!("panic: {}", info));
    }));

    let start_visible = should_start_visible_from_args();

    iced::application(
        move || {
            let app = App::default();
            let should_start_visible = start_visible || app.show_quick_help_overlay;
            let initial_scope = app.scope.clone();

            let mut tasks = vec![Task::done(Message::StartIndex(initial_scope))];

            if should_start_visible {
                tasks.push(prepare_panel_for_show_mode());
                tasks.push(sync_window_to_progress(1.0));
                tasks.push(operation::focus(app.search_input_id.clone()));
                tasks.push(operation::move_cursor_to_end(app.search_input_id.clone()));
            } else {
                tasks.push(initialize_panel_hidden_mode());
            }

            let mut app = app;
            if should_start_visible {
                app.panel_visible = true;
                app.panel_progress = 1.0;
            }

            (app, Task::batch(tasks))
        },
        update,
        view,
    )
    .title("WizMini")
    .font(CONSOLAS_REGULAR)
    .font(CONSOLAS_BOLD)
    .default_font(Font::with_name("Consolas"))
    .theme(theme)
    .window(native_window_settings())
    .subscription(subscription)
    .run()
}

fn should_start_visible_from_args() -> bool {
    env::args().any(|arg| arg == "--show")
}

fn startup_scope_override_from_args() -> Option<SearchScope> {
    for arg in env::args() {
        let Some(value) = arg.strip_prefix("--scope=") else {
            continue;
        };

        let lower = value.trim().to_ascii_lowercase();
        if lower == "current-folder" {
            return Some(SearchScope::CurrentFolder);
        }
        if lower == "entire-current-drive" {
            return Some(SearchScope::EntireCurrentDrive);
        }
        if lower == "all-local-drives" {
            return Some(SearchScope::AllLocalDrives);
        }

        let bytes = lower.as_bytes();
        if bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            return Some(SearchScope::Drive((bytes[0] as char).to_ascii_uppercase()));
        }
    }

    None
}

fn debug_log_path_localappdata() -> std::path::PathBuf {
    let base = env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(base)
        .join("WizMini")
        .join("wizmini-debug.log")
}

fn debug_log_path_exe_dir() -> std::path::PathBuf {
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|v| v.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    exe_dir.join("wizmini-debug.log")
}

fn init_debug_log_file() -> Result<(), String> {
    let mut files = Vec::new();
    let mut opened_paths = Vec::new();

    for path in [debug_log_path_localappdata(), debug_log_path_exe_dir()] {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
        {
            files.push(file);
            opened_paths.push(path.display().to_string());
        }
    }

    if files.is_empty() {
        return Err("failed to open any debug log file".to_string());
    }

    let _ = DEBUG_LOG_FILES.set(std::sync::Mutex::new(files));
    debug_log(&format!(
        "log files initialized at {}",
        opened_paths.join(" | ")
    ));
    Ok(())
}

#[derive(Debug, Clone)]
enum Message {
    QueryChanged(String),
    ActivateSelected,
    PollExternal,
    AnimateFrame,
    Keyboard(KeyboardEvent),
    StartIndex(SearchScope),
    CloseQuickHelp,
    CloseQuickHelpForever,
}

#[derive(Debug, Clone)]
struct SearchItem {
    path: Box<str>,
    modified_unix_secs: i64,
}

#[derive(Serialize, Deserialize)]
struct ScopeIndexSnapshot {
    version: u32,
    scope: String,
    items: Vec<SnapshotItem>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotItem {
    path: String,
    modified_unix_secs: i64,
}

enum IndexEvent {
    Progress {
        job_id: u64,
        current: usize,
        total: usize,
        phase: &'static str,
    },
    Done {
        job_id: u64,
        items: Vec<SearchItem>,
        backend: IndexBackend,
    },
    Delta {
        job_id: u64,
        upserts: Vec<SearchItem>,
        deleted_paths: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SearchScope {
    CurrentFolder,
    EntireCurrentDrive,
    AllLocalDrives,
    Drive(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexBackend {
    Detecting,
    WalkDir,
    NtfsMft,
    NtfsUsnLive,
    Mixed,
}

impl IndexBackend {
    fn label(self) -> &'static str {
        match self {
            Self::Detecting => "detecting",
            Self::WalkDir => "dirwalk",
            Self::NtfsMft => "ntfs-mft",
            Self::NtfsUsnLive => "ntfs-usn-live",
            Self::Mixed => "mixed",
        }
    }

    fn live_updates(self) -> bool {
        matches!(self, Self::NtfsUsnLive)
    }
}

fn backend_status_color(backend: IndexBackend) -> Color {
    match backend {
        IndexBackend::NtfsUsnLive => Color::from_rgb8(117, 227, 140),
        IndexBackend::NtfsMft => Color::from_rgb8(130, 210, 255),
        IndexBackend::Mixed => Color::from_rgb8(255, 198, 92),
        IndexBackend::WalkDir => Color::from_rgb8(184, 184, 184),
        IndexBackend::Detecting => Color::from_rgb8(170, 170, 170),
    }
}

fn state_status_color(indexing_in_progress: bool) -> Color {
    if indexing_in_progress {
        Color::from_rgb8(255, 184, 76)
    } else {
        Color::from_rgb8(117, 227, 140)
    }
}

fn selected_row_style() -> container::Style {
    container::Style {
        background: Some(Color::from_rgb8(58, 84, 122).into()),
        border: iced::Border {
            color: Color::from_rgb8(255, 213, 128),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

impl SearchScope {
    fn label(&self) -> String {
        match self {
            Self::CurrentFolder => "current-folder".to_string(),
            Self::EntireCurrentDrive => "entire-current-drive".to_string(),
            Self::AllLocalDrives => "all-local-drives".to_string(),
            Self::Drive(letter) => format!("{}:", letter.to_ascii_uppercase()),
        }
    }
}

fn load_persisted_scope() -> SearchScope {
    let Ok(content) = std::fs::read_to_string(scope_config_path()) else {
        return SearchScope::CurrentFolder;
    };

    let value = content.trim().to_ascii_lowercase();
    if value == "current-folder" {
        SearchScope::CurrentFolder
    } else if value == "entire-current-drive" {
        SearchScope::EntireCurrentDrive
    } else if value == "all-local-drives" {
        SearchScope::AllLocalDrives
    } else {
        let bytes = value.as_bytes();
        if bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            SearchScope::Drive((bytes[0] as char).to_ascii_uppercase())
        } else {
            SearchScope::CurrentFolder
        }
    }
}

fn persist_scope(scope: &SearchScope) {
    let path = scope_config_path();
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }

    let _ = std::fs::write(path, scope.label());
}

fn scope_config_path() -> std::path::PathBuf {
    let base = env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(base)
        .join("WizMini")
        .join("scope.txt")
}

fn quick_help_config_path() -> std::path::PathBuf {
    let base = env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(base)
        .join("WizMini")
        .join("quick-help-dismissed.txt")
}

fn load_quick_help_dismissed() -> bool {
    let Ok(content) = std::fs::read_to_string(quick_help_config_path()) else {
        return false;
    };

    content.trim().eq_ignore_ascii_case("1")
}

fn persist_quick_help_dismissed(value: bool) {
    let path = quick_help_config_path();
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }

    let _ = std::fs::write(path, if value { "1" } else { "0" });
}

fn scope_snapshot_path(scope: &SearchScope) -> std::path::PathBuf {
    let base = env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(base)
        .join("WizMini")
        .join("snapshots")
        .join(format!("scope-{}.bin", scope.label()))
}

fn load_scope_snapshot(scope: &SearchScope) -> Option<Vec<SearchItem>> {
    if let Ok(file) = std::fs::File::open(scope_snapshot_path(scope)) {
        if let Ok(snapshot) = bincode::deserialize_from::<_, ScopeIndexSnapshot>(file) {
            if snapshot.version == 1 && snapshot.scope == scope.label() {
                return Some(
                    snapshot
                        .items
                        .into_iter()
                        .map(|item| SearchItem {
                            path: item.path.into_boxed_str(),
                            modified_unix_secs: item.modified_unix_secs,
                        })
                        .collect(),
                );
            }
        }
    }

    None
}

fn persist_scope_snapshot_async(scope: SearchScope, items: Vec<SearchItem>) {
    thread::spawn(move || {
        let path = scope_snapshot_path(&scope);
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }

        let snapshot = ScopeIndexSnapshot {
            version: 1,
            scope: scope.label(),
            items: items
                .into_iter()
                .map(|item| SnapshotItem {
                    path: item.path.into_string(),
                    modified_unix_secs: item.modified_unix_secs,
                })
                .collect(),
        };

        let Ok(file) = std::fs::File::create(path) else {
            return;
        };
        let _ = bincode::serialize_into(file, &snapshot);
    });
}

struct App {
    raw_query: String,
    query: String,
    all_items: Vec<SearchItem>,
    items: Vec<SearchItem>,
    selected: usize,
    last_action: String,
    panel_visible: bool,
    panel_progress: f32,
    panel_anim_last_tick: Option<Instant>,
    _hotkey_manager: Option<GlobalHotKeyManager>,
    _hotkey: Option<HotKey>,
    _tray_icon: Option<TrayIcon>,
    menu_toggle_id: Option<MenuId>,
    menu_quit_id: Option<MenuId>,
    last_toggle_at: Option<Instant>,
    search_input_id: widget::Id,
    results_scroll_id: widget::Id,
    scope: SearchScope,
    command_selected: usize,
    index_rx: Option<mpsc::Receiver<IndexEvent>>,
    index_job_counter: u64,
    active_index_job: Option<u64>,
    indexing_in_progress: bool,
    indexing_progress: f32,
    indexing_phase: &'static str,
    index_backend: IndexBackend,
    index_memory_bytes: usize,
    visual_progress_test_active: bool,
    indexing_is_refresh: bool,
    is_elevated: bool,
    use_dirwalk_fallback: bool,
    show_privilege_overlay: bool,
    show_quick_help_overlay: bool,
    quick_help_selected_action: usize,
    pending_query: Option<(String, Instant, u64)>,
    query_edit_counter: u64,
    active_search_query: Option<String>,
    active_search_cursor: usize,
    active_search_results: Vec<SearchItem>,
    filename_exact_index: HashMap<String, Vec<usize>>,
    filename_prefix_index: HashMap<String, Vec<usize>>,
    filename_index_dirty: bool,
    filename_index_building: bool,
    filename_index_build_cursor: usize,
    needs_search_refresh: bool,
    next_search_refresh_at: Instant,
    latest_only_mode: bool,
    latest_window_secs: i64,
    tracking_enabled: bool,
    recent_event_by_path: HashMap<Box<str>, i64>,
    changes_added_since_index: usize,
    changes_updated_since_index: usize,
    changes_deleted_since_index: usize,
    hotkey_retry_after: Option<Instant>,
    skip_scope_persist_once: bool,
}

impl Default for App {
    fn default() -> Self {
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
        let arg_scope_override = startup_scope_override_from_args();
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
            panel_visible: false,
            panel_progress: 0.0,
            panel_anim_last_tick: None,
            _hotkey_manager: hotkey_manager,
            _hotkey: hotkey,
            _tray_icon: tray_icon,
            menu_toggle_id,
            menu_quit_id,
            last_toggle_at: None,
            search_input_id: widget::Id::new("search-input"),
            results_scroll_id: widget::Id::new("results-scroll"),
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
            show_quick_help_overlay: !load_quick_help_dismissed(),
            quick_help_selected_action: 0,
            pending_query: None,
            query_edit_counter: 0,
            active_search_query: None,
            active_search_cursor: 0,
            active_search_results: Vec::new(),
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
        };

        app.try_restore_scope_snapshot();
        app
    }
}

fn update(app: &mut App, message: Message) -> Task<Message> {
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
            app.active_search_query = None;
            app.active_search_cursor = 0;
            app.active_search_results.clear();
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

                return keep_search_input_focus(app.search_input_id.clone());
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

            let move_task = sync_window_to_progress(app.panel_progress);

            let reached_target = (app.panel_progress - target).abs() <= f32::EPSILON;
            if reached_target {
                app.panel_anim_last_tick = None;

                if !app.panel_visible {
                    return Task::batch(vec![move_task, finalize_panel_hidden_mode()]);
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

            if let Some((pending_query, due_at, edit_id)) = app.pending_query.clone() {
                if Instant::now() >= due_at && edit_id == app.query_edit_counter {
                    app.pending_query = None;
                    let _ = app.apply_raw_query(pending_query, false);
                }
            }

            if app.pending_query.is_some() {
                app.active_search_query = None;
                app.active_search_cursor = 0;
                app.active_search_results.clear();
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

                let search_start = Instant::now();
                while app.active_search_query.is_some()
                    && search_start.elapsed() < SEARCH_TIME_BUDGET_PER_TICK
                {
                    app.process_search_step();
                }
            }

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
                        IndexEvent::Progress {
                            job_id,
                            current,
                            total,
                            phase,
                        } => {
                            if app.active_index_job == Some(job_id) {
                                debug_log(&format!(
                                    "IndexEvent::Progress accepted job_id={} phase={} current={} total={} active_job={:?}",
                                    job_id, phase, current, total, app.active_index_job
                                ));
                                app.indexing_in_progress = true;
                                app.indexing_phase = phase;
                                app.indexing_progress = if total == 0 {
                                    0.0
                                } else {
                                    (current as f32 / total as f32).clamp(0.0, 1.0)
                                };
                            } else {
                                debug_log(&format!(
                                    "IndexEvent::Progress ignored job_id={} phase={} current={} total={} active_job={:?}",
                                    job_id, phase, current, total, app.active_index_job
                                ));
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
                                debug_log(&format!(
                                    "IndexEvent::Done job_id={} backend={} items={}",
                                    job_id,
                                    backend.label(),
                                    app.all_items.len()
                                ));
                                if app.all_items.is_empty() && backend == IndexBackend::Detecting {
                                    app.last_action =
                                        "NTFS indexing unavailable (run elevated and ensure USN journal is available)"
                                            .to_string();
                                } else {
                                    app.last_action = format!(
                                        "Indexed {} files [{}]",
                                        app.all_items.len(),
                                        app.scope.label()
                                    );
                                }
                                app.schedule_search_from_current_query();
                            } else {
                                debug_log(&format!(
                                    "IndexEvent::Done ignored job_id={} active_job={:?} backend={} items={}",
                                    job_id,
                                    app.active_index_job,
                                    backend.label(),
                                    items.len()
                                ));
                            }
                        }
                        IndexEvent::Delta {
                            job_id,
                            upserts,
                            deleted_paths,
                        } => {
                            if app.active_index_job == Some(job_id) {
                                debug_log(&format!(
                                    "IndexEvent::Delta accepted job_id={} upserts={} deletes={}",
                                    job_id,
                                    upserts.len(),
                                    deleted_paths.len()
                                ));
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
                                app.schedule_search_from_current_query();
                            } else {
                                debug_log(&format!(
                                    "IndexEvent::Delta ignored job_id={} active_job={:?}",
                                    job_id, app.active_index_job
                                ));
                            }
                        }
                    }
                }
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
                    prepare_panel_for_show_mode()
                } else {
                    Task::none()
                };

                if app.panel_visible {
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
                            return keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected = (app.selected + 1).min(app.items.len() - 1);
                            return Task::batch(vec![
                                sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                keep_search_input_focus(app.search_input_id.clone()),
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
                            return keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected = app.selected.saturating_sub(1);
                            return Task::batch(vec![
                                sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                keep_search_input_focus(app.search_input_id.clone()),
                            ]);
                        }
                    }
                    Key::Named(key::Named::PageDown) => {
                        if command_mode {
                            app.command_selected = (app.command_selected + KEYBOARD_PAGE_JUMP)
                                .min(suggestions.len() - 1);
                            return keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected =
                                (app.selected + KEYBOARD_PAGE_JUMP).min(app.items.len() - 1);
                            return Task::batch(vec![
                                sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                keep_search_input_focus(app.search_input_id.clone()),
                            ]);
                        }
                    }
                    Key::Named(key::Named::PageUp) => {
                        if command_mode {
                            app.command_selected =
                                app.command_selected.saturating_sub(KEYBOARD_PAGE_JUMP);
                            return keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected = app.selected.saturating_sub(KEYBOARD_PAGE_JUMP);
                            return Task::batch(vec![
                                sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                keep_search_input_focus(app.search_input_id.clone()),
                            ]);
                        }
                    }
                    Key::Named(key::Named::Home) => {
                        if command_mode {
                            app.command_selected = 0;
                            return keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected = 0;
                            return Task::batch(vec![
                                sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                keep_search_input_focus(app.search_input_id.clone()),
                            ]);
                        }
                    }
                    Key::Named(key::Named::End) => {
                        if command_mode {
                            app.command_selected = suggestions.len() - 1;
                            return keep_search_input_focus(app.search_input_id.clone());
                        } else if !app.items.is_empty() {
                            app.selected = app.items.len() - 1;
                            return Task::batch(vec![
                                sync_results_scroll(
                                    app.results_scroll_id.clone(),
                                    app.selected,
                                    app.items.len(),
                                ),
                                keep_search_input_focus(app.search_input_id.clone()),
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
            return keep_search_input_focus(app.search_input_id.clone());
        }
        Message::CloseQuickHelpForever => {
            app.show_quick_help_overlay = false;
            persist_quick_help_dismissed(true);
            return keep_search_input_focus(app.search_input_id.clone());
        }
    }

    Task::none()
}

fn view(app: &App) -> Element<'_, Message> {
    let search_enabled = !app.indexing_in_progress;
    let prompt = row![
        text(">"),
        if search_enabled {
            text_input("Type to search files...", &app.raw_query)
                .id(app.search_input_id.clone())
                .on_input(Message::QueryChanged)
                .on_submit(Message::ActivateSelected)
                .padding(8)
                .size(18)
                .width(Fill)
        } else {
            text_input("Indexing in progress...", &app.raw_query)
                .id(app.search_input_id.clone())
                .padding(8)
                .size(18)
                .width(Fill)
        }
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut listed = column![];
    for (index, item) in app.items.iter().enumerate() {
        let is_selected = index == app.selected;
        let marker = container(
            text(if is_selected { "▶" } else { " " })
                .size(18)
                .line_height(1.0)
                .align_y(iced::alignment::Vertical::Center)
                .color(if is_selected {
                    Color::from_rgb8(255, 213, 128)
                } else {
                    Color::from_rgb8(100, 105, 112)
                })
                .font(Font {
                    weight: if is_selected {
                        iced::font::Weight::Bold
                    } else {
                        iced::font::Weight::Normal
                    },
                    ..Font::with_name("Segoe UI Symbol")
                }),
        )
        .width(Length::Fixed(18.0))
        .height(Length::Fill)
        .padding(Padding {
            top: 0.0,
            right: 0.0,
            bottom: 7.0,
            left: 0.0,
        })
        .center_y(Fill);
        let display_path = truncate_middle(item.path.as_ref(), FILE_PATH_MAX_CHARS);
        let item_name = file_name_from_path(item.path.as_ref());
        let name_color = file_type_color(item_name);
        listed = listed.push(
            container(
                row![
                    marker,
                    text(item_name)
                        .color(name_color)
                        .size(FILE_NAME_FONT_SIZE)
                        .width(Length::FillPortion(3)),
                    text(display_path)
                        .color(Color::from_rgb8(145, 150, 160))
                        .width(Length::FillPortion(5))
                        .size(FILE_PATH_FONT_SIZE)
                ]
                .align_y(Alignment::Center)
                .spacing(8)
                .padding(6),
            )
            .width(Fill)
            .style(move |_theme| {
                if is_selected {
                    selected_row_style()
                } else {
                    container::Style::default()
                }
            }),
        );
    }

    let command_items = command_menu_items(&app.raw_query, app.tracking_enabled);
    let mut command_dropdown = column![];
    for (index, item) in command_items.iter().enumerate() {
        let is_selected = index == app.command_selected;
        let marker = container(
            text(if is_selected { "▶" } else { " " })
                .size(18)
                .line_height(1.0)
                .align_y(iced::alignment::Vertical::Center)
                .color(if is_selected {
                    Color::from_rgb8(255, 213, 128)
                } else {
                    Color::from_rgb8(100, 105, 112)
                })
                .font(Font {
                    weight: if is_selected {
                        iced::font::Weight::Bold
                    } else {
                        iced::font::Weight::Normal
                    },
                    ..Font::with_name("Segoe UI Symbol")
                }),
        )
        .width(Length::Fixed(18.0))
        .height(Length::Fill)
        .padding(Padding {
            top: 0.0,
            right: 0.0,
            bottom: 7.0,
            left: 0.0,
        })
        .center_y(Fill);

        let mut command_text = text(item.command).width(Length::Fixed(120.0));
        if item.command.eq_ignore_ascii_case("/exit") {
            command_text = command_text
                .color(Color::from_rgb8(235, 72, 72))
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::DEFAULT
                });
        }

        command_dropdown = command_dropdown.push(
            container(
                row![marker, command_text, text(item.description).size(13)]
                    .align_y(Alignment::Center)
                    .spacing(8)
                    .padding(4),
            )
            .width(Fill)
            .style(move |_theme| {
                if is_selected {
                    selected_row_style()
                } else {
                    container::Style::default()
                }
            }),
        );
    }

    let command_dropdown = if command_items.is_empty() {
        None
    } else {
        Some(
            container(command_dropdown)
                .padding(6)
                .style(container::bordered_box)
                .width(Fill)
                .height(Length::Shrink),
        )
    };

    let index_progress = if app.indexing_in_progress {
        Some(
            container(
                column![
                    text(format!(
                        "{} {} ... {:.0}%",
                        if app.indexing_phase == "write" {
                            if app.indexing_is_refresh {
                                "Updating index map"
                            } else {
                                "Building index map"
                            }
                        } else if app.indexing_is_refresh {
                            "Updating index"
                        } else {
                            "Building full index"
                        },
                        app.scope.label(),
                        app.indexing_progress * 100.0
                    ))
                    .size(13),
                    progress_bar(0.0..=1.0, app.indexing_progress)
                ]
                .spacing(4),
            )
            .padding(6)
            .style(container::bordered_box)
            .width(Fill)
            .height(Length::Shrink),
        )
    } else {
        None
    };

    let mut top_stack = column![prompt].spacing(6);
    if let Some(dropdown) = command_dropdown {
        top_stack = top_stack.push(dropdown);
    }
    if let Some(progress) = index_progress {
        top_stack = top_stack.push(progress);
    }

    let base_list = scrollable(listed)
        .id(app.results_scroll_id.clone())
        .height(Length::Fill);

    let list_area: Element<'_, Message> = if app.show_privilege_overlay {
        stack![
            base_list,
            container(
                column![
                    text("NOT ELEVATED")
                        .size(58)
                        .color(Color::from_rgb8(255, 64, 64)),
                    text("NTFS access is unavailable in this mode")
                        .size(22)
                        .color(Color::from_rgb8(255, 150, 150)),
                    text("Using DIRWALK fallback (SLOWER)")
                        .size(24)
                        .color(Color::from_rgb8(255, 92, 92)),
                    text("Type /up and press Enter to relaunch elevated")
                        .size(18)
                        .color(Color::from_rgb8(255, 180, 180)),
                    text("This message will go away as soon as you start to type")
                        .size(16)
                        .color(Color::from_rgb8(255, 205, 205)),
                ]
                .spacing(14)
                .align_x(Alignment::Center),
            )
            .width(Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Color::from_rgba8(180, 20, 20, 0.1).into()),
                ..container::Style::default()
            })
            .center_x(Fill)
            .center_y(Fill)
        ]
        .into()
    } else {
        base_list.into()
    };

    let content = column![
        top_stack,
        text(format!(
            "SCOPE: {}{} | MEM: {} | CHG: +{} ~{} -{} | SORT: relevance | RESULTS: {} | LAST: {}",
            app.scope.label(),
            if app.latest_only_mode {
                format!(
                    " | FILTER: latest-{}",
                    format_latest_window(app.latest_window_secs)
                )
            } else {
                String::new()
            },
            format_bytes(app.index_memory_bytes),
            app.changes_added_since_index,
            app.changes_updated_since_index,
            app.changes_deleted_since_index,
            app.items.len(),
            app.last_action
        ))
        .size(14),
        list_area,
        row![
            container(text("Enter open | Alt+Enter reveal | Esc hide").size(13))
                .width(Length::FillPortion(1))
                .align_x(Horizontal::Left),
            container(
                row![
                    text("IDX: ").size(13),
                    text(app.index_backend.label())
                        .size(13)
                        .color(backend_status_color(app.index_backend)),
                    text(" | LIVE: ").size(13),
                    text(if app.index_backend.live_updates() {
                        "on"
                    } else {
                        "off"
                    })
                    .size(13)
                    .color(if app.index_backend.live_updates() {
                        Color::from_rgb8(117, 227, 140)
                    } else {
                        Color::from_rgb8(184, 184, 184)
                    }),
                    text(" | JOB: ").size(13),
                    text(
                        app.active_index_job
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    )
                    .size(13)
                    .color(Color::from_rgb8(130, 210, 255)),
                ]
                .spacing(0)
                .align_y(Alignment::Center)
            )
            .width(Length::FillPortion(1))
            .align_x(Horizontal::Center),
            container(
                row![
                    text("STATE: ").size(13),
                    text(if app.indexing_in_progress {
                        "indexing"
                    } else {
                        "idle"
                    })
                    .size(13)
                    .color(state_status_color(app.indexing_in_progress)),
                ]
                .spacing(0)
                .align_y(Alignment::Center)
            )
            .width(Length::FillPortion(1))
            .align_x(Horizontal::Right),
        ]
        .align_y(Alignment::Center)
    ]
    .spacing(10)
    .padding(12);

    let main_panel = container(content)
        .width(Fill)
        .height(Length::Fixed(PANEL_HEIGHT))
        .style(container::rounded_box);

    if app.show_quick_help_overlay {
        stack![
            main_panel,
            container(
                container(
                    column![
                        text("Quick Start").size(24),
                        text("Press ` to show or hide WizMini. Type to search immediately. Use Arrow Up and Arrow Down to move. Press Enter to open. Press Alt+Enter to reveal in Explorer. Press Esc to hide the panel. Type / to open commands like /all, /entire, /reindex, /track, and /exit.")
                            .size(15),
                        row![
                            button(text("Close"))
                                .on_press(Message::CloseQuickHelp)
                                .style(if app.quick_help_selected_action == 0 {
                                    button::primary
                                } else {
                                    button::secondary
                                })
                                .padding([6, 12]),
                            button(text("Don't show again"))
                                .on_press(Message::CloseQuickHelpForever)
                                .style(if app.quick_help_selected_action == 1 {
                                    button::primary
                                } else {
                                    button::secondary
                                })
                                .padding([6, 12])
                        ]
                        .spacing(10),
                        text("Tab to switch. Enter to confirm. D = don't show again. Esc = close once.")
                            .size(12)
                    ]
                    .spacing(12),
                )
                .padding(14)
                .width(Length::Fixed(620.0))
                .style(container::bordered_box)
                .center_x(Fill)
                .center_y(Fill),
            )
            .width(Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Color::from_rgba8(8, 10, 14, 0.65).into()),
                ..container::Style::default()
            })
        ]
        .into()
    } else {
        main_panel.into()
    }
}

fn theme(_app: &App) -> Theme {
    Theme::TokyoNight
}

fn subscription(app: &App) -> Subscription<Message> {
    let mut subs = vec![
        iced::time::every(POLL_INTERVAL).map(|_| Message::PollExternal),
        iced::keyboard::listen().map(Message::Keyboard),
    ];

    if (app.panel_visible && app.panel_progress < 1.0)
        || (!app.panel_visible && app.panel_progress > 0.0)
    {
        subs.push(iced::time::every(Duration::from_millis(16)).map(|_| Message::AnimateFrame));
    }

    Subscription::batch(subs)
}

fn native_window_settings() -> window::Settings {
    let mut settings = window::Settings::default();
    settings.size = Size::new(PANEL_WIDTH, PANEL_HEIGHT);
    settings.min_size = Some(Size::new(520.0, 1.0));
    settings.max_size = None;
    settings.position = window::Position::SpecificWith(start_hidden_position);
    settings.resizable = false;
    settings.decorations = false;
    settings.level = window::Level::AlwaysOnTop;
    settings.exit_on_close_request = false;
    settings.transparent = false;

    #[cfg(target_os = "windows")]
    {
        settings.platform_specific.skip_taskbar = true;
        settings.platform_specific.drag_and_drop = false;
        settings.platform_specific.undecorated_shadow = false;
    }

    settings
}

fn start_hidden_position(window: Size, monitor: Size) -> Point {
    let target_width = panel_width_for_monitor(monitor.width);
    let x = ((monitor.width - target_width) / 2.0).max(0.0);
    Point::new(x, -window.height)
}

fn panel_width_for_monitor(monitor_width: f32) -> f32 {
    (monitor_width * PANEL_WIDTH_RATIO).clamp(640.0, 1800.0)
}

fn index_files_for_scope_with_progress(
    scope: SearchScope,
    job_id: u64,
    tx: &mpsc::Sender<IndexEvent>,
    allow_dirwalk_fallback: bool,
) -> (Vec<SearchItem>, IndexBackend) {
    let roots = scope_roots(&scope);
    let mut out = Vec::new();
    let mut scanned = 0usize;
    let mut used_ntfs = false;
    let mut used_walkdir = false;

    for root in roots {
        let Some(drive_letter) = drive_letter_from_root_str(&root) else {
            if !allow_dirwalk_fallback {
                continue;
            }

            used_walkdir = true;
            for entry in WalkDir::new(&root)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
            {
                if !entry.file_type().is_file() {
                    continue;
                }

                let path = entry.path().to_string_lossy().to_string();

                out.push(SearchItem {
                    path: path.into_boxed_str(),
                    modified_unix_secs: UNKNOWN_TS,
                });
                scanned += 1;

                if scanned.is_multiple_of(500) {
                    let _ = tx.send(IndexEvent::Progress {
                        job_id,
                        current: scanned,
                        total: 0,
                        phase: "index",
                    });
                }
            }
            continue;
        };

        let volume_root = format!("{}:\\", drive_letter);

        if let Some(mut ntfs_items) = try_index_ntfs_volume(&volume_root, job_id, tx) {
            used_ntfs = true;
            let before_filter = ntfs_items.len();

            if matches!(scope, SearchScope::CurrentFolder) {
                let prefix = normalized_folder_prefix(&root);
                ntfs_items.retain(|item| path_starts_with_folder(item.path.as_ref(), &prefix));
            }

            let _ = before_filter;

            scanned += ntfs_items.len();
            out.extend(ntfs_items);

            continue;
        }

        if !allow_dirwalk_fallback {
            continue;
        }

        used_walkdir = true;

        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path().to_string_lossy().to_string();

            out.push(SearchItem {
                path: path.into_boxed_str(),
                modified_unix_secs: UNKNOWN_TS,
            });
            scanned += 1;

            if scanned.is_multiple_of(500) {
                let _ = tx.send(IndexEvent::Progress {
                    job_id,
                    current: scanned,
                    total: 0,
                    phase: "index",
                });
            }
        }
    }

    let _ = tx.send(IndexEvent::Progress {
        job_id,
        current: scanned,
        total: scanned.max(1),
        phase: "index",
    });
    let backend = if used_ntfs && used_walkdir {
        IndexBackend::Mixed
    } else if used_ntfs {
        IndexBackend::NtfsMft
    } else if used_walkdir {
        IndexBackend::WalkDir
    } else {
        IndexBackend::Detecting
    };
    (out, backend)
}

#[cfg(not(target_os = "windows"))]
fn try_index_ntfs_volume(
    _root: &str,
    _job_id: u64,
    _tx: &mpsc::Sender<IndexEvent>,
) -> Option<Vec<SearchItem>> {
    None
}

#[cfg(target_os = "windows")]
#[derive(Clone, Serialize, Deserialize)]
struct NtfsNode {
    parent_id: u64,
    name: String,
    is_dir: bool,
    modified_unix_secs: i64,
    file_attributes: u32,
}

#[cfg(target_os = "windows")]
fn try_index_ntfs_volume(
    root: &str,
    job_id: u64,
    tx: &mpsc::Sender<IndexEvent>,
) -> Option<Vec<SearchItem>> {
    let drive = parse_drive_root_letter(root)?;
    let handle = open_volume_handle(drive)?;

    let mut journal = USN_JOURNAL_DATA_V0::default();
    let mut bytes_returned = 0u32;
    let query_ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null_mut(),
            0,
            &mut journal as *mut _ as *mut c_void,
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };

    if query_ok == 0 {
        let err = unsafe { GetLastError() };
        debug_log(&format!(
            "try_index_ntfs_volume FSCTL_QUERY_USN_JOURNAL failed drive={} err={}",
            drive, err
        ));
        let _ = unsafe { CloseHandle(handle) };
        return None;
    }

    let mut enum_data = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: 0,
        LowUsn: 0,
        HighUsn: journal.NextUsn,
    };

    let usn_start = journal.FirstUsn.max(0);
    let usn_total = (journal.NextUsn - usn_start).max(1) as usize;

    let mut raw_nodes: HashMap<u64, NtfsNode> = HashMap::new();
    let mut scanned = 0usize;
    let mut buffer = vec![0u8; 1024 * 1024];

    loop {
        let mut out_bytes = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_ENUM_USN_DATA,
                &mut enum_data as *mut _ as *mut c_void,
                std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
                &mut out_bytes,
                std::ptr::null_mut(),
            )
        };

        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_HANDLE_EOF || err == ERROR_INVALID_FUNCTION {
                break;
            }

            raw_nodes.clear();
            let _ = unsafe { CloseHandle(handle) };
            return None;
        }

        if out_bytes < 8 {
            break;
        }

        enum_data.StartFileReferenceNumber = unsafe { *(buffer.as_ptr() as *const u64) };

        let mut offset = 8usize;
        while offset < out_bytes as usize {
            let rec = unsafe { &*(buffer.as_ptr().add(offset) as *const USN_RECORD_V2) };
            let record_len = rec.RecordLength as usize;
            if record_len == 0 {
                break;
            }

            if rec.MajorVersion == 2 {
                let name = read_usn_v2_name(buffer.as_ptr(), offset, rec);
                if !name.is_empty() {
                    let is_dir = (rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                    raw_nodes.insert(
                        rec.FileReferenceNumber,
                        NtfsNode {
                            parent_id: rec.ParentFileReferenceNumber,
                            name,
                            is_dir,
                            modified_unix_secs: filetime_100ns_to_unix_secs(rec.TimeStamp)
                                .unwrap_or(UNKNOWN_TS),
                            file_attributes: rec.FileAttributes,
                        },
                    );

                    scanned += 1;
                    if scanned.is_multiple_of(5000) {
                        let current = (rec.Usn - usn_start).max(0) as usize;
                        let _ = tx.send(IndexEvent::Progress {
                            job_id,
                            current: current.min(usn_total),
                            total: usn_total,
                            phase: "index",
                        });
                    }
                }
            }

            offset += record_len;
        }
    }

    let _ = unsafe { CloseHandle(handle) };

    let drive_prefix = format!("{}:\\", drive.to_ascii_uppercase());
    let mut path_cache: HashMap<u64, String> = HashMap::new();
    let mut out = Vec::new();

    for (id, node) in &raw_nodes {
        if node.is_dir {
            continue;
        }

        let path = materialize_full_path(*id, &raw_nodes, &mut path_cache, &drive_prefix);
        out.push(SearchItem {
            path: path.into_boxed_str(),
            modified_unix_secs: node.modified_unix_secs,
        });
    }

    Some(out)
}

#[cfg(target_os = "windows")]
struct NtfsVolumeState {
    drive_letter: char,
    drive_prefix: String,
    handle: HANDLE,
    journal_id: u64,
    next_usn: i64,
    nodes: HashMap<u64, NtfsNode>,
    path_cache: HashMap<u64, String>,
    id_to_path: HashMap<u64, String>,
    last_snapshot_write: Instant,
    changed_since_snapshot: usize,
}

#[cfg(target_os = "windows")]
#[derive(Serialize, Deserialize)]
struct NtfsSnapshot {
    version: u32,
    drive_letter: char,
    journal_id: u64,
    next_usn: i64,
    nodes: Vec<NtfsSnapshotNode>,
}

#[cfg(target_os = "windows")]
#[derive(Serialize, Deserialize)]
struct NtfsSnapshotNode {
    id: u64,
    parent_id: u64,
    name: String,
    is_dir: bool,
    #[serde(default = "unknown_ts")]
    modified_unix_secs: i64,
    #[serde(default)]
    file_attributes: u32,
}

#[cfg(target_os = "windows")]
fn run_ntfs_live_index_job(scope: SearchScope, job_id: u64, tx: &mpsc::Sender<IndexEvent>) -> bool {
    let mut states = Vec::new();
    for root in scope_roots(&scope) {
        debug_log(&format!(
            "run_ntfs_live_index_job opening state start job_id={} root={}",
            job_id, root
        ));
        if let Some(state) = open_ntfs_volume_state(&root, job_id, tx) {
            debug_log(&format!(
                "run_ntfs_live_index_job opening state success job_id={} root={} nodes={}",
                job_id,
                root,
                state.nodes.len()
            ));
            states.push(state);
        } else {
            debug_log(&format!(
                "run_ntfs_live_index_job opening state failed job_id={} root={}",
                job_id, root
            ));
        }
    }

    debug_log(&format!(
        "run_ntfs_live_index_job job_id={} scope={} states={}",
        job_id,
        scope.label(),
        states.len()
    ));

    if states.is_empty() {
        return false;
    }

    let initial = collect_items_from_ntfs_states(&mut states);
    persist_scope_snapshot_async(scope.clone(), initial.clone());
    debug_log(&format!(
        "run_ntfs_live_index_job initial done job_id={} items={}",
        job_id,
        initial.len()
    ));
    if tx
        .send(IndexEvent::Done {
            job_id,
            items: initial,
            backend: IndexBackend::NtfsUsnLive,
        })
        .is_err()
    {
        for state in states {
            let _ = unsafe { CloseHandle(state.handle) };
        }
        return true;
    }

    let mut keep_running = true;
    while keep_running {
        for state in &mut states {
            match poll_ntfs_journal(state) {
                Some(batch) => {
                    persist_usn_checkpoint(state.drive_letter, state.journal_id, state.next_usn);

                    if batch.changed_entries > 0 {
                        state.changed_since_snapshot += batch.changed_entries;
                    }

                    maybe_persist_ntfs_snapshot(state);

                    if !batch.upserts.is_empty() || !batch.deleted_paths.is_empty() {
                        debug_log(&format!(
                            "run_ntfs_live_index_job delta job_id={} upserts={} deletes={}",
                            job_id,
                            batch.upserts.len(),
                            batch.deleted_paths.len()
                        ));
                        if tx
                            .send(IndexEvent::Delta {
                                job_id,
                                upserts: batch.upserts,
                                deleted_paths: batch.deleted_paths,
                            })
                            .is_err()
                        {
                            keep_running = false;
                            break;
                        }
                    }
                }
                None => {
                    if !recover_ntfs_state(state, job_id, tx) {
                        continue;
                    }

                    let items = collect_items_from_ntfs_states(std::slice::from_mut(state));
                    let items_len = items.len();
                    persist_scope_snapshot_async(scope.clone(), items.clone());
                    if tx
                        .send(IndexEvent::Done {
                            job_id,
                            items,
                            backend: IndexBackend::NtfsUsnLive,
                        })
                        .is_err()
                    {
                        keep_running = false;
                        break;
                    }

                    debug_log(&format!(
                        "run_ntfs_live_index_job recovery done job_id={} items_sent={}",
                        job_id, items_len
                    ));
                }
            }
        }

        if keep_running {
            thread::sleep(Duration::from_millis(300));
        }
    }

    for mut state in states {
        if state.changed_since_snapshot > 0 {
            persist_ntfs_snapshot(&mut state);
        }
        let _ = unsafe { CloseHandle(state.handle) };
    }

    true
}

#[cfg(target_os = "windows")]
fn open_ntfs_volume_state(
    root: &str,
    job_id: u64,
    tx: &mpsc::Sender<IndexEvent>,
) -> Option<NtfsVolumeState> {
    debug_log(&format!(
        "open_ntfs_volume_state start job_id={} root={}",
        job_id, root
    ));
    let drive = parse_drive_root_letter(root)?;
    debug_log(&format!(
        "open_ntfs_volume_state parsed drive job_id={} drive={}",
        job_id, drive
    ));
    let (handle, journal) = open_volume_and_query_journal(drive)?;
    debug_log(&format!(
        "open_ntfs_volume_state journal job_id={} drive={} first_usn={} next_usn={}",
        job_id, drive, journal.FirstUsn, journal.NextUsn
    ));
    debug_log(&format!(
        "open_ntfs_volume_state snapshot restore skipped job_id={} drive={}",
        job_id, drive
    ));

    let Some(nodes) = enumerate_ntfs_nodes(handle, journal.FirstUsn, journal.NextUsn, job_id, tx)
    else {
        let _ = unsafe { CloseHandle(handle) };
        return None;
    };
    debug_log(&format!(
        "open_ntfs_volume_state enumerate complete job_id={} drive={} nodes={}",
        job_id,
        drive,
        nodes.len()
    ));
    let next_usn = journal.NextUsn;

    let mut state = NtfsVolumeState {
        drive_letter: drive,
        drive_prefix: format!("{}:\\", drive.to_ascii_uppercase()),
        handle,
        journal_id: journal.UsnJournalID,
        next_usn,
        nodes,
        path_cache: HashMap::new(),
        id_to_path: HashMap::new(),
        last_snapshot_write: Instant::now(),
        changed_since_snapshot: 0,
    };

    initialize_id_path_map(&mut state, job_id, tx);
    persist_usn_checkpoint(drive, state.journal_id, state.next_usn);
    debug_log(&format!(
        "open_ntfs_volume_state ready job_id={} drive={} nodes={} (skip initial snapshot write)",
        job_id,
        drive,
        state.nodes.len()
    ));

    Some(state)
}

#[cfg(target_os = "windows")]
fn enumerate_ntfs_nodes(
    handle: HANDLE,
    low_usn: i64,
    high_usn: i64,
    job_id: u64,
    tx: &mpsc::Sender<IndexEvent>,
) -> Option<HashMap<u64, NtfsNode>> {
    debug_log(&format!(
        "enumerate_ntfs_nodes start job_id={} high_usn={}",
        job_id, high_usn
    ));
    let mut enum_data = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: 0,
        LowUsn: 0,
        HighUsn: high_usn,
    };

    let progress_low = low_usn.max(0);
    let progress_total = (high_usn - progress_low).max(1) as usize;

    let mut raw_nodes: HashMap<u64, NtfsNode> = HashMap::new();
    let mut scanned = 0usize;
    let mut buffer = vec![0u8; 1024 * 1024];

    loop {
        debug_log(&format!(
            "enumerate_ntfs_nodes ioctl start job_id={} start_frn={}",
            job_id, enum_data.StartFileReferenceNumber
        ));
        let mut out_bytes = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_ENUM_USN_DATA,
                &mut enum_data as *mut _ as *mut c_void,
                std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
                &mut out_bytes,
                std::ptr::null_mut(),
            )
        };

        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_HANDLE_EOF || err == ERROR_INVALID_FUNCTION {
                debug_log(&format!(
                    "enumerate_ntfs_nodes ioctl reached end job_id={} err={}",
                    job_id, err
                ));
                break;
            }

            debug_log(&format!(
                "enumerate_ntfs_nodes ioctl failed job_id={} err={}",
                job_id, err
            ));

            return None;
        }

        if out_bytes < 8 {
            break;
        }

        enum_data.StartFileReferenceNumber = unsafe { *(buffer.as_ptr() as *const u64) };

        let mut offset = 8usize;
        while offset < out_bytes as usize {
            let rec = unsafe { &*(buffer.as_ptr().add(offset) as *const USN_RECORD_V2) };
            let record_len = rec.RecordLength as usize;
            if record_len == 0 {
                break;
            }

            if rec.MajorVersion == 2 {
                let name = read_usn_v2_name(buffer.as_ptr(), offset, rec);
                if !name.is_empty() {
                    let is_dir = (rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                    raw_nodes.insert(
                        rec.FileReferenceNumber,
                        NtfsNode {
                            parent_id: rec.ParentFileReferenceNumber,
                            name,
                            is_dir,
                            modified_unix_secs: filetime_100ns_to_unix_secs(rec.TimeStamp)
                                .unwrap_or(UNKNOWN_TS),
                            file_attributes: rec.FileAttributes,
                        },
                    );

                    scanned += 1;
                    if scanned.is_multiple_of(5000) {
                        debug_log(&format!(
                            "enumerate_ntfs_nodes progress job_id={} scanned={}",
                            job_id, scanned
                        ));
                        let current = (rec.Usn - progress_low).max(0) as usize;
                        let _ = tx.send(IndexEvent::Progress {
                            job_id,
                            current: current.min(progress_total),
                            total: progress_total,
                            phase: "index",
                        });
                    }
                }
            }

            offset += record_len;
        }
    }

    debug_log(&format!(
        "enumerate_ntfs_nodes done job_id={} nodes={}",
        job_id,
        raw_nodes.len()
    ));

    Some(raw_nodes)
}

#[cfg(target_os = "windows")]
struct JournalBatch {
    upserts: Vec<SearchItem>,
    deleted_paths: Vec<String>,
    changed_entries: usize,
}

#[cfg(target_os = "windows")]
fn poll_ntfs_journal(state: &mut NtfsVolumeState) -> Option<JournalBatch> {
    let mut read_data = READ_USN_JOURNAL_DATA_V0 {
        StartUsn: state.next_usn,
        ReasonMask: u32::MAX,
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        BytesToWaitFor: 0,
        UsnJournalID: state.journal_id,
    };

    let mut buffer = vec![0u8; 512 * 1024];
    let mut out_bytes = 0u32;

    let ok = unsafe {
        DeviceIoControl(
            state.handle,
            FSCTL_READ_USN_JOURNAL,
            &mut read_data as *mut _ as *mut c_void,
            std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len() as u32,
            &mut out_bytes,
            std::ptr::null_mut(),
        )
    };

    if ok == 0 {
        let err = unsafe { GetLastError() };
        if err == ERROR_HANDLE_EOF {
            return Some(JournalBatch {
                upserts: Vec::new(),
                deleted_paths: Vec::new(),
                changed_entries: 0,
            });
        }
        return None;
    }

    if out_bytes < 8 {
        return Some(JournalBatch {
            upserts: Vec::new(),
            deleted_paths: Vec::new(),
            changed_entries: 0,
        });
    }

    state.next_usn = unsafe { *(buffer.as_ptr() as *const i64) };

    let mut changed_ids: HashSet<u64> = HashSet::new();
    let mut deleted_ids: Vec<u64> = Vec::new();
    let mut offset = 8usize;

    while offset < out_bytes as usize {
        let rec = unsafe { &*(buffer.as_ptr().add(offset) as *const USN_RECORD_V2) };
        let record_len = rec.RecordLength as usize;
        if record_len == 0 {
            break;
        }

        if rec.MajorVersion == 2 {
            let reason = rec.Reason;
            let id = rec.FileReferenceNumber;

            if (reason & USN_REASON_FILE_DELETE) != 0 {
                let removed_ids = remove_ntfs_node_and_descendants(&mut state.nodes, id);
                if !removed_ids.is_empty() {
                    deleted_ids.extend(removed_ids);
                }
                offset += record_len;
                continue;
            }

            let name = read_usn_v2_name(buffer.as_ptr(), offset, rec);
            if !name.is_empty() {
                let is_dir = (rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                let new_node = NtfsNode {
                    parent_id: rec.ParentFileReferenceNumber,
                    name,
                    is_dir,
                    modified_unix_secs: filetime_100ns_to_unix_secs(rec.TimeStamp)
                        .unwrap_or(UNKNOWN_TS),
                    file_attributes: rec.FileAttributes,
                };

                let needs_update = state.nodes.get(&id).map_or(true, |existing| {
                    existing.parent_id != new_node.parent_id
                        || existing.name != new_node.name
                        || existing.is_dir != new_node.is_dir
                        || existing.modified_unix_secs != new_node.modified_unix_secs
                        || existing.file_attributes != new_node.file_attributes
                });

                if needs_update {
                    state.nodes.insert(id, new_node);
                    changed_ids.insert(id);
                }

                if (reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME)) != 0 {
                    changed_ids.insert(id);
                }
            }
        }

        offset += record_len;
    }

    if !changed_ids.is_empty() || !deleted_ids.is_empty() {
        state.path_cache.clear();
    }

    let mut deleted_paths = Vec::new();
    for removed_id in deleted_ids {
        if let Some(path) = state.id_to_path.remove(&removed_id) {
            deleted_paths.push(path);
        }
    }

    let mut upserts = Vec::new();
    for id in changed_ids {
        let Some(node) = state.nodes.get(&id) else {
            continue;
        };

        if node.is_dir {
            continue;
        }

        let path =
            materialize_full_path(id, &state.nodes, &mut state.path_cache, &state.drive_prefix);
        if let Some(old_path) = state.id_to_path.insert(id, path.clone()) {
            if old_path != path {
                deleted_paths.push(old_path);
            }
        }

        upserts.push(SearchItem {
            path: path.into_boxed_str(),
            modified_unix_secs: node.modified_unix_secs,
        });
    }

    let changed_entries = upserts.len() + deleted_paths.len();

    Some(JournalBatch {
        upserts,
        deleted_paths,
        changed_entries,
    })
}

#[cfg(target_os = "windows")]
fn remove_ntfs_node_and_descendants(nodes: &mut HashMap<u64, NtfsNode>, id: u64) -> Vec<u64> {
    if !nodes.contains_key(&id) {
        return Vec::new();
    }

    let mut to_remove = vec![id];
    let mut index = 0usize;

    while index < to_remove.len() {
        let parent = to_remove[index];
        for (candidate_id, node) in nodes.iter() {
            if node.parent_id == parent && *candidate_id != parent {
                to_remove.push(*candidate_id);
            }
        }
        index += 1;
    }

    let mut removed_ids = Vec::new();
    for target in to_remove {
        if nodes.remove(&target).is_some() {
            removed_ids.push(target);
        }
    }

    removed_ids
}

#[cfg(target_os = "windows")]
fn collect_items_from_ntfs_states(states: &mut [NtfsVolumeState]) -> Vec<SearchItem> {
    let mut out = Vec::new();

    for state in states {
        for (id, node) in &state.nodes {
            if node.is_dir {
                continue;
            }

            let path = materialize_full_path(
                *id,
                &state.nodes,
                &mut state.path_cache,
                &state.drive_prefix,
            );
            out.push(SearchItem {
                path: path.into_boxed_str(),
                modified_unix_secs: node.modified_unix_secs,
            });
        }
    }

    out
}

#[cfg(target_os = "windows")]
fn initialize_id_path_map(state: &mut NtfsVolumeState, job_id: u64, tx: &mpsc::Sender<IndexEvent>) {
    state.id_to_path.clear();

    let ids: Vec<u64> = state
        .nodes
        .iter()
        .filter_map(|(id, node)| (!node.is_dir).then_some(*id))
        .collect();

    debug_log(&format!(
        "initialize_id_path_map start job_id={} total_ids={}",
        job_id,
        ids.len()
    ));

    let ids_total = ids.len().max(1);

    for (idx, id) in ids.into_iter().enumerate() {
        let path =
            materialize_full_path(id, &state.nodes, &mut state.path_cache, &state.drive_prefix);
        state.id_to_path.insert(id, path);

        if (idx + 1).is_multiple_of(5000) {
            let _ = tx.send(IndexEvent::Progress {
                job_id,
                current: idx + 1,
                total: ids_total,
                phase: "write",
            });
            debug_log(&format!(
                "initialize_id_path_map progress job_id={} processed={}",
                job_id,
                idx + 1
            ));
        }
    }

    debug_log(&format!(
        "initialize_id_path_map done job_id={} built_paths={}",
        job_id,
        state.id_to_path.len()
    ));
}

#[cfg(target_os = "windows")]
fn recover_ntfs_state(
    state: &mut NtfsVolumeState,
    job_id: u64,
    tx: &mpsc::Sender<IndexEvent>,
) -> bool {
    let old_handle = state.handle;

    let Some((new_handle, journal)) = open_volume_and_query_journal(state.drive_letter) else {
        return false;
    };

    let mut resume_usn = state.next_usn;
    if let Some(saved) = load_usn_checkpoint(state.drive_letter) {
        if saved.journal_id == journal.UsnJournalID {
            resume_usn = resume_usn.max(saved.next_usn);
        }
    }

    if resume_usn < journal.FirstUsn {
        resume_usn = journal.FirstUsn;
    }
    if resume_usn > journal.NextUsn {
        resume_usn = journal.NextUsn;
    }

    let can_continue = journal.UsnJournalID == state.journal_id
        && resume_usn >= journal.FirstUsn
        && resume_usn <= journal.NextUsn;

    if can_continue {
        state.handle = new_handle;
        state.next_usn = resume_usn;
        persist_usn_checkpoint(state.drive_letter, state.journal_id, state.next_usn);
        let _ = unsafe { CloseHandle(old_handle) };
        return true;
    }

    let Some(nodes) =
        enumerate_ntfs_nodes(new_handle, journal.FirstUsn, journal.NextUsn, job_id, tx)
    else {
        let _ = unsafe { CloseHandle(new_handle) };
        return false;
    };

    state.handle = new_handle;
    state.journal_id = journal.UsnJournalID;
    state.next_usn = journal.NextUsn;
    state.nodes = nodes;
    state.path_cache.clear();
    initialize_id_path_map(state, job_id, tx);
    state.changed_since_snapshot = 0;
    state.last_snapshot_write = Instant::now();
    persist_usn_checkpoint(state.drive_letter, state.journal_id, state.next_usn);
    debug_log(&format!(
        "recover_ntfs_state rebuilt state drive={} nodes={} (skip immediate snapshot write)",
        state.drive_letter,
        state.nodes.len()
    ));

    let _ = unsafe { CloseHandle(old_handle) };
    true
}

#[cfg(target_os = "windows")]
fn open_volume_and_query_journal(drive: char) -> Option<(HANDLE, USN_JOURNAL_DATA_V0)> {
    let handle = open_volume_handle(drive)?;

    let mut journal = USN_JOURNAL_DATA_V0::default();
    let mut bytes_returned = 0u32;
    let query_ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null_mut(),
            0,
            &mut journal as *mut _ as *mut c_void,
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };

    if query_ok == 0 {
        let err = unsafe { GetLastError() };
        debug_log(&format!(
            "open_volume_and_query_journal FSCTL_QUERY_USN_JOURNAL failed drive={} err={}",
            drive, err
        ));
        let _ = unsafe { CloseHandle(handle) };
        return None;
    }

    Some((handle, journal))
}

#[cfg(target_os = "windows")]
fn open_volume_handle(drive: char) -> Option<HANDLE> {
    let volume_path = format!(r"\\.\{}:", drive.to_ascii_uppercase());
    let volume_wide = to_wide(&volume_path);

    for desired_access in [FILE_GENERIC_READ, 0] {
        let handle = unsafe {
            CreateFileW(
                volume_wide.as_ptr(),
                desired_access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };

        if handle != INVALID_HANDLE_VALUE {
            if desired_access == 0 {
                debug_log(&format!(
                    "open_volume_handle drive={} succeeded with desired_access=0",
                    drive
                ));
            }
            return Some(handle);
        }

        let err = unsafe { GetLastError() };
        debug_log(&format!(
            "open_volume_handle drive={} failed desired_access={} err={}",
            drive, desired_access, err
        ));
    }

    None
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
struct UsnCheckpoint {
    journal_id: u64,
    next_usn: i64,
}

#[cfg(target_os = "windows")]
fn checkpoint_file_path() -> std::path::PathBuf {
    let base = env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(base)
        .join("WizMini")
        .join("usn_checkpoints.txt")
}

#[cfg(target_os = "windows")]
fn load_usn_checkpoint(drive: char) -> Option<UsnCheckpoint> {
    let path = checkpoint_file_path();
    let content = std::fs::read_to_string(path).ok()?;
    let key = drive.to_ascii_uppercase();

    for line in content.lines() {
        let mut parts = line.split(',');
        let drive_part = parts.next()?.chars().next()?.to_ascii_uppercase();
        let journal_id = parts.next()?.parse::<u64>().ok()?;
        let next_usn = parts.next()?.parse::<i64>().ok()?;

        if drive_part == key {
            return Some(UsnCheckpoint {
                journal_id,
                next_usn,
            });
        }
    }

    None
}

#[cfg(target_os = "windows")]
fn persist_usn_checkpoint(drive: char, journal_id: u64, next_usn: i64) {
    let path = checkpoint_file_path();
    let parent = path.parent();
    if let Some(dir) = parent {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }

    let mut map: HashMap<char, UsnCheckpoint> = HashMap::new();
    if let Ok(content) = std::fs::read_to_string(&path) {
        for line in content.lines() {
            let mut parts = line.split(',');
            let Some(drive_part) = parts.next().and_then(|v| v.chars().next()) else {
                continue;
            };
            let Some(journal) = parts.next().and_then(|v| v.parse::<u64>().ok()) else {
                continue;
            };
            let Some(usn) = parts.next().and_then(|v| v.parse::<i64>().ok()) else {
                continue;
            };
            map.insert(
                drive_part.to_ascii_uppercase(),
                UsnCheckpoint {
                    journal_id: journal,
                    next_usn: usn,
                },
            );
        }
    }

    map.insert(
        drive.to_ascii_uppercase(),
        UsnCheckpoint {
            journal_id,
            next_usn,
        },
    );

    let mut lines = Vec::new();
    for (letter, entry) in map {
        lines.push(format!(
            "{},{},{}",
            letter, entry.journal_id, entry.next_usn
        ));
    }

    let _ = std::fs::write(path, lines.join("\n"));
}

#[cfg(target_os = "windows")]
fn snapshot_file_path(drive: char) -> std::path::PathBuf {
    let base = env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(base)
        .join("WizMini")
        .join("snapshots")
        .join(format!("{}.bin", drive.to_ascii_uppercase()))
}

#[cfg(target_os = "windows")]
fn maybe_persist_ntfs_snapshot(state: &mut NtfsVolumeState) {
    if state.changed_since_snapshot == 0 {
        return;
    }

    let elapsed = state.last_snapshot_write.elapsed();
    let threshold_hit = state.changed_since_snapshot >= 4000;
    let time_hit = elapsed >= Duration::from_secs(12);
    if threshold_hit || time_hit {
        persist_ntfs_snapshot(state);
    }
}

#[cfg(target_os = "windows")]
fn persist_ntfs_snapshot(state: &mut NtfsVolumeState) {
    let path = snapshot_file_path(state.drive_letter);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }

    let mut nodes = Vec::with_capacity(state.nodes.len());
    for (id, node) in &state.nodes {
        nodes.push(NtfsSnapshotNode {
            id: *id,
            parent_id: node.parent_id,
            name: node.name.clone(),
            is_dir: node.is_dir,
            modified_unix_secs: node.modified_unix_secs,
            file_attributes: node.file_attributes,
        });
    }

    let snapshot = NtfsSnapshot {
        version: 1,
        drive_letter: state.drive_letter,
        journal_id: state.journal_id,
        next_usn: state.next_usn,
        nodes,
    };

    let Ok(file) = std::fs::File::create(path) else {
        return;
    };

    if bincode::serialize_into(file, &snapshot).is_ok() {
        state.last_snapshot_write = Instant::now();
        state.changed_since_snapshot = 0;
    }
}

#[cfg(target_os = "windows")]
fn parse_drive_root_letter(root: &str) -> Option<char> {
    let trimmed = root.trim();
    let bytes = trimmed.as_bytes();
    let is_drive_root = bytes.len() == 3
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
        && bytes[0].is_ascii_alphabetic();

    if is_drive_root {
        Some((bytes[0] as char).to_ascii_uppercase())
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn to_wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(target_os = "windows")]
fn read_usn_v2_name(buffer: *const u8, record_offset: usize, rec: &USN_RECORD_V2) -> String {
    let name_offset = record_offset + rec.FileNameOffset as usize;
    let name_len_u16 = rec.FileNameLength as usize / 2;
    if name_len_u16 == 0 {
        return String::new();
    }

    let name_ptr = unsafe { buffer.add(name_offset) as *const u16 };
    let name_slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len_u16) };
    String::from_utf16_lossy(name_slice)
}

#[cfg(target_os = "windows")]
fn materialize_full_path(
    id: u64,
    raw_nodes: &HashMap<u64, NtfsNode>,
    path_cache: &mut HashMap<u64, String>,
    drive_prefix: &str,
) -> String {
    if let Some(found) = path_cache.get(&id) {
        return found.clone();
    }

    let mut parts = Vec::new();
    let mut current = id;
    let mut depth = 0usize;

    while depth < 1024 {
        let Some(node) = raw_nodes.get(&current) else {
            break;
        };

        parts.push(node.name.clone());
        if node.parent_id == current {
            break;
        }

        current = node.parent_id;
        depth += 1;
    }

    parts.reverse();
    let path = if parts.is_empty() {
        drive_prefix.to_string()
    } else {
        format!("{}{}", drive_prefix, parts.join("\\"))
    };

    path_cache.insert(id, path.clone());
    path
}

fn scope_roots(scope: &SearchScope) -> Vec<String> {
    match scope {
        SearchScope::CurrentFolder => vec![env::current_dir()
            .unwrap_or_else(|_| "C:\\".into())
            .to_string_lossy()
            .to_string()],
        SearchScope::EntireCurrentDrive => {
            let cwd = env::current_dir().unwrap_or_else(|_| "C:\\".into());
            let drive = drive_letter_from_path(&cwd).unwrap_or('C');
            vec![format!("{}:\\", drive.to_ascii_uppercase())]
        }
        SearchScope::AllLocalDrives => available_drive_roots(),
        SearchScope::Drive(letter) => vec![format!("{}:\\", letter.to_ascii_uppercase())],
    }
}

fn available_drive_roots() -> Vec<String> {
    let mut roots = Vec::new();

    for letter in 'A'..='Z' {
        let root = format!("{}:\\", letter);
        if std::path::Path::new(&root).exists() {
            roots.push(root);
        }
    }

    if roots.is_empty() {
        roots.push("C:\\".to_string());
    }

    roots
}

fn drive_letter_from_path(path: &std::path::Path) -> Option<char> {
    let raw = path.to_string_lossy();
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        Some(raw.chars().next()?.to_ascii_uppercase())
    } else {
        None
    }
}

fn drive_letter_from_root_str(root: &str) -> Option<char> {
    let bytes = root.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        Some((bytes[0] as char).to_ascii_uppercase())
    } else {
        None
    }
}

fn normalized_folder_prefix(path: &str) -> String {
    let mut normalized = path.replace('/', "\\").to_ascii_lowercase();
    if !normalized.ends_with('\\') {
        normalized.push('\\');
    }
    normalized
}

fn path_starts_with_folder(path: &str, folder_prefix: &str) -> bool {
    let normalized = path.replace('/', "\\").to_ascii_lowercase();
    normalized.starts_with(folder_prefix)
}

fn debug_log(message: &str) {
    if !*DEBUG_ENABLED.get_or_init(|| false) {
        return;
    }

    let line = format!("[wizmini-debug] {}\n", message);

    if let Some(files_mutex) = DEBUG_LOG_FILES.get() {
        if let Ok(mut files) = files_mutex.lock() {
            for file in files.iter_mut() {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
        }
    }

    eprintln!("{}", line.trim_end());
}

fn unknown_ts() -> i64 {
    UNKNOWN_TS
}

#[cfg(target_os = "windows")]
fn is_process_elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

#[cfg(not(target_os = "windows"))]
fn is_process_elevated() -> bool {
    true
}

#[cfg(target_os = "windows")]
fn request_self_elevation(scope: &SearchScope) -> Result<(), String> {
    let exe_path = env::current_exe().map_err(|e| e.to_string())?;
    let exe = to_wide(exe_path.to_string_lossy().as_ref());
    let verb = to_wide("runas");
    let params = to_wide(&format!("--show --scope={}", scope_arg_value(scope)));

    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWDEFAULT,
        )
    } as isize;

    if result <= 32 {
        Err(format!(
            "UAC elevation failed or cancelled (code {})",
            result
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn request_self_elevation(_scope: &SearchScope) -> Result<(), String> {
    Err("Elevation is only supported on Windows".to_string())
}

fn open_path(path: &str) -> Result<(), String> {
    Command::new("cmd")
        .args(["/C", "start", "", path])
        .spawn()
        .map_err(|e| e.to_string())?;

    Ok(())
}

fn reveal_path(path: &str) -> Result<(), String> {
    Command::new("explorer")
        .arg(format!("/select,{}", path))
        .spawn()
        .map_err(|e| e.to_string())?;

    Ok(())
}

fn initialize_panel_hidden_mode() -> Task<Message> {
    window::latest().then(move |maybe_id| {
        if let Some(id) = maybe_id {
            window::monitor_size(id).then(move |monitor| {
                let monitor = monitor.unwrap_or(Size::new(PANEL_WIDTH, PANEL_HEIGHT));
                let panel_width = panel_width_for_monitor(monitor.width);
                let x = ((monitor.width - panel_width) / 2.0).max(0.0);
                let y = -PANEL_HEIGHT;

                Task::batch(vec![
                    window::resize(id, Size::new(panel_width, PANEL_HEIGHT)),
                    window::move_to(id, Point::new(x, y)),
                    window::enable_mouse_passthrough(id),
                ])
            })
        } else {
            Task::none()
        }
    })
}

fn prepare_panel_for_show_mode() -> Task<Message> {
    window::latest().then(move |maybe_id| {
        if let Some(id) = maybe_id {
            Task::batch(vec![
                window::disable_mouse_passthrough(id),
                window::gain_focus(id),
            ])
        } else {
            Task::none()
        }
    })
}

fn finalize_panel_hidden_mode() -> Task<Message> {
    window::latest().then(move |maybe_id| {
        if let Some(id) = maybe_id {
            window::enable_mouse_passthrough(id)
        } else {
            Task::none()
        }
    })
}

fn ease_out_cubic(t: f32) -> f32 {
    let clamped = t.clamp(0.0, 1.0);
    1.0 - (1.0 - clamped).powi(3)
}

fn sync_window_to_progress(progress: f32) -> Task<Message> {
    window::latest().then(move |maybe_id| {
        if let Some(id) = maybe_id {
            window::monitor_size(id).then(move |monitor| {
                let monitor = monitor.unwrap_or(Size::new(PANEL_WIDTH, PANEL_HEIGHT));
                let panel_width = panel_width_for_monitor(monitor.width);
                let x = ((monitor.width - panel_width) / 2.0).max(0.0);
                let y = -PANEL_HEIGHT * (1.0 - ease_out_cubic(progress));

                Task::batch(vec![
                    window::resize(id, Size::new(panel_width, PANEL_HEIGHT)),
                    window::move_to(id, Point::new(x, y)),
                ])
            })
        } else {
            Task::none()
        }
    })
}

impl App {
    fn try_restore_scope_snapshot(&mut self) {
        let Some(items) = load_scope_snapshot(&self.scope) else {
            return;
        };

        self.all_items = items;
        self.recompute_index_memory_bytes();
        self.schedule_search_from_current_query();
        self.last_action = format!(
            "Loaded snapshot: {} items [{}]",
            self.all_items.len(),
            self.scope.label()
        );
    }

    fn apply_raw_query(&mut self, raw_query: String, execute_directives: bool) -> Task<Message> {
        self.pending_query = None;
        self.needs_search_refresh = false;
        self.raw_query = raw_query;

        let parsed = parse_scope_directive(&self.raw_query);
        debug_log(&format!(
            "apply_raw_query execute={} raw='{}' clean='{}' scope_override={:?} latest_only={} latest_window_secs={:?} reindex_current_scope={} toggle_tracking={} test_progress={} exit_app={}",
            execute_directives,
            self.raw_query,
            parsed.clean_query,
            parsed.scope_override,
            parsed.latest_only,
            parsed.latest_window_secs,
            parsed.reindex_current_scope,
            parsed.toggle_tracking,
            parsed.test_progress,
            parsed.exit_app
        ));
        self.query = parsed.clean_query;

        if !execute_directives {
            let cmd = self.raw_query.trim_start();
            if !cmd.starts_with("/latest") && !cmd.starts_with("/last") {
                self.latest_only_mode = false;
            }
            self.schedule_search_from_current_query();
            return Task::none();
        }

        if parsed.test_progress {
            self.visual_progress_test_active = true;
            self.indexing_in_progress = true;
            self.indexing_progress = 0.0;
            self.last_action = "Running visual progress test".to_string();
            return Task::none();
        }

        if parsed.exit_app {
            return iced::exit();
        }

        if parsed.elevate_app {
            if self.is_elevated {
                self.last_action = "Already elevated".to_string();
                return Task::none();
            }

            match request_self_elevation(&SearchScope::EntireCurrentDrive) {
                Ok(()) => return iced::exit(),
                Err(err) => {
                    self.last_action = err;
                    return Task::none();
                }
            }
        }

        if parsed.latest_only {
            if !self.tracking_enabled {
                self.last_action = "Tracking is off (use /track to enable)".to_string();
                return Task::none();
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
            return Task::none();
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
            return Task::none();
        }

        if parsed.reindex_current_scope {
            self.latest_only_mode = false;
            self.query.clear();
            self.last_action = format!("Reindexing scope: {}", self.scope.label());
            return Task::done(Message::StartIndex(self.scope.clone()));
        }

        let cmd = self.raw_query.trim_start();
        if !cmd.starts_with("/latest") && !cmd.starts_with("/last") {
            self.latest_only_mode = false;
        }

        if let Some(new_scope) = parsed.scope_override {
            if self.indexing_in_progress && self.scope == new_scope {
                self.last_action = format!("Already indexing scope: {}", self.scope.label());
                return Task::none();
            }

            self.scope = new_scope;
            self.all_items.clear();
            self.items.clear();
            self.selected = 0;
            self.last_action = format!("Indexing scope: {}", self.scope.label());
            debug_log(&format!(
                "apply_raw_query starting index for scope={} ",
                self.scope.label()
            ));
            return Task::done(Message::StartIndex(self.scope.clone()));
        }

        self.schedule_search_from_current_query();
        Task::none()
    }

    fn begin_index(&mut self, scope: SearchScope) {
        self.index_job_counter += 1;
        let job_id = self.index_job_counter;
        debug_log(&format!(
            "begin_index job_id={} scope={}",
            job_id,
            scope.label()
        ));
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
        self.needs_search_refresh = false;
        self.recent_event_by_path.clear();
        self.changes_added_since_index = 0;
        self.changes_updated_since_index = 0;
        self.changes_deleted_since_index = 0;

        if let Some(items) = load_scope_snapshot(&scope) {
            self.all_items = items;
            self.indexing_is_refresh = true;
            self.recompute_index_memory_bytes();
            self.schedule_search_from_current_query();
            self.last_action = format!(
                "Loaded snapshot: {} items [{}]",
                self.all_items.len(),
                self.scope.label()
            );
        }

        let (tx, rx) = mpsc::channel::<IndexEvent>();
        self.index_rx = Some(rx);

        let allow_dirwalk_fallback = self.use_dirwalk_fallback;
        thread::spawn(move || {
            run_index_job(scope, job_id, tx, allow_dirwalk_fallback);
        });
    }

    fn recompute_index_memory_bytes(&mut self) {
        self.index_memory_bytes = estimate_index_memory_bytes(&self.all_items);
    }

    fn schedule_search_from_current_query(&mut self) {
        let q = self.query.trim().to_ascii_lowercase();

        if q.is_empty() && !self.latest_only_mode {
            self.items = self
                .all_items
                .iter()
                .take(VISIBLE_RESULTS_LIMIT)
                .cloned()
                .collect();
            self.active_search_query = None;
            self.active_search_cursor = 0;
            self.active_search_results.clear();
            self.clamp_selected();
        } else {
            if !self.latest_only_mode {
                if let Some(results) = self.try_fast_filename_search(&q) {
                    self.items = results;
                    self.active_search_query = None;
                    self.active_search_cursor = 0;
                    self.active_search_results.clear();
                    self.clamp_selected();
                    return;
                }
            }

            self.active_search_query = Some(q);
            self.active_search_cursor = 0;
            self.active_search_results.clear();
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
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();

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

    fn process_search_step(&mut self) {
        let Some(query) = self.active_search_query.clone() else {
            return;
        };

        let latest_cutoff = if self.latest_only_mode {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some(now - self.latest_window_secs)
        } else {
            None
        };

        if self.latest_only_mode && query.is_empty() {
            let cutoff = latest_cutoff.unwrap_or(i64::MIN);
            let mut matched: Vec<SearchItem> = self
                .all_items
                .iter()
                .filter(|item| {
                    self.recent_event_by_path
                        .get(item.path.as_ref())
                        .copied()
                        .or((item.modified_unix_secs != UNKNOWN_TS)
                            .then_some(item.modified_unix_secs))
                        .map(|ts| ts >= cutoff)
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            matched.sort_by_key(|item| {
                std::cmp::Reverse(
                    self.recent_event_by_path
                        .get(item.path.as_ref())
                        .copied()
                        .or((item.modified_unix_secs != UNKNOWN_TS)
                            .then_some(item.modified_unix_secs))
                        .unwrap_or(i64::MIN),
                )
            });
            if matched.len() > VISIBLE_RESULTS_LIMIT {
                matched.truncate(VISIBLE_RESULTS_LIMIT);
            }

            self.items = matched;
            self.active_search_query = None;
            self.active_search_cursor = 0;
            self.active_search_results.clear();
            self.clamp_selected();
            return;
        }

        let start = self.active_search_cursor;
        if start >= self.all_items.len() {
            self.items = std::mem::take(&mut self.active_search_results);
            self.active_search_query = None;
            self.active_search_cursor = 0;
            self.clamp_selected();
            return;
        }

        let end = (start + SEARCH_BATCH_SIZE).min(self.all_items.len());
        for item in &self.all_items[start..end] {
            let matches_latest = latest_cutoff
                .map(|cutoff| {
                    self.recent_event_by_path
                        .get(item.path.as_ref())
                        .copied()
                        .or((item.modified_unix_secs != UNKNOWN_TS)
                            .then_some(item.modified_unix_secs))
                        .map(|ts| ts >= cutoff)
                        .unwrap_or(false)
                })
                .unwrap_or(true);

            let matches_query = if query.is_empty() {
                true
            } else {
                query_matches_item(&query, item)
            };

            if matches_latest && matches_query {
                self.active_search_results.push(item.clone());
                if self.active_search_results.len() >= VISIBLE_RESULTS_LIMIT {
                    break;
                }
            }
        }

        self.active_search_cursor = end;

        if self.active_search_results.len() >= VISIBLE_RESULTS_LIMIT
            || self.active_search_cursor >= self.all_items.len()
        {
            if self.latest_only_mode {
                self.active_search_results.sort_by_key(|item| {
                    std::cmp::Reverse(if item.modified_unix_secs == UNKNOWN_TS {
                        i64::MIN
                    } else {
                        item.modified_unix_secs
                    })
                });
            }
            self.items = std::mem::take(&mut self.active_search_results);
            self.active_search_query = None;
            self.active_search_cursor = 0;
            self.clamp_selected();
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
            let delete_set: std::collections::HashSet<String> = deleted_paths.into_iter().collect();
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

        self.needs_search_refresh = true;
        self.filename_index_dirty = true;
        self.filename_index_building = false;
        self.filename_index_build_cursor = 0;
        (added_count, updated_count, deleted_count)
    }
}

fn estimate_index_memory_bytes(items: &[SearchItem]) -> usize {
    let mut total = std::mem::size_of_val(items);
    for item in items {
        total += std::mem::size_of::<SearchItem>();
        total += item.path.len();
    }
    total
}

fn format_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(target_os = "windows")]
fn filetime_100ns_to_unix_secs(filetime_100ns: i64) -> Option<i64> {
    if filetime_100ns <= 0 {
        return None;
    }

    let windows_epoch_to_unix_secs = 11_644_473_600i64;
    let secs = filetime_100ns / 10_000_000 - windows_epoch_to_unix_secs;
    Some(secs)
}

fn sync_results_scroll(scroll_id: widget::Id, selected: usize, total: usize) -> Task<Message> {
    if total <= 1 {
        return Task::none();
    }

    let y = (selected as f32 / (total.saturating_sub(1)) as f32).clamp(0.0, 1.0);
    operation::snap_to(scroll_id, widget::scrollable::RelativeOffset { x: 0.0, y })
}

fn keep_search_input_focus(search_input_id: widget::Id) -> Task<Message> {
    Task::batch(vec![
        operation::focus(search_input_id.clone()),
        operation::move_cursor_to_end(search_input_id),
    ])
}

fn run_index_job(
    scope: SearchScope,
    job_id: u64,
    tx: mpsc::Sender<IndexEvent>,
    allow_dirwalk_fallback: bool,
) {
    debug_log(&format!(
        "run_index_job start job_id={} scope={}",
        job_id,
        scope.label()
    ));
    #[cfg(target_os = "windows")]
    {
        if run_ntfs_live_index_job(scope.clone(), job_id, &tx) {
            debug_log(&format!(
                "run_index_job live index active job_id={} scope={}",
                job_id,
                scope.label()
            ));
            return;
        }

        debug_log(&format!(
            "run_index_job live index unavailable job_id={} scope={}",
            job_id,
            scope.label()
        ));
    }

    let (items, backend) =
        index_files_for_scope_with_progress(scope.clone(), job_id, &tx, allow_dirwalk_fallback);
    persist_scope_snapshot_async(scope.clone(), items.clone());
    debug_log(&format!(
        "run_index_job finished job_id={} items={} backend={}",
        job_id,
        items.len(),
        backend.label()
    ));
    let _ = tx.send(IndexEvent::Done {
        job_id,
        items,
        backend,
    });
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
        .with_tooltip("WizMini")
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
