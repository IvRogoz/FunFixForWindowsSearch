use serde::{Deserialize, Serialize};
use wizcore_index::FileId;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortMode {
    Relevance,
    Name,
    Path,
    Date,
    Size,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub request_id: String,
    pub query: String,
    pub sort: SortMode,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedHit {
    pub file_id: FileId,
    pub score: f32,
}

pub trait QueryEngine {
    fn search(&self, request: &SearchRequest) -> Vec<RankedHit>;
}
