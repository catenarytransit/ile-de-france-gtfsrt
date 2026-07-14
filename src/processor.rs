use crate::matching::{MatchKind, MatchMissReason};
use crate::matching_diagnostics::StopAlignmentDiagnostics;
use crate::siri_models::{EstimatedCall, EstimatedVehicleJourney, SiriResponse};
use crate::state::{AppState, PlatformInfo};
use chrono::{DateTime, Utc};
use gtfs_realtime::{
    FeedEntity, FeedHeader, FeedMessage, TripDescriptor, TripUpdate,
    trip_update::{
        StopTimeEvent, StopTimeUpdate,
        stop_time_update::ScheduleRelationship as StopTimeScheduleRelationship,
    },
};
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
use tokio::io::AsyncWriteExt;

pub async fn process_siri(state: Arc<AppState>, siri: SiriResponse) -> FeedMessage {
    const MAX_MISSED_EXAMPLES_PER_REASON: usize = 20;

    let mut missed_examples = Vec::new();
    let mut missed_examples_by_reason = HashMap::<&'static str, usize>::new();

    let mut feed_msg = FeedMessage::default();
    let mut header = FeedHeader::default();
    header.gtfs_realtime_version = "2.0".to_string();
    header.timestamp = Some(Utc::now().timestamp() as u64);
    feed_msg.header = header;

    let mut exact_matches = 0;
    let mut sequence_matches = 0;
    let mut parent_station_matches = 0;
    let mut unmatched = 0;
    let mut missed_by_reason = HashMap::<&'static str, usize>::new();
    let mut scored_matches = 0_u64;
    let mut total_mean_difference_seconds = 0_i128;

    // Clone the immutable GTFS snapshot, then release the RwLock. A daily GTFS
    // refresh can replace the snapshot while this SIRI payload is processed.
    let loaded_gtfs = {
        let lock = state.gtfs.read().await;
        lock.clone()
    };

    let Some(loaded_gtfs) = loaded_gtfs else {
        return FeedMessage::default();
    };

    let gtfs = &loaded_gtfs.gtfs;
    let match_index = &loaded_gtfs.match_index;

    for delivery in siri.siri.service_delivery.estimated_timetable_delivery {
        for frame in delivery.estimated_journey_version_frame {
            for mut journey in frame.estimated_vehicle_journey {
                // IDFM does not consistently return EstimatedCall entries in journey order.
                // Sort them by their absolute SIRI timestamps before stop-sequence matching.
                sort_estimated_calls_by_time(&mut journey);
                let matched = match match_index.match_journey(&journey, gtfs) {
                    Ok(matched) => matched,
                    Err(reason) => {
                        let stop_alignment = if reason == MatchMissReason::NoStopAlignment {
                            match_index.diagnose_stop_alignment(&journey)
                        } else {
                            None
                        };
                        let reason_name = stop_alignment
                            .as_ref()
                            .map(|diagnostics| diagnostics.kind.as_str())
                            .unwrap_or_else(|| reason.as_str());

                        unmatched += 1;
                        *missed_by_reason.entry(reason_name).or_default() += 1;

                        let sampled = missed_examples_by_reason.entry(reason_name).or_default();
                        if *sampled < MAX_MISSED_EXAMPLES_PER_REASON {
                            missed_examples.push(build_missed_example(
                                &journey,
                                reason,
                                reason_name,
                                stop_alignment.as_ref(),
                            ));
                            *sampled += 1;
                        }

                        continue;
                    }
                };

                match matched.kind {
                    MatchKind::ExactId => exact_matches += 1,
                    MatchKind::DirectionAndTime => sequence_matches += 1,
                }
                if matched.used_parent_station_match {
                    parent_station_matches += 1;
                }

                if let Some(difference) = matched.mean_abs_difference_seconds {
                    scored_matches += 1;
                    total_mean_difference_seconds += i128::from(difference);
                }

                let matched_trip = gtfs.trips.get(&matched.trip_id);
                let trip_id = matched.trip_id.clone();
                let mut trip_update = TripUpdate::default();
                let mut trip_desc = TripDescriptor::default();
                trip_desc.trip_id = Some(trip_id.clone());
                trip_desc.start_date = matched
                    .service_date
                    .map(|date| date.format("%Y%m%d").to_string());
                trip_update.trip = trip_desc;

                let mut platforms = Vec::new();

                if let Some(calls) = &journey.estimated_calls {
                    let mut observed_call_index = 0_usize;

                    for call in &calls.estimated_call {
                        let mut stu = StopTimeUpdate::default();

                        if call
                            .arrival_status
                            .as_deref()
                            .is_some_and(is_cancelled_status)
                            || call
                                .departure_status
                                .as_deref()
                                .is_some_and(is_cancelled_status)
                        {
                            stu.schedule_relationship =
                                Some(StopTimeScheduleRelationship::Skipped as i32);
                        }

                        let has_stop_point_ref = call
                            .stop_point_ref
                            .as_ref()
                            .and_then(|r| r.value.as_deref())
                            .map(|val| !match_index.resolve_siri_stop_ids(val).is_empty())
                            .unwrap_or(false);

                        if has_stop_point_ref {
                            let gtfs_stop_id = matched
                                .stop_indices
                                .get(observed_call_index)
                                .and_then(|stop_index| matched_trip?.stop_times.get(*stop_index))
                                .map(|stop_time| stop_time.stop.id.clone());

                            observed_call_index += 1;

                            if let Some(gtfs_stop_id) = gtfs_stop_id {
                                stu.stop_id = Some(gtfs_stop_id.clone());

                                let platform_name = call
                                    .departure_platform_name
                                    .as_ref()
                                    .or(call.arrival_platform_name.as_ref())
                                    .and_then(|value| value.value.as_deref());

                                if let Some(platform_name) = platform_name {
                                    if platform_name != "unknown" {
                                        platforms.push(PlatformInfo {
                                            stop_id: gtfs_stop_id,
                                            platform_name: platform_name.to_string(),
                                        });
                                    }
                                }
                            } else {
                                continue;
                            }
                        } else {
                            continue;
                        }

                        // Prefer a realtime prediction, but use the aimed schedule time
                        // when IDFM does not provide an expected timestamp.
                        if let Some(time_str) = call
                            .expected_arrival_time
                            .as_ref()
                            .or(call.aimed_arrival_time.as_ref())
                        {
                            if let Ok(date_time) = DateTime::parse_from_rfc3339(time_str) {
                                let mut event = StopTimeEvent::default();
                                event.time = Some(date_time.timestamp());
                                stu.arrival = Some(event);
                            }
                        }

                        if let Some(time_str) = call
                            .expected_departure_time
                            .as_ref()
                            .or(call.aimed_departure_time.as_ref())
                        {
                            if let Ok(date_time) = DateTime::parse_from_rfc3339(time_str) {
                                let mut event = StopTimeEvent::default();
                                event.time = Some(date_time.timestamp());
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

    if let Err(error) = append_missed_examples("debug/siri-unmatched.jsonl", &missed_examples).await
    {
        eprintln!("Failed to write unmatched SIRI examples: {error}");
    }

    let average_difference_seconds =
        (scored_matches > 0).then(|| total_mean_difference_seconds / i128::from(scored_matches));

    println!(
        "Matched: Exact = {}, Direction/time = {}, Parent-assisted = {}, Missed = {}, Average aimed-time difference = {:?}s",
        exact_matches,
        sequence_matches,
        parent_station_matches,
        unmatched,
        average_difference_seconds
    );

    if !missed_by_reason.is_empty() {
        let mut reasons = missed_by_reason.into_iter().collect::<Vec<_>>();
        reasons.sort_unstable_by(|left, right| left.0.cmp(right.0));
        println!(
            "Miss reasons: {}",
            reasons
                .into_iter()
                .map(|(reason, count)| format!("{reason}={count}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    feed_msg
}

fn build_missed_example(
    journey: &EstimatedVehicleJourney,
    original_reason: MatchMissReason,
    reason: &str,
    stop_alignment: Option<&StopAlignmentDiagnostics>,
) -> serde_json::Value {
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
                            .and_then(|value| value.value.as_deref()),
                        "aimed_arrival_time": call.aimed_arrival_time.as_deref(),
                        "aimed_departure_time": call.aimed_departure_time.as_deref(),
                        "expected_arrival_time": call.expected_arrival_time.as_deref(),
                        "expected_departure_time": call.expected_departure_time.as_deref(),
                        "arrival_status": call.arrival_status.as_deref(),
                        "departure_status": call.departure_status.as_deref(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    json!({
        "logged_at": Utc::now().to_rfc3339(),
        "reason": reason,
        "original_reason": original_reason.as_str(),
        "stop_alignment": stop_alignment,

        "dated_vehicle_journey_ref": journey
            .dated_vehicle_journey_ref
            .as_ref()
            .and_then(|value| value.value.as_deref()),

        "line_ref": journey
            .line_ref
            .as_ref()
            .and_then(|value| value.value.as_deref()),

        "destination_ref": journey
            .destination_ref
            .as_ref()
            .and_then(|value| value.value.as_deref()),

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

fn sort_estimated_calls_by_time(journey: &mut EstimatedVehicleJourney) {
    let Some(calls) = journey.estimated_calls.as_mut() else {
        return;
    };

    // sort_by_cached_key is stable, so calls with equal timestamps, or calls with
    // no usable timestamp, retain their original relative order.
    calls
        .estimated_call
        .sort_by_cached_key(estimated_call_sort_key);
}

fn estimated_call_sort_key(call: &EstimatedCall) -> (bool, i64) {
    // Aimed times describe the static journey order and are therefore preferred.
    // Expected times are only a fallback for calls where IDFM omitted aimed times.
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
}

fn parse_siri_timestamp(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date_time| date_time.timestamp())
}

fn is_cancelled_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("CANCELLED") || status.eq_ignore_ascii_case("CANCELED")
}

#[cfg(test)]
mod tests {
    use super::{estimated_call_sort_key, is_cancelled_status};
    use crate::siri_models::{EstimatedCall, ValueWrapper};

    fn estimated_call(
        stop_id: &str,
        aimed_departure: Option<&str>,
        expected_departure: Option<&str>,
    ) -> EstimatedCall {
        EstimatedCall {
            stop_point_ref: Some(ValueWrapper {
                value: Some(stop_id.to_string()),
            }),
            aimed_arrival_time: None,
            aimed_departure_time: aimed_departure.map(|value| value.to_string()),
            expected_arrival_time: None,
            expected_departure_time: expected_departure.map(|value| value.to_string()),
            arrival_status: None,
            departure_status: None,
            arrival_platform_name: None,
            departure_platform_name: None,
        }
    }

    #[test]
    fn sorts_estimated_calls_chronologically_and_keeps_missing_times_last() {
        let mut calls = vec![
            estimated_call("late", Some("2026-07-12T10:00:00+02:00"), None),
            estimated_call("missing", None, None),
            estimated_call("early-expected", None, Some("2026-07-12T08:00:00+02:00")),
            estimated_call("middle", Some("2026-07-12T09:00:00+02:00"), None),
        ];

        calls.sort_by_cached_key(estimated_call_sort_key);

        let stop_ids = calls
            .iter()
            .map(|call| call.stop_point_ref.as_ref().unwrap().value.as_deref().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            stop_ids,
            vec!["early-expected", "middle", "late", "missing"]
        );
    }

    #[test]
    fn recognises_cancelled_siri_statuses() {
        assert!(is_cancelled_status("CANCELLED"));
        assert!(is_cancelled_status("cancelled"));
        assert!(is_cancelled_status("CANCELED"));
        assert!(!is_cancelled_status("ON_TIME"));
    }

    #[tokio::test]
    async fn rail_trip_updates_only_use_static_trip_stop_ids() {
        use std::collections::HashMap;
        use std::sync::Arc;
        use dashmap::DashMap;
        use gtfs_structures::{StopTime, Stop, Trip, Gtfs};
        use crate::state::AppState;
        use crate::state::LoadedGtfs;
        use crate::siri_models::{
            SiriResponse, Siri, ServiceDelivery, EstimatedTimetableDelivery,
            EstimatedJourneyVersionFrame, EstimatedVehicleJourney, EstimatedCalls,
            EstimatedCall, ValueWrapper,
        };

        let stop = Arc::new(Stop {
            id: "IDFM:monomodalStopPlace:12345".to_string(),
            ..Default::default()
        });

        let mut trips = HashMap::new();
        trips.insert("trip-1".to_string(), Trip {
            id: "trip-1".to_string(),
            service_id: "service-1".to_string(),
            route_id: "IDFM:route-1".to_string(),
            stop_times: vec![StopTime {
                stop: stop.clone(),
                arrival_time: None,
                departure_time: None,
                ..Default::default()
            }],
            ..Default::default()
        });

        let mut stops = HashMap::new();
        stops.insert("IDFM:monomodalStopPlace:12345".to_string(), Arc::new(Stop {
            id: "IDFM:monomodalStopPlace:12345".to_string(),
            ..Default::default()
        }));
        stops.insert("IDFM:monomodalStopPlace:47874".to_string(), Arc::new(Stop {
            id: "IDFM:monomodalStopPlace:47874".to_string(),
            ..Default::default()
        }));

        let gtfs = Gtfs {
            trips,
            stops,
            ..Default::default()
        };

        let loaded = Arc::new(LoadedGtfs::new(gtfs));
        let state = Arc::new(AppState {
            gtfs: tokio::sync::RwLock::new(Some(loaded)),
            gtfs_rt_feed: tokio::sync::RwLock::new(Arc::new(Default::default())),
            trip_platforms: DashMap::new(),
            vehicle_assignments: DashMap::new(),
        });

        let journey = EstimatedVehicleJourney {
            dated_vehicle_journey_ref: Some(ValueWrapper {
                value: Some("SIRI::trip-1".to_string()),
            }),
            line_ref: Some(ValueWrapper {
                value: Some("STIF:Line::route-1:".to_string()),
            }),
            operator_ref: None,
            direction_ref: None,
            direction_name: None,
            destination_ref: None,
            journey_note: None,
            estimated_calls: Some(EstimatedCalls {
                estimated_call: vec![
                    EstimatedCall {
                        stop_point_ref: Some(ValueWrapper {
                            value: Some("STIF:StopPoint:Q:47874:".to_string()),
                        }),
                        aimed_arrival_time: None,
                        aimed_departure_time: None,
                        expected_arrival_time: None,
                        expected_departure_time: None,
                        arrival_status: None,
                        departure_status: None,
                        arrival_platform_name: None,
                        departure_platform_name: None,
                    }
                ],
            }),
        };

        let siri = SiriResponse {
            siri: Siri {
                service_delivery: ServiceDelivery {
                    estimated_timetable_delivery: vec![EstimatedTimetableDelivery {
                        estimated_journey_version_frame: vec![EstimatedJourneyVersionFrame {
                            estimated_vehicle_journey: vec![journey],
                        }],
                    }],
                },
            },
        };

        let feed = super::process_siri(state.clone(), siri).await;
        
        assert_eq!(feed.entity.len(), 1);
        let update = feed.entity[0].trip_update.as_ref().unwrap();
        assert!(update.stop_time_update.is_empty());
    }
}
