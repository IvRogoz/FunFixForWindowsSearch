use std::{env, sync::mpsc};

use walkdir::WalkDir;

use crate::indexing_ntfs::{run_ntfs_live_index_job, try_index_ntfs_volume};
use crate::storage::{load_scope_snapshot, persist_scope_snapshot_async};
use crate::{
    debug_log, IndexBackend, IndexEvent, SearchItem, SearchItemKind, SearchScope, UNKNOWN_TS,
};

pub(crate) fn run_index_job(scope: SearchScope, job_id: u64, tx: mpsc::Sender<IndexEvent>) {
    debug_log(&format!(
        "run_index_job start job_id={} scope={}",
        job_id,
        scope.label()
    ));

    #[cfg(target_os = "windows")]
    {
        if run_ntfs_live_index_job(scope.clone(), job_id, &tx) {
            debug_log(&format!(
                "run_index_job live index active job_id={} scope={}",
                job_id,
                scope.label()
            ));
            return;
        }

        debug_log(&format!(
            "run_index_job live index unavailable job_id={} scope={}",
            job_id,
            scope.label()
        ));
    }

    let _ = tx.send(IndexEvent::Progress {
        job_id,
        current: 0,
        total: 1,
        phase: "snapshot",
    });

    if let Some(items) = load_scope_snapshot(&scope) {
        let _ = tx.send(IndexEvent::SnapshotLoaded { job_id, items });
    }

    let (items, backend) = index_files_for_scope_with_progress(scope.clone(), job_id, &tx);
    persist_scope_snapshot_async(scope.clone(), items.clone());
    debug_log(&format!(
        "run_index_job finished job_id={} items={} backend= {}",
        job_id,
        items.len(),
        backend.label()
    ));
    let _ = tx.send(IndexEvent::Done {
        job_id,
        items,
        backend,
    });
}

fn index_files_for_scope_with_progress(
    scope: SearchScope,
    job_id: u64,
    tx: &mpsc::Sender<IndexEvent>,
) -> (Vec<SearchItem>, IndexBackend) {
    let roots = scope_roots(&scope);
    let mut out = Vec::new();
    let mut scanned = 0usize;
    let mut used_ntfs = false;
    let mut used_walkdir = false;

    for root in roots {
        let Some(drive_letter) = drive_letter_from_root_str(&root) else {
            used_walkdir = true;
            let items = walk_root_with_progress(&root, job_id, tx);
            scanned += items.len();
            out.extend(items);
            continue;
        };

        let volume_root = format!("{}:\\", drive_letter);

        if let Some(mut ntfs_items) = try_index_ntfs_volume(&volume_root, job_id, tx) {
            used_ntfs = true;

            if matches!(scope, SearchScope::CurrentFolder) {
                let prefix = normalized_folder_prefix(&root);
                ntfs_items.retain(|item| path_starts_with_folder(item.path.as_ref(), &prefix));
            }

            scanned += ntfs_items.len();
            out.extend(ntfs_items);

            continue;
        }

        used_walkdir = true;
        let items = walk_root_with_progress(&root, job_id, tx);
        scanned += items.len();
        out.extend(items);
    }

    let _ = tx.send(IndexEvent::Progress {
        job_id,
        current: scanned,
        total: scanned.max(1),
        phase: "index",
    });
    let backend = if used_ntfs && used_walkdir {
        IndexBackend::Mixed
    } else if used_ntfs {
        IndexBackend::NtfsMft
    } else if used_walkdir {
        IndexBackend::WalkDir
    } else {
        IndexBackend::Detecting
    };
    (out, backend)
}

pub(crate) fn walk_root_with_progress(
    root: &str,
    job_id: u64,
    tx: &mpsc::Sender<IndexEvent>,
) -> Vec<SearchItem> {
    let mut items = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() && !entry.file_type().is_dir() {
            continue;
        }
        items.push(search_item_from_walkdir_entry(&entry));
        if items.len().is_multiple_of(500) {
            let _ = tx.send(IndexEvent::Progress {
                job_id,
                current: items.len(),
                total: 0,
                phase: "index",
            });
        }
    }
    items
}

fn search_item_from_walkdir_entry(entry: &walkdir::DirEntry) -> SearchItem {
    let modified_unix_secs = entry
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(UNKNOWN_TS);

    SearchItem {
        path: entry.path().to_string_lossy().into_owned().into_boxed_str(),
        modified_unix_secs,
        kind: if entry.file_type().is_dir() {
            SearchItemKind::Folder
        } else {
            SearchItemKind::File
        },
    }
}

pub(crate) fn scope_roots(scope: &SearchScope) -> Vec<String> {
    match scope {
        SearchScope::CurrentFolder => vec![env::current_dir()
            .unwrap_or_else(|_| "C:\\".into())
            .to_string_lossy()
            .to_string()],
        SearchScope::EntireCurrentDrive => {
            let cwd = env::current_dir().unwrap_or_else(|_| "C:\\".into());
            let drive = drive_letter_from_path(&cwd).unwrap_or('C');
            vec![format!("{}:\\", drive.to_ascii_uppercase())]
        }
        SearchScope::AllLocalDrives => available_drive_roots(),
        SearchScope::Drive(letter) => vec![format!("{}:\\", letter.to_ascii_uppercase())],
    }
}

fn available_drive_roots() -> Vec<String> {
    let mut roots = Vec::new();

    for letter in 'A'..='Z' {
        let root = format!("{}:\\", letter);
        if std::path::Path::new(&root).exists() {
            roots.push(root);
        }
    }

    if roots.is_empty() {
        roots.push("C:\\".to_string());
    }

    roots
}

fn drive_letter_from_path(path: &std::path::Path) -> Option<char> {
    let raw = path.to_string_lossy();
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        Some(raw.chars().next()?.to_ascii_uppercase())
    } else {
        None
    }
}

fn drive_letter_from_root_str(root: &str) -> Option<char> {
    let bytes = root.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        Some((bytes[0] as char).to_ascii_uppercase())
    } else {
        None
    }
}

fn normalized_folder_prefix(path: &str) -> String {
    let mut normalized = path.replace('/', "\\").to_ascii_lowercase();
    if !normalized.ends_with('\\') {
        normalized.push('\\');
    }
    normalized
}

fn path_starts_with_folder(path: &str, folder_prefix: &str) -> bool {
    let normalized = path.replace('/', "\\").to_ascii_lowercase();
    normalized.starts_with(folder_prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_fallback_includes_files_and_folders() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rustsearch-fallback-{}-{unique}",
            std::process::id()
        ));
        let folder = root.join("folder");
        let file = folder.join("file.txt");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(&file, "test").unwrap();
        let (tx, _rx) = mpsc::channel();
        let items = walk_root_with_progress(root.to_str().unwrap(), 1, &tx);
        assert!(items
            .iter()
            .any(|item| item.path.as_ref() == file.to_str().unwrap()
                && item.kind == SearchItemKind::File));
        assert!(items
            .iter()
            .any(|item| item.path.as_ref() == folder.to_str().unwrap()
                && item.kind == SearchItemKind::Folder));
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(folder).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
