use iced::widget::operation;
use iced::{widget, window, Point, Size, Task};

use crate::{Message, PANEL_HEIGHT, PANEL_WIDTH, PANEL_WIDTH_RATIO};

pub(crate) fn native_window_settings() -> window::Settings {
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

pub(crate) fn initialize_panel_hidden_mode() -> Task<Message> {
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

pub(crate) fn prepare_panel_for_show_mode() -> Task<Message> {
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

pub(crate) fn finalize_panel_hidden_mode() -> Task<Message> {
    window::latest().then(move |maybe_id| {
        if let Some(id) = maybe_id {
            window::enable_mouse_passthrough(id)
        } else {
            Task::none()
        }
    })
}

pub(crate) fn sync_window_to_progress(progress: f32) -> Task<Message> {
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

pub(crate) fn sync_results_scroll(
    scroll_id: widget::Id,
    selected: usize,
    total: usize,
) -> Task<Message> {
    if total <= 1 {
        return Task::none();
    }

    let y = (selected as f32 / (total.saturating_sub(1)) as f32).clamp(0.0, 1.0);
    operation::snap_to(scroll_id, widget::scrollable::RelativeOffset { x: 0.0, y })
}

pub(crate) fn keep_search_input_focus(search_input_id: widget::Id) -> Task<Message> {
    Task::batch(vec![
        operation::focus(search_input_id.clone()),
        operation::move_cursor_to_end(search_input_id),
    ])
}

fn start_hidden_position(window: Size, monitor: Size) -> Point {
    let target_width = panel_width_for_monitor(monitor.width);
    let x = ((monitor.width - target_width) / 2.0).max(0.0);
    Point::new(x, -window.height)
}

fn panel_width_for_monitor(monitor_width: f32) -> f32 {
    (monitor_width * PANEL_WIDTH_RATIO).clamp(640.0, 1800.0)
}

fn ease_out_cubic(t: f32) -> f32 {
    let clamped = t.clamp(0.0, 1.0);
    1.0 - (1.0 - clamped).powi(3)
}
