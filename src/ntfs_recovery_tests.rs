use super::*;
use std::collections::VecDeque;

fn handle(id: usize) -> HANDLE {
    id as HANDLE
}
fn journal(id: u64, first: i64, next: i64) -> USN_JOURNAL_DATA_V0 {
    USN_JOURNAL_DATA_V0 {
        UsnJournalID: id,
        FirstUsn: first,
        NextUsn: next,
        ..Default::default()
    }
}
fn nodes(name: &str) -> HashMap<u64, NtfsNode> {
    HashMap::from([(
        1,
        NtfsNode {
            parent_id: 0,
            name: name.into(),
            is_dir: false,
            modified_unix_secs: 0,
            file_attributes: 0,
        },
    )])
}
fn state(drive: char, id: usize, name: &str) -> NtfsVolumeState {
    let path = format!("{drive}:\\{name}");
    NtfsVolumeState {
        drive_letter: drive,
        drive_prefix: format!("{}:\\", drive),
        handle: handle(id),
        journal_id: 7,
        next_usn: 100,
        nodes: nodes(name),
        path_cache: HashMap::from([(1, path.clone())]),
        id_to_path: HashMap::from([(1, path)]),
    }
}
#[derive(Default)]
struct FakeIo {
    opens: VecDeque<(char, Option<(usize, USN_JOURNAL_DATA_V0)>)>,
    enumerations: VecDeque<(usize, Option<HashMap<u64, NtfsNode>>, bool)>,
    polls: VecDeque<(char, bool)>,
    closed: Vec<usize>,
    observed: Vec<(char, usize, i64)>,
    enumerated_ranges: Vec<(i64, i64)>,
    snapshots: Vec<Vec<String>>,
    cancel: Option<Arc<AtomicBool>>,
    cancel_on_open: bool,
    pauses: usize,
    stop_after: usize,
}
impl JournalIo for FakeIo {
    fn open(&mut self, drive: char) -> Option<(HANDLE, USN_JOURNAL_DATA_V0)> {
        let (expected, result) = self.opens.pop_front().expect("unscripted open");
        assert_eq!(drive, expected);
        if self.cancel_on_open {
            self.cancel.as_ref().unwrap().store(true, Ordering::Relaxed);
        }
        result.map(|(h, j)| (handle(h), j))
    }
    fn enumerate(
        &mut self,
        h: HANDLE,
        low: i64,
        high: i64,
        _: u64,
        _: &mpsc::Sender<IndexEvent>,
        cancel: &AtomicBool,
    ) -> Option<HashMap<u64, NtfsNode>> {
        let (expected, result, cancel_now) = self
            .enumerations
            .pop_front()
            .expect("unscripted enumeration");
        assert_eq!(h as usize, expected);
        self.enumerated_ranges.push((low, high));
        if cancel_now {
            cancel.store(true, Ordering::Relaxed);
        }
        result
    }
    fn poll(&mut self, state: &mut NtfsVolumeState, cancel: &AtomicBool) -> Option<JournalBatch> {
        self.observed
            .push((state.drive_letter, state.handle as usize, state.next_usn));
        let (drive, success) = self.polls.pop_front().expect("unscripted poll");
        assert_eq!(state.drive_letter, drive);
        if !success {
            return None;
        }
        state.nodes.insert(
            2,
            NtfsNode {
                parent_id: 0,
                name: "delta.txt".into(),
                is_dir: false,
                modified_unix_secs: 0,
                file_attributes: 0,
            },
        );
        state.next_usn += 1;
        journal_batch_from_changes(state, HashSet::from([2]), Vec::new(), cancel)
    }
    fn close(&mut self, h: HANDLE) {
        self.closed.push(h as usize);
    }
    fn snapshot(
        &mut self,
        _: SearchScope,
        _: Vec<String>,
        items: Vec<SearchItem>,
        _: Arc<AtomicBool>,
    ) {
        self.snapshots.push(
            items
                .into_iter()
                .map(|item| item.path.to_string())
                .collect(),
        );
    }
    fn pause(&mut self) {
        self.pauses += 1;
        assert!(self.pauses <= self.stop_after, "unscripted pause");
        if self.pauses == self.stop_after {
            self.cancel.as_ref().unwrap().store(true, Ordering::Relaxed);
        }
    }
}
fn recover(s: &mut NtfsVolumeState, io: &mut FakeIo, cancel: &AtomicBool) -> bool {
    let (tx, _rx) = mpsc::channel();
    recover_ntfs_state(s, 1, &tx, cancel, io)
}
fn live(
    io: &mut FakeIo,
    states: Vec<NtfsVolumeState>,
    fallback: Vec<SearchItem>,
    cancel: &Arc<AtomicBool>,
) -> Vec<IndexEvent> {
    let (tx, rx) = mpsc::channel();
    run_live_states(
        SearchScope::AllLocalDrives,
        &[],
        1,
        &tx,
        cancel,
        states,
        fallback,
        IndexBackend::NtfsUsnLiveMixed,
        io,
    );
    rx.try_iter().collect()
}
fn paths(events: &[IndexEvent]) -> Vec<Vec<String>> {
    events
        .iter()
        .filter_map(|event| match event {
            IndexEvent::Done { items, .. } => {
                Some(items.iter().map(|item| item.path.to_string()).collect())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn reset_and_lost_range_rebuild_only_affected_state() {
    for j in [journal(8, 0, 200), journal(7, 101, 200), journal(7, 0, 99)] {
        let mut s = state('C', 1, "old.txt");
        let other = state('D', 3, "other.txt");
        let mut io = FakeIo::default();
        io.opens.push_back(('C', Some((2, j))));
        io.enumerations
            .push_back((2, Some(nodes("new.txt")), false));
        assert!(recover(&mut s, &mut io, &AtomicBool::new(false)));
        assert_eq!(s.handle as usize, 2);
        assert_eq!(s.journal_id, j.UsnJournalID);
        assert_eq!(s.next_usn, j.NextUsn);
        assert_eq!(s.nodes[&1].name, "new.txt");
        assert_eq!(s.id_to_path[&1], "C:\\new.txt");
        assert_eq!(s.path_cache[&1], "C:\\new.txt");
        assert_eq!(io.enumerated_ranges, vec![(j.FirstUsn, j.NextUsn)]);
        assert_eq!(other.nodes[&1].name, "other.txt");
        assert_eq!(io.closed, vec![1]);
    }
}
#[test]
fn resumable_journal_preserves_cursor_and_nodes() {
    for (first, next) in [(100, 200), (0, 100)] {
        let mut s = state('C', 1, "old.txt");
        let mut io = FakeIo::default();
        io.opens
            .push_back(('C', Some((2, journal(7, first, next)))));
        assert!(recover(&mut s, &mut io, &AtomicBool::new(false)));
        assert_eq!(
            (s.handle as usize, s.next_usn, s.nodes[&1].name.as_str()),
            (2, 100, "old.txt")
        );
        assert_eq!(io.closed, vec![1]);
    }
}
#[test]
fn failed_enumeration_keeps_original_and_closes_new_handle() {
    let mut s = state('C', 1, "old.txt");
    let mut io = FakeIo::default();
    io.opens.push_back(('C', Some((2, journal(8, 0, 200)))));
    io.enumerations.push_back((2, None, false));
    assert!(!recover(&mut s, &mut io, &AtomicBool::new(false)));
    assert_eq!(
        (
            s.handle as usize,
            s.journal_id,
            s.next_usn,
            s.nodes[&1].name.as_str()
        ),
        (1, 7, 100, "old.txt")
    );
    assert_eq!(io.closed, vec![2]);
}
#[test]
fn cancellation_after_open_keeps_original_and_closes_new_handle() {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut s = state('C', 1, "old.txt");
    let mut io = FakeIo {
        cancel: Some(cancel.clone()),
        cancel_on_open: true,
        ..Default::default()
    };
    io.opens.push_back(('C', Some((2, journal(7, 0, 200)))));
    assert!(!recover(&mut s, &mut io, &cancel));
    assert_eq!(
        (s.handle as usize, s.next_usn, s.nodes[&1].name.as_str()),
        (1, 100, "old.txt")
    );
    assert_eq!(io.closed, vec![2]);
}
#[test]
fn cancellation_after_enumeration_keeps_original_and_closes_new_handle() {
    let cancel = AtomicBool::new(false);
    let mut s = state('C', 1, "old.txt");
    let mut io = FakeIo::default();
    io.opens.push_back(('C', Some((2, journal(8, 0, 200)))));
    io.enumerations.push_back((2, Some(nodes("new.txt")), true));
    assert!(!recover(&mut s, &mut io, &cancel));
    assert_eq!(
        (
            s.handle as usize,
            s.journal_id,
            s.next_usn,
            s.nodes[&1].name.as_str()
        ),
        (1, 7, 100, "old.txt")
    );
    assert_eq!(io.closed, vec![2]);
}
#[test]
fn disconnected_drive_retries_while_other_drive_continues() {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut io = FakeIo {
        cancel: Some(cancel.clone()),
        stop_after: 2,
        ..Default::default()
    };
    io.polls
        .extend([('C', false), ('D', true), ('C', false), ('D', true)]);
    io.opens.extend([('C', None), ('C', None)]);
    let events = live(
        &mut io,
        vec![state('C', 1, "old.txt"), state('D', 2, "other.txt")],
        vec![],
        &cancel,
    );
    assert_eq!(
        io.observed.iter().map(|x| x.0).collect::<Vec<_>>(),
        vec!['C', 'D', 'C', 'D']
    );
    assert_eq!(paths(&events).len(), 1);
    assert_eq!(io.snapshots.len(), 1);
    assert_eq!(events.iter().filter(|event| matches!(event, IndexEvent::Delta { upserts, .. } if upserts.iter().any(|item| item.path.as_ref() == "D:\\delta.txt"))).count(), 2);
    assert_eq!(io.closed, vec![1, 2]);
}
#[test]
fn reconnect_resumes_then_emits_real_delta_from_saved_cursor() {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut io = FakeIo {
        cancel: Some(cancel.clone()),
        stop_after: 3,
        ..Default::default()
    };
    io.polls.extend([('C', false), ('C', false), ('C', true)]);
    io.opens
        .extend([('C', None), ('C', Some((2, journal(7, 0, 200))))]);
    let events = live(&mut io, vec![state('C', 1, "old.txt")], vec![], &cancel);
    assert_eq!(
        io.observed,
        vec![('C', 1, 100), ('C', 1, 100), ('C', 2, 100)]
    );
    assert!(events.iter().any(|e| matches!(e, IndexEvent::Delta { upserts, .. } if upserts.iter().any(|i| i.path.as_ref() == "C:\\delta.txt"))));
    assert_eq!(io.closed, vec![1, 2]);
}
#[test]
fn reset_reconnect_publishes_all_volumes_and_fallback() {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut io = FakeIo {
        cancel: Some(cancel.clone()),
        stop_after: 2,
        ..Default::default()
    };
    io.polls
        .extend([('C', false), ('D', true), ('C', true), ('D', true)]);
    io.opens.push_back(('C', Some((3, journal(8, 0, 200)))));
    io.enumerations
        .push_back((3, Some(nodes("new.txt")), false));
    let fallback = vec![SearchItem {
        path: "E:\\fallback.txt".into(),
        modified_unix_secs: 0,
        kind: SearchItemKind::File,
    }];
    let events = live(
        &mut io,
        vec![state('C', 1, "old.txt"), state('D', 2, "other.txt")],
        fallback,
        &cancel,
    );
    let done = paths(&events);
    assert_eq!(done.len(), 2);
    for path in ["C:\\new.txt", "D:\\other.txt", "E:\\fallback.txt"] {
        assert!(done[1].iter().any(|p| p == path));
    }
    assert!(!done[1].iter().any(|p| p == "C:\\old.txt"));
    assert!(done[1].iter().any(|p| p == "D:\\delta.txt"));
    assert_eq!(io.observed[2], ('C', 3, 200));
    assert_eq!(io.snapshots, done);
    assert_eq!(io.closed, vec![1, 3, 2]);
}

#[test]
fn failed_rebuild_publishes_no_replacement_and_other_drive_keeps_updating() {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut io = FakeIo {
        cancel: Some(cancel.clone()),
        stop_after: 1,
        ..Default::default()
    };
    io.polls.extend([('C', false), ('D', true)]);
    io.opens.push_back(('C', Some((3, journal(8, 0, 200)))));
    io.enumerations.push_back((3, None, false));
    let events = live(
        &mut io,
        vec![state('C', 1, "old.txt"), state('D', 2, "other.txt")],
        vec![],
        &cancel,
    );
    assert_eq!(paths(&events).len(), 1);
    assert_eq!(io.snapshots.len(), 1);
    assert!(events.iter().any(|event| matches!(event, IndexEvent::Delta { upserts, .. } if upserts.iter().any(|item| item.path.as_ref() == "D:\\delta.txt"))));
    assert_eq!(io.closed, vec![3, 1, 2]);
}

#[test]
fn cancellation_during_rebuild_emits_no_partial_corpus_and_closes_all_handles() {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut io = FakeIo {
        cancel: Some(cancel.clone()),
        stop_after: 1,
        ..Default::default()
    };
    io.polls.push_back(('C', false));
    io.opens.push_back(('C', Some((3, journal(8, 0, 200)))));
    io.enumerations
        .push_back((3, Some(nodes("partial.txt")), true));
    let events = live(
        &mut io,
        vec![state('C', 1, "old.txt"), state('D', 2, "other.txt")],
        vec![],
        &cancel,
    );
    assert_eq!(paths(&events).len(), 1);
    assert_eq!(io.snapshots.len(), 1);
    assert_eq!(io.observed.len(), 1);
    assert_eq!(io.closed, vec![3, 1, 2]);
}
#[test]
fn disconnected_receiver_closes_every_handle() {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut io = FakeIo::default();
    let (tx, rx) = mpsc::channel();
    drop(rx);
    run_live_states(
        SearchScope::AllLocalDrives,
        &[],
        1,
        &tx,
        &cancel,
        vec![state('C', 1, "a"), state('D', 2, "b")],
        vec![],
        IndexBackend::NtfsUsnLive,
        &mut io,
    );
    assert_eq!(io.closed, vec![1, 2]);
}
