use serde::{Deserialize, Serialize};
use wizcore_config::Settings;
use wizcore_query::{SearchRequest, SortMode};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchItem {
    pub file_id: u32,
    pub display_name: String,
    pub full_path: String,
    pub size: u64,
    pub mtime_unix_ms: i64,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchChunk {
    pub request_id: String,
    pub items: Vec<SearchItem>,
}

pub struct AppService {
    pub settings: Settings,
}

impl Default for AppService {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
        }
    }
}

impl AppService {
    pub fn start_search(&self, query: String, request_id: String) -> SearchRequest {
        SearchRequest {
            request_id,
            query,
            sort: SortMode::Relevance,
            limit: 500,
        }
    }
}
