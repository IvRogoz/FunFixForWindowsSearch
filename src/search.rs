use ratatui::style::Color;

use crate::SearchItem;

pub(crate) struct SearchQuery {
    expr: SearchExpr,
}

enum SearchExpr {
    Single(String),
    Or(Vec<Vec<String>>),
}

#[derive(Clone, Copy)]
enum QueryOp {
    And,
    Or,
}

impl SearchQuery {
    pub(crate) fn parse(query: &str) -> Self {
        let query = query.trim();
        if let Some(groups) = parse_boolean_query(query) {
            Self {
                expr: SearchExpr::Or(groups),
            }
        } else {
            Self {
                expr: SearchExpr::Single(query.to_string()),
            }
        }
    }

    pub(crate) fn matches_item(&self, item: &SearchItem) -> bool {
        match &self.expr {
            SearchExpr::Single(query) => query_matches_item(query, item),
            SearchExpr::Or(groups) => groups
                .iter()
                .any(|terms| terms.iter().all(|term| query_matches_item(term, item))),
        }
    }

    pub(crate) fn boolean_groups(&self) -> Option<&[Vec<String>]> {
        match &self.expr {
            SearchExpr::Single(_) => None,
            SearchExpr::Or(groups) => Some(groups),
        }
    }
}

pub(crate) fn query_uses_boolean_logic(query: &str) -> bool {
    parse_boolean_query(query.trim()).is_some()
}

pub(crate) fn query_has_incomplete_boolean_logic(query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return false;
    }

    let mut saw_operator = false;
    let mut expecting_term = false;
    let mut saw_term = false;

    for word in query.split_whitespace() {
        if parse_query_operator(word).is_some() {
            saw_operator = true;
            if !saw_term || expecting_term {
                return true;
            }
            expecting_term = true;
        } else {
            saw_term = true;
            expecting_term = false;
        }
    }

    saw_operator && expecting_term
}

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

fn parse_boolean_query(query: &str) -> Option<Vec<Vec<String>>> {
    if query.is_empty() {
        return None;
    }

    let mut terms = Vec::new();
    let mut operators = Vec::new();
    let mut term_words = Vec::new();

    for word in query.split_whitespace() {
        if let Some(op) = parse_query_operator(word) {
            if term_words.is_empty() {
                return None;
            }
            terms.push(term_words.join(" "));
            operators.push(op);
            term_words.clear();
        } else {
            term_words.push(word);
        }
    }

    if operators.is_empty() || term_words.is_empty() {
        return None;
    }
    terms.push(term_words.join(" "));

    let mut groups: Vec<Vec<String>> = vec![Vec::new()];
    for (idx, term) in terms.into_iter().enumerate() {
        groups.last_mut()?.push(term);
        if let Some(QueryOp::Or) = operators.get(idx) {
            groups.push(Vec::new());
        }
    }

    groups
        .iter()
        .all(|group| group.iter().all(|term| !term.trim().is_empty()))
        .then_some(groups)
}

fn parse_query_operator(word: &str) -> Option<QueryOp> {
    if word.eq_ignore_ascii_case("and") {
        Some(QueryOp::And)
    } else if word.eq_ignore_ascii_case("or") {
        Some(QueryOp::Or)
    } else {
        None
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
        Color::Rgb(255, 153, 85)
    } else if lower.ends_with(".ts") || lower.ends_with(".tsx") {
        Color::Rgb(99, 179, 237)
    } else if lower.ends_with(".js") || lower.ends_with(".jsx") {
        Color::Rgb(246, 224, 94)
    } else if lower.ends_with(".json") {
        Color::Rgb(104, 211, 145)
    } else if lower.ends_with(".md") {
        Color::Rgb(180, 178, 255)
    } else {
        Color::Rgb(220, 220, 220)
    }
}

pub(crate) fn file_name_from_path(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SearchItemKind;

    #[test]
    fn contains_ascii_case_insensitive_works() {
        assert!(contains_ascii_case_insensitive("HelloWorld", "hello"));
        assert!(!contains_ascii_case_insensitive("HelloWorld", "xyz"));
    }

    #[test]
    fn wildcard_match_works() {
        let item = SearchItem {
            path: "C:\\tmp\\notes.txt".into(),
            modified_unix_secs: 0,
            kind: SearchItemKind::File,
        };
        assert!(query_matches_item("n*.txt", &item));
        assert!(query_matches_item("*tmp*", &item));
    }

    #[test]
    fn boolean_and_requires_all_terms() {
        let item = SearchItem {
            path: "C:\\tmp\\project notes.txt".into(),
            modified_unix_secs: 0,
            kind: SearchItemKind::File,
        };

        assert!(SearchQuery::parse("project AND notes").matches_item(&item));
        assert!(!SearchQuery::parse("project AND report").matches_item(&item));
    }

    #[test]
    fn boolean_or_allows_any_group() {
        let item = SearchItem {
            path: "C:\\tmp\\budget.xlsx".into(),
            modified_unix_secs: 0,
            kind: SearchItemKind::File,
        };

        assert!(SearchQuery::parse("notes OR budget").matches_item(&item));
        assert!(SearchQuery::parse("notes OR bud*").matches_item(&item));
        assert!(!SearchQuery::parse("notes OR report").matches_item(&item));
    }

    #[test]
    fn boolean_and_binds_inside_or_groups() {
        let item = SearchItem {
            path: "C:\\tmp\\client invoice.pdf".into(),
            modified_unix_secs: 0,
            kind: SearchItemKind::File,
        };

        assert!(SearchQuery::parse("notes AND draft OR client AND invoice").matches_item(&item));
        assert!(!SearchQuery::parse("notes OR client AND draft").matches_item(&item));
    }

    #[test]
    fn boolean_operators_must_be_standalone_words() {
        let item = SearchItem {
            path: "C:\\tmp\\candy orange.txt".into(),
            modified_unix_secs: 0,
            kind: SearchItemKind::File,
        };

        assert!(!query_uses_boolean_logic("candy"));
        assert!(!query_uses_boolean_logic("orange"));
        assert!(SearchQuery::parse("candy orange").matches_item(&item));
    }

    #[test]
    fn incomplete_boolean_queries_are_detected() {
        assert!(query_has_incomplete_boolean_logic("AND"));
        assert!(query_has_incomplete_boolean_logic("OR"));
        assert!(query_has_incomplete_boolean_logic("project AND"));
        assert!(query_has_incomplete_boolean_logic("project OR"));
        assert!(query_has_incomplete_boolean_logic("project AND OR notes"));
        assert!(!query_has_incomplete_boolean_logic("project"));
        assert!(!query_has_incomplete_boolean_logic("project AND notes"));
        assert!(!query_has_incomplete_boolean_logic("project OR notes"));
    }
}
