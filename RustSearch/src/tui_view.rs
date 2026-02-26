use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Wrap};

use crate::app_state::AppState;
use crate::commands::{command_menu_items, format_latest_window};
use crate::search::{file_name_from_path, file_type_color, truncate_middle};
use crate::{backend_status_color, format_bytes, state_status_color, FILE_PATH_MAX_CHARS};

pub(crate) fn draw(frame: &mut ratatui::Frame<'_>, app: &AppState) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(10, 14, 20))),
        area,
    );

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(
                if app.indexing_in_progress || app.active_search_query.is_some() {
                    3
                } else {
                    0
                },
            ),
            Constraint::Min(10),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_prompt(frame, sections[0], app);
    if sections[1].height > 0 {
        draw_progress(frame, sections[1], app);
    }
    draw_results(frame, sections[2], app);
    draw_status(frame, sections[3], app);
    draw_footer(frame, sections[4], app);

    if let Some(area) = commands_popup_area(sections[2], app) {
        draw_commands(frame, area, app);
    }

    if app.show_quick_help_overlay {
        draw_overlay(
            frame,
            area,
            vec![
                "Quick Start",
                "Press ` to show or hide RustSearch",
                "Type to search, Enter to open, Alt+Enter to reveal",
                "Use / for commands: /all /entire /reindex /track /exit",
            ],
            Color::Rgb(130, 210, 255),
        );
    }

    if app.show_privilege_overlay {
        draw_overlay(
            frame,
            area,
            vec![
                "███    ██  ██████  ████████     ███████ ██      ███████ ██    ██  █████  ████████ ███████ ██████  ",
                "████   ██ ██    ██    ██        ██      ██      ██      ██    ██ ██   ██    ██    ██      ██   ██ ",
                "██ ██  ██ ██    ██    ██        █████   ██      █████   ██    ██ ███████    ██    █████   ██   ██ ",
                "██  ██ ██ ██    ██    ██        ██      ██      ██       ██  ██  ██   ██    ██    ██      ██   ██ ",
                "██   ████  ██████     ██        ███████ ███████ ███████   ████   ██   ██    ██    ███████ ██████  ",
                "                                                                                                     ",
                "NTFS access is unavailable in this mode",
                "Using DIRWALK fallback (SLOWER)",
                "Type /up and press Enter to relaunch elevated",
            ],
            Color::Rgb(230, 80, 80),
        );
    }
}

fn draw_prompt(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let title = if app.indexing_in_progress {
        "Indexing"
    } else {
        "Search"
    };
    let line = Line::from(vec![
        Span::styled(
            "> ",
            Style::default()
                .fg(Color::Rgb(255, 213, 128))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(app.raw_query.as_str()),
        Span::styled("█", Style::default().fg(Color::Rgb(130, 210, 255))),
    ]);
    let paragraph = Paragraph::new(line)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_commands(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let suggestions = command_menu_items(&app.raw_query, app.tracking_enabled);
    if suggestions.is_empty() {
        return;
    }

    let items: Vec<ListItem<'_>> = suggestions
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let marker = if index == app.command_selected {
                ">"
            } else {
                " "
            };
            let cmd_color = if item.command.eq_ignore_ascii_case("/exit") {
                Color::Rgb(235, 72, 72)
            } else {
                Color::Rgb(130, 210, 255)
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!("{} ", marker)),
                Span::styled(
                    format!("{:<12}", item.command),
                    Style::default().fg(cmd_color),
                ),
                Span::raw(item.description),
            ]))
        })
        .collect();

    frame.render_widget(Clear, area);

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Commands")
            .style(Style::default().bg(Color::Rgb(20, 26, 36))),
    );
    frame.render_widget(list, area);
}

fn draw_progress(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let (label, value, color) = if app.indexing_in_progress {
        (
            format!(
                "{} {}",
                index_phase_label(app.indexing_phase),
                app.scope.label()
            ),
            app.indexing_progress,
            Color::Rgb(178, 126, 28),
        )
    } else if app.active_search_query.is_some() {
        let total = app.all_items.len().max(1);
        (
            "search".to_string(),
            (app.active_search_cursor as f32 / total as f32).clamp(0.0, 1.0),
            Color::Rgb(56, 122, 168),
        )
    } else {
        ("idle".to_string(), 1.0, Color::Rgb(117, 227, 140))
    };

    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title("Progress"))
        .gauge_style(Style::default().fg(color))
        .label(Span::styled(
            format!(" {} {:.0}% ", label, value * 100.0),
            Style::default()
                .fg(Color::Rgb(245, 245, 245))
                .add_modifier(Modifier::BOLD),
        ))
        .ratio(value as f64);
    frame.render_widget(gauge, area);
}

fn index_phase_label(phase: &str) -> &'static str {
    match phase {
        "snapshot" => "reading snapshot",
        "index" => "reading index",
        "write" => "finalizing index",
        "live" => "live updates",
        "done" => "ready",
        _ => "indexing",
    }
}

fn commands_popup_area(results_area: Rect, app: &AppState) -> Option<Rect> {
    if !app.raw_query.trim_start().starts_with('/') {
        return None;
    }

    let count = command_menu_items(&app.raw_query, app.tracking_enabled).len() as u16;
    let width = results_area.width.saturating_sub(4).min(74);
    let height = (count + 2).min(results_area.height.saturating_sub(1));
    if width < 20 || height < 3 {
        return None;
    }

    Some(Rect {
        x: results_area.x + 2,
        y: results_area.y,
        width,
        height,
    })
}

fn draw_results(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let viewport_rows = area.height.saturating_sub(2) as usize;
    let total = app.items.len();

    let start = if viewport_rows == 0 || total <= viewport_rows {
        0
    } else {
        let max_start = total - viewport_rows;
        let preferred = app.selected.saturating_sub(viewport_rows / 2);
        preferred.min(max_start)
    };
    let end = if viewport_rows == 0 {
        total
    } else {
        (start + viewport_rows).min(total)
    };

    let items: Vec<ListItem<'_>> = app
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|(index, item)| {
            let selected = index == app.selected;
            let marker = if selected { ">" } else { " " };
            let name = file_name_from_path(item.path.as_ref());
            let path = truncate_middle(item.path.as_ref(), FILE_PATH_MAX_CHARS);
            let style = if selected {
                Style::default()
                    .bg(Color::Rgb(58, 84, 122))
                    .fg(Color::Rgb(255, 213, 128))
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", marker), style),
                Span::styled(format!("{:<42}", name), style.fg(file_type_color(name))),
                Span::styled(path, style.fg(Color::Rgb(145, 150, 160))),
            ]))
        })
        .collect();

    let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Results"));
    frame.render_widget(list, area);
}

fn draw_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let status = format!(
        "{}SCOPE: {}{} | MEM: {} | CHG: +{} ~{} -{} | RESULTS: {} | LAST: {}",
        if app.is_elevated {
            ""
        } else {
            "[NOT ELEVATED] "
        },
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
    );
    let p = Paragraph::new(status).style(Style::default().fg(Color::Rgb(160, 168, 178)));
    frame.render_widget(p, area);
}

fn draw_footer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let line = Line::from(vec![
        Span::raw("Enter open | Alt+Enter reveal | Esc hide | IDX: "),
        Span::styled(
            app.index_backend.label(),
            Style::default().fg(backend_status_color(app.index_backend)),
        ),
        Span::raw(" | LIVE: "),
        Span::styled(
            if app.index_backend.live_updates() {
                "on"
            } else {
                "off"
            },
            Style::default().fg(if app.index_backend.live_updates() {
                Color::Rgb(117, 227, 140)
            } else {
                Color::Rgb(184, 184, 184)
            }),
        ),
        Span::raw(" | STATE: "),
        Span::styled(
            if app.indexing_in_progress {
                "indexing"
            } else {
                "idle"
            },
            Style::default().fg(state_status_color(app.indexing_in_progress)),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, lines: Vec<&str>, color: Color) {
    let max_line = lines.iter().map(|line| line.len()).max().unwrap_or(10) as u16;
    let desired_width = max_line.saturating_add(6);
    let width = desired_width.min(area.width.saturating_sub(2)).max(24);
    let desired_height = (lines.len() as u16).saturating_add(4);
    let height = desired_height.min(area.height.saturating_sub(2)).max(5);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let box_area = Rect {
        x,
        y,
        width,
        height,
    };

    let mut rendered_lines = Vec::new();
    rendered_lines.push(Line::from(" ").centered());
    for text in lines {
        if text == "Type /up and press Enter to relaunch elevated" {
            rendered_lines.push(
                Line::from(vec![
                    Span::styled("Type ", Style::default().fg(color)),
                    Span::styled(
                        "/up",
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " and press Enter to relaunch elevated",
                        Style::default().fg(color),
                    ),
                ])
                .centered(),
            );
        } else {
            rendered_lines.push(
                Line::from(Span::styled(text.to_string(), Style::default().fg(color))).centered(),
            );
        }
    }
    rendered_lines.push(Line::from(" ").centered());

    frame.render_widget(Clear, box_area);

    let p = Paragraph::new(rendered_lines)
        .block(Block::default().borders(Borders::ALL).title("Notice"))
        .style(Style::default().bg(Color::Rgb(18, 22, 30)))
        .wrap(Wrap { trim: true });
    frame.render_widget(p, box_area);
}
