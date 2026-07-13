use crate::matching::MatchKind;
use crate::siri_models::{EstimatedVehicleJourney, SiriResponse};
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
    let mut scored_matches = 0_u64;
    let mut total_mean_difference_seconds = 0_i128;

    // Clone the immutable GTFS snapshot, then release the RwLock. A daily GTFS
    // refresh can replace the snapshot while this SIRI payload is processed.
    let loaded_gtfs = {
        let lock = state.gtfs.read().await;
        lock.clone()
    };

    let Some(loaded_gtfs) = loaded_gtfs else {
        return;
    };

    let gtfs = &loaded_gtfs.gtfs;
    let match_index = &loaded_gtfs.match_index;

    for delivery in siri.siri.service_delivery.estimated_timetable_delivery {
        for frame in delivery.estimated_journey_version_frame {
            for journey in frame.estimated_vehicle_journey {
                let Some(matched) = match_index.match_journey(&journey, gtfs) else {
                    unmatched += 1;

                    if missed_examples.len() < MAX_MISSED_EXAMPLES {
                        missed_examples.push(build_missed_example(&journey));
                    }

                    continue;
                };

                match matched.kind {
                    MatchKind::ExactId => exact_matches += 1,
                    MatchKind::DirectionAndTime => sequence_matches += 1,
                }

                if let Some(difference) = matched.mean_abs_difference_seconds {
                    scored_matches += 1;
                    total_mean_difference_seconds += i128::from(difference);
                }

                let trip_id = matched.trip_id;
                let mut trip_update = TripUpdate::default();
                let mut trip_desc = TripDescriptor::default();
                trip_desc.trip_id = Some(trip_id.clone());
                trip_desc.start_date = matched
                    .service_date
                    .map(|date| date.format("%Y%m%d").to_string());
                trip_update.trip = trip_desc;

                let mut platforms = Vec::new();

                if let Some(calls) = &journey.estimated_calls {
                    for call in &calls.estimated_call {
                        let mut stu = StopTimeUpdate::default();

                        if call
                            .arrival_status
                            .as_deref()
                            .is_some_and(is_cancelled_status)
                            || call.departure_status.as_deref().is_some_and(is_cancelled_status)
                        {
                            stu.schedule_relationship = Some(StopTimeScheduleRelationship::Skipped as i32);
                        }

                        if let Some(stop_ref) = &call.stop_point_ref {
                            // Extract GTFS stop_id from STIF:StopPoint:Q:30785:
                            // Result should be IDFM:30785.
                            if let Some(stop_id_num) =
                                stop_ref.value.rsplit(':').find(|part| !part.is_empty())
                            {
                                let gtfs_stop_id = format!("IDFM:{stop_id_num}");
                                stu.stop_id = Some(gtfs_stop_id.clone());

                                let platform_name = call
                                    .departure_platform_name
                                    .as_ref()
                                    .or(call.arrival_platform_name.as_ref())
                                    .map(|value| value.value.as_str());

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

    let average_difference_seconds = (scored_matches > 0).then(|| {
        total_mean_difference_seconds / i128::from(scored_matches)
    });

    println!(
        "Matched: Exact = {}, Direction/time = {}, Missed = {}, Average expected-time difference = {:?}s",
        exact_matches, sequence_matches, unmatched, average_difference_seconds
    );

    let mut lock = state.gtfs_rt_feed.write().await;
    *lock = feed_msg;
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

        "dated_vehicle_journey_ref": journey
            .dated_vehicle_journey_ref
            .as_ref()
            .map(|value| value.value.as_str()),

        "line_ref": journey
            .line_ref
            .as_ref()
            .map(|value| value.value.as_str()),

        "destination_ref": journey
            .destination_ref
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

fn is_cancelled_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("CANCELLED") || status.eq_ignore_ascii_case("CANCELED")
}

#[cfg(test)]
mod tests {
    use super::is_cancelled_status;

    #[test]
    fn recognises_cancelled_siri_statuses() {
        assert!(is_cancelled_status("CANCELLED"));
        assert!(is_cancelled_status("cancelled"));
        assert!(is_cancelled_status("CANCELED"));
        assert!(!is_cancelled_status("ON_TIME"));
    }
}
