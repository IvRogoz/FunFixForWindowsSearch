use std::time::Duration;
use std::{env, process::Command};
use std::time::Instant;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
use iced::keyboard::{Event as KeyboardEvent, Key, key};
use iced::widget::{column, container, row, scrollable, text, text_input};
use iced::window;
use iced::{Alignment, Element, Fill, Length, Point, Size, Subscription, Task, Theme};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use walkdir::WalkDir;

const PANEL_WIDTH: f32 = 980.0;
const PANEL_HEIGHT: f32 = 560.0;

fn main() -> iced::Result {
    iced::application(
        || {
            (
                App::default(),
                Task::batch(vec![
                    apply_panel_interaction_mode(false),
                    Task::perform(async { index_files() }, Message::IndexLoaded),
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
    IndexLoaded(Vec<SearchItem>),
}

#[derive(Debug, Clone)]
struct SearchItem {
    name: String,
    path: String,
}

struct App {
    query: String,
    all_items: Vec<SearchItem>,
    items: Vec<SearchItem>,
    selected: usize,
    last_action: String,
    panel_visible: bool,
    panel_progress: f32,
    _hotkey_manager: Option<GlobalHotKeyManager>,
    _hotkey: Option<HotKey>,
    _tray_icon: Option<TrayIcon>,
    menu_toggle_id: Option<MenuId>,
    menu_quit_id: Option<MenuId>,
    last_toggle_at: Option<Instant>,
}

impl Default for App {
    fn default() -> Self {
        let (tray_icon, menu_toggle_id, menu_quit_id) = init_tray().unwrap_or((None, None, None));
        let (hotkey_manager, hotkey) = init_hotkey().unwrap_or((None, None));

        Self {
            query: String::new(),
            all_items: Vec::new(),
            items: Vec::new(),
            selected: 0,
            last_action: "Indexing files...".to_string(),
            panel_visible: false,
            panel_progress: 0.0,
            _hotkey_manager: hotkey_manager,
            _hotkey: hotkey,
            _tray_icon: tray_icon,
            menu_toggle_id,
            menu_quit_id,
            last_toggle_at: None,
        }
    }
}

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::QueryChanged(query) => {
            app.query = query;
            app.refresh_results();
        }
        Message::ActivateSelected => {
            if let Some(item) = app.items.get(app.selected) {
                app.last_action = format!("Open: {}", item.path);
                let _ = open_path(&item.path);
            }
        }
        Message::AnimateFrame => {
            let target = if app.panel_visible { 1.0 } else { 0.0 };
            let step = 0.14;
            if app.panel_progress < target {
                app.panel_progress = (app.panel_progress + step).min(1.0);
            } else if app.panel_progress > target {
                app.panel_progress = (app.panel_progress - step).max(0.0);
            }

            return sync_window_to_progress(app.panel_progress);
        }
        Message::PollExternal => {
            let mut toggled = false;

            while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                if let Some(hotkey) = &app._hotkey {
                    if event.id == hotkey.id() {
                        toggled = true;
                    }
                }
            }

            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if app.menu_toggle_id.as_ref().is_some_and(|id| event.id == *id) {
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
                if app.panel_visible {
                    app.last_action = "Panel shown".to_string();
                }

                return apply_panel_interaction_mode(app.panel_visible);
            }
        }
        Message::Keyboard(event) => {
            if !app.panel_visible {
                return Task::none();
            }

            if let KeyboardEvent::KeyPressed { key, modifiers, .. } = event {
                match key.as_ref() {
                    Key::Named(key::Named::Escape) => {
                        app.panel_visible = false;
                        return apply_panel_interaction_mode(false);
                    }
                    Key::Named(key::Named::ArrowDown) => {
                        if !app.items.is_empty() {
                            app.selected = (app.selected + 1).min(app.items.len() - 1);
                        }
                    }
                    Key::Named(key::Named::ArrowUp) => {
                        app.selected = app.selected.saturating_sub(1);
                    }
                    Key::Named(key::Named::Enter) => {
                        if let Some(item) = app.items.get(app.selected) {
                            if modifiers.alt() {
                                app.last_action = format!("Reveal: {}", item.path);
                                let _ = reveal_path(&item.path);
                            } else {
                                app.last_action = format!("Open: {}", item.path);
                                let _ = open_path(&item.path);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Message::IndexLoaded(items) => {
            app.all_items = items;
            app.last_action = format!("Indexed {} files", app.all_items.len());
            app.refresh_results();
        }
    }

    Task::none()
}

fn view(app: &App) -> Element<'_, Message> {
    let prompt = row![
        text(">"),
        text_input("Type to search files...", &app.query)
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

    let content = column![
        prompt,
        text(format!("SORT: relevance | RESULTS: {}", app.items.len())).size(14),
        scrollable(listed).height(Length::Fill),
        text(format!("Enter open | Esc hide | {}", app.last_action)).size(13)
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

fn index_files() -> Vec<SearchItem> {
    let root = env::current_dir().unwrap_or_else(|_| "C:\\".into());
    let mut out = Vec::new();

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

        if out.len() >= 120_000 {
            break;
        }
    }

    out
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

fn apply_panel_interaction_mode(show: bool) -> Task<Message> {
    window::latest().then(move |maybe_id| {
        if let Some(id) = maybe_id {
            window::monitor_size(id).then(move |monitor| {
                let monitor = monitor.unwrap_or(Size::new(PANEL_WIDTH, PANEL_HEIGHT));
                let x = ((monitor.width - PANEL_WIDTH) / 2.0).max(0.0);
                let y = if show { 0.0 } else { -PANEL_HEIGHT };

                if show {
                    Task::batch(vec![
                        window::move_to(id, Point::new(x, y)),
                        window::disable_mouse_passthrough(id),
                        window::gain_focus(id),
                    ])
                } else {
                    Task::batch(vec![
                        window::move_to(id, Point::new(x, y)),
                        window::enable_mouse_passthrough(id),
                    ])
                }
            })
        } else {
            Task::none()
        }
    })
}

fn sync_window_to_progress(progress: f32) -> Task<Message> {
    window::latest().then(move |maybe_id| {
        if let Some(id) = maybe_id {
            window::monitor_size(id).then(move |monitor| {
                let monitor = monitor.unwrap_or(Size::new(PANEL_WIDTH, PANEL_HEIGHT));
                let x = ((monitor.width - PANEL_WIDTH) / 2.0).max(0.0);
                let y = -PANEL_HEIGHT * (1.0 - progress.clamp(0.0, 1.0));

                let mut tasks = vec![window::move_to(id, Point::new(x, y))];

                if progress <= 0.001 {
                    tasks.push(window::enable_mouse_passthrough(id));
                } else {
                    tasks.push(window::disable_mouse_passthrough(id));
                }

                Task::batch(tasks)
            })
        } else {
            Task::none()
        }
    })
}

impl App {
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

    Ok((Some(tray), Some(toggle.id().clone()), Some(quit.id().clone())))
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
