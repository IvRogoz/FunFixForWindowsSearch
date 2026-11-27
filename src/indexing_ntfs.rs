#[cfg(target_os = "windows")]
mod imp {
    use std::collections::{HashMap, HashSet};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    };
    use std::thread;
    use std::time::Duration;

    use crate::indexing::walk_root_with_progress;
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

    #[derive(Clone)]
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
    }

    struct JournalBatch {
        upserts: Vec<SearchItem>,
        deleted_paths: Vec<String>,
    }

    // All external effects of the recovery loop are replaceable for isolated simulations.
    trait JournalIo {
        fn open(&mut self, drive: char) -> Option<(HANDLE, USN_JOURNAL_DATA_V0)>;
        fn enumerate(
            &mut self,
            handle: HANDLE,
            low_usn: i64,
            high_usn: i64,
            job_id: u64,
            tx: &mpsc::Sender<IndexEvent>,
            cancel: &AtomicBool,
        ) -> Option<HashMap<u64, NtfsNode>>;
        fn poll(
            &mut self,
            state: &mut NtfsVolumeState,
            cancel: &AtomicBool,
        ) -> Option<JournalBatch>;
        fn close(&mut self, handle: HANDLE);
        fn snapshot(
            &mut self,
            scope: SearchScope,
            roots: Vec<String>,
            items: Vec<SearchItem>,
            cancel: Arc<AtomicBool>,
        );
        fn pause(&mut self);
    }

    struct WindowsJournalIo;

    impl JournalIo for WindowsJournalIo {
        fn open(&mut self, drive: char) -> Option<(HANDLE, USN_JOURNAL_DATA_V0)> {
            open_volume_and_query_journal(drive)
        }
        fn enumerate(
            &mut self,
            handle: HANDLE,
            low_usn: i64,
            high_usn: i64,
            job_id: u64,
            tx: &mpsc::Sender<IndexEvent>,
            cancel: &AtomicBool,
        ) -> Option<HashMap<u64, NtfsNode>> {
            enumerate_ntfs_nodes(handle, low_usn, high_usn, job_id, tx, cancel)
        }
        fn poll(
            &mut self,
            state: &mut NtfsVolumeState,
            cancel: &AtomicBool,
        ) -> Option<JournalBatch> {
            poll_ntfs_journal(state, cancel)
        }
        fn close(&mut self, handle: HANDLE) {
            let _ = unsafe { CloseHandle(handle) };
        }
        fn snapshot(
            &mut self,
            scope: SearchScope,
            roots: Vec<String>,
            items: Vec<SearchItem>,
            cancel: Arc<AtomicBool>,
        ) {
            persist_scope_snapshot_async(scope, roots, items, cancel);
        }
        fn pause(&mut self) {
            thread::sleep(Duration::from_millis(300));
        }
    }

    pub(crate) fn run_ntfs_live_index_job(
        scope: SearchScope,
        roots: &[String],
        job_id: u64,
        tx: &mpsc::Sender<IndexEvent>,
        cancel: &Arc<AtomicBool>,
    ) -> bool {
        let mut states = Vec::new();
        let mut fallback_roots = Vec::new();
        for root in roots {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            debug_log(&format!(
                "run_ntfs_live_index_job opening state start job_id={} root={}",
                job_id, root
            ));
            let volume_root =
                drive_letter_from_path_str(&root).map(|letter| format!("{}:\\", letter));
            if let Some(state) = volume_root
                .as_deref()
                .and_then(|volume| open_ntfs_volume_state(volume, job_id, tx, cancel))
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
                fallback_roots.push(root.clone());
            }
        }

        if states.is_empty() {
            return cancel.load(Ordering::Relaxed);
        }

        let fallback_items: Vec<SearchItem> = fallback_roots
            .iter()
            .flat_map(|root| walk_root_with_progress(root, job_id, tx, cancel))
            .collect();
        let backend = if fallback_roots.is_empty() {
            IndexBackend::NtfsUsnLive
        } else {
            IndexBackend::NtfsUsnLiveMixed
        };
        run_live_states(
            scope,
            roots,
            job_id,
            tx,
            cancel,
            states,
            fallback_items,
            backend,
            &mut WindowsJournalIo,
        )
    }

    fn run_live_states(
        scope: SearchScope,
        roots: &[String],
        job_id: u64,
        tx: &mpsc::Sender<IndexEvent>,
        cancel: &Arc<AtomicBool>,
        mut states: Vec<NtfsVolumeState>,
        fallback_items: Vec<SearchItem>,
        backend: IndexBackend,
        io: &mut impl JournalIo,
    ) -> bool {
        let initial = collect_scope_items(&scope, roots, &mut states, &fallback_items, cancel);
        if cancel.load(Ordering::Relaxed) {
            for state in states {
                io.close(state.handle);
            }
            return true;
        }
        io.snapshot(
            scope.clone(),
            roots.to_vec(),
            initial.clone(),
            cancel.clone(),
        );
        if cancel.load(Ordering::Relaxed) {
            for state in states {
                io.close(state.handle);
            }
            return true;
        }
        if tx
            .send(IndexEvent::Done {
                job_id,
                items: initial,
                backend,
            })
            .is_err()
        {
            for state in states {
                io.close(state.handle);
            }
            return true;
        }

        let mut keep_running = true;
        while keep_running && !cancel.load(Ordering::Relaxed) {
            let mut recovered = false;
            for state in &mut states {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                match io.poll(state, cancel) {
                    Some(batch) => {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let batch = filter_journal_batch_for_scope(&scope, roots, batch);
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
                        recovered |= recover_ntfs_state(state, job_id, tx, cancel, io);
                    }
                }
            }

            if recovered && keep_running && !cancel.load(Ordering::Relaxed) {
                let items =
                    collect_scope_items(&scope, roots, &mut states, &fallback_items, cancel);
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                io.snapshot(scope.clone(), roots.to_vec(), items.clone(), cancel.clone());
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                keep_running = tx
                    .send(IndexEvent::Done {
                        job_id,
                        items,
                        backend,
                    })
                    .is_ok();
            }

            if keep_running {
                io.pause();
            }
        }

        for state in states {
            io.close(state.handle);
        }

        true
    }

    pub(crate) fn try_index_ntfs_volume(
        root: &str,
        job_id: u64,
        tx: &mpsc::Sender<IndexEvent>,
        cancel: &AtomicBool,
    ) -> Option<Vec<SearchItem>> {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
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
            if cancel.load(Ordering::Relaxed) {
                let _ = unsafe { CloseHandle(handle) };
                return None;
            }
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
                if cancel.load(Ordering::Relaxed) {
                    let _ = unsafe { CloseHandle(handle) };
                    return None;
                }
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
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
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
        cancel: &AtomicBool,
    ) -> Option<NtfsVolumeState> {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let drive = parse_drive_root_letter(root)?;
        let (handle, journal) = open_volume_and_query_journal(drive)?;

        let Some(nodes) = enumerate_ntfs_nodes(
            handle,
            journal.FirstUsn,
            journal.NextUsn,
            job_id,
            tx,
            cancel,
        ) else {
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
        };

        initialize_id_path_map(&mut state, job_id, tx, cancel);
        if cancel.load(Ordering::Relaxed) {
            let _ = unsafe { CloseHandle(handle) };
            return None;
        }
        Some(state)
    }

    fn enumerate_ntfs_nodes(
        handle: HANDLE,
        low_usn: i64,
        high_usn: i64,
        job_id: u64,
        tx: &mpsc::Sender<IndexEvent>,
        cancel: &AtomicBool,
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
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
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
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
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

    fn poll_ntfs_journal(state: &mut NtfsVolumeState, cancel: &AtomicBool) -> Option<JournalBatch> {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
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
                });
            }
            return None;
        }

        if out_bytes < 8 {
            return Some(JournalBatch {
                upserts: Vec::new(),
                deleted_paths: Vec::new(),
            });
        }

        state.next_usn = unsafe { *(buffer.as_ptr() as *const i64) };

        let mut changed_ids: HashSet<u64> = HashSet::new();
        let mut deleted_ids: Vec<u64> = Vec::new();
        let mut offset = 8usize;

        while offset < out_bytes as usize {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let rec = unsafe { &*(buffer.as_ptr().add(offset) as *const USN_RECORD_V2) };
            let record_len = rec.RecordLength as usize;
            if record_len == 0 {
                break;
            }

            if rec.MajorVersion == 2 {
                let reason = rec.Reason;
                let id = rec.FileReferenceNumber;

                if (reason & USN_REASON_FILE_DELETE) != 0 {
                    let removed_ids =
                        remove_ntfs_node_and_descendants(&mut state.nodes, id, cancel)?;
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

        journal_batch_from_changes(state, changed_ids, deleted_ids, cancel)
    }

    fn journal_batch_from_changes(
        state: &mut NtfsVolumeState,
        mut changed_ids: HashSet<u64>,
        deleted_ids: Vec<u64>,
        cancel: &AtomicBool,
    ) -> Option<JournalBatch> {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        if !changed_ids.is_empty() || !deleted_ids.is_empty() {
            state.path_cache.clear();
        }

        let mut moved_dirs = Vec::new();
        for &id in &changed_ids {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if !state.nodes.get(&id).is_some_and(|node| node.is_dir) {
                continue;
            }
            let path =
                materialize_full_path(id, &state.nodes, &mut state.path_cache, &state.drive_prefix);
            if state.id_to_path.get(&id).is_some_and(|old| old != &path) {
                moved_dirs.push(id);
            }
        }
        if !moved_dirs.is_empty() {
            changed_ids.extend(ntfs_subtree_ids(&state.nodes, &moved_dirs, cancel)?);
        }

        let mut deleted_paths = Vec::new();
        for removed_id in deleted_ids {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(path) = state.id_to_path.remove(&removed_id) {
                deleted_paths.push(path);
            }
        }

        let mut upserts = Vec::new();
        for id in changed_ids {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
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
        Some(JournalBatch {
            upserts,
            deleted_paths,
        })
    }

    fn remove_ntfs_node_and_descendants(
        nodes: &mut HashMap<u64, NtfsNode>,
        id: u64,
        cancel: &AtomicBool,
    ) -> Option<Vec<u64>> {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        match nodes.get(&id) {
            None => return Some(Vec::new()),
            Some(node) if !node.is_dir => {
                nodes.remove(&id);
                return Some(vec![id]);
            }
            Some(_) => {}
        }
        let ids = ntfs_subtree_ids(nodes, &[id], cancel)?;
        for id in &ids {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            nodes.remove(id);
        }
        Some(ids)
    }

    fn ntfs_subtree_ids(
        nodes: &HashMap<u64, NtfsNode>,
        roots: &[u64],
        cancel: &AtomicBool,
    ) -> Option<Vec<u64>> {
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        for (&id, node) in nodes {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            children.entry(node.parent_id).or_default().push(id);
        }
        let mut pending = roots.to_vec();
        let mut seen = HashSet::new();
        let mut ids = Vec::new();
        let mut index = 0usize;
        while index < pending.len() {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
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
        Some(ids)
    }

    fn collect_scope_items(
        scope: &SearchScope,
        roots: &[String],
        states: &mut [NtfsVolumeState],
        fallback_items: &[SearchItem],
        cancel: &AtomicBool,
    ) -> Vec<SearchItem> {
        let mut items = collect_items_from_ntfs_states(states, cancel);
        items.extend_from_slice(fallback_items);
        filter_items_for_scope(scope, roots, items)
    }

    fn collect_items_from_ntfs_states(
        states: &mut [NtfsVolumeState],
        cancel: &AtomicBool,
    ) -> Vec<SearchItem> {
        let mut out = Vec::new();

        for state in states {
            for (id, node) in &state.nodes {
                if cancel.load(Ordering::Relaxed) {
                    return out;
                }
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
        cancel: &AtomicBool,
    ) {
        state.id_to_path.clear();

        let ids: Vec<u64> = state.nodes.keys().copied().collect();

        let ids_total = ids.len().max(1);

        for (idx, id) in ids.into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
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
        cancel: &AtomicBool,
        io: &mut impl JournalIo,
    ) -> bool {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let old_handle = state.handle;

        let Some((new_handle, journal)) = io.open(state.drive_letter) else {
            return false;
        };
        if cancel.load(Ordering::Relaxed) {
            io.close(new_handle);
            return false;
        }

        if journal_can_resume(state, &journal) {
            state.handle = new_handle;
            io.close(old_handle);
            return true;
        }

        let Some(nodes) = io.enumerate(
            new_handle,
            journal.FirstUsn,
            journal.NextUsn,
            job_id,
            tx,
            cancel,
        ) else {
            io.close(new_handle);
            return false;
        };

        if cancel.load(Ordering::Relaxed) {
            io.close(new_handle);
            return false;
        }
        let mut replacement = NtfsVolumeState {
            drive_letter: state.drive_letter,
            drive_prefix: state.drive_prefix.clone(),
            handle: new_handle,
            journal_id: journal.UsnJournalID,
            next_usn: journal.NextUsn,
            nodes,
            path_cache: HashMap::new(),
            id_to_path: HashMap::new(),
        };
        initialize_id_path_map(&mut replacement, job_id, tx, cancel);
        if cancel.load(Ordering::Relaxed) {
            io.close(new_handle);
            return false;
        }
        *state = replacement;
        io.close(old_handle);
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

    fn filter_items_for_scope(
        scope: &SearchScope,
        roots: &[String],
        items: Vec<SearchItem>,
    ) -> Vec<SearchItem> {
        if !matches!(scope, SearchScope::CurrentFolder) {
            return items;
        }

        let Some(folder) = roots.first() else {
            return items;
        };
        items
            .into_iter()
            .filter(|item| path_is_in_folder_scope(item.path.as_ref(), folder))
            .collect()
    }

    fn filter_journal_batch_for_scope(
        scope: &SearchScope,
        roots: &[String],
        batch: JournalBatch,
    ) -> JournalBatch {
        if !matches!(scope, SearchScope::CurrentFolder) {
            return batch;
        }

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
        JournalBatch {
            upserts,
            deleted_paths,
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

    #[cfg(test)]
    mod recovery_simulations {
        include!("ntfs_recovery_tests.rs");
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
            };
            let changed_ids = state.nodes.keys().copied().collect();
            journal_batch_from_changes(
                &mut state,
                changed_ids,
                Vec::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
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
            let batch = journal_batch_from_changes(
                &mut state,
                HashSet::from([1]),
                Vec::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
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
        fn cancelled_journal_batch_and_subtree_return_no_result() {
            let mut state = state_with_nodes('C', &[(1, 0, "folder", true), (2, 1, "file", false)]);
            let cancel = AtomicBool::new(true);
            assert!(ntfs_subtree_ids(&state.nodes, &[1], &cancel).is_none());
            assert!(journal_batch_from_changes(
                &mut state,
                HashSet::from([1]),
                Vec::new(),
                &cancel
            )
            .is_none());
            assert!(remove_ntfs_node_and_descendants(&mut state.nodes, 1, &cancel).is_none());
            assert_eq!(state.nodes.len(), 2);
        }

        #[test]
        fn deletion_removes_only_the_requested_file_or_folder_tree() {
            let mut state = state_with_nodes(
                'C',
                &[
                    (1, 0, "folder", true),
                    (2, 1, "child", false),
                    (3, 0, "other", false),
                ],
            );
            let cancel = AtomicBool::new(false);
            assert_eq!(
                remove_ntfs_node_and_descendants(&mut state.nodes, 99, &cancel),
                Some(vec![])
            );
            assert_eq!(
                remove_ntfs_node_and_descendants(&mut state.nodes, 3, &cancel),
                Some(vec![3])
            );
            assert!(state.nodes.contains_key(&1));
            assert!(state.nodes.contains_key(&2));
            let mut removed =
                remove_ntfs_node_and_descendants(&mut state.nodes, 1, &cancel).unwrap();
            removed.sort_unstable();
            assert_eq!(removed, vec![1, 2]);
            assert!(state.nodes.is_empty());
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
            let batch = journal_batch_from_changes(
                &mut state,
                HashSet::from([2]),
                Vec::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
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
            let batch = journal_batch_from_changes(
                &mut state,
                HashSet::from([2]),
                Vec::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
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
            let items = collect_scope_items(
                &SearchScope::AllLocalDrives,
                &[],
                &mut states,
                &fallback,
                &AtomicBool::new(false),
            );
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
    _roots: &[String],
    _job_id: u64,
    _tx: &std::sync::mpsc::Sender<crate::IndexEvent>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> bool {
    cancel.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn try_index_ntfs_volume(
    _root: &str,
    _job_id: u64,
    _tx: &std::sync::mpsc::Sender<crate::IndexEvent>,
    _cancel: &std::sync::atomic::AtomicBool,
) -> Option<Vec<crate::SearchItem>> {
    None
}
