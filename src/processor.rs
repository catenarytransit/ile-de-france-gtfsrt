use crate::siri_models::{EstimatedVehicleJourney, SiriResponse};
use crate::state::{AppState, PlatformInfo};
use chrono::{DateTime, Utc};
use gtfs_realtime::{
    FeedEntity, FeedHeader, FeedMessage, TripDescriptor, TripUpdate, trip_update::StopTimeEvent,
    trip_update::StopTimeUpdate,
};
use gtfs_structures::Gtfs;
use serde_json::json;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

pub async fn process_siri(state: Arc<AppState>, siri: SiriResponse) {
    const MAX_MISSED_EXAMPLES: usize = 50;

    let mut missed_examples = Vec::new();

    let mut feed_msg = FeedMessage::default();
    let mut header = FeedHeader::default();
    header.gtfs_realtime_version = "2.0".to_string();
    header.timestamp = Some(Utc::now().timestamp() as u64);
    feed_msg.header = header;

    let mut exact_matches = 0;
    let mut sequence_matches = 0;
    let mut unmatched = 0;

    let gtfs_lock = state.gtfs.read().await;
    let gtfs_opt = gtfs_lock.as_ref();

    for delivery in siri.siri.service_delivery.estimated_timetable_delivery {
        for frame in delivery.estimated_journey_version_frame {
            for journey in frame.estimated_vehicle_journey {
                let Some(gtfs) = gtfs_opt else { continue };

                let mut matched_trip_id = None;

                // 1. Try Exact match
                if let Some(siri_ref) = &journey.dated_vehicle_journey_ref {
                    let siri_id = &siri_ref.value;
                    // Siri ID often looks like: 515FPM:VehicleJourney::389057:LOC
                    // Or SNCF_MAGENTA_PRD:VehicleJourney::64ef1d52-0e2a-4a7b-9c21-9f0b68aa9a80:LOC
                    // GTFS ID might be: IDFM:TN:SNCF:64ef1d52-0e2a-4a7b-9c21-9f0b68aa9a80

                    let parts: Vec<&str> = siri_id.split("::").collect();
                    if parts.len() > 1 {
                        let id_part = parts[1].trim_end_matches(":LOC");
                        for (gtfs_id, _) in &gtfs.trips {
                            if gtfs_id.contains(id_part) {
                                matched_trip_id = Some(gtfs_id.clone());
                                exact_matches += 1;
                                break;
                            }
                        }
                    }
                }

                // 2. Try Sequence Fallback Match
                if matched_trip_id.is_none() {
                    if let Some(matched) = fallback_match(&journey, gtfs) {
                        matched_trip_id = Some(matched);
                        sequence_matches += 1;
                    } else {
                        unmatched += 1;

                        if missed_examples.len() < MAX_MISSED_EXAMPLES {
                            missed_examples.push(build_missed_example(&journey));
                        }
                    }
                }

                // Build TripUpdate and Platforms
                if let Some(trip_id) = matched_trip_id {
                    let mut trip_update = TripUpdate::default();
                    let mut trip_desc = TripDescriptor::default();
                    trip_desc.trip_id = Some(trip_id.clone());
                    trip_update.trip = trip_desc;

                    let mut platforms = Vec::new();

                    if let Some(calls) = &journey.estimated_calls {
                        for call in &calls.estimated_call {
                            let mut stu = StopTimeUpdate::default();

                            if let Some(stop_ref) = &call.stop_point_ref {
                                // Extract GTFS stop_id from STIF:StopPoint:Q:30785:
                                // Result should be IDFM:30785
                                let parts: Vec<&str> = stop_ref.value.split(':').collect();
                                if parts.len() >= 4 {
                                    let stop_id_num = parts[3];
                                    let gtfs_stop_id = format!("IDFM:{}", stop_id_num);
                                    stu.stop_id = Some(gtfs_stop_id.clone());

                                    let platform_name = &call
                                        .departure_platform_name
                                        .as_ref()
                                        .or(call.arrival_platform_name.as_ref())
                                        .map(|s| s.value.clone());
                                    if let Some(platform_name) = platform_name {
                                        if platform_name != "unknown" {
                                            platforms.push(PlatformInfo {
                                            stop_id: gtfs_stop_id,
                                            platform_name: platform_name.to_string(),
                                        });
                                        }
                                    }
                                }
                            }

                            if let Some(time_str) = &call
                                .expected_arrival_time
                                .as_ref()
                                .or(call.aimed_arrival_time.as_ref())
                            {
                                if let Ok(dt) = DateTime::parse_from_rfc3339(time_str) {
                                    let mut event = StopTimeEvent::default();
                                    event.time = Some(dt.timestamp());
                                    stu.arrival = Some(event);
                                }
                            }

                            if let Some(time_str) = &call
                                .expected_departure_time
                                .as_ref()
                                .or(call.aimed_departure_time.as_ref())
                            {
                                if let Ok(dt) = DateTime::parse_from_rfc3339(time_str) {
                                    let mut event = StopTimeEvent::default();
                                    event.time = Some(dt.timestamp());
                                    stu.departure = Some(event);
                                }
                            }

                            trip_update.stop_time_update.push(stu);
                        }
                    }

                    if !platforms.is_empty() {
                        state.trip_platforms.insert(trip_id.clone(), platforms);
                    }

                    let mut entity = FeedEntity::default();
                    entity.id = trip_id;
                    entity.trip_update = Some(trip_update);
                    feed_msg.entity.push(entity);
                }
            }
        }
    }

    if let Err(error) = append_missed_examples("debug/siri-unmatched.jsonl", &missed_examples).await
    {
        eprintln!("Failed to write unmatched SIRI examples: {error}");
    }

    println!(
        "Matched: Exact = {}, Sequence = {}, Missed = {}",
        exact_matches, sequence_matches, unmatched
    );
    let mut lock = state.gtfs_rt_feed.write().await;
    *lock = feed_msg;
}

fn fallback_match(journey: &EstimatedVehicleJourney, gtfs: &Gtfs) -> Option<String> {
    let line_ref = journey.line_ref.as_ref()?;

    // Extract route ID from STIF:Line::C01443: -> IDFM:C01443
    let parts: Vec<&str> = line_ref.value.split("::").collect();
    if parts.len() < 2 {
        return None;
    }
    let route_id_part = parts[1].trim_end_matches(':');
    let target_route_id = format!("IDFM:{}", route_id_part);

    let calls = journey.estimated_calls.as_ref()?;
    if calls.estimated_call.is_empty() {
        return None;
    }

    // Map siri stops to GTFS stop ids
    let mut target_stops = Vec::new();
    for call in &calls.estimated_call {
        if let Some(stop_ref) = &call.stop_point_ref {
            let parts: Vec<&str> = stop_ref.value.split(':').collect();
            if parts.len() >= 4 {
                target_stops.push(format!("IDFM:{}", parts[3]));
            }
        }
    }

    if target_stops.is_empty() {
        return None;
    }

    let mut best_trip_id = None;
    // let mut min_diff = i64::MAX;

    // Find all trips on the target route
    for (trip_id, trip) in &gtfs.trips {
        if trip.route_id != target_route_id {
            continue;
        }

        // Quick check if sequences align loosely.
        // We'll just check if the first GTFS stop matches the first Siri stop.
        let first_siri_stop = &target_stops[0];

        let mut trip_stops = Vec::new();
        let mut match_found = false;

        for st in &trip.stop_times {
            if st.stop.id == *first_siri_stop {
                match_found = true;
            }
            if match_found {
                trip_stops.push(st);
            }
        }

        if !match_found || trip_stops.len() < target_stops.len() {
            continue;
        }

        // Compare sequence
        let mut sequence_ok = true;
        for i in 0..target_stops.len() {
            if trip_stops[i].stop.id != target_stops[i] {
                sequence_ok = false;
                break;
            }
        }

        if sequence_ok {
            // Compare times if possible to find closest match.
            // Siri time is absolute UTC. GTFS time is relative seconds past midnight.
            // We can compare the time difference between the first stops if we want absolute precision,
            // but for simplicity we just return the first sequence matched trip, or we could compare scheduled times.

            // To do full time comparison, we need the day of operation.
            // For now, let's just pick the first trip that matches the sequence.
            // A more robust implementation would check the aimed arrival time against the GTFS stop_time (seconds from midnight).
            best_trip_id = Some(trip_id.clone());
            break;
        }
    }

    best_trip_id
}

fn build_missed_example(journey: &EstimatedVehicleJourney) -> serde_json::Value {
    let calls = journey
        .estimated_calls
        .as_ref()
        .map(|calls| {
            calls
                .estimated_call
                .iter()
                .map(|call| {
                    json!({
                        "stop_point_ref": call
                            .stop_point_ref
                            .as_ref()
                            .map(|value| value.value.as_str()),

                        "aimed_arrival_time": call.aimed_arrival_time.as_deref(),
                        "expected_arrival_time": call.expected_arrival_time.as_deref(),

                        "aimed_departure_time": call.aimed_departure_time.as_deref(),
                        "expected_departure_time": call.expected_departure_time.as_deref(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    json!({
        "logged_at": Utc::now().to_rfc3339(),

        "dated_vehicle_journey_ref": journey
            .dated_vehicle_journey_ref
            .as_ref()
            .map(|value| value.value.as_str()),

        "line_ref": journey
            .line_ref
            .as_ref()
            .map(|value| value.value.as_str()),

        "calls": calls,
    })
}

async fn append_missed_examples(path: &str, examples: &[serde_json::Value]) -> std::io::Result<()> {
    if examples.is_empty() {
        return Ok(());
    }

    if let Some(parent) = std::path::Path::new(path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;

    for example in examples {
        let mut line = serde_json::to_vec(example).map_err(std::io::Error::other)?;

        line.push(b'\n');
        file.write_all(&line).await?;
    }

    file.flush().await?;

    Ok(())
}
