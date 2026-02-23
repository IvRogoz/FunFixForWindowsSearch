use iced::Color;

use crate::SearchItem;

pub(crate) fn query_matches_item(query: &str, item: &SearchItem) -> bool {
    let name = file_name_from_path(item.path.as_ref());
    if query.contains('*') || query.contains('?') {
        wildcard_match_ascii_insensitive(query, name)
            || wildcard_match_ascii_insensitive(query, item.path.as_ref())
    } else {
        contains_ascii_case_insensitive(name, query)
            || contains_ascii_case_insensitive(item.path.as_ref(), query)
    }
}

pub(crate) fn contains_ascii_case_insensitive(haystack: &str, needle_lower_ascii: &str) -> bool {
    if needle_lower_ascii.is_empty() {
        return true;
    }

    let h = haystack.as_bytes();
    let n = needle_lower_ascii.as_bytes();
    if n.len() > h.len() {
        return false;
    }

    if n.len() == 1 {
        let b = n[0];
        return h.iter().any(|ch| ch.to_ascii_lowercase() == b);
    }

    let first = n[0];
    for start in 0..=h.len() - n.len() {
        if h[start].to_ascii_lowercase() != first {
            continue;
        }

        let mut ok = true;
        for i in 1..n.len() {
            if h[start + i].to_ascii_lowercase() != n[i] {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
    }

    false
}

fn wildcard_match_ascii_insensitive(pattern_lower_ascii: &str, text: &str) -> bool {
    let p = pattern_lower_ascii.as_bytes();
    let t = text.as_bytes();

    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti].to_ascii_lowercase() || p[pi] == b'?') {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

pub(crate) fn truncate_middle(input: &str, max_chars: usize) -> String {
    let chars: Vec<char> = input.chars().collect();
    if chars.len() <= max_chars {
        return input.to_string();
    }

    if max_chars <= 3 {
        return "...".to_string();
    }

    let keep = max_chars - 3;
    let left = keep / 2;
    let right = keep - left;

    let start: String = chars[..left].iter().collect();
    let end: String = chars[chars.len().saturating_sub(right)..].iter().collect();
    format!("{}...{}", start, end)
}

pub(crate) fn file_type_color(name: &str) -> Color {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".rs") {
        Color::from_rgb8(255, 153, 85)
    } else if lower.ends_with(".ts") || lower.ends_with(".tsx") {
        Color::from_rgb8(99, 179, 237)
    } else if lower.ends_with(".js") || lower.ends_with(".jsx") {
        Color::from_rgb8(246, 224, 94)
    } else if lower.ends_with(".json") {
        Color::from_rgb8(104, 211, 145)
    } else if lower.ends_with(".md") {
        Color::from_rgb8(180, 178, 255)
    } else {
        Color::from_rgb8(220, 220, 220)
    }
}

pub(crate) fn file_name_from_path(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}
