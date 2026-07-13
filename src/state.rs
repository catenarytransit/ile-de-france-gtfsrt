use crate::adaptive_matching::StopAliasIndex;
use crate::matching::GtfsMatchIndex;
use chrono::NaiveDate;
use dashmap::DashMap;
use gtfs_realtime::FeedMessage;
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
    pub stop_alias_index: StopAliasIndex,
}

impl LoadedGtfs {
    pub fn new(gtfs: Gtfs) -> Self {
        let match_index = GtfsMatchIndex::build(&gtfs);
        let stop_alias_index = StopAliasIndex::build(&gtfs);
        Self {
            gtfs,
            match_index,
            stop_alias_index,
        }
    }
}

pub struct AppState {
    pub gtfs: RwLock<Option<Arc<LoadedGtfs>>>,
    pub gtfs_rt_feed: RwLock<FeedMessage>,
    pub trip_platforms: DashMap<String, Vec<PlatformInfo>>,
    pub vehicle_assignments: DashMap<String, VehicleAssignment>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            gtfs: RwLock::new(None),
            gtfs_rt_feed: RwLock::new(FeedMessage::default()),
            trip_platforms: DashMap::new(),
            vehicle_assignments: DashMap::new(),
        }
    }
}
