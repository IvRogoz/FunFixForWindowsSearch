use crate::SearchScope;

pub(crate) struct ParsedDirective {
    pub(crate) scope_override: Option<SearchScope>,
    pub(crate) clean_query: String,
    pub(crate) test_progress: bool,
    pub(crate) exit_app: bool,
    pub(crate) elevate_app: bool,
    pub(crate) latest_only: bool,
    pub(crate) latest_window_secs: Option<i64>,
    pub(crate) reindex_current_scope: bool,
    pub(crate) toggle_tracking: bool,
}

pub(crate) fn parse_scope_directive(input: &str) -> ParsedDirective {
    let mut scope_override = None;
    let mut remaining = Vec::new();
    let mut test_progress = false;
    let mut exit_app = false;
    let mut elevate_app = false;
    let mut latest_only = false;
    let mut latest_window_secs = None;
    let mut reindex_current_scope = false;
    let mut toggle_tracking = false;

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

        if normalized == "/up" {
            elevate_app = true;
            continue;
        }

        if normalized == "/latest" || normalized == "/last" {
            latest_only = true;
            continue;
        }

        if normalized == "/reindex" {
            reindex_current_scope = true;
            continue;
        }

        if normalized == "/track" {
            toggle_tracking = true;
            continue;
        }

        if latest_only && latest_window_secs.is_none() {
            if let Some(seconds) = parse_latest_window_token(&normalized) {
                latest_window_secs = Some(seconds);
                continue;
            }
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
        elevate_app,
        latest_only,
        latest_window_secs,
        reindex_current_scope,
        toggle_tracking,
    }
}

pub(crate) struct CommandMenuItem {
    pub(crate) command: &'static str,
    pub(crate) description: &'static str,
}

pub(crate) fn command_menu_items(input: &str, tracking_enabled: bool) -> Vec<CommandMenuItem> {
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
            command: "/up",
            description: "Relaunch app elevated",
        },
        CommandMenuItem {
            command: "/track",
            description: "Toggle live event tracking",
        },
        CommandMenuItem {
            command: "/latest",
            description: "Recent changes (/latest 30sec)",
        },
        CommandMenuItem {
            command: "/last",
            description: "Alias for /latest",
        },
        CommandMenuItem {
            command: "/reindex",
            description: "Reindex current scope now",
        },
        CommandMenuItem {
            command: "/exit",
            description: "Exit app immediately",
        },
    ];

    items
        .into_iter()
        .filter(|item| {
            if !tracking_enabled && (item.command == "/latest" || item.command == "/last") {
                return false;
            }
            true
        })
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

pub(crate) fn apply_command_choice(raw_query: &str, command: &str) -> String {
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

fn parse_latest_window_token(token: &str) -> Option<i64> {
    let trimmed = token.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return None;
    }

    let split_at = trimmed
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if split_at == 0 || split_at == trimmed.len() {
        return None;
    }

    let value = trimmed[..split_at].parse::<i64>().ok()?;
    if value <= 0 {
        return None;
    }

    let unit = &trimmed[split_at..];
    let factor = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        _ => return None,
    };

    Some(value.saturating_mul(factor))
}

pub(crate) fn format_latest_window(secs: i64) -> String {
    if secs % 86_400 == 0 {
        format!("{}d", secs / 86_400)
    } else if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

pub(crate) fn is_exact_directive_token(token: &str, tracking_enabled: bool) -> bool {
    let normalized = token.to_ascii_lowercase();
    let mut is_known = matches!(
        normalized.as_str(),
        "/entire" | "/all" | "/testprogress" | "/up" | "/track" | "/reindex" | "/exit"
    ) || parse_drive_directive(token).is_some();

    if tracking_enabled {
        is_known = is_known || normalized == "/latest" || normalized == "/last";
    }

    is_known
}

pub(crate) fn scope_arg_value(scope: &SearchScope) -> String {
    scope.label()
}
