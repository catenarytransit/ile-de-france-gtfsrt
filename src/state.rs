use crate::adaptive_matching::StopAliasIndex;
use crate::matching::GtfsMatchIndex;
use crate::maple_syrup::MapleSyrup;
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
    pub maple: MapleSyrup,
}

impl LoadedGtfs {
    pub fn new(mut gtfs: Gtfs) -> Self {
        let mut match_index = GtfsMatchIndex::build(&gtfs);
        let stop_alias_index = StopAliasIndex::build(&gtfs);
        let maple = MapleSyrup::compress(&mut gtfs);
        match_index.sort_trip_starts(&maple);
        let (trips, patterns, stops) = maple.stats();
        println!("Maple Syrup: {trips} trips, {patterns} unique itineraries, {stops} stop-times compressed");
        Self {
            gtfs,
            match_index,
            stop_alias_index,
            maple,
        }
    }
}

pub struct AppState {
    pub gtfs: RwLock<Option<Arc<LoadedGtfs>>>,
    pub gtfs_rt_feed: RwLock<Arc<FeedMessage>>,
    pub trip_platforms: DashMap<String, Vec<PlatformInfo>>,
    pub vehicle_assignments: DashMap<String, VehicleAssignment>,
}

impl AppState {
    pub fn new(initial_feed: FeedMessage) -> Self {
        Self {
            gtfs: RwLock::new(None),
            gtfs_rt_feed: RwLock::new(Arc::new(initial_feed)),
            trip_platforms: DashMap::new(),
            vehicle_assignments: DashMap::new(),
        }
    }
}
