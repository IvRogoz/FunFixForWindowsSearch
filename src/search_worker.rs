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
        let mut recent_event_by_path: HashMap<Box<str>, i64> = HashMap::new();
        let mut pending_run: Option<SearchRun> = None;

        loop {
            if let Some(run) = pending_run.take() {
                if run_search_query(
                    run,
                    &mut corpus,
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
                    recent_event_by_path = recent;
                }
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
                    *recent_event_by_path = recent;
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
                SearchWorkerMessage::Clear | SearchWorkerMessage::Cancel => {
                    *pending_run = None;
                    return false;
                }
            }
        }

        let end = (start + SEARCH_BATCH_SIZE).min(corpus.len());
        for item in &corpus[start..end] {
            let matches_latest = latest_cutoff
                .map(|cutoff| {
                    recent_event_by_path
                        .get(item.path.as_ref())
                        .copied()
                        .or((item.modified_unix_secs != UNKNOWN_TS)
                            .then_some(item.modified_unix_secs))
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
                if out.len() >= VISIBLE_RESULTS_LIMIT {
                    break;
                }
            }
        }

        let scanned = end.min(total);
        let _ = event_tx.send(SearchEvent::Progress {
            generation: run.generation,
            scanned,
            total,
        });

        if out.len() >= VISIBLE_RESULTS_LIMIT {
            break;
        }

        start = end;
    }

    if run.latest_only_mode {
        out.sort_by_key(|item| {
            std::cmp::Reverse(
                recent_event_by_path
                    .get(item.path.as_ref())
                    .copied()
                    .or((item.modified_unix_secs != UNKNOWN_TS).then_some(item.modified_unix_secs))
                    .unwrap_or(i64::MIN),
            )
        });
    }

    let _ = event_tx.send(SearchEvent::Done {
        generation: run.generation,
        items: out,
    });
    false
}
