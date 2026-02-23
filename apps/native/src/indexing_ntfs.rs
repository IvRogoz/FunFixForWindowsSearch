#[cfg(target_os = "windows")]
mod imp {
    use std::collections::{HashMap, HashSet};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use std::{env, thread};

    use serde::{Deserialize, Serialize};

    use crate::indexing::scope_roots;
    use crate::storage::persist_scope_snapshot_async;
    use crate::{debug_log, IndexBackend, IndexEvent, SearchItem, SearchScope, UNKNOWN_TS};
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
        for root in scope_roots(&scope) {
            debug_log(&format!(
                "run_ntfs_live_index_job opening state start job_id={} root={}",
                job_id, root
            ));
            if let Some(state) = open_ntfs_volume_state(&root, job_id, tx) {
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
            }
        }

        if states.is_empty() {
            return false;
        }

        let initial = collect_items_from_ntfs_states(&mut states);
        persist_scope_snapshot_async(scope.clone(), initial.clone());
        if tx
            .send(IndexEvent::Done {
                job_id,
                items: initial,
                backend: IndexBackend::NtfsUsnLive,
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

                        if !batch.upserts.is_empty() || !batch.deleted_paths.is_empty() {
                            if tx
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
                    }
                    None => {
                        if !recover_ntfs_state(state, job_id, tx) {
                            continue;
                        }

                        let items = collect_items_from_ntfs_states(std::slice::from_mut(state));
                        persist_scope_snapshot_async(scope.clone(), items.clone());
                        if tx
                            .send(IndexEvent::Done {
                                job_id,
                                items,
                                backend: IndexBackend::NtfsUsnLive,
                            })
                            .is_err()
                        {
                            keep_running = false;
                            break;
                        }
                    }
                }
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
            if node.is_dir {
                continue;
            }

            let path = materialize_full_path(*id, &raw_nodes, &mut path_cache, &drive_prefix);
            out.push(SearchItem {
                path: path.into_boxed_str(),
                modified_unix_secs: node.modified_unix_secs,
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

        if !changed_ids.is_empty() || !deleted_ids.is_empty() {
            state.path_cache.clear();
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

            if node.is_dir {
                continue;
            }

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
            });
        }

        let changed_entries = upserts.len() + deleted_paths.len();

        Some(JournalBatch {
            upserts,
            deleted_paths,
            changed_entries,
        })
    }

    fn remove_ntfs_node_and_descendants(nodes: &mut HashMap<u64, NtfsNode>, id: u64) -> Vec<u64> {
        if !nodes.contains_key(&id) {
            return Vec::new();
        }

        let mut to_remove = vec![id];
        let mut index = 0usize;

        while index < to_remove.len() {
            let parent = to_remove[index];
            for (candidate_id, node) in nodes.iter() {
                if node.parent_id == parent && *candidate_id != parent {
                    to_remove.push(*candidate_id);
                }
            }
            index += 1;
        }

        let mut removed_ids = Vec::new();
        for target in to_remove {
            if nodes.remove(&target).is_some() {
                removed_ids.push(target);
            }
        }

        removed_ids
    }

    fn collect_items_from_ntfs_states(states: &mut [NtfsVolumeState]) -> Vec<SearchItem> {
        let mut out = Vec::new();

        for state in states {
            for (id, node) in &state.nodes {
                if node.is_dir {
                    continue;
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

        let ids: Vec<u64> = state
            .nodes
            .iter()
            .filter_map(|(id, node)| (!node.is_dir).then_some(*id))
            .collect();

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

        let mut resume_usn = state.next_usn;
        if let Some(saved) = load_usn_checkpoint(state.drive_letter) {
            if saved.journal_id == journal.UsnJournalID {
                resume_usn = resume_usn.max(saved.next_usn);
            }
        }

        if resume_usn < journal.FirstUsn {
            resume_usn = journal.FirstUsn;
        }
        if resume_usn > journal.NextUsn {
            resume_usn = journal.NextUsn;
        }

        let can_continue = journal.UsnJournalID == state.journal_id
            && resume_usn >= journal.FirstUsn
            && resume_usn <= journal.NextUsn;

        if can_continue {
            state.handle = new_handle;
            state.next_usn = resume_usn;
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

    fn load_usn_checkpoint(drive: char) -> Option<UsnCheckpoint> {
        let path = checkpoint_file_path();
        let content = std::fs::read_to_string(path).ok()?;
        let key = drive.to_ascii_uppercase();

        for line in content.lines() {
            let mut parts = line.split(',');
            let drive_part = parts.next()?.chars().next()?.to_ascii_uppercase();
            let journal_id = parts.next()?.parse::<u64>().ok()?;
            let next_usn = parts.next()?.parse::<i64>().ok()?;

            if drive_part == key {
                return Some(UsnCheckpoint {
                    journal_id,
                    next_usn,
                });
            }
        }

        None
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
