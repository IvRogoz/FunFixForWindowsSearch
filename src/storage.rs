use std::{env, thread};

use serde::{Deserialize, Serialize};

use crate::{SearchItem, SearchItemKind, SearchScope};

#[derive(Serialize, Deserialize)]
struct ScopeIndexSnapshot {
    version: u32,
    scope: String,
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

pub(crate) fn load_scope_snapshot(scope: &SearchScope) -> Option<Vec<SearchItem>> {
    if let Ok(file) = std::fs::File::open(scope_snapshot_path(scope)) {
        if let Ok(snapshot) = bincode::deserialize_from::<_, ScopeIndexSnapshot>(file) {
            if snapshot.version == 1 && snapshot.scope == scope.label() {
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

pub(crate) fn persist_scope_snapshot_async(scope: SearchScope, items: Vec<SearchItem>) {
    thread::spawn(move || {
        let path = scope_snapshot_path(&scope);
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }

        let snapshot = ScopeIndexSnapshot {
            version: 1,
            scope: scope.label(),
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

        let Ok(file) = std::fs::File::create(path) else {
            return;
        };
        let _ = bincode::serialize_into(file, &snapshot);
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

fn scope_snapshot_path(scope: &SearchScope) -> std::path::PathBuf {
    app_dir()
        .join("snapshots")
        .join(format!("scope-{}.bin", scope.label()))
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
            scope_snapshot_path(&SearchScope::AllLocalDrives),
            directory
                .join("snapshots")
                .join("scope-all-local-drives.bin")
        );
    }
}
