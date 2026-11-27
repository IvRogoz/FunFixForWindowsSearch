use std::{
    collections::HashMap,
    env, fs, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
};

use serde::{Deserialize, Serialize};

use crate::{SearchItem, SearchItemKind, SearchScope};

#[derive(Serialize, Deserialize)]
struct ScopeIndexSnapshot {
    version: u32,
    scope: String,
    roots: Vec<String>,
    items: Vec<SnapshotItem>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotItem {
    path: String,
    modified_unix_secs: i64,
    #[serde(default = "default_snapshot_kind")]
    kind: SnapshotItemKind,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum SnapshotItemKind {
    File,
    Folder,
}

pub(crate) fn load_persisted_scope() -> SearchScope {
    let Ok(content) = std::fs::read_to_string(scope_config_path()) else {
        return SearchScope::CurrentFolder;
    };

    let value = content.trim().to_ascii_lowercase();
    if value == "current-folder" {
        SearchScope::CurrentFolder
    } else if value == "entire-current-drive" {
        SearchScope::EntireCurrentDrive
    } else if value == "all-local-drives" {
        SearchScope::AllLocalDrives
    } else {
        let bytes = value.as_bytes();
        if bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            SearchScope::Drive((bytes[0] as char).to_ascii_uppercase())
        } else {
            SearchScope::CurrentFolder
        }
    }
}

pub(crate) fn persist_scope(scope: &SearchScope) {
    let path = scope_config_path();
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }

    let _ = std::fs::write(path, scope.label());
}

pub(crate) fn load_quick_help_dismissed() -> bool {
    let Ok(content) = std::fs::read_to_string(quick_help_config_path()) else {
        return false;
    };

    content.trim().eq_ignore_ascii_case("1")
}

pub(crate) fn persist_quick_help_dismissed(value: bool) {
    let path = quick_help_config_path();
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }

    let _ = std::fs::write(path, if value { "1" } else { "0" });
}

pub(crate) fn load_scope_snapshot(
    scope: &SearchScope,
    roots: &[String],
) -> Option<Vec<SearchItem>> {
    let roots = normalized_roots(roots);
    load_snapshot_at(&scope_snapshot_path(scope, &roots), scope, &roots)
}

fn load_snapshot_at(path: &Path, scope: &SearchScope, roots: &[String]) -> Option<Vec<SearchItem>> {
    if let Ok(file) = std::fs::File::open(path) {
        if let Ok(snapshot) = bincode::deserialize_from::<_, ScopeIndexSnapshot>(file) {
            if snapshot.version == 2 && snapshot.scope == scope.label() && snapshot.roots == roots {
                return Some(
                    snapshot
                        .items
                        .into_iter()
                        .map(|item| SearchItem {
                            path: item.path.into_boxed_str(),
                            modified_unix_secs: item.modified_unix_secs,
                            kind: match item.kind {
                                SnapshotItemKind::File => SearchItemKind::File,
                                SnapshotItemKind::Folder => SearchItemKind::Folder,
                            },
                        })
                        .collect(),
                );
            }
        }
    }

    None
}

pub(crate) fn persist_scope_snapshot_async(
    scope: SearchScope,
    roots: Vec<String>,
    items: Vec<SearchItem>,
    cancel: Arc<AtomicBool>,
) {
    let roots = normalized_roots(&roots);
    let path = scope_snapshot_path(&scope, &roots);
    let Some(ticket) = reserve_write(&path) else {
        return;
    };
    thread::spawn(move || {
        let snapshot = ScopeIndexSnapshot {
            version: 2,
            scope: scope.label(),
            roots,
            items: items
                .into_iter()
                .map(|item| SnapshotItem {
                    path: item.path.into_string(),
                    modified_unix_secs: item.modified_unix_secs,
                    kind: match item.kind {
                        SearchItemKind::File => SnapshotItemKind::File,
                        SearchItemKind::Folder => SnapshotItemKind::Folder,
                    },
                })
                .collect(),
        };

        let _ = write_snapshot(&path, ticket, &cancel, |file| {
            bincode::serialize_into(file, &snapshot)
                .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
        });
    });
}

fn default_snapshot_kind() -> SnapshotItemKind {
    SnapshotItemKind::File
}

pub(crate) fn app_dir() -> std::path::PathBuf {
    let mut path = env::current_exe().expect("Cannot locate the portable application directory");
    path.pop();
    path
}

fn scope_config_path() -> std::path::PathBuf {
    app_dir().join("scope.txt")
}

fn quick_help_config_path() -> std::path::PathBuf {
    app_dir().join("quick-help-dismissed.txt")
}

fn normalized_roots(roots: &[String]) -> Vec<String> {
    let mut roots: Vec<_> = roots
        .iter()
        .map(|root| {
            let mut root = root.replace('\\', "/").to_ascii_lowercase();
            while root.ends_with('/') && root.len() > 1 {
                root.pop();
            }
            root
        })
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

fn scope_snapshot_path(scope: &SearchScope, roots: &[String]) -> PathBuf {
    let mut hash = 0xcbf29ce484222325u64;
    for value in std::iter::once(scope.label()).chain(roots.iter().cloned()) {
        for byte in (value.len() as u64)
            .to_le_bytes()
            .iter()
            .chain(value.as_bytes())
        {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    }
    app_dir()
        .join("snapshots")
        .join(format!("scope-{hash:016x}.bin"))
}

static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);
static LATEST: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn reserve_write(path: &Path) -> Option<u64> {
    let mut latest = LATEST
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()?;
    let ticket = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
    latest.insert(path.to_owned(), ticket);
    Some(ticket)
}

fn write_snapshot(
    path: &Path,
    ticket: u64,
    cancel: &AtomicBool,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let _write_guard = WRITE_LOCK
        .lock()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "snapshot write lock poisoned"))?;
    let is_current = || {
        LATEST
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .ok()
            .map_or(false, |latest| latest.get(path) == Some(&ticket))
    };
    if cancel.load(Ordering::Acquire) || !is_current() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), ticket));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let result = (|| {
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        let latest = LATEST
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "snapshot registry poisoned"))?;
        if !cancel.load(Ordering::Acquire) && latest.get(path) == Some(&ticket) {
            fs::rename(&temp, path)?;
        }
        Ok(())
    })();
    let _ = fs::remove_file(temp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_data_stays_beside_the_executable() {
        let exe = std::env::current_exe().unwrap();
        let directory = exe.parent().unwrap();
        assert_eq!(scope_config_path(), directory.join("scope.txt"));
        assert_eq!(
            quick_help_config_path(),
            directory.join("quick-help-dismissed.txt")
        );
        assert_eq!(
            scope_snapshot_path(&SearchScope::AllLocalDrives, &[])
                .parent()
                .unwrap(),
            directory.join("snapshots")
        );
    }

    #[test]
    fn snapshot_identity_uses_normalized_roots_and_safe_filename() {
        let a = normalized_roots(&["C:\\Users\\One\\".into(), "c:/users/one".into()]);
        let b = normalized_roots(&["C:\\Users\\Two".into()]);
        assert_eq!(a, vec!["c:/users/one"]);
        for scope in [SearchScope::CurrentFolder, SearchScope::EntireCurrentDrive] {
            assert_ne!(
                scope_snapshot_path(&scope, &a),
                scope_snapshot_path(&scope, &b)
            );
            assert!(!scope_snapshot_path(&scope, &a)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(':'));
        }
    }

    #[test]
    fn snapshot_replacement_preserves_old_data_on_failure_or_stale_write() {
        use std::io::Write;
        let directory = std::env::temp_dir().join(format!(
            "funfix-snapshot-test-{}-{}",
            std::process::id(),
            NEXT_TICKET.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("snapshot.bin");
        fs::write(&path, b"old").unwrap();
        let cancel = AtomicBool::new(false);
        let ticket = reserve_write(&path).unwrap();
        assert!(write_snapshot(&path, ticket, &cancel, |file| {
            file.write_all(b"partial")?;
            Err(io::Error::new(io::ErrorKind::Other, "failure"))
        })
        .is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old");
        let stale = reserve_write(&path).unwrap();
        let current = reserve_write(&path).unwrap();
        write_snapshot(&path, stale, &cancel, |file| file.write_all(b"stale")).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"old");
        cancel.store(true, Ordering::Release);
        write_snapshot(&path, current, &cancel, |file| file.write_all(b"cancelled")).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"old");
        cancel.store(false, Ordering::Release);
        write_snapshot(&path, current, &cancel, |file| file.write_all(b"new")).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn snapshot_load_validates_complete_identity() {
        let directory = std::env::temp_dir().join(format!(
            "funfix-snapshot-load-{}-{}",
            std::process::id(),
            NEXT_TICKET.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("snapshot.bin");
        let roots = normalized_roots(&["C:\\One".into()]);
        let mut snapshot = ScopeIndexSnapshot {
            version: 2,
            scope: SearchScope::CurrentFolder.label(),
            roots: roots.clone(),
            items: vec![SnapshotItem {
                path: "C:\\One\\file.txt".into(),
                modified_unix_secs: 42,
                kind: SnapshotItemKind::File,
            }],
        };
        let save = |snapshot: &ScopeIndexSnapshot| {
            fs::write(&path, bincode::serialize(snapshot).unwrap()).unwrap()
        };
        save(&snapshot);
        let loaded = load_snapshot_at(&path, &SearchScope::CurrentFolder, &roots).unwrap();
        assert_eq!(&*loaded[0].path, "C:\\One\\file.txt");
        assert_eq!(loaded[0].modified_unix_secs, 42);
        assert!(load_snapshot_at(&path, &SearchScope::EntireCurrentDrive, &roots).is_none());
        assert!(load_snapshot_at(
            &path,
            &SearchScope::CurrentFolder,
            &normalized_roots(&["C:\\Two".into()])
        )
        .is_none());
        snapshot.version = 1;
        save(&snapshot);
        assert!(load_snapshot_at(&path, &SearchScope::CurrentFolder, &roots).is_none());
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn newer_ticket_and_mid_write_cancellation_preserve_snapshot() {
        use std::{io::Write, sync::mpsc};
        let directory = std::env::temp_dir().join(format!(
            "funfix-snapshot-race-{}-{}",
            std::process::id(),
            NEXT_TICKET.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("snapshot.bin");
        fs::write(&path, b"old").unwrap();
        let old_ticket = reserve_write(&path).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            write_snapshot(&writer_path, old_ticket, &AtomicBool::new(false), |file| {
                file.write_all(b"stale")?;
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        });
        started_rx.recv().unwrap();
        let current = reserve_write(&path).unwrap();
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"old");
        let cancel = AtomicBool::new(false);
        write_snapshot(&path, current, &cancel, |file| {
            file.write_all(b"cancelled")?;
            cancel.store(true, Ordering::Release);
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"old");
        cancel.store(false, Ordering::Release);
        write_snapshot(&path, current, &cancel, |file| file.write_all(b"new")).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
