use iced::alignment::Horizontal;
use iced::widget::{
    button, column, container, progress_bar, row, scrollable, stack, text, text_input,
};
use iced::{Alignment, Color, Element, Fill, Font, Length, Padding, Subscription, Theme};

use crate::commands::{command_menu_items, format_latest_window};
use crate::search::{file_name_from_path, file_type_color, truncate_middle};
use crate::{
    backend_status_color, format_bytes, state_status_color, App, Message, FILE_NAME_FONT_SIZE,
    FILE_PATH_FONT_SIZE, FILE_PATH_MAX_CHARS, PANEL_HEIGHT, POLL_INTERVAL,
};

pub(crate) fn view(app: &App) -> Element<'_, Message> {
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
        let marker = selection_marker(is_selected);
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
        let marker = selection_marker(is_selected);

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

    let work_progress = if app.indexing_in_progress {
        Some(
            container(
                column![
                    text(format!(
                        "{} {} ... {:.0}%",
                        if app.indexing_phase == "snapshot" {
                            "Loading cached snapshot"
                        } else if app.indexing_phase == "write" {
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
    } else if app.active_search_query.is_some() {
        let total = app.all_items.len().max(1);
        let progress = (app.active_search_cursor as f32 / total as f32).clamp(0.0, 1.0);
        Some(
            container(
                column![
                    text(format!("Searching ... {:.0}%", progress * 100.0)).size(13),
                    progress_bar(0.0..=1.0, progress)
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
    if let Some(progress) = work_progress {
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

pub(crate) fn theme(_app: &App) -> Theme {
    Theme::TokyoNight
}

pub(crate) fn subscription(app: &App) -> Subscription<Message> {
    let mut subs = vec![
        iced::time::every(POLL_INTERVAL).map(|_| Message::PollExternal),
        iced::keyboard::listen().map(Message::Keyboard),
    ];

    if (app.panel_visible && app.panel_progress < 1.0)
        || (!app.panel_visible && app.panel_progress > 0.0)
    {
        subs.push(
            iced::time::every(std::time::Duration::from_millis(16)).map(|_| Message::AnimateFrame),
        );
    }

    Subscription::batch(subs)
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

fn selection_marker(is_selected: bool) -> Element<'static, Message> {
    container(
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
    .center_y(Fill)
    .into()
}
