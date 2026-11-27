//! Explicitly invoked integration test: real filesystem operations and the production USN loop.
//! No native window, hotkey, tray, shell action, desktop, or screen API is used.
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    indexing_ntfs, search_worker, storage, IndexBackend, IndexEvent, SearchItem, SearchItemKind,
    SearchScope,
};
use search_worker::{SearchEvent, SearchWorkerMessage};

struct LiveJob {
    cancel: Arc<AtomicBool>,
    finished: mpsc::Receiver<bool>,
    thread: Option<JoinHandle<()>>,
    events: mpsc::Receiver<IndexEvent>,
    items: HashMap<String, SearchItem>,
    revisions: HashMap<String, usize>,
    deltas: usize,
    root: String,
    search_tx: mpsc::Sender<SearchWorkerMessage>,
    search_rx: mpsc::Receiver<SearchEvent>,
}

fn key(path: &Path) -> String {
    path.to_str()
        .expect("fixture path is UTF-8")
        .replace('/', "\\")
        .to_ascii_lowercase()
}

impl LiveJob {
    fn start(root: &Path, job_id: u64) -> Self {
        let roots = vec![root.to_str().unwrap().to_owned()];
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (tx, events) = mpsc::channel();
        let (done_tx, finished) = mpsc::channel();
        let thread = thread::spawn(move || {
            let live = indexing_ntfs::run_ntfs_live_index_job(
                SearchScope::CurrentFolder,
                &roots,
                job_id,
                &tx,
                &worker_cancel,
            );
            let _ = done_tx.send(live);
        });
        let (search_tx, search_rx) = search_worker::spawn_search_worker();
        Self {
            cancel,
            finished,
            thread: Some(thread),
            events,
            items: HashMap::new(),
            revisions: HashMap::new(),
            deltas: 0,
            root: key(root),
            search_tx,
            search_rx,
        }
    }

    fn apply(&mut self, event: IndexEvent) -> bool {
        match event {
            IndexEvent::Done { items, backend, .. } => {
                assert_eq!(
                    backend,
                    IndexBackend::NtfsUsnLive,
                    "directory-walking fallback is not a live USN pass"
                );
                self.search_tx
                    .send(SearchWorkerMessage::SetCorpus {
                        items: items.clone(),
                        recent_event_by_path: HashMap::new(),
                    })
                    .unwrap();
                self.items.clear();
                for item in items {
                    let path = key(Path::new(item.path.as_ref()));
                    self.assert_scoped(&path);
                    self.items.insert(path, item);
                }
                true
            }
            IndexEvent::Delta {
                upserts,
                deleted_paths,
                ..
            } => {
                self.deltas += 1;
                self.search_tx
                    .send(SearchWorkerMessage::ApplyDelta {
                        upserts: upserts.clone(),
                        deleted_paths: deleted_paths
                            .iter()
                            .cloned()
                            .map(String::into_boxed_str)
                            .collect(),
                        tracking_enabled: true,
                        now_unix: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs() as i64,
                    })
                    .unwrap();
                for path in deleted_paths {
                    let path = key(Path::new(&path));
                    self.assert_scoped(&path);
                    self.items.remove(&path);
                }
                for item in upserts {
                    let path = key(Path::new(item.path.as_ref()));
                    self.assert_scoped(&path);
                    *self.revisions.entry(path.clone()).or_default() += 1;
                    self.items.insert(path, item);
                }
                false
            }
            IndexEvent::Progress { .. } => false,
            IndexEvent::SnapshotLoaded { .. } => panic!("unexpected fallback snapshot event"),
        }
    }

    fn assert_scoped(&self, path: &str) {
        assert!(
            path == self.root || path.starts_with(&(self.root.clone() + "\\")),
            "an event escaped the fixture scope"
        );
    }

    fn initial(&mut self) {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(120);
        let mut last_report = start;
        loop {
            assert!(
                Instant::now() < deadline,
                "initial live enumeration exceeded 120 seconds"
            );
            match self.events.recv_timeout(Duration::from_secs(1)) {
                Ok(event) => {
                    if self.apply(event) {
                        println!(
                            "PASS initial NTFS enumeration: {} scoped items in {:?}",
                            self.items.len(),
                            start.elapsed()
                        );
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                    "live USN worker exited before Done; journal enumeration/access is unavailable"
                ),
            }
            if last_report.elapsed() >= Duration::from_secs(5) {
                println!("Waiting for live NTFS enumeration ({:?})", start.elapsed());
                last_report = Instant::now();
            }
        }
    }

    fn until(&mut self, name: &str, condition: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(20);
        while !condition(self) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for {name}");
            match self
                .events
                .recv_timeout(remaining.min(Duration::from_millis(500)))
            {
                Ok(event) => {
                    self.apply(event);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => panic!("{name}: worker disconnected: {error}"),
            }
        }
        println!("PASS {name} ({:?})", start.elapsed());
    }

    fn contains(&self, path: &Path) -> bool {
        self.items.contains_key(&key(path))
    }

    fn quiet(&mut self, duration: Duration, settle: bool) {
        let deadline = Instant::now()
            + duration
            + if settle {
                Duration::from_secs(15)
            } else {
                Duration::ZERO
            };
        let mut quiet_since = Instant::now();
        loop {
            if quiet_since.elapsed() >= duration {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "scoped USN stream never became idle"
            );
            match self.events.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => {
                    assert!(settle, "unexpected scoped event during idle observation");
                    self.apply(event);
                    quiet_since = Instant::now();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => panic!("live worker exited during idle observation: {error}"),
            }
        }
    }

    fn search(&self, query: &str, expected: Option<&Path>) {
        self.search_tx
            .send(SearchWorkerMessage::Run {
                generation: 1,
                query: query.into(),
                latest_only_mode: false,
                latest_window_secs: 0,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "search worker timed out");
            if let SearchEvent::Done { items, .. } = self.search_rx.recv_timeout(remaining).unwrap()
            {
                match expected {
                    Some(path) => assert!(items
                        .iter()
                        .any(|item| key(Path::new(item.path.as_ref())) == key(path))),
                    None => assert!(items.is_empty(), "search retained a deleted/renamed result"),
                }
                println!("PASS live delta propagated to search: {query}");
                return;
            }
        }
    }

    fn stop(&mut self) {
        let start = Instant::now();
        self.cancel.store(true, Ordering::Release);
        assert!(self
            .finished
            .recv_timeout(Duration::from_secs(3))
            .expect("live worker ignored cancellation"));
        self.thread
            .take()
            .unwrap()
            .join()
            .expect("live worker panicked");
        println!(
            "PASS idle cancellation and worker join ({:?})",
            start.elapsed()
        );
    }
}

impl Drop for LiveJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.thread.take() {
            if self.finished.recv_timeout(Duration::from_secs(3)).is_ok() {
                let _ = worker.join();
            }
        }
    }
}

fn write_new(path: &Path, bytes: &[u8]) {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn snapshot_fingerprint(root: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root.join("snapshots")).unwrap() {
        let entry = entry.unwrap();
        let meta = entry.metadata().unwrap();
        files.push((entry.path(), meta.len(), meta.modified().unwrap()));
    }
    files.sort();
    files
}

#[test]
#[ignore = "real NTFS journal integration; run scripts/test-live-usn.ps1 explicitly"]
fn live_usn_roundtrip() {
    let project = std::env::current_dir().unwrap().canonicalize().unwrap();
    let allowed = project.join("target/live-usn").canonicalize().unwrap();
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let run = executable.parent().unwrap();
    assert!(
        run.starts_with(&allowed) && run != allowed,
        "live test executable must be in a unique target/live-usn child"
    );
    assert!(run
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("run-"));
    // Canonical Windows paths contain the verbatim prefix; production scopes use drive paths.
    let run_path = PathBuf::from(run.to_str().unwrap().trim_start_matches(r"\\?\"));
    let outside = allowed.join(format!(
        "{}-outside",
        run.file_name().unwrap().to_string_lossy()
    ));
    fs::create_dir(&outside).unwrap();
    let content = run_path.join("fixture");
    fs::create_dir(&content).unwrap();
    let tree = content.join("tree");
    fs::create_dir(&tree).unwrap();
    fs::create_dir(tree.join("child")).unwrap();
    write_new(&tree.join("child/seed.txt"), b"seed");

    let mut job = LiveJob::start(&run_path, 1);
    job.initial();
    assert!(
        job.contains(&tree.join("child/seed.txt")),
        "initial scoped corpus omitted the fixture seed"
    );
    assert_eq!(job.items[&key(&tree)].kind, SearchItemKind::Folder);

    let roots = vec![run_path.to_str().unwrap().to_owned()];
    let snapshot_deadline = Instant::now() + Duration::from_secs(10);
    while storage::load_scope_snapshot(&SearchScope::CurrentFolder, &roots).is_none() {
        assert!(
            Instant::now() < snapshot_deadline,
            "initial atomic scope snapshot was not readable"
        );
        thread::sleep(Duration::from_millis(50));
    }
    println!("PASS initial scope snapshot readable beside isolated executable");

    let alpha = content.join("usn-alpha.txt");
    write_new(&alpha, b"created");
    job.until("file creation", |job| job.contains(&alpha));
    job.search("usn-alpha.txt", Some(&alpha));
    job.quiet(Duration::from_secs(1), true);
    let before = job.revisions.get(&key(&alpha)).copied().unwrap_or(0);
    // USN timestamps are stored as whole seconds by the app.
    thread::sleep(Duration::from_millis(1100));
    let mut file = fs::OpenOptions::new().append(true).open(&alpha).unwrap();
    file.write_all(b" updated").unwrap();
    file.sync_all().unwrap();
    drop(file);
    job.until("file write", |job| {
        job.revisions.get(&key(&alpha)).copied().unwrap_or(0) > before
    });

    let beta = content.join("usn-beta.txt");
    fs::rename(&alpha, &beta).unwrap();
    job.until("file rename removes old path", |job| {
        !job.contains(&alpha) && job.contains(&beta)
    });
    job.search("usn-alpha.txt", None);
    job.search("usn-beta.txt", Some(&beta));
    fs::remove_file(&beta).unwrap();
    job.until("file deletion", |job| !job.contains(&beta));
    job.search("usn-beta.txt", None);

    let renamed = content.join("renamed-tree");
    fs::rename(&tree, &renamed).unwrap();
    job.until("folder rename updates descendants", |job| {
        !job.contains(&tree)
            && !job.contains(&tree.join("child/seed.txt"))
            && job.contains(&renamed)
            && job.contains(&renamed.join("child/seed.txt"))
    });
    let moved_out = outside.join("moved-tree");
    fs::rename(&renamed, &moved_out).unwrap();
    job.until("folder move outside scope deletes descendants", |job| {
        !job.contains(&renamed) && !job.contains(&renamed.join("child/seed.txt"))
    });
    assert!(!job.contains(&moved_out));
    fs::rename(&moved_out, &renamed).unwrap();
    job.until("folder move into scope restores descendants", |job| {
        job.contains(&renamed) && job.contains(&renamed.join("child/seed.txt"))
    });
    fs::remove_file(renamed.join("child/seed.txt")).unwrap();
    fs::remove_dir(renamed.join("child")).unwrap();
    fs::remove_dir(&renamed).unwrap();
    job.until("folder and descendant deletion", |job| {
        !job.contains(&renamed) && !job.contains(&renamed.join("child/seed.txt"))
    });

    let burst = content.join("burst");
    fs::create_dir(&burst).unwrap();
    let files: Vec<_> = (0..128)
        .map(|n| burst.join(format!("burst-{n:03}.txt")))
        .collect();
    for path in &files {
        write_new(path, b"burst");
    }
    job.until("128-file creation burst", |job| {
        files.iter().all(|path| job.contains(path))
    });
    for path in &files {
        fs::remove_file(path).unwrap();
    }
    fs::remove_dir(&burst).unwrap();
    job.until("128-file deletion burst", |job| {
        files.iter().all(|path| !job.contains(path)) && !job.contains(&burst)
    });

    job.quiet(Duration::from_secs(2), true);
    let snapshot_before = snapshot_fingerprint(&run_path);
    assert_eq!(
        snapshot_before.len(),
        1,
        "only one scope snapshot should be present"
    );
    let outside_file = outside.join("outside-only.txt");
    write_new(&outside_file, b"outside");
    println!("Observing idle/scoped-out updates for 15 seconds...");
    job.quiet(Duration::from_secs(15), false);
    assert_eq!(
        snapshot_fingerprint(&run_path),
        snapshot_before,
        "idle polling rewrote a snapshot"
    );
    assert!(!run_path.join("usn_checkpoints.txt").exists());
    println!("PASS 15-second idle: no scoped deltas, checkpoint writes, or snapshot rewrites");
    let total_deltas = job.deltas;
    job.stop();

    let after_cancel = content.join("after-cancel.txt");
    write_new(&after_cancel, b"new corpus");
    assert!(matches!(
        job.events.recv_timeout(Duration::from_millis(600)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
    let mut replacement = LiveJob::start(&run_path, 2);
    replacement.initial();
    assert!(replacement.contains(&after_cancel));
    replacement.search("after-cancel.txt", Some(&after_cancel));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if storage::load_scope_snapshot(&SearchScope::CurrentFolder, &roots).is_some_and(|items| {
            items
                .iter()
                .any(|item| key(Path::new(item.path.as_ref())) == key(&after_cancel))
        }) {
            println!("PASS replacement snapshot contains the new corpus");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "replacement snapshot did not update"
        );
        thread::sleep(Duration::from_millis(50));
    }
    replacement.stop();
    fs::remove_file(after_cancel).unwrap();
    fs::remove_file(outside_file).unwrap();
    fs::remove_dir(content).unwrap();
    fs::remove_dir(outside).unwrap();
    println!("PASS live USN roundtrip: {total_deltas} scoped delta batches, replacement index current, both workers joined");
}
