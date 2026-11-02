use crate::memory_optimisation::prune_gtfs_for_matching;
use crate::matching::GtfsMatchIndex;
use chrono::NaiveDate;
use dashmap::DashMap;
use gtfs_realtime::FeedMessage;
use prost::Message;
use gtfs_structures::Gtfs;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct VehicleAssignment {
    pub trip_id: String,
    pub route_id: String,
    pub service_date: Option<NaiveDate>,
    pub last_seen_epoch: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlatformInfo {
    pub stop_id: String,
    pub platform_name: String,
}

pub struct LoadedGtfs {
    pub gtfs: Gtfs,
    pub match_index: GtfsMatchIndex,
}

impl LoadedGtfs {
    pub fn new(mut gtfs: Gtfs) -> Self {
        prune_gtfs_for_matching(&mut gtfs);
        let match_index = GtfsMatchIndex::build(&gtfs);
        Self { gtfs, match_index }
    }
}

pub struct AppState {
    pub gtfs: RwLock<Option<Arc<LoadedGtfs>>>,
    pub gtfs_rt_feed: RwLock<Arc<Vec<u8>>>,
    pub trip_platforms: DashMap<String, Vec<PlatformInfo>>,
    pub vehicle_assignments: DashMap<String, VehicleAssignment>,
}

impl AppState {
    pub fn new(initial_feed: FeedMessage) -> Self {
        Self {
            gtfs: RwLock::new(None),
            gtfs_rt_feed: RwLock::new(Arc::new(initial_feed.encode_to_vec())),
            trip_platforms: DashMap::new(),
            vehicle_assignments: DashMap::new(),
        }
    }
}
