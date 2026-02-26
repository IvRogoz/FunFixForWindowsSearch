use std::time::Duration;

use eframe::egui;

use crate::app_state::AppState;
use crate::commands::{command_menu_items, format_latest_window};
use crate::search::{file_name_from_path, truncate_middle};
use crate::{format_bytes, FILE_PATH_MAX_CHARS};

pub(crate) fn draw(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    app: &AppState,
    frame_time_ms: f32,
    repaint_after: Duration,
) {
    let mut results_rect = egui::Rect::NOTHING;
    let full_rect = ui.max_rect();
    ui.painter()
        .rect_filled(full_rect, 0.0, egui::Color32::from_rgb(10, 14, 20));
    ui.set_min_size(full_rect.size());

    let mut remaining_h = ui.available_height();

    ui.vertical(|ui| {
        draw_prompt(ui, app);
        remaining_h -= 58.0;

        if app.indexing_in_progress || app.active_search_query.is_some() {
            ui.add_space(4.0);
            draw_progress(ui, app);
            remaining_h -= 38.0;
        }

        ui.add_space(6.0);
        remaining_h -= 6.0;

        let results_h = (remaining_h - 48.0).max(120.0);
        results_rect = draw_results(ui, app, results_h);

        ui.add_space(4.0);
        draw_status(ui, app);
        draw_footer(ui, app, frame_time_ms, repaint_after);
    });

    draw_command_popup(ctx, app, results_rect);
    draw_notice_overlay(ctx, app);
}

fn draw_prompt(ui: &mut egui::Ui, app: &AppState) {
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(15, 20, 28))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(62, 72, 86)))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let title = if app.indexing_in_progress {
                "Indexing"
            } else {
                "Search"
            };
            ui.label(
                egui::RichText::new(title)
                    .color(egui::Color32::from_rgb(155, 168, 185))
                    .small(),
            );
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(">")
                        .color(egui::Color32::from_rgb(255, 213, 128))
                        .strong(),
                );
                let w = ui.available_width();
                ui.allocate_ui_with_layout(
                    egui::vec2(w, 18.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(
                            egui::RichText::new(format!("{}{}", app.raw_query, "█"))
                                .color(egui::Color32::from_rgb(236, 239, 244))
                                .monospace(),
                        );
                    },
                );
            });
        });
}

fn draw_progress(ui: &mut egui::Ui, app: &AppState) {
    let (label, value, fill) = if app.indexing_in_progress {
        (
            format!(
                "{} {}",
                index_phase_label(app.indexing_phase),
                app.scope.label()
            ),
            app.indexing_progress,
            egui::Color32::from_rgb(178, 126, 28),
        )
    } else {
        let total = app.all_items.len().max(1);
        (
            "search".to_string(),
            (app.active_search_cursor as f32 / total as f32).clamp(0.0, 1.0),
            egui::Color32::from_rgb(56, 122, 168),
        )
    };

    let ratio = value.clamp(0.0, 1.0);

    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 40.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.set_width(ui.available_width());
            egui::Frame::default()
                .fill(egui::Color32::from_rgb(12, 16, 22))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(62, 72, 86)))
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    let bar_h = 18.0;
                    let (bar_rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), bar_h),
                        egui::Sense::hover(),
                    );

                    let painter = ui.painter();
                    painter.rect_filled(bar_rect, 0.0, egui::Color32::from_rgb(24, 30, 40));
                    painter.rect_stroke(
                        bar_rect,
                        0.0,
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(62, 72, 86)),
                        egui::StrokeKind::Outside,
                    );

                    let fill_w = (bar_rect.width() * ratio).clamp(0.0, bar_rect.width());
                    let fill_rect = egui::Rect::from_min_size(
                        bar_rect.min,
                        egui::vec2(fill_w, bar_rect.height()),
                    );
                    painter.rect_filled(fill_rect, 0.0, fill);

                    painter.text(
                        bar_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        format!("{} {:.0}%", label, ratio * 100.0),
                        egui::FontId::monospace(12.0),
                        egui::Color32::WHITE,
                    );
                });
        },
    );
}

fn draw_results(ui: &mut egui::Ui, app: &AppState, target_height: f32) -> egui::Rect {
    let frame = egui::Frame::default()
        .fill(egui::Color32::from_rgb(10, 14, 20))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(62, 72, 86)))
        .inner_margin(egui::Margin::same(0));

    let out = ui
        .allocate_ui_with_layout(
            egui::vec2(ui.available_width(), target_height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(ui.available_width());
                frame.show(ui, |ui| {
                    ui.set_min_size(egui::vec2(ui.available_width(), target_height));
                    ui.set_min_width(ui.available_width());
                    ui.label(
                        egui::RichText::new("Results")
                            .color(egui::Color32::from_rgb(155, 168, 185))
                            .small(),
                    );

                    let row_h = 20.0;
                    let list_h = (ui.available_height() - 2.0).max(80.0);
                    egui::ScrollArea::vertical()
                        .id_salt("results-scroll")
                        .auto_shrink([false, false])
                        .max_height(list_h)
                        .show(ui, |ui| {
                            for (row, item) in app.items.iter().enumerate() {
                                let selected = row == app.selected;
                                let name = file_name_from_path(item.path.as_ref());
                                let path = truncate_middle(item.path.as_ref(), FILE_PATH_MAX_CHARS);

                                let text = format!(
                                    "{} {}  {}",
                                    if selected { ">" } else { " " },
                                    name,
                                    path
                                );

                                let (row_rect, response) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), row_h),
                                    egui::Sense::hover(),
                                );

                                if selected {
                                    ui.painter().rect_filled(
                                        row_rect,
                                        0.0,
                                        egui::Color32::from_rgb(58, 84, 122),
                                    );
                                }

                                ui.painter().text(
                                    egui::pos2(row_rect.left() + 2.0, row_rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    text,
                                    egui::FontId::monospace(13.0),
                                    if selected {
                                        egui::Color32::from_rgb(255, 213, 128)
                                    } else {
                                        file_color(name)
                                    },
                                );

                                if selected {
                                    ui.scroll_to_rect(response.rect, Some(egui::Align::Center));
                                }
                            }
                        });
                })
            },
        )
        .inner;

    out.response.rect
}

fn draw_status(ui: &mut egui::Ui, app: &AppState) {
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

    ui.add_sized(
        [ui.available_width(), 18.0],
        egui::Label::new(
            egui::RichText::new(status)
                .monospace()
                .color(egui::Color32::from_rgb(160, 168, 178)),
        )
        .truncate(),
    );
}

fn draw_footer(ui: &mut egui::Ui, app: &AppState, frame_time_ms: f32, repaint_after: Duration) {
    ui.add_sized(
        [ui.available_width(), 18.0],
        egui::Label::new(
            egui::RichText::new(format!(
                "Enter open | Alt+Enter reveal | Esc hide | IDX: {} | LIVE: {} | STATE: {} | RENDER: gpu {:.1}ms | TICK: {}ms",
                app.index_backend.label(),
                if app.index_backend.live_updates() {
                    "on"
                } else {
                    "off"
                },
                if app.indexing_in_progress {
                    "indexing"
                } else {
                    "idle"
                },
                frame_time_ms,
                repaint_after.as_millis(),
            ))
            .monospace()
            .color(egui::Color32::from_rgb(150, 162, 178)),
        )
        .truncate(),
    );
}

fn draw_command_popup(ctx: &egui::Context, app: &AppState, results_rect: egui::Rect) {
    let items = command_menu_items(&app.raw_query, app.tracking_enabled);
    if items.is_empty() || !results_rect.is_positive() {
        return;
    }

    let pos = egui::pos2(results_rect.left() + 8.0, results_rect.top() + 8.0);
    egui::Area::new(egui::Id::new("commands-popup"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .fill(egui::Color32::from_rgb(20, 26, 36))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(78, 92, 112)))
                .show(ui, |ui| {
                    ui.set_max_width(640.0);
                    ui.set_min_width(500.0);
                    ui.label(
                        egui::RichText::new("Commands")
                            .color(egui::Color32::from_rgb(160, 170, 190))
                            .small(),
                    );

                    egui::ScrollArea::vertical()
                        .max_height((results_rect.height() - 20.0).max(140.0))
                        .show(ui, |ui| {
                            for (idx, item) in items.iter().enumerate() {
                                let selected = idx == app.command_selected;
                                let color = if selected {
                                    egui::Color32::from_rgb(255, 213, 128)
                                } else {
                                    egui::Color32::from_rgb(210, 220, 235)
                                };
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} {:<12} {}",
                                        if selected { ">" } else { " " },
                                        item.command,
                                        item.description
                                    ))
                                    .monospace()
                                    .color(color),
                                );
                            }
                        });
                });
        });
}

fn draw_notice_overlay(ctx: &egui::Context, app: &AppState) {
    if !app.show_quick_help_overlay && !app.show_privilege_overlay && !app.show_about_overlay {
        return;
    }

    let (title, color, lines): (&str, egui::Color32, Vec<&str>) = if app.show_privilege_overlay {
        (
            "Notice",
            egui::Color32::from_rgb(230, 80, 80),
            vec![
                "███    ██  ██████  ████████     ███████ ██      ███████ ██    ██  █████  ████████ ███████ ██████  ",
                "_████   ██ ██    ██    ██        ██      ██      ██      ██    ██ ██   ██    ██    ██      ██   ██ ",
                "_██ ██  ██ ██    ██    ██        █████   ██      █████   ██    ██ ███████    ██    █████   ██   ██ ",
                "_██  ██ ██ ██    ██    ██        ██      ██      ██       ██  ██  ██   ██    ██    ██      ██   ██ ",
                "██   ████  ██████     ██        ███████ ███████ ███████   ████   ██   ██    ██    ███████ ██████  ",
                "",
                "NTFS access is unavailable in this mode",
                "Using DIRWALK fallback (SLOWER)",
                "Type /up and press Enter to relaunch elevated",
            ],
        )
    } else if app.show_about_overlay {
        (
            "About",
            egui::Color32::from_rgb(130, 210, 255),
            vec![
                "NTFSSearch",
                "made by IvRogoz - 2026",
                "Rendering: egui native GPU UI (fallback: /soft)",
                "Indexing: NTFS/USN live when elevated, DIRWALK fallback otherwise",
                "Hotkey: ` toggles panel | Enter opens | Alt+Enter reveals",
                "Commands: /all /entire /reindex /up /track /latest /fullscreen /fullheight",
                "",
                "Press any key to close",
            ],
        )
    } else {
        (
            "Notice",
            egui::Color32::from_rgb(130, 210, 255),
            vec![
                "Quick Start",
                "Press ` to show or hide RustSearch",
                "Type to search, Enter to open, Alt+Enter to reveal",
                "Use / for commands: /all /entire /reindex /track /exit",
            ],
        )
    };

    let screen = ctx.content_rect();
    let width = (screen.width() * 0.86).min(980.0);

    egui::Area::new(egui::Id::new("notice-overlay"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .fill(egui::Color32::from_rgb(18, 22, 30))
                .stroke(egui::Stroke::new(1.0, color))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(width);
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new(title).strong().color(color).monospace());
                        ui.add_space(4.0);

                        for line in lines {
                            if line == "Type /up and press Enter to relaunch elevated" {
                                ui.horizontal_centered(|ui| {
                                    ui.label(egui::RichText::new("Type ").color(color).monospace());
                                    ui.label(
                                        egui::RichText::new("/up")
                                            .strong()
                                            .color(color)
                                            .monospace(),
                                    );
                                    ui.label(
                                        egui::RichText::new(
                                            " and press Enter to relaunch elevated",
                                        )
                                        .color(color)
                                        .monospace(),
                                    );
                                });
                            } else {
                                ui.label(egui::RichText::new(line).color(color).monospace());
                            }
                        }
                    });
                });
        });
}

fn file_color(name: &str) -> egui::Color32 {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".rs") {
        egui::Color32::from_rgb(255, 153, 85)
    } else if lower.ends_with(".ts") || lower.ends_with(".tsx") {
        egui::Color32::from_rgb(99, 179, 237)
    } else if lower.ends_with(".js") || lower.ends_with(".jsx") {
        egui::Color32::from_rgb(246, 224, 94)
    } else if lower.ends_with(".json") {
        egui::Color32::from_rgb(104, 211, 145)
    } else if lower.ends_with(".md") {
        egui::Color32::from_rgb(180, 178, 255)
    } else {
        egui::Color32::from_rgb(220, 220, 220)
    }
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
