use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;

use crate::search::SearchQuery;
use crate::{SearchItem, SEARCH_BATCH_SIZE, UNKNOWN_TS, VISIBLE_RESULTS_LIMIT};

pub(crate) enum SearchEvent {
    Progress {
        generation: u64,
        scanned: usize,
        total: usize,
    },
    Done {
        generation: u64,
        items: Vec<SearchItem>,
    },
}

pub(crate) enum SearchWorkerMessage {
    SetCorpus {
        items: Vec<SearchItem>,
        recent_event_by_path: HashMap<Box<str>, i64>,
    },
    ApplyDelta {
        upserts: Vec<SearchItem>,
        deleted_paths: Vec<Box<str>>,
        tracking_enabled: bool,
        now_unix: i64,
    },
    ClearRecentEvents,
    Run {
        generation: u64,
        query: String,
        latest_only_mode: bool,
        latest_window_secs: i64,
    },
    Cancel,
    Clear,
}

struct SearchRun {
    generation: u64,
    query: String,
    latest_only_mode: bool,
    latest_window_secs: i64,
}

pub(crate) fn spawn_search_worker() -> (
    mpsc::Sender<SearchWorkerMessage>,
    mpsc::Receiver<SearchEvent>,
) {
    let (request_tx, request_rx) = mpsc::channel::<SearchWorkerMessage>();
    let (event_tx, event_rx) = mpsc::channel::<SearchEvent>();

    thread::spawn(move || {
        let mut corpus: Vec<SearchItem> = Vec::new();
        let mut positions = HashMap::new();
        let mut recent_event_by_path: HashMap<Box<str>, i64> = HashMap::new();
        let mut pending_run: Option<SearchRun> = None;

        loop {
            if let Some(run) = pending_run.take() {
                if run_search_query(
                    run,
                    &mut corpus,
                    &mut positions,
                    &mut recent_event_by_path,
                    &request_rx,
                    &event_tx,
                    &mut pending_run,
                ) {
                    break;
                }
                continue;
            }

            match request_rx.recv() {
                Ok(SearchWorkerMessage::SetCorpus {
                    items,
                    recent_event_by_path: recent,
                }) => {
                    corpus = items;
                    positions = crate::corpus::item_positions(&corpus);
                    recent_event_by_path = recent;
                }
                Ok(SearchWorkerMessage::ApplyDelta {
                    upserts,
                    deleted_paths,
                    tracking_enabled,
                    now_unix,
                }) => {
                    apply_worker_delta(
                        &mut corpus,
                        &mut positions,
                        &mut recent_event_by_path,
                        upserts,
                        &deleted_paths,
                        tracking_enabled,
                        now_unix,
                    );
                }
                Ok(SearchWorkerMessage::ClearRecentEvents) => recent_event_by_path.clear(),
                Ok(SearchWorkerMessage::Run {
                    generation,
                    query,
                    latest_only_mode,
                    latest_window_secs,
                }) => {
                    pending_run = Some(SearchRun {
                        generation,
                        query,
                        latest_only_mode,
                        latest_window_secs,
                    });
                }
                Ok(SearchWorkerMessage::Clear) => {
                    corpus.clear();
                    positions.clear();
                    recent_event_by_path.clear();
                    pending_run = None;
                }
                Ok(SearchWorkerMessage::Cancel) => {
                    pending_run = None;
                }
                Err(_) => break,
            }
        }
    });

    (request_tx, event_rx)
}

fn run_search_query(
    run: SearchRun,
    corpus: &mut Vec<SearchItem>,
    positions: &mut HashMap<Box<str>, usize>,
    recent_event_by_path: &mut HashMap<Box<str>, i64>,
    request_rx: &mpsc::Receiver<SearchWorkerMessage>,
    event_tx: &mpsc::Sender<SearchEvent>,
    pending_run: &mut Option<SearchRun>,
) -> bool {
    let total = corpus.len().max(1);
    let latest_cutoff = if run.latest_only_mode {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Some(now - run.latest_window_secs)
    } else {
        None
    };

    let mut out: Vec<SearchItem> = Vec::new();
    let parsed_query = (!run.query.is_empty()).then(|| SearchQuery::parse(&run.query));

    let mut start = 0usize;
    while start < corpus.len() {
        if let Ok(message) = request_rx.try_recv() {
            match message {
                SearchWorkerMessage::SetCorpus {
                    items,
                    recent_event_by_path: recent,
                } => {
                    *corpus = items;
                    *positions = crate::corpus::item_positions(corpus);
                    *recent_event_by_path = recent;
                    return false;
                }
                SearchWorkerMessage::ApplyDelta {
                    upserts,
                    deleted_paths,
                    tracking_enabled,
                    now_unix,
                } => {
                    apply_worker_delta(
                        corpus,
                        positions,
                        recent_event_by_path,
                        upserts,
                        &deleted_paths,
                        tracking_enabled,
                        now_unix,
                    );
                    return false;
                }
                SearchWorkerMessage::ClearRecentEvents => {
                    recent_event_by_path.clear();
                    return false;
                }
                SearchWorkerMessage::Run {
                    generation,
                    query,
                    latest_only_mode,
                    latest_window_secs,
                } => {
                    *pending_run = Some(SearchRun {
                        generation,
                        query,
                        latest_only_mode,
                        latest_window_secs,
                    });
                    return false;
                }
                SearchWorkerMessage::Clear => {
                    corpus.clear();
                    positions.clear();
                    recent_event_by_path.clear();
                    *pending_run = None;
                    return false;
                }
                SearchWorkerMessage::Cancel => {
                    *pending_run = None;
                    return false;
                }
            }
        }

        let end = (start + SEARCH_BATCH_SIZE).min(corpus.len());
        for item in &corpus[start..end] {
            let matches_latest = latest_cutoff
                .map(|cutoff| {
                    latest_timestamp(item, recent_event_by_path)
                        .map(|ts| ts >= cutoff)
                        .unwrap_or(false)
                })
                .unwrap_or(true);

            let matches_query = parsed_query
                .as_ref()
                .map(|query| query.matches_item(item))
                .unwrap_or(true);

            if matches_latest && matches_query {
                out.push(item.clone());
                if !run.latest_only_mode && out.len() >= VISIBLE_RESULTS_LIMIT {
                    break;
                }
            }
        }

        if run.latest_only_mode && out.len() > VISIBLE_RESULTS_LIMIT {
            out.select_nth_unstable_by_key(VISIBLE_RESULTS_LIMIT, |item| {
                std::cmp::Reverse(latest_timestamp(item, recent_event_by_path))
            });
            out.truncate(VISIBLE_RESULTS_LIMIT);
        }

        let scanned = end.min(total);
        let _ = event_tx.send(SearchEvent::Progress {
            generation: run.generation,
            scanned,
            total,
        });

        if !run.latest_only_mode && out.len() >= VISIBLE_RESULTS_LIMIT {
            break;
        }

        start = end;
    }

    if run.latest_only_mode {
        out.sort_by_key(|item| std::cmp::Reverse(latest_timestamp(item, recent_event_by_path)));
        out.truncate(VISIBLE_RESULTS_LIMIT);
    }

    let _ = event_tx.send(SearchEvent::Done {
        generation: run.generation,
        items: out,
    });
    false
}

fn apply_worker_delta(
    corpus: &mut Vec<SearchItem>,
    positions: &mut HashMap<Box<str>, usize>,
    recent: &mut HashMap<Box<str>, i64>,
    upserts: Vec<SearchItem>,
    deleted_paths: &[Box<str>],
    tracking_enabled: bool,
    now_unix: i64,
) {
    for path in deleted_paths {
        recent.remove(path.as_ref());
    }
    if tracking_enabled {
        for item in &upserts {
            recent.insert(
                item.path.clone(),
                if item.modified_unix_secs == UNKNOWN_TS {
                    now_unix
                } else {
                    item.modified_unix_secs
                },
            );
        }
    }
    crate::corpus::apply_delta(corpus, positions, upserts, deleted_paths);
}
fn latest_timestamp(
    item: &SearchItem,
    recent_event_by_path: &HashMap<Box<str>, i64>,
) -> Option<i64> {
    recent_event_by_path
        .get(item.path.as_ref())
        .copied()
        .or((item.modified_unix_secs != UNKNOWN_TS).then_some(item.modified_unix_secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SearchItemKind;

    #[test]
    fn worker_applies_deltas_and_tracks_recent_events() {
        let (tx, rx) = spawn_search_worker();
        let item = |path: &str, ts| SearchItem {
            path: path.into(),
            modified_unix_secs: ts,
            kind: SearchItemKind::File,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        tx.send(SearchWorkerMessage::SetCorpus {
            items: vec![item("a", 0), item("b", 0), item("c", 0)],
            recent_event_by_path: HashMap::new(),
        })
        .unwrap();
        tx.send(SearchWorkerMessage::ApplyDelta {
            upserts: vec![item("c", UNKNOWN_TS), item("d", UNKNOWN_TS)],
            deleted_paths: vec!["a".into()],
            tracking_enabled: true,
            now_unix: now,
        })
        .unwrap();
        tx.send(SearchWorkerMessage::ApplyDelta {
            upserts: vec![item("c", UNKNOWN_TS)],
            deleted_paths: vec!["b".into()],
            tracking_enabled: true,
            now_unix: now,
        })
        .unwrap();
        tx.send(SearchWorkerMessage::Run {
            generation: 1,
            query: String::new(),
            latest_only_mode: true,
            latest_window_secs: 60,
        })
        .unwrap();
        let items = loop {
            if let SearchEvent::Done {
                generation: 1,
                items,
            } = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap()
            {
                break items;
            }
        };
        let mut paths: Vec<_> = items.iter().map(|item| item.path.as_ref()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["c", "d"]);
        tx.send(SearchWorkerMessage::ClearRecentEvents).unwrap();
        tx.send(SearchWorkerMessage::Run {
            generation: 2,
            query: String::new(),
            latest_only_mode: true,
            latest_window_secs: 60,
        })
        .unwrap();
        let items = loop {
            if let SearchEvent::Done {
                generation: 2,
                items,
            } = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap()
            {
                break items;
            }
        };
        assert!(items.is_empty());
    }

    #[test]
    fn clear_during_search_clears_all_worker_state() {
        let mut corpus = vec![SearchItem {
            path: "a".into(),
            modified_unix_secs: 0,
            kind: SearchItemKind::File,
        }];
        let mut positions = crate::corpus::item_positions(&corpus);
        let mut recent = HashMap::from([("a".into(), 1)]);
        let (tx, rx) = mpsc::channel();
        let (event_tx, _) = mpsc::channel();
        tx.send(SearchWorkerMessage::Clear).unwrap();
        run_search_query(
            SearchRun {
                generation: 1,
                query: String::new(),
                latest_only_mode: false,
                latest_window_secs: 0,
            },
            &mut corpus,
            &mut positions,
            &mut recent,
            &rx,
            &event_tx,
            &mut None,
        );
        assert!(corpus.is_empty() && positions.is_empty() && recent.is_empty());
    }
    #[test]
    fn searches_substrings_and_paths_for_plain_and_boolean_queries() {
        let mut corpus: Vec<SearchItem> = [
            "C:\\files\\invoice.pdf",
            "C:\\files\\annual-report.txt",
            "C:\\report\\other.txt",
        ]
        .into_iter()
        .map(|path| SearchItem {
            path: path.into(),
            modified_unix_secs: 0,
            kind: SearchItemKind::File,
        })
        .collect();
        let (_request_tx, request_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();

        for (query, expected) in [("invoice AND pdf", 1), ("report", 2)] {
            run_search_query(
                SearchRun {
                    generation: 1,
                    query: query.to_ascii_lowercase(),
                    latest_only_mode: false,
                    latest_window_secs: 0,
                },
                &mut corpus,
                &mut HashMap::new(),
                &mut HashMap::new(),
                &request_rx,
                &event_tx,
                &mut None,
            );
            let items = event_rx
                .try_iter()
                .find_map(|event| match event {
                    SearchEvent::Done { items, .. } => Some(items),
                    _ => None,
                })
                .unwrap();
            assert_eq!(items.len(), expected, "query: {query}");
        }
    }

    #[test]
    fn latest_results_are_limited_after_sorting() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let count = SEARCH_BATCH_SIZE + VISIBLE_RESULTS_LIMIT + 1;
        let mut corpus: Vec<SearchItem> = (0..count)
            .map(|i| SearchItem {
                path: format!("C:\\test\\{i}.txt").into(),
                modified_unix_secs: now - (count - 1 - i) as i64,
                kind: SearchItemKind::File,
            })
            .collect();
        let (_request_tx, request_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();

        run_search_query(
            SearchRun {
                generation: 1,
                query: String::new(),
                latest_only_mode: true,
                latest_window_secs: count as i64 + 60,
            },
            &mut corpus,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &request_rx,
            &event_tx,
            &mut None,
        );

        let items = event_rx
            .try_iter()
            .find_map(|event| match event {
                SearchEvent::Done { items, .. } => Some(items),
                _ => None,
            })
            .unwrap();
        assert_eq!(items.len(), VISIBLE_RESULTS_LIMIT);
        assert_eq!(items.first().unwrap().modified_unix_secs, now);
        assert_eq!(
            items.last().unwrap().modified_unix_secs,
            now - VISIBLE_RESULTS_LIMIT as i64 + 1
        );
    }
}
