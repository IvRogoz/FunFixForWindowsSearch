use serde::{Deserialize, Serialize};

pub type FileId = u32;
pub type PathId = u32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub id: FileId,
    pub name_lc: String,
    pub ext_lc: String,
    pub parent_path_id: PathId,
    pub size: u64,
    pub mtime_unix_ms: i64,
    pub attrs: u32,
}

#[derive(Debug, Clone, Default)]
pub struct PathTable {
    pub paths: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("not implemented")]
    NotImplemented,
}

pub trait IndexSource {
    fn build_initial_index(&self) -> Result<Vec<FileEntry>, IndexError>;
}
