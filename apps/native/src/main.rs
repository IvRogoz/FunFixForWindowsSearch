#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::time::Duration;
use std::time::Instant;
use std::{env, process::Command};
use std::{sync::mpsc, thread};

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

const PANEL_WIDTH: f32 = 980.0;
const PANEL_HEIGHT: f32 = 560.0;
const MAX_INDEX_FILES: usize = 200_000;
const PANEL_ANIMATION_DURATION: Duration = Duration::from_millis(180);

fn main() -> iced::Result {
    iced::application(
        || {
            (
                App::default(),
                Task::batch(vec![
                    initialize_panel_hidden_mode(),
                    Task::done(Message::StartIndex(SearchScope::CurrentFolder)),
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
    Progress { job_id: u64, scanned: usize },
    Done { job_id: u64, items: Vec<SearchItem> },
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
            scope: SearchScope::CurrentFolder,
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
        self.visual_progress_test_active = false;
        self.indexing_in_progress = true;
        self.indexing_progress = 0.0;

        let (tx, rx) = mpsc::channel::<IndexEvent>();
        self.index_rx = Some(rx);

        thread::spawn(move || {
            let items = index_files_for_scope_with_progress(scope, job_id, &tx);
            let _ = tx.send(IndexEvent::Done { job_id, items });
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
                .filter(|item| {
                    item.name.to_ascii_lowercase().contains(&q)
                        || item.path.to_ascii_lowercase().contains(&q)
                })
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
