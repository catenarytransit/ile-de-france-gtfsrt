//! Recovery layer around the existing strict SIRI processor.
//!
//! `processor::process_siri` remains the source of truth for exact and clean ordered matches.
//! This module inspects only journeys that the strict matcher rejects, then appends adaptive,
//! cached, operator-specific, and ExtraJourney updates after the strict feed has been built.

use crate::adaptive_matching;
use crate::matching::JourneyMatch;
use crate::processor;
use crate::siri_models::{EstimatedCall, EstimatedVehicleJourney, SiriResponse};
use crate::state::{AppState, PlatformInfo, VehicleAssignment};
use chrono::{DateTime, Utc};
use gtfs_realtime::{
    FeedEntity, FeedMessage, TripDescriptor, TripUpdate,
    trip_descriptor::ScheduleRelationship as TripScheduleRelationship,
    trip_update::{
        StopTimeEvent, StopTimeUpdate,
        stop_time_update::ScheduleRelationship as StopTimeScheduleRelationship,
    },
};
use gtfs_structures::Gtfs;
use std::collections::HashSet;
use std::sync::Arc;

const VEHICLE_ASSIGNMENT_TTL_SECONDS: i64 = 15 * 60;
const VEHICLE_ASSIGNMENT_RETENTION_SECONDS: i64 = 6 * 60 * 60;

pub async fn process_siri(state: Arc<AppState>, siri: SiriResponse) {
    let loaded_gtfs = {
        let lock = state.gtfs.read().await;
        lock.clone()
    };

    let Some(loaded_gtfs) = loaded_gtfs else {
        let feed_msg = processor::process_siri(state.clone(), siri).await;
        publish_and_cache_feed(state, feed_msg).await;
        return;
    };

    let gtfs = &loaded_gtfs.gtfs;
    let match_index = &loaded_gtfs.match_index;
    let stop_alias_index = &loaded_gtfs.stop_alias_index;
    let now = Utc::now().timestamp();
    let mut recovered = Vec::<(FeedEntity, Option<(String, Vec<PlatformInfo>)>)>::new();
    let mut adaptive_matches = 0_u64;
    let mut cached_matches = 0_u64;
    let mut extra_journeys = 0_u64;

    for delivery in &siri.siri.service_delivery.estimated_timetable_delivery {
        for frame in &delivery.estimated_journey_version_frame {
            for journey in &frame.estimated_vehicle_journey {
                let mut strict_view = journey.clone();
                sort_for_strict_matcher(&mut strict_view);

                if let Ok(matched) = match_index.match_journey(&strict_view, gtfs) {
                    remember_assignment(&state, journey, &matched, gtfs, now);
                    continue;
                }

                let mut match_source = None;
                let mut matched = adaptive_matching::match_journey(journey, gtfs, stop_alias_index);
                if matched.is_some() {
                    match_source = Some("adaptive");
                }

                if matched.is_none() {
                    if let Some(vehicle_key) = vehicle_key(journey) {
                        if let Some(assignment) = state.vehicle_assignments.get(vehicle_key) {
                            let fresh = now.saturating_sub(assignment.last_seen_epoch)
                                <= VEHICLE_ASSIGNMENT_TTL_SECONDS;
                            if fresh {
                                matched = adaptive_matching::match_cached_assignment(
                                    journey,
                                    gtfs,
                                    stop_alias_index,
                                    &assignment.trip_id,
                                );
                                if matched.is_some() {
                                    match_source = Some("cache");
                                }
                            }
                        }
                    }
                }

                if let Some(matched) = matched {
                    if let Some((entity, platforms)) = build_matched_entity(journey, &matched, gtfs)
                    {
                        remember_assignment(&state, journey, &matched, gtfs, now);
                        recovered.push((entity, platforms));
                        match match_source {
                            Some("cache") => cached_matches += 1,
                            _ => adaptive_matches += 1,
                        }
                    }
                    continue;
                }

                /*
                if journey.extra_journey {
                    if let Some(entity) =
                        build_extra_journey_entity(journey, gtfs, stop_alias_index)
                    {
                        recovered.push((entity, None));
                        extra_journeys += 1;
                    }
                }
                    */
            }
        }
    }

    // Build all exact/strict updates with the existing implementation first.
    let mut feed = processor::process_siri(state.clone(), siri).await;

    // Then append only trips that were not already emitted by the strict processor.
    let mut existing_trip_ids = feed
        .entity
        .iter()
        .filter_map(|entity| entity.trip_update.as_ref())
        .filter_map(|update| update.trip.trip_id.as_deref())
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    let mut existing_entity_ids = feed
        .entity
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<HashSet<_>>();

    for (entity, platforms) in recovered {
        let trip_id = entity
            .trip_update
            .as_ref()
            .and_then(|update| update.trip.trip_id.as_deref())
            .map(str::to_owned);

        if trip_id
            .as_ref()
            .is_some_and(|trip_id| existing_trip_ids.contains(trip_id))
            || existing_entity_ids.contains(&entity.id)
        {
            continue;
        }

        if let Some((trip_id, platforms)) = platforms {
            if !platforms.is_empty() {
                state.trip_platforms.insert(trip_id, platforms);
            }
        }
        if let Some(trip_id) = trip_id {
            existing_trip_ids.insert(trip_id);
        }
        existing_entity_ids.insert(entity.id.clone());
        feed.entity.push(entity);
    }

    state.vehicle_assignments.retain(|_, assignment| {
        now.saturating_sub(assignment.last_seen_epoch) <= VEHICLE_ASSIGNMENT_RETENTION_SECONDS
    });

    println!(
        "Recovered matches: adaptive = {adaptive_matches}, cache = {cached_matches}, extra journeys = {extra_journeys}"
    );

    publish_and_cache_feed(state.clone(), feed).await;
}

fn remember_assignment(
    state: &AppState,
    journey: &EstimatedVehicleJourney,
    matched: &JourneyMatch,
    gtfs: &Gtfs,
    now: i64,
) {
    let Some(vehicle_key) = vehicle_key(journey) else {
        return;
    };
    let Some(trip) = gtfs.trips.get(&matched.trip_id) else {
        return;
    };
    state.vehicle_assignments.insert(
        vehicle_key.to_owned(),
        VehicleAssignment {
            trip_id: matched.trip_id.clone(),
            route_id: trip.route_id.clone(),
            service_date: matched.service_date,
            last_seen_epoch: now,
        },
    );
}

fn vehicle_key(journey: &EstimatedVehicleJourney) -> Option<&str> {
    journey
        .dated_vehicle_journey_ref
        .as_ref()
        .and_then(|value| value.value.as_deref())
}

fn build_matched_entity(
    journey: &EstimatedVehicleJourney,
    matched: &JourneyMatch,
    gtfs: &Gtfs,
) -> Option<(FeedEntity, Option<(String, Vec<PlatformInfo>)>)> {
    let trip = gtfs.trips.get(&matched.trip_id)?;
    let calls = journey.estimated_calls.as_ref()?;

    let mut descriptor = TripDescriptor::default();
    descriptor.trip_id = Some(matched.trip_id.clone());
    descriptor.start_date = matched
        .service_date
        .map(|date| date.format("%Y%m%d").to_string());

    let mut update = TripUpdate::default();
    update.trip = descriptor;
    let mut platforms = Vec::new();

    for (call_index, call) in calls.estimated_call.iter().enumerate() {
        let Some(stop_index) = matched.stop_indices.get(call_index).copied() else {
            continue;
        };
        if stop_index == usize::MAX {
            continue;
        }
        let Some(stop_time) = trip.stop_times.get(stop_index) else {
            continue;
        };

        let stop_id = stop_time.stop.id.clone();
        let mut stop_update = StopTimeUpdate::default();
        stop_update.stop_id = Some(stop_id.clone());

        if call
            .arrival_status
            .as_deref()
            .is_some_and(is_cancelled_status)
            || call
                .departure_status
                .as_deref()
                .is_some_and(is_cancelled_status)
        {
            stop_update.schedule_relationship =
                Some(StopTimeScheduleRelationship::Skipped as i32);
        }

        if let Some(timestamp) = realtime_or_aimed_arrival(call) {
            stop_update.arrival = Some(StopTimeEvent {
                time: Some(timestamp),
                ..Default::default()
            });
        }
        if let Some(timestamp) = realtime_or_aimed_departure(call) {
            stop_update.departure = Some(StopTimeEvent {
                time: Some(timestamp),
                ..Default::default()
            });
        }

        if let Some(platform_name) = call
            .departure_platform_name
            .as_ref()
            .or(call.arrival_platform_name.as_ref())
                .and_then(|value| value.value.as_deref())
            .filter(|value| !value.eq_ignore_ascii_case("unknown"))
        {
            platforms.push(PlatformInfo {
                stop_id,
                platform_name: platform_name.to_owned(),
            });
        }

        update.stop_time_update.push(stop_update);
    }

    if update.stop_time_update.is_empty() {
        return None;
    }

    Some((
        FeedEntity {
            id: matched.trip_id.clone(),
            trip_update: Some(update),
            ..Default::default()
        },
        Some((matched.trip_id.clone(), platforms)),
    ))
}

fn build_extra_journey_entity(
    journey: &EstimatedVehicleJourney,
    gtfs: &Gtfs,
    stop_alias_index: &adaptive_matching::StopAliasIndex,
) -> Option<FeedEntity> {
    let vehicle_ref = vehicle_key(journey).unwrap_or("unknown");
    let route_id = journey
        .line_ref
        .as_ref()
        .and_then(|value| value.value.as_deref().and_then(extract_idfm_id))?;
    if !gtfs.routes.contains_key(&route_id) {
        return None;
    }

    let synthetic_id = format!("IDFM:EXTRA:{}", sanitize_entity_id(vehicle_ref));
    let mut descriptor = TripDescriptor::default();
    descriptor.trip_id = Some(synthetic_id.clone());
    descriptor.route_id = Some(route_id);
    descriptor.schedule_relationship = Some(TripScheduleRelationship::Added as i32);

    let mut update = TripUpdate::default();
    update.trip = descriptor;

    for call in journey
        .estimated_calls
        .as_ref()
        .into_iter()
        .flat_map(|calls| &calls.estimated_call)
    {
        let Some(stop_ref) = call.stop_point_ref.as_ref() else {
            continue;
        };
        let Some(stop_id) = stop_ref
            .value
            .as_deref()
            .and_then(|value| stop_alias_index.first(value)) else {
            continue;
        };

        let mut stop_update = StopTimeUpdate::default();
        stop_update.stop_id = Some(stop_id);
        if call
            .arrival_status
            .as_deref()
            .is_some_and(is_cancelled_status)
            || call
                .departure_status
                .as_deref()
                .is_some_and(is_cancelled_status)
        {
            stop_update.schedule_relationship =
                Some(StopTimeScheduleRelationship::Skipped as i32);
        }
        if let Some(timestamp) = realtime_or_aimed_arrival(call) {
            stop_update.arrival = Some(StopTimeEvent {
                time: Some(timestamp),
                ..Default::default()
            });
        }
        if let Some(timestamp) = realtime_or_aimed_departure(call) {
            stop_update.departure = Some(StopTimeEvent {
                time: Some(timestamp),
                ..Default::default()
            });
        }
        update.stop_time_update.push(stop_update);
    }

    if update.stop_time_update.is_empty() {
        return None;
    }

    Some(FeedEntity {
        id: synthetic_id,
        trip_update: Some(update),
        ..Default::default()
    })
}

fn extract_idfm_id(value: &str) -> Option<String> {
    let suffix = value.rsplit(':').find(|part| !part.is_empty())?;
    Some(format!("IDFM:{suffix}"))
}

fn realtime_or_aimed_arrival(call: &EstimatedCall) -> Option<i64> {
    call.expected_arrival_time
        .as_deref()
        .or(call.aimed_arrival_time.as_deref())
        .and_then(parse_siri_timestamp)
}

fn realtime_or_aimed_departure(call: &EstimatedCall) -> Option<i64> {
    call.expected_departure_time
        .as_deref()
        .or(call.aimed_departure_time.as_deref())
        .and_then(parse_siri_timestamp)
}

fn parse_siri_timestamp(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date_time| date_time.timestamp())
}

fn sort_for_strict_matcher(journey: &mut EstimatedVehicleJourney) {
    let Some(calls) = journey.estimated_calls.as_mut() else {
        return;
    };
    calls.estimated_call.sort_by_cached_key(|call| {
        let aimed_timestamp = [
            call.aimed_arrival_time.as_deref(),
            call.aimed_departure_time.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(parse_siri_timestamp)
        .min();
        let expected_timestamp = [
            call.expected_arrival_time.as_deref(),
            call.expected_departure_time.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(parse_siri_timestamp)
        .min();
        let timestamp = aimed_timestamp.or(expected_timestamp);
        (timestamp.is_none(), timestamp.unwrap_or(i64::MAX))
    });
}

fn sanitize_entity_id(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn is_cancelled_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("CANCELLED") || status.eq_ignore_ascii_case("CANCELED")
}

async fn publish_and_cache_feed(state: Arc<AppState>, feed_msg: FeedMessage) {
    let feed_arc = Arc::new(feed_msg);
    {
        let mut lock = state.gtfs_rt_feed.write().await;
        *lock = feed_arc.clone();
    }

    let cache_path = std::env::var("GTFS_RT_CACHE_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("./gtfs-rt.pb"));

    tokio::spawn(async move {
        use prost::Message;
        let mut buf = Vec::new();
        if let Err(e) = feed_arc.encode(&mut buf) {
            eprintln!("Failed to encode GTFS-RT feed for caching: {e}");
            return;
        }

        let temp_path = cache_path.with_extension("pb.tmp");
        if let Err(e) = tokio::fs::write(&temp_path, buf).await {
            eprintln!("Failed to write temporary GTFS-RT cache file {}: {e}", temp_path.display());
            return;
        }

        if let Err(e) = tokio::fs::rename(&temp_path, &cache_path).await {
            eprintln!("Failed to rename temporary GTFS-RT cache to {}: {e}", cache_path.display());
        }
    });
}
