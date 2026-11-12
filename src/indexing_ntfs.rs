#[cfg(target_os = "windows")]
mod imp {
    use std::collections::{HashMap, HashSet};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use std::{env, thread};

    use serde::{Deserialize, Serialize};

    use crate::indexing::{scope_roots, walk_root_with_progress};
    use crate::storage::persist_scope_snapshot_async;
    use crate::{
        debug_log, IndexBackend, IndexEvent, SearchItem, SearchItemKind, SearchScope, UNKNOWN_TS,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_HANDLE_EOF, ERROR_INVALID_FUNCTION, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0,
        READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0, USN_REASON_FILE_CREATE,
        USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME, USN_RECORD_V2,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    #[derive(Clone, Serialize, Deserialize)]
    struct NtfsNode {
        parent_id: u64,
        name: String,
        is_dir: bool,
        modified_unix_secs: i64,
        file_attributes: u32,
    }

    struct NtfsVolumeState {
        drive_letter: char,
        drive_prefix: String,
        handle: HANDLE,
        journal_id: u64,
        next_usn: i64,
        nodes: HashMap<u64, NtfsNode>,
        path_cache: HashMap<u64, String>,
        id_to_path: HashMap<u64, String>,
        last_snapshot_write: Instant,
        changed_since_snapshot: usize,
    }

    #[derive(Serialize, Deserialize)]
    struct NtfsSnapshot {
        version: u32,
        drive_letter: char,
        journal_id: u64,
        next_usn: i64,
        nodes: Vec<NtfsSnapshotNode>,
    }

    #[derive(Serialize, Deserialize)]
    struct NtfsSnapshotNode {
        id: u64,
        parent_id: u64,
        name: String,
        is_dir: bool,
        #[serde(default = "unknown_ts")]
        modified_unix_secs: i64,
        #[serde(default)]
        file_attributes: u32,
    }

    struct JournalBatch {
        upserts: Vec<SearchItem>,
        deleted_paths: Vec<String>,
        changed_entries: usize,
    }

    #[derive(Clone, Copy)]
    struct UsnCheckpoint {
        journal_id: u64,
        next_usn: i64,
    }

    pub(crate) fn run_ntfs_live_index_job(
        scope: SearchScope,
        job_id: u64,
        tx: &mpsc::Sender<IndexEvent>,
    ) -> bool {
        let mut states = Vec::new();
        let mut fallback_roots = Vec::new();
        for root in scope_roots(&scope) {
            debug_log(&format!(
                "run_ntfs_live_index_job opening state start job_id={} root={}",
                job_id, root
            ));
            let volume_root =
                drive_letter_from_path_str(&root).map(|letter| format!("{}:\\", letter));
            if let Some(state) = volume_root
                .as_deref()
                .and_then(|volume| open_ntfs_volume_state(volume, job_id, tx))
            {
                debug_log(&format!(
                    "run_ntfs_live_index_job opening state success job_id={} root={} nodes={}",
                    job_id,
                    root,
                    state.nodes.len()
                ));
                states.push(state);
            } else {
                debug_log(&format!(
                    "run_ntfs_live_index_job opening state failed job_id={} root={}",
                    job_id, root
                ));
                fallback_roots.push(root);
            }
        }

        if states.is_empty() {
            return false;
        }

        let fallback_items: Vec<SearchItem> = fallback_roots
            .iter()
            .flat_map(|root| walk_root_with_progress(root, job_id, tx))
            .collect();
        let backend = if fallback_roots.is_empty() {
            IndexBackend::NtfsUsnLive
        } else {
            IndexBackend::NtfsUsnLiveMixed
        };
        let initial = collect_scope_items(&scope, &mut states, &fallback_items);
        persist_scope_snapshot_async(scope.clone(), initial.clone());
        if tx
            .send(IndexEvent::Done {
                job_id,
                items: initial,
                backend,
            })
            .is_err()
        {
            for state in states {
                let _ = unsafe { CloseHandle(state.handle) };
            }
            return true;
        }

        let mut keep_running = true;
        while keep_running {
            let mut recovered = false;
            for state in &mut states {
                match poll_ntfs_journal(state) {
                    Some(batch) => {
                        persist_usn_checkpoint(
                            state.drive_letter,
                            state.journal_id,
                            state.next_usn,
                        );

                        if batch.changed_entries > 0 {
                            state.changed_since_snapshot += batch.changed_entries;
                        }

                        maybe_persist_ntfs_snapshot(state);

                        let batch = filter_journal_batch_for_scope(&scope, batch);
                        if (!batch.upserts.is_empty() || !batch.deleted_paths.is_empty())
                            && tx
                                .send(IndexEvent::Delta {
                                    job_id,
                                    upserts: batch.upserts,
                                    deleted_paths: batch.deleted_paths,
                                })
                                .is_err()
                        {
                            keep_running = false;
                            break;
                        }
                    }
                    None => {
                        recovered |= recover_ntfs_state(state, job_id, tx);
                    }
                }
            }

            if recovered && keep_running {
                let items = collect_scope_items(&scope, &mut states, &fallback_items);
                persist_scope_snapshot_async(scope.clone(), items.clone());
                keep_running = tx
                    .send(IndexEvent::Done {
                        job_id,
                        items,
                        backend,
                    })
                    .is_ok();
            }

            if keep_running {
                thread::sleep(Duration::from_millis(300));
            }
        }

        for mut state in states {
            if state.changed_since_snapshot > 0 {
                persist_ntfs_snapshot(&mut state);
            }
            let _ = unsafe { CloseHandle(state.handle) };
        }

        true
    }

    pub(crate) fn try_index_ntfs_volume(
        root: &str,
        job_id: u64,
        tx: &mpsc::Sender<IndexEvent>,
    ) -> Option<Vec<SearchItem>> {
        let drive = parse_drive_root_letter(root)?;
        let handle = open_volume_handle(drive)?;

        let mut journal = USN_JOURNAL_DATA_V0::default();
        let mut bytes_returned = 0u32;
        let query_ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null_mut(),
                0,
                &mut journal as *mut _ as *mut c_void,
                std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };

        if query_ok == 0 {
            let _ = unsafe { CloseHandle(handle) };
            return None;
        }

        let mut enum_data = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            HighUsn: journal.NextUsn,
        };

        let usn_start = journal.FirstUsn.max(0);
        let usn_total = (journal.NextUsn - usn_start).max(1) as usize;

        let mut raw_nodes: HashMap<u64, NtfsNode> = HashMap::new();
        let mut scanned = 0usize;
        let mut buffer = vec![0u8; 1024 * 1024];

        loop {
            let mut out_bytes = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_ENUM_USN_DATA,
                    &mut enum_data as *mut _ as *mut c_void,
                    std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                    buffer.as_mut_ptr() as *mut c_void,
                    buffer.len() as u32,
                    &mut out_bytes,
                    std::ptr::null_mut(),
                )
            };

            if ok == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_HANDLE_EOF || err == ERROR_INVALID_FUNCTION {
                    break;
                }

                raw_nodes.clear();
                let _ = unsafe { CloseHandle(handle) };
                return None;
            }

            if out_bytes < 8 {
                break;
            }

            enum_data.StartFileReferenceNumber = unsafe { *(buffer.as_ptr() as *const u64) };

            let mut offset = 8usize;
            while offset < out_bytes as usize {
                let rec = unsafe { &*(buffer.as_ptr().add(offset) as *const USN_RECORD_V2) };
                let record_len = rec.RecordLength as usize;
                if record_len == 0 {
                    break;
                }

                if rec.MajorVersion == 2 {
                    let name = read_usn_v2_name(buffer.as_ptr(), offset, rec);
                    if !name.is_empty() {
                        let is_dir = (rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                        raw_nodes.insert(
                            rec.FileReferenceNumber,
                            NtfsNode {
                                parent_id: rec.ParentFileReferenceNumber,
                                name,
                                is_dir,
                                modified_unix_secs: filetime_100ns_to_unix_secs(rec.TimeStamp)
                                    .unwrap_or(UNKNOWN_TS),
                                file_attributes: rec.FileAttributes,
                            },
                        );

                        scanned += 1;
                        if scanned.is_multiple_of(5000) {
                            let current = (rec.Usn - usn_start).max(0) as usize;
                            let _ = tx.send(IndexEvent::Progress {
                                job_id,
                                current: current.min(usn_total),
                                total: usn_total,
                                phase: "index",
                            });
                        }
                    }
                }

                offset += record_len;
            }
        }

        let _ = unsafe { CloseHandle(handle) };

        let drive_prefix = format!("{}:\\", drive.to_ascii_uppercase());
        let mut path_cache: HashMap<u64, String> = HashMap::new();
        let mut out = Vec::new();

        for (id, node) in &raw_nodes {
            let path = materialize_full_path(*id, &raw_nodes, &mut path_cache, &drive_prefix);
            out.push(SearchItem {
                path: path.into_boxed_str(),
                modified_unix_secs: node.modified_unix_secs,
                kind: search_item_kind(node),
            });
        }

        Some(out)
    }

    fn open_ntfs_volume_state(
        root: &str,
        job_id: u64,
        tx: &mpsc::Sender<IndexEvent>,
    ) -> Option<NtfsVolumeState> {
        let drive = parse_drive_root_letter(root)?;
        let (handle, journal) = open_volume_and_query_journal(drive)?;

        let Some(nodes) =
            enumerate_ntfs_nodes(handle, journal.FirstUsn, journal.NextUsn, job_id, tx)
        else {
            let _ = unsafe { CloseHandle(handle) };
            return None;
        };

        let mut state = NtfsVolumeState {
            drive_letter: drive,
            drive_prefix: format!("{}:\\", drive.to_ascii_uppercase()),
            handle,
            journal_id: journal.UsnJournalID,
            next_usn: journal.NextUsn,
            nodes,
            path_cache: HashMap::new(),
            id_to_path: HashMap::new(),
            last_snapshot_write: Instant::now(),
            changed_since_snapshot: 0,
        };

        initialize_id_path_map(&mut state, job_id, tx);
        persist_usn_checkpoint(drive, state.journal_id, state.next_usn);
        Some(state)
    }

    fn enumerate_ntfs_nodes(
        handle: HANDLE,
        low_usn: i64,
        high_usn: i64,
        job_id: u64,
        tx: &mpsc::Sender<IndexEvent>,
    ) -> Option<HashMap<u64, NtfsNode>> {
        let mut enum_data = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            HighUsn: high_usn,
        };

        let progress_low = low_usn.max(0);
        let progress_total = (high_usn - progress_low).max(1) as usize;

        let mut raw_nodes: HashMap<u64, NtfsNode> = HashMap::new();
        let mut scanned = 0usize;
        let mut buffer = vec![0u8; 1024 * 1024];

        loop {
            let mut out_bytes = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_ENUM_USN_DATA,
                    &mut enum_data as *mut _ as *mut c_void,
                    std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                    buffer.as_mut_ptr() as *mut c_void,
                    buffer.len() as u32,
                    &mut out_bytes,
                    std::ptr::null_mut(),
                )
            };

            if ok == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_HANDLE_EOF || err == ERROR_INVALID_FUNCTION {
                    break;
                }
                return None;
            }

            if out_bytes < 8 {
                break;
            }

            enum_data.StartFileReferenceNumber = unsafe { *(buffer.as_ptr() as *const u64) };

            let mut offset = 8usize;
            while offset < out_bytes as usize {
                let rec = unsafe { &*(buffer.as_ptr().add(offset) as *const USN_RECORD_V2) };
                let record_len = rec.RecordLength as usize;
                if record_len == 0 {
                    break;
                }

                if rec.MajorVersion == 2 {
                    let name = read_usn_v2_name(buffer.as_ptr(), offset, rec);
                    if !name.is_empty() {
                        let is_dir = (rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                        raw_nodes.insert(
                            rec.FileReferenceNumber,
                            NtfsNode {
                                parent_id: rec.ParentFileReferenceNumber,
                                name,
                                is_dir,
                                modified_unix_secs: filetime_100ns_to_unix_secs(rec.TimeStamp)
                                    .unwrap_or(UNKNOWN_TS),
                                file_attributes: rec.FileAttributes,
                            },
                        );

                        scanned += 1;
                        if scanned.is_multiple_of(5000) {
                            let current = (rec.Usn - progress_low).max(0) as usize;
                            let _ = tx.send(IndexEvent::Progress {
                                job_id,
                                current: current.min(progress_total),
                                total: progress_total,
                                phase: "index",
                            });
                        }
                    }
                }

                offset += record_len;
            }
        }

        Some(raw_nodes)
    }

    fn poll_ntfs_journal(state: &mut NtfsVolumeState) -> Option<JournalBatch> {
        let mut read_data = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: state.next_usn,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: state.journal_id,
        };

        let mut buffer = vec![0u8; 512 * 1024];
        let mut out_bytes = 0u32;

        let ok = unsafe {
            DeviceIoControl(
                state.handle,
                FSCTL_READ_USN_JOURNAL,
                &mut read_data as *mut _ as *mut c_void,
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
                &mut out_bytes,
                std::ptr::null_mut(),
            )
        };

        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_HANDLE_EOF {
                return Some(JournalBatch {
                    upserts: Vec::new(),
                    deleted_paths: Vec::new(),
                    changed_entries: 0,
                });
            }
            return None;
        }

        if out_bytes < 8 {
            return Some(JournalBatch {
                upserts: Vec::new(),
                deleted_paths: Vec::new(),
                changed_entries: 0,
            });
        }

        state.next_usn = unsafe { *(buffer.as_ptr() as *const i64) };

        let mut changed_ids: HashSet<u64> = HashSet::new();
        let mut deleted_ids: Vec<u64> = Vec::new();
        let mut offset = 8usize;

        while offset < out_bytes as usize {
            let rec = unsafe { &*(buffer.as_ptr().add(offset) as *const USN_RECORD_V2) };
            let record_len = rec.RecordLength as usize;
            if record_len == 0 {
                break;
            }

            if rec.MajorVersion == 2 {
                let reason = rec.Reason;
                let id = rec.FileReferenceNumber;

                if (reason & USN_REASON_FILE_DELETE) != 0 {
                    let removed_ids = remove_ntfs_node_and_descendants(&mut state.nodes, id);
                    if !removed_ids.is_empty() {
                        deleted_ids.extend(removed_ids);
                    }
                    offset += record_len;
                    continue;
                }

                let name = read_usn_v2_name(buffer.as_ptr(), offset, rec);
                if !name.is_empty() {
                    let is_dir = (rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                    let new_node = NtfsNode {
                        parent_id: rec.ParentFileReferenceNumber,
                        name,
                        is_dir,
                        modified_unix_secs: filetime_100ns_to_unix_secs(rec.TimeStamp)
                            .unwrap_or(UNKNOWN_TS),
                        file_attributes: rec.FileAttributes,
                    };

                    let needs_update = state.nodes.get(&id).is_none_or(|existing| {
                        existing.parent_id != new_node.parent_id
                            || existing.name != new_node.name
                            || existing.is_dir != new_node.is_dir
                            || existing.modified_unix_secs != new_node.modified_unix_secs
                            || existing.file_attributes != new_node.file_attributes
                    });

                    if needs_update {
                        state.nodes.insert(id, new_node);
                        changed_ids.insert(id);
                    }

                    if (reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME)) != 0 {
                        changed_ids.insert(id);
                    }
                }
            }

            offset += record_len;
        }

        Some(journal_batch_from_changes(state, changed_ids, deleted_ids))
    }

    fn journal_batch_from_changes(
        state: &mut NtfsVolumeState,
        mut changed_ids: HashSet<u64>,
        deleted_ids: Vec<u64>,
    ) -> JournalBatch {
        if !changed_ids.is_empty() || !deleted_ids.is_empty() {
            state.path_cache.clear();
        }

        let moved_dirs: Vec<u64> = changed_ids
            .iter()
            .copied()
            .filter(|id| {
                if !state.nodes.get(id).is_some_and(|node| node.is_dir) {
                    return false;
                }
                let path = materialize_full_path(
                    *id,
                    &state.nodes,
                    &mut state.path_cache,
                    &state.drive_prefix,
                );
                state.id_to_path.get(id).is_some_and(|old| old != &path)
            })
            .collect();
        if !moved_dirs.is_empty() {
            changed_ids.extend(ntfs_subtree_ids(&state.nodes, &moved_dirs));
        }

        let mut deleted_paths = Vec::new();
        for removed_id in deleted_ids {
            if let Some(path) = state.id_to_path.remove(&removed_id) {
                deleted_paths.push(path);
            }
        }

        let mut upserts = Vec::new();
        for id in changed_ids {
            let Some(node) = state.nodes.get(&id) else {
                continue;
            };

            let path =
                materialize_full_path(id, &state.nodes, &mut state.path_cache, &state.drive_prefix);
            if let Some(old_path) = state.id_to_path.insert(id, path.clone()) {
                if old_path != path {
                    deleted_paths.push(old_path);
                }
            }

            upserts.push(SearchItem {
                path: path.into_boxed_str(),
                modified_unix_secs: node.modified_unix_secs,
                kind: search_item_kind(node),
            });
        }

        let changed_entries = upserts.len() + deleted_paths.len();

        JournalBatch {
            upserts,
            deleted_paths,
            changed_entries,
        }
    }

    fn remove_ntfs_node_and_descendants(nodes: &mut HashMap<u64, NtfsNode>, id: u64) -> Vec<u64> {
        let ids = ntfs_subtree_ids(nodes, &[id]);
        for id in &ids {
            nodes.remove(id);
        }
        ids
    }

    fn ntfs_subtree_ids(nodes: &HashMap<u64, NtfsNode>, roots: &[u64]) -> Vec<u64> {
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        for (&id, node) in nodes {
            children.entry(node.parent_id).or_default().push(id);
        }
        let mut pending = roots.to_vec();
        let mut seen = HashSet::new();
        let mut ids = Vec::new();
        let mut index = 0usize;
        while index < pending.len() {
            let id = pending[index];
            index += 1;
            if !seen.insert(id) || !nodes.contains_key(&id) {
                continue;
            }
            ids.push(id);
            if let Some(child_ids) = children.get(&id) {
                pending.extend(child_ids);
            }
        }
        ids
    }

    fn collect_scope_items(
        scope: &SearchScope,
        states: &mut [NtfsVolumeState],
        fallback_items: &[SearchItem],
    ) -> Vec<SearchItem> {
        let mut items = collect_items_from_ntfs_states(states);
        items.extend_from_slice(fallback_items);
        filter_items_for_scope(scope, items)
    }

    fn collect_items_from_ntfs_states(states: &mut [NtfsVolumeState]) -> Vec<SearchItem> {
        let mut out = Vec::new();

        for state in states {
            for (id, node) in &state.nodes {
                let path = materialize_full_path(
                    *id,
                    &state.nodes,
                    &mut state.path_cache,
                    &state.drive_prefix,
                );
                out.push(SearchItem {
                    path: path.into_boxed_str(),
                    modified_unix_secs: node.modified_unix_secs,
                    kind: search_item_kind(node),
                });
            }
        }

        out
    }

    fn initialize_id_path_map(
        state: &mut NtfsVolumeState,
        job_id: u64,
        tx: &mpsc::Sender<IndexEvent>,
    ) {
        state.id_to_path.clear();

        let ids: Vec<u64> = state.nodes.keys().copied().collect();

        let ids_total = ids.len().max(1);

        for (idx, id) in ids.into_iter().enumerate() {
            let path =
                materialize_full_path(id, &state.nodes, &mut state.path_cache, &state.drive_prefix);
            state.id_to_path.insert(id, path);

            if (idx + 1).is_multiple_of(5000) {
                let _ = tx.send(IndexEvent::Progress {
                    job_id,
                    current: idx + 1,
                    total: ids_total,
                    phase: "write",
                });
            }
        }
    }

    fn recover_ntfs_state(
        state: &mut NtfsVolumeState,
        job_id: u64,
        tx: &mpsc::Sender<IndexEvent>,
    ) -> bool {
        let old_handle = state.handle;

        let Some((new_handle, journal)) = open_volume_and_query_journal(state.drive_letter) else {
            return false;
        };

        if journal_can_resume(state, &journal) {
            state.handle = new_handle;
            persist_usn_checkpoint(state.drive_letter, state.journal_id, state.next_usn);
            let _ = unsafe { CloseHandle(old_handle) };
            return true;
        }

        let Some(nodes) =
            enumerate_ntfs_nodes(new_handle, journal.FirstUsn, journal.NextUsn, job_id, tx)
        else {
            let _ = unsafe { CloseHandle(new_handle) };
            return false;
        };

        state.handle = new_handle;
        state.journal_id = journal.UsnJournalID;
        state.next_usn = journal.NextUsn;
        state.nodes = nodes;
        state.path_cache.clear();
        initialize_id_path_map(state, job_id, tx);
        state.changed_since_snapshot = 0;
        state.last_snapshot_write = Instant::now();
        persist_usn_checkpoint(state.drive_letter, state.journal_id, state.next_usn);

        let _ = unsafe { CloseHandle(old_handle) };
        true
    }

    fn journal_can_resume(state: &NtfsVolumeState, journal: &USN_JOURNAL_DATA_V0) -> bool {
        journal.UsnJournalID == state.journal_id
            && state.next_usn >= journal.FirstUsn
            && state.next_usn <= journal.NextUsn
    }

    fn open_volume_and_query_journal(drive: char) -> Option<(HANDLE, USN_JOURNAL_DATA_V0)> {
        let handle = open_volume_handle(drive)?;

        let mut journal = USN_JOURNAL_DATA_V0::default();
        let mut bytes_returned = 0u32;
        let query_ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null_mut(),
                0,
                &mut journal as *mut _ as *mut c_void,
                std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };

        if query_ok == 0 {
            let _ = unsafe { CloseHandle(handle) };
            return None;
        }

        Some((handle, journal))
    }

    fn open_volume_handle(drive: char) -> Option<HANDLE> {
        let volume_path = format!(r"\\.\{}:", drive.to_ascii_uppercase());
        let volume_wide = to_wide(&volume_path);

        for desired_access in [FILE_GENERIC_READ, 0] {
            let handle = unsafe {
                CreateFileW(
                    volume_wide.as_ptr(),
                    desired_access,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                )
            };

            if handle != INVALID_HANDLE_VALUE {
                return Some(handle);
            }
        }

        None
    }

    fn checkpoint_file_path() -> std::path::PathBuf {
        let base = env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
        std::path::PathBuf::from(base)
            .join("WizMini")
            .join("usn_checkpoints.txt")
    }

    fn persist_usn_checkpoint(drive: char, journal_id: u64, next_usn: i64) {
        let path = checkpoint_file_path();
        if let Some(dir) = path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
        }

        let mut map: HashMap<char, UsnCheckpoint> = HashMap::new();
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                let mut parts = line.split(',');
                let Some(drive_part) = parts.next().and_then(|v| v.chars().next()) else {
                    continue;
                };
                let Some(journal) = parts.next().and_then(|v| v.parse::<u64>().ok()) else {
                    continue;
                };
                let Some(usn) = parts.next().and_then(|v| v.parse::<i64>().ok()) else {
                    continue;
                };
                map.insert(
                    drive_part.to_ascii_uppercase(),
                    UsnCheckpoint {
                        journal_id: journal,
                        next_usn: usn,
                    },
                );
            }
        }

        map.insert(
            drive.to_ascii_uppercase(),
            UsnCheckpoint {
                journal_id,
                next_usn,
            },
        );

        let mut lines = Vec::new();
        for (letter, entry) in map {
            lines.push(format!(
                "{},{},{}",
                letter, entry.journal_id, entry.next_usn
            ));
        }

        let _ = std::fs::write(path, lines.join("\n"));
    }

    fn snapshot_file_path(drive: char) -> std::path::PathBuf {
        let base = env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
        std::path::PathBuf::from(base)
            .join("WizMini")
            .join("snapshots")
            .join(format!("{}.bin", drive.to_ascii_uppercase()))
    }

    fn filter_items_for_scope(scope: &SearchScope, items: Vec<SearchItem>) -> Vec<SearchItem> {
        if !matches!(scope, SearchScope::CurrentFolder) {
            return items;
        }

        let roots = scope_roots(scope);
        let Some(folder) = roots.first() else {
            return items;
        };
        items
            .into_iter()
            .filter(|item| path_is_in_folder_scope(item.path.as_ref(), folder))
            .collect()
    }

    fn filter_journal_batch_for_scope(scope: &SearchScope, batch: JournalBatch) -> JournalBatch {
        if !matches!(scope, SearchScope::CurrentFolder) {
            return batch;
        }

        let roots = scope_roots(scope);
        let Some(folder) = roots.first() else {
            return batch;
        };

        let upserts: Vec<SearchItem> = batch
            .upserts
            .into_iter()
            .filter(|item| path_is_in_folder_scope(item.path.as_ref(), folder))
            .collect();
        let deleted_paths: Vec<String> = batch
            .deleted_paths
            .into_iter()
            .filter(|path| path_is_in_folder_scope(path, folder))
            .collect();
        let changed_entries = upserts.len() + deleted_paths.len();

        JournalBatch {
            upserts,
            deleted_paths,
            changed_entries,
        }
    }

    fn path_is_in_folder_scope(path: &str, folder: &str) -> bool {
        let normalized_path = normalize_path_for_scope(path);
        let normalized_folder = normalize_path_for_scope(folder);
        if normalized_path == normalized_folder {
            return true;
        }

        let mut prefix = normalized_folder;
        if !prefix.ends_with('\\') {
            prefix.push('\\');
        }
        normalized_path.starts_with(&prefix)
    }

    fn normalize_path_for_scope(path: &str) -> String {
        path.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    }

    fn drive_letter_from_path_str(path: &str) -> Option<char> {
        let bytes = path.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            Some((bytes[0] as char).to_ascii_uppercase())
        } else {
            None
        }
    }

    fn search_item_kind(node: &NtfsNode) -> SearchItemKind {
        if node.is_dir {
            SearchItemKind::Folder
        } else {
            SearchItemKind::File
        }
    }

    fn maybe_persist_ntfs_snapshot(state: &mut NtfsVolumeState) {
        if state.changed_since_snapshot == 0 {
            return;
        }

        let elapsed = state.last_snapshot_write.elapsed();
        let threshold_hit = state.changed_since_snapshot >= 4000;
        let time_hit = elapsed >= Duration::from_secs(12);
        if threshold_hit || time_hit {
            persist_ntfs_snapshot(state);
        }
    }

    fn persist_ntfs_snapshot(state: &mut NtfsVolumeState) {
        let path = snapshot_file_path(state.drive_letter);
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }

        let mut nodes = Vec::with_capacity(state.nodes.len());
        for (id, node) in &state.nodes {
            nodes.push(NtfsSnapshotNode {
                id: *id,
                parent_id: node.parent_id,
                name: node.name.clone(),
                is_dir: node.is_dir,
                modified_unix_secs: node.modified_unix_secs,
                file_attributes: node.file_attributes,
            });
        }

        let snapshot = NtfsSnapshot {
            version: 1,
            drive_letter: state.drive_letter,
            journal_id: state.journal_id,
            next_usn: state.next_usn,
            nodes,
        };

        let Ok(file) = std::fs::File::create(path) else {
            return;
        };

        if bincode::serialize_into(file, &snapshot).is_ok() {
            state.last_snapshot_write = Instant::now();
            state.changed_since_snapshot = 0;
        }
    }

    fn parse_drive_root_letter(root: &str) -> Option<char> {
        let trimmed = root.trim();
        let bytes = trimmed.as_bytes();
        let is_drive_root = bytes.len() == 3
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
            && bytes[0].is_ascii_alphabetic();

        if is_drive_root {
            Some((bytes[0] as char).to_ascii_uppercase())
        } else {
            None
        }
    }

    fn to_wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn read_usn_v2_name(buffer: *const u8, record_offset: usize, rec: &USN_RECORD_V2) -> String {
        let name_offset = record_offset + rec.FileNameOffset as usize;
        let name_len_u16 = rec.FileNameLength as usize / 2;
        if name_len_u16 == 0 {
            return String::new();
        }

        let name_ptr = unsafe { buffer.add(name_offset) as *const u16 };
        let name_slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len_u16) };
        String::from_utf16_lossy(name_slice)
    }

    fn materialize_full_path(
        id: u64,
        raw_nodes: &HashMap<u64, NtfsNode>,
        path_cache: &mut HashMap<u64, String>,
        drive_prefix: &str,
    ) -> String {
        if let Some(found) = path_cache.get(&id) {
            return found.clone();
        }

        let mut parts = Vec::new();
        let mut current = id;
        let mut depth = 0usize;

        while depth < 1024 {
            let Some(node) = raw_nodes.get(&current) else {
                break;
            };

            parts.push(node.name.clone());
            if node.parent_id == current {
                break;
            }

            current = node.parent_id;
            depth += 1;
        }

        parts.reverse();
        let path = if parts.is_empty() {
            drive_prefix.to_string()
        } else {
            format!("{}{}", drive_prefix, parts.join("\\"))
        };

        path_cache.insert(id, path.clone());
        path
    }

    fn filetime_100ns_to_unix_secs(filetime_100ns: i64) -> Option<i64> {
        if filetime_100ns <= 0 {
            return None;
        }

        let windows_epoch_to_unix_secs = 11_644_473_600i64;
        let secs = filetime_100ns / 10_000_000 - windows_epoch_to_unix_secs;
        Some(secs)
    }

    fn unknown_ts() -> i64 {
        UNKNOWN_TS
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn state_with_nodes(drive: char, entries: &[(u64, u64, &str, bool)]) -> NtfsVolumeState {
            let nodes = entries
                .iter()
                .map(|&(id, parent_id, name, is_dir)| {
                    (
                        id,
                        NtfsNode {
                            parent_id,
                            name: name.to_string(),
                            is_dir,
                            modified_unix_secs: 0,
                            file_attributes: 0,
                        },
                    )
                })
                .collect();
            let mut state = NtfsVolumeState {
                drive_letter: drive,
                drive_prefix: format!("{}:\\", drive),
                handle: std::ptr::null_mut(),
                journal_id: 7,
                next_usn: 100,
                nodes,
                path_cache: HashMap::new(),
                id_to_path: HashMap::new(),
                last_snapshot_write: Instant::now(),
                changed_since_snapshot: 0,
            };
            let changed_ids = state.nodes.keys().copied().collect();
            journal_batch_from_changes(&mut state, changed_ids, Vec::new());
            state
        }

        #[test]
        fn folder_rename_updates_all_descendant_paths() {
            let mut state = state_with_nodes(
                'C',
                &[
                    (1, 0, "old", true),
                    (2, 1, "nested", true),
                    (3, 2, "file.txt", false),
                ],
            );
            state.nodes.get_mut(&1).unwrap().name = "new".to_string();
            let batch = journal_batch_from_changes(&mut state, HashSet::from([1]), Vec::new());
            let paths: HashSet<&str> = batch
                .upserts
                .iter()
                .map(|item| item.path.as_ref())
                .collect();
            assert_eq!(
                paths,
                HashSet::from(["C:\\new", "C:\\new\\nested", "C:\\new\\nested\\file.txt"])
            );
            assert_eq!(
                batch.deleted_paths.into_iter().collect::<HashSet<_>>(),
                HashSet::from([
                    "C:\\old".to_string(),
                    "C:\\old\\nested".to_string(),
                    "C:\\old\\nested\\file.txt".to_string(),
                ])
            );
            assert_eq!(state.id_to_path[&3], "C:\\new\\nested\\file.txt");
        }

        #[test]
        fn folder_move_emits_child_changes_across_scope_boundary() {
            let mut state = state_with_nodes(
                'C',
                &[
                    (1, 0, "scope", true),
                    (2, 1, "moved", true),
                    (3, 2, "file.txt", false),
                ],
            );
            state.nodes.get_mut(&2).unwrap().parent_id = 0;
            let batch = journal_batch_from_changes(&mut state, HashSet::from([2]), Vec::new());
            assert_eq!(batch.deleted_paths.len(), 2);
            assert!(batch
                .deleted_paths
                .iter()
                .all(|path| path_is_in_folder_scope(path, "C:\\scope")));
            assert!(batch
                .upserts
                .iter()
                .all(|item| !path_is_in_folder_scope(&item.path, "C:\\scope")));

            state.nodes.get_mut(&2).unwrap().parent_id = 1;
            let batch = journal_batch_from_changes(&mut state, HashSet::from([2]), Vec::new());
            assert_eq!(batch.upserts.len(), 2);
            assert!(batch
                .upserts
                .iter()
                .all(|item| path_is_in_folder_scope(&item.path, "C:\\scope")));
        }

        #[test]
        fn replacement_collection_preserves_other_volumes_and_fallback() {
            let mut states = [
                state_with_nodes('C', &[(1, 0, "one.txt", false)]),
                state_with_nodes('D', &[(1, 0, "two.txt", false)]),
            ];
            states[1].nodes.get_mut(&1).unwrap().name = "recovered.txt".to_string();
            states[1].path_cache.clear();
            let fallback = [SearchItem {
                path: "E:\\fallback.txt".into(),
                modified_unix_secs: 0,
                kind: SearchItemKind::File,
            }];
            let items = collect_scope_items(&SearchScope::AllLocalDrives, &mut states, &fallback);
            assert_eq!(
                items
                    .iter()
                    .map(|item| item.path.as_ref())
                    .collect::<HashSet<_>>(),
                HashSet::from(["C:\\one.txt", "D:\\recovered.txt", "E:\\fallback.txt"])
            );
        }

        #[test]
        fn journal_resume_requires_the_original_position_to_be_available() {
            let state = state_with_nodes('C', &[]);
            let mut journal = USN_JOURNAL_DATA_V0 {
                UsnJournalID: state.journal_id,
                FirstUsn: 100,
                NextUsn: 200,
                ..Default::default()
            };
            assert!(journal_can_resume(&state, &journal));
            journal.FirstUsn = 101;
            assert!(!journal_can_resume(&state, &journal));
            journal.FirstUsn = 0;
            journal.NextUsn = 99;
            assert!(!journal_can_resume(&state, &journal));
            journal.NextUsn = 200;
            journal.UsnJournalID += 1;
            assert!(!journal_can_resume(&state, &journal));
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) use imp::{run_ntfs_live_index_job, try_index_ntfs_volume};

#[cfg(not(target_os = "windows"))]
pub(crate) fn run_ntfs_live_index_job(
    _scope: crate::SearchScope,
    _job_id: u64,
    _tx: &std::sync::mpsc::Sender<crate::IndexEvent>,
) -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn try_index_ntfs_volume(
    _root: &str,
    _job_id: u64,
    _tx: &std::sync::mpsc::Sender<crate::IndexEvent>,
) -> Option<Vec<crate::SearchItem>> {
    None
}
