use serde::{Deserialize, Serialize};
use wizcore_index::FileId;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Created,
    Modified,
    Renamed,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDelta {
    pub file_id: FileId,
    pub kind: ChangeKind,
}

pub trait WatchSource {
    fn next_delta(&mut self) -> Option<FileDelta>;
}
