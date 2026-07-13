use gtfs_structures::Gtfs;
use gtfs_realtime::FeedMessage;
use serde::Serialize;
use std::sync::Arc;
use dashmap::DashMap;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize)]
pub struct PlatformInfo {
    pub stop_id: String,
    pub platform_name: String,
}

pub struct AppState {
    pub gtfs: RwLock<Option<Arc<Gtfs>>>,
    pub gtfs_rt_feed: RwLock<FeedMessage>,
    pub trip_platforms: DashMap<String, Vec<PlatformInfo>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            gtfs: RwLock::new(None),
            gtfs_rt_feed: RwLock::new(FeedMessage::default()),
            trip_platforms: DashMap::new(),
        }
    }
}
