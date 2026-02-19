#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::time::Duration;
use std::time::Instant;
use std::{env, process::Command};
use std::{sync::mpsc, thread};

#[cfg(target_os = "windows")]
use serde::{Deserialize, Serialize};
#[cfg(target_os = "windows")]
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "windows")]
use std::ffi::c_void;
#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
use iced::keyboard::{key, Event as KeyboardEvent, Key};
use iced::widget::operation;
use iced::widget::{self, column, container, progress_bar, row, scrollable, text, text_input};
use iced::window;
use iced::{Alignment, Element, Fill, Length, Point, Size, Subscription, Task, Theme};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use walkdir::WalkDir;

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

const PANEL_WIDTH: f32 = 980.0;
const PANEL_HEIGHT: f32 = 560.0;
const MAX_INDEX_FILES: usize = 200_000;
const PANEL_ANIMATION_DURATION: Duration = Duration::from_millis(180);

fn main() -> iced::Result {
    iced::application(
        || {
            let app = App::default();
            let initial_scope = app.scope.clone();

            (
                app,
                Task::batch(vec![
                    initialize_panel_hidden_mode(),
                    Task::done(Message::StartIndex(initial_scope)),
                ]),
            )
        },
        update,
        view,
    )
    .title("WizMini")
    .theme(theme)
    .window(native_window_settings())
    .subscription(subscription)
    .run()
}

#[derive(Debug, Clone)]
enum Message {
    QueryChanged(String),
    ActivateSelected,
    PollExternal,
    AnimateFrame,
    Keyboard(KeyboardEvent),
    StartIndex(SearchScope),
}

#[derive(Debug, Clone)]
struct SearchItem {
    name: String,
    path: String,
}

enum IndexEvent {
    Progress {
        job_id: u64,
        scanned: usize,
    },
    Done {
        job_id: u64,
        items: Vec<SearchItem>,
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
    scope: SearchScope,
    command_selected: usize,
    index_rx: Option<mpsc::Receiver<IndexEvent>>,
    index_job_counter: u64,
    active_index_job: Option<u64>,
    indexing_in_progress: bool,
    indexing_progress: f32,
    visual_progress_test_active: bool,
}

impl Default for App {
    fn default() -> Self {
        let (tray_icon, menu_toggle_id, menu_quit_id) = init_tray().unwrap_or((None, None, None));
        let (hotkey_manager, hotkey) = init_hotkey().unwrap_or((None, None));
        let persisted_scope = load_persisted_scope();

        Self {
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
            scope: persisted_scope,
            command_selected: 0,
            index_rx: None,
            index_job_counter: 0,
            active_index_job: None,
            indexing_in_progress: false,
            indexing_progress: 0.0,
            visual_progress_test_active: false,
        }
    }
}

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::QueryChanged(query) => {
            let task = app.apply_raw_query(query);

            let suggestions = command_menu_items(&app.raw_query);
            if suggestions.is_empty() {
                app.command_selected = 0;
            } else {
                app.command_selected = app.command_selected.min(suggestions.len() - 1);
            }

            return task;
        }
        Message::ActivateSelected => {
            let suggestions = command_menu_items(&app.raw_query);

            if !suggestions.is_empty() {
                if let Some(choice) = suggestions.get(app.command_selected) {
                    let new_raw = apply_command_choice(&app.raw_query, choice.command);
                    let task = app.apply_raw_query(new_raw);

                    return Task::batch(vec![
                        task,
                        operation::focus(app.search_input_id.clone()),
                        operation::move_cursor_to_end(app.search_input_id.clone()),
                    ]);
                }
            } else if let Some(item) = app.items.get(app.selected) {
                app.last_action = format!("Open: {}", item.path);
                let _ = open_path(&item.path);
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

            if let Some(rx) = &app.index_rx {
                let mut pending = Vec::new();
                while let Ok(event) = rx.try_recv() {
                    pending.push(event);
                }

                for event in pending {
                    match event {
                        IndexEvent::Progress { job_id, scanned } => {
                            if app.active_index_job == Some(job_id) {
                                app.indexing_in_progress = true;
                                app.indexing_progress =
                                    (scanned as f32 / MAX_INDEX_FILES as f32).min(0.99);
                            }
                        }
                        IndexEvent::Done { job_id, items } => {
                            if app.active_index_job == Some(job_id) {
                                app.indexing_in_progress = false;
                                app.indexing_progress = 1.0;
                                app.all_items = items;
                                app.last_action = format!(
                                    "Indexed {} files [{}]",
                                    app.all_items.len(),
                                    app.scope.label()
                                );
                                app.refresh_results();
                            }
                        }
                        IndexEvent::Delta {
                            job_id,
                            upserts,
                            deleted_paths,
                        } => {
                            if app.active_index_job == Some(job_id) {
                                app.apply_index_delta(upserts, deleted_paths);
                                app.indexing_in_progress = false;
                                app.indexing_progress = 1.0;
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
                if app.panel_visible {
                    app.last_action = "Panel shown".to_string();
                }

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
                let suggestions = command_menu_items(&app.raw_query);
                let command_mode = !suggestions.is_empty();

                match key.as_ref() {
                    Key::Named(key::Named::Escape) => {
                        app.panel_visible = false;
                        app.panel_anim_last_tick = None;
                        return Task::none();
                    }
                    Key::Named(key::Named::ArrowDown) => {
                        if command_mode {
                            app.command_selected =
                                (app.command_selected + 1).min(suggestions.len() - 1);
                        } else if !app.items.is_empty() {
                            app.selected = (app.selected + 1).min(app.items.len() - 1);
                        }
                    }
                    Key::Named(key::Named::ArrowUp) => {
                        if command_mode {
                            app.command_selected = app.command_selected.saturating_sub(1);
                        } else {
                            app.selected = app.selected.saturating_sub(1);
                        }
                    }
                    Key::Named(key::Named::Enter) if modifiers.alt() && !command_mode => {
                        if let Some(item) = app.items.get(app.selected) {
                            app.last_action = format!("Reveal: {}", item.path);
                            let _ = reveal_path(&item.path);
                        }
                    }
                    Key::Named(key::Named::Enter) => {}
                    _ => {}
                }
            }
        }
        Message::StartIndex(scope) => {
            app.begin_index(scope);
        }
    }

    Task::none()
}

fn view(app: &App) -> Element<'_, Message> {
    let prompt = row![
        text(">"),
        text_input("Type to search files...", &app.raw_query)
            .id(app.search_input_id.clone())
            .on_input(Message::QueryChanged)
            .on_submit(Message::ActivateSelected)
            .padding(8)
            .size(18)
            .width(Fill)
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut listed = column![];
    for (index, item) in app.items.iter().enumerate() {
        let marker = if index == app.selected { ">" } else { " " };
        listed = listed.push(
            row![
                text(marker),
                text(&item.name).width(Length::Fixed(252.0)),
                text(&item.path).size(14)
            ]
            .spacing(8)
            .padding(6),
        );
    }

    let command_items = command_menu_items(&app.raw_query);
    let mut command_dropdown = column![];
    for (index, item) in command_items.iter().enumerate() {
        let marker = if index == app.command_selected {
            ">"
        } else {
            " "
        };
        command_dropdown = command_dropdown.push(
            row![
                text(marker),
                text(item.command).width(Length::Fixed(120.0)),
                text(item.description).size(13)
            ]
            .spacing(8)
            .padding(4),
        );
    }

    let command_dropdown = if command_items.is_empty() {
        container(column![]).height(Length::Shrink)
    } else {
        container(command_dropdown)
            .padding(6)
            .style(container::bordered_box)
            .width(Fill)
            .height(Length::Shrink)
    };

    let index_progress = if app.indexing_in_progress {
        container(
            column![
                text(format!(
                    "Indexing {} ... {:.0}%",
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
        .height(Length::Shrink)
    } else {
        container(column![]).height(Length::Shrink)
    };

    let content = column![
        prompt,
        command_dropdown,
        index_progress,
        text(format!(
            "SCOPE: {} | SORT: relevance | RESULTS: {}",
            app.scope.label(),
            app.items.len()
        ))
        .size(14),
        scrollable(listed).height(Length::Fill),
        text(format!(
            "Enter select/open | Alt+Enter reveal | Esc hide | {}",
            app.last_action
        ))
        .size(13)
    ]
    .spacing(10)
    .padding(12);

    container(content)
        .width(Length::Fixed(PANEL_WIDTH))
        .height(Length::Fixed(PANEL_HEIGHT))
        .style(container::rounded_box)
        .into()
}

fn theme(_app: &App) -> Theme {
    Theme::TokyoNight
}

fn subscription(app: &App) -> Subscription<Message> {
    let mut subs = vec![
        iced::time::every(Duration::from_millis(60)).map(|_| Message::PollExternal),
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
    settings.min_size = Some(Size::new(PANEL_WIDTH, 1.0));
    settings.max_size = Some(Size::new(PANEL_WIDTH, PANEL_HEIGHT));
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
    let x = ((monitor.width - window.width) / 2.0).max(0.0);
    Point::new(x, -window.height)
}

fn index_files_for_scope_with_progress(
    scope: SearchScope,
    job_id: u64,
    tx: &mpsc::Sender<IndexEvent>,
) -> Vec<SearchItem> {
    let roots = scope_roots(&scope);
    let mut out = Vec::new();
    let mut scanned = 0usize;

    for root in roots {
        if let Some(ntfs_items) =
            try_index_ntfs_volume(&root, job_id, tx, MAX_INDEX_FILES - out.len())
        {
            scanned += ntfs_items.len();
            out.extend(ntfs_items);

            if out.len() >= MAX_INDEX_FILES {
                let _ = tx.send(IndexEvent::Progress {
                    job_id,
                    scanned: MAX_INDEX_FILES,
                });
                return out;
            }

            continue;
        }

        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path().to_string_lossy().to_string();
            let name = entry.file_name().to_string_lossy().to_string();

            out.push(SearchItem { name, path });
            scanned += 1;

            if scanned.is_multiple_of(500) {
                let _ = tx.send(IndexEvent::Progress { job_id, scanned });
            }

            if out.len() >= MAX_INDEX_FILES {
                let _ = tx.send(IndexEvent::Progress {
                    job_id,
                    scanned: MAX_INDEX_FILES,
                });
                return out;
            }
        }
    }

    let _ = tx.send(IndexEvent::Progress { job_id, scanned });
    out
}

#[cfg(not(target_os = "windows"))]
fn try_index_ntfs_volume(
    _root: &str,
    _job_id: u64,
    _tx: &mpsc::Sender<IndexEvent>,
    _remaining: usize,
) -> Option<Vec<SearchItem>> {
    None
}

#[cfg(target_os = "windows")]
#[derive(Clone, Serialize, Deserialize)]
struct NtfsNode {
    parent_id: u64,
    name: String,
    is_dir: bool,
}

#[cfg(target_os = "windows")]
fn try_index_ntfs_volume(
    root: &str,
    job_id: u64,
    tx: &mpsc::Sender<IndexEvent>,
    remaining: usize,
) -> Option<Vec<SearchItem>> {
    if remaining == 0 {
        return Some(Vec::new());
    }

    let drive = parse_drive_root_letter(root)?;
    let volume_path = format!(r"\\.\{}:", drive.to_ascii_uppercase());
    let volume_wide = to_wide(&volume_path);

    let handle = unsafe {
        CreateFileW(
            volume_wide.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

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
        let _ = unsafe { CloseHandle(handle) };
        return None;
    }

    let mut enum_data = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: 0,
        LowUsn: 0,
        HighUsn: journal.NextUsn,
    };

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
                        },
                    );

                    scanned += 1;
                    if scanned.is_multiple_of(5000) {
                        let _ = tx.send(IndexEvent::Progress { job_id, scanned });
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

        if out.len() >= remaining {
            break;
        }

        let path = materialize_full_path(*id, &raw_nodes, &mut path_cache, &drive_prefix);
        out.push(SearchItem {
            name: node.name.clone(),
            path,
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
}

#[cfg(target_os = "windows")]
fn run_ntfs_live_index_job(scope: SearchScope, job_id: u64, tx: &mpsc::Sender<IndexEvent>) -> bool {
    let mut states = Vec::new();
    for root in scope_roots(&scope) {
        if let Some(state) = open_ntfs_volume_state(&root, job_id, tx) {
            states.push(state);
        }
    }

    if states.is_empty() {
        return false;
    }

    let initial = collect_items_from_ntfs_states(&mut states, MAX_INDEX_FILES);
    if tx
        .send(IndexEvent::Done {
            job_id,
            items: initial,
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

                    let items = collect_items_from_ntfs_states(
                        std::slice::from_mut(state),
                        MAX_INDEX_FILES,
                    );
                    if tx.send(IndexEvent::Done { job_id, items }).is_err() {
                        keep_running = false;
                        break;
                    }
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
    let drive = parse_drive_root_letter(root)?;
    let (handle, journal) = open_volume_and_query_journal(drive)?;
    let snapshot = load_ntfs_snapshot(drive);

    let (nodes, next_usn) = if let Some(snapshot) = snapshot {
        if snapshot.version == 1
            && snapshot.journal_id == journal.UsnJournalID
            && snapshot.next_usn >= journal.FirstUsn
            && snapshot.next_usn <= journal.NextUsn
        {
            (snapshot_nodes_to_map(snapshot.nodes), snapshot.next_usn)
        } else {
            let Some(nodes) = enumerate_ntfs_nodes(handle, journal.NextUsn, job_id, tx) else {
                let _ = unsafe { CloseHandle(handle) };
                return None;
            };
            (nodes, journal.NextUsn)
        }
    } else {
        let Some(nodes) = enumerate_ntfs_nodes(handle, journal.NextUsn, job_id, tx) else {
            let _ = unsafe { CloseHandle(handle) };
            return None;
        };
        (nodes, journal.NextUsn)
    };

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

    initialize_id_path_map(&mut state);
    persist_usn_checkpoint(drive, state.journal_id, state.next_usn);
    persist_ntfs_snapshot(&mut state);

    Some(state)
}

#[cfg(target_os = "windows")]
fn enumerate_ntfs_nodes(
    handle: HANDLE,
    high_usn: i64,
    job_id: u64,
    tx: &mpsc::Sender<IndexEvent>,
) -> Option<HashMap<u64, NtfsNode>> {
    let mut enum_data = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: 0,
        LowUsn: 0,
        HighUsn: high_usn,
    };

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
                        },
                    );

                    scanned += 1;
                    if scanned.is_multiple_of(5000) {
                        let _ = tx.send(IndexEvent::Progress { job_id, scanned });
                    }
                }
            }

            offset += record_len;
        }
    }

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
                };

                let needs_update = state.nodes.get(&id).map_or(true, |existing| {
                    existing.parent_id != new_node.parent_id
                        || existing.name != new_node.name
                        || existing.is_dir != new_node.is_dir
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
            name: node.name.clone(),
            path,
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
fn collect_items_from_ntfs_states(states: &mut [NtfsVolumeState], limit: usize) -> Vec<SearchItem> {
    let mut out = Vec::new();

    for state in states {
        for (id, node) in &state.nodes {
            if node.is_dir {
                continue;
            }

            if out.len() >= limit {
                return out;
            }

            let path = materialize_full_path(
                *id,
                &state.nodes,
                &mut state.path_cache,
                &state.drive_prefix,
            );
            out.push(SearchItem {
                name: node.name.clone(),
                path,
            });
        }
    }

    out
}

#[cfg(target_os = "windows")]
fn initialize_id_path_map(state: &mut NtfsVolumeState) {
    state.id_to_path.clear();

    let ids: Vec<u64> = state
        .nodes
        .iter()
        .filter_map(|(id, node)| (!node.is_dir).then_some(*id))
        .collect();

    for id in ids {
        let path =
            materialize_full_path(id, &state.nodes, &mut state.path_cache, &state.drive_prefix);
        state.id_to_path.insert(id, path);
    }
}

#[cfg(target_os = "windows")]
fn snapshot_nodes_to_map(nodes: Vec<NtfsSnapshotNode>) -> HashMap<u64, NtfsNode> {
    let mut map = HashMap::with_capacity(nodes.len());
    for node in nodes {
        map.insert(
            node.id,
            NtfsNode {
                parent_id: node.parent_id,
                name: node.name,
                is_dir: node.is_dir,
            },
        );
    }
    map
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

    let Some(nodes) = enumerate_ntfs_nodes(new_handle, journal.NextUsn, job_id, tx) else {
        let _ = unsafe { CloseHandle(new_handle) };
        return false;
    };

    state.handle = new_handle;
    state.journal_id = journal.UsnJournalID;
    state.next_usn = journal.NextUsn;
    state.nodes = nodes;
    state.path_cache.clear();
    initialize_id_path_map(state);
    state.changed_since_snapshot = 0;
    state.last_snapshot_write = Instant::now();
    persist_usn_checkpoint(state.drive_letter, state.journal_id, state.next_usn);
    persist_ntfs_snapshot(state);

    let _ = unsafe { CloseHandle(old_handle) };
    true
}

#[cfg(target_os = "windows")]
fn open_volume_and_query_journal(drive: char) -> Option<(HANDLE, USN_JOURNAL_DATA_V0)> {
    let volume_path = format!(r"\\.\{}:", drive.to_ascii_uppercase());
    let volume_wide = to_wide(&volume_path);

    let handle = unsafe {
        CreateFileW(
            volume_wide.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

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
        let _ = unsafe { CloseHandle(handle) };
        return None;
    }

    Some((handle, journal))
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
        .join(format!("{}.json", drive.to_ascii_uppercase()))
}

#[cfg(target_os = "windows")]
fn load_ntfs_snapshot(drive: char) -> Option<NtfsSnapshot> {
    let path = snapshot_file_path(drive);
    let file = std::fs::File::open(path).ok()?;
    serde_json::from_reader(file).ok()
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

    if serde_json::to_writer(file, &snapshot).is_ok() {
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

struct ParsedDirective {
    scope_override: Option<SearchScope>,
    clean_query: String,
    test_progress: bool,
    exit_app: bool,
}

fn parse_scope_directive(input: &str) -> ParsedDirective {
    let mut scope_override = None;
    let mut remaining = Vec::new();
    let mut test_progress = false;
    let mut exit_app = false;

    for token in input.split_whitespace() {
        let normalized = token.to_ascii_lowercase();

        if normalized == "/entire" {
            scope_override = Some(SearchScope::EntireCurrentDrive);
            continue;
        }

        if normalized == "/all" {
            scope_override = Some(SearchScope::AllLocalDrives);
            continue;
        }

        if let Some(letter) = parse_drive_directive(&normalized) {
            scope_override = Some(SearchScope::Drive(letter));
            continue;
        }

        if normalized == "/testprogress" {
            test_progress = true;
            continue;
        }

        if normalized == "/exit" {
            exit_app = true;
            continue;
        }

        if normalized.starts_with('/') {
            continue;
        }

        remaining.push(token);
    }

    ParsedDirective {
        scope_override,
        clean_query: remaining.join(" "),
        test_progress,
        exit_app,
    }
}

struct CommandMenuItem {
    command: &'static str,
    description: &'static str,
}

fn command_menu_items(input: &str) -> Vec<CommandMenuItem> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') {
        return Vec::new();
    }

    let prefix = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();

    let items = [
        CommandMenuItem {
            command: "/entire",
            description: "Search entire current drive",
        },
        CommandMenuItem {
            command: "/all",
            description: "Search all local drives",
        },
        CommandMenuItem {
            command: "/x:",
            description: "Search specific drive (example /d:)",
        },
        CommandMenuItem {
            command: "/testProgress",
            description: "Visual progress bar test",
        },
        CommandMenuItem {
            command: "/exit",
            description: "Exit app immediately",
        },
    ];

    items
        .into_iter()
        .filter(|item| {
            if prefix == "/" {
                return true;
            }

            item.command.to_ascii_lowercase().starts_with(&prefix)
                || (prefix.len() == 3
                    && prefix.starts_with('/')
                    && prefix.ends_with(':')
                    && prefix.as_bytes()[1].is_ascii_alphabetic()
                    && item.command == "/x:")
        })
        .collect()
}

fn apply_command_choice(raw_query: &str, command: &str) -> String {
    let trimmed = raw_query.trim_start();
    let mut parts = trimmed.split_whitespace();
    let _first = parts.next();
    let rest = parts.collect::<Vec<_>>().join(" ");

    if rest.is_empty() {
        format!("{} ", command)
    } else {
        format!("{} {}", command, rest)
    }
}

fn parse_drive_directive(token: &str) -> Option<char> {
    let bytes = token.as_bytes();
    if bytes.len() == 3 && bytes[0] == b'/' && bytes[2] == b':' && bytes[1].is_ascii_alphabetic() {
        Some((bytes[1] as char).to_ascii_uppercase())
    } else {
        None
    }
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
                let x = ((monitor.width - PANEL_WIDTH) / 2.0).max(0.0);
                let y = -PANEL_HEIGHT;

                Task::batch(vec![
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
                let x = ((monitor.width - PANEL_WIDTH) / 2.0).max(0.0);
                let y = -PANEL_HEIGHT * (1.0 - ease_out_cubic(progress));

                window::move_to(id, Point::new(x, y))
            })
        } else {
            Task::none()
        }
    })
}

impl App {
    fn apply_raw_query(&mut self, raw_query: String) -> Task<Message> {
        self.raw_query = raw_query;

        let parsed = parse_scope_directive(&self.raw_query);
        self.query = parsed.clean_query;

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

        if let Some(new_scope) = parsed.scope_override {
            if new_scope != self.scope {
                self.scope = new_scope;
                self.all_items.clear();
                self.items.clear();
                self.selected = 0;
                self.last_action = format!("Indexing scope: {}", self.scope.label());
                return Task::done(Message::StartIndex(self.scope.clone()));
            }
        }

        self.refresh_results();
        Task::none()
    }

    fn begin_index(&mut self, scope: SearchScope) {
        self.index_job_counter += 1;
        let job_id = self.index_job_counter;
        self.active_index_job = Some(job_id);
        self.scope = scope.clone();
        persist_scope(&self.scope);
        self.visual_progress_test_active = false;
        self.indexing_in_progress = true;
        self.indexing_progress = 0.0;

        let (tx, rx) = mpsc::channel::<IndexEvent>();
        self.index_rx = Some(rx);

        thread::spawn(move || {
            run_index_job(scope, job_id, tx);
        });
    }

    fn refresh_results(&mut self) {
        let q = self.query.trim().to_ascii_lowercase();

        if q.is_empty() {
            self.items = self.all_items.iter().take(600).cloned().collect();
        } else {
            self.items = self
                .all_items
                .iter()
                .filter(|item| query_matches_item(&q, item))
                .take(600)
                .cloned()
                .collect();
        }

        if self.items.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.items.len() - 1);
        }
    }

    fn apply_index_delta(&mut self, upserts: Vec<SearchItem>, deleted_paths: Vec<String>) {
        if !deleted_paths.is_empty() {
            let delete_set: std::collections::HashSet<String> = deleted_paths.into_iter().collect();
            self.all_items
                .retain(|item| !delete_set.contains(&item.path));
        }

        for upsert in upserts {
            if let Some(existing) = self
                .all_items
                .iter_mut()
                .find(|item| item.path == upsert.path)
            {
                *existing = upsert;
            } else {
                self.all_items.push(upsert);
            }
        }

        self.refresh_results();
    }
}

fn query_matches_item(query: &str, item: &SearchItem) -> bool {
    if query.contains('*') || query.contains('?') {
        let name = item.name.to_ascii_lowercase();
        let path = item.path.to_ascii_lowercase();
        wildcard_match(query, &name) || wildcard_match(query, &path)
    } else {
        item.name.to_ascii_lowercase().contains(query)
            || item.path.to_ascii_lowercase().contains(query)
    }
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();

    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti] || p[pi] == b'?') {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(star) = star_idx {
            pi = star + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

fn run_index_job(scope: SearchScope, job_id: u64, tx: mpsc::Sender<IndexEvent>) {
    #[cfg(target_os = "windows")]
    {
        if run_ntfs_live_index_job(scope.clone(), job_id, &tx) {
            return;
        }
    }

    let items = index_files_for_scope_with_progress(scope, job_id, &tx);
    let _ = tx.send(IndexEvent::Done { job_id, items });
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
