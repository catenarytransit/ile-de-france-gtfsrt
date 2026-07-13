//! Tolerant IDFM SIRI -> GTFS matching.
//!
//! The primary matcher intentionally remains strict. This module is the recovery path for
//! feeds where EstimatedCalls are duplicated, contain aliases, are not in stop order, or
//! represent point observations from RATP-SIV / MeC_Bus_PC rather than one clean trajectory.

use crate::matching::{JourneyMatch, MatchKind};
use crate::siri_models::{EstimatedCall, EstimatedVehicleJourney};
use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::{Europe::Paris, Tz};
use gtfs_structures::{Exception, Gtfs, Trip};
use std::collections::{HashMap, HashSet};

const CACHE_MAX_MEAN_DIFFERENCE_SECONDS: i64 = 10 * 60;

#[derive(Debug, Default)]
pub struct StopAliasIndex {
    by_suffix: HashMap<String, Vec<String>>,
}

impl StopAliasIndex {
    pub fn build(gtfs: &Gtfs) -> Self {
        let mut by_suffix = HashMap::<String, Vec<String>>::new();
        for stop_id in gtfs.stops.keys() {
            if let Some(suffix) = stop_id.rsplit(':').next() {
                by_suffix
                    .entry(suffix.to_owned())
                    .or_default()
                    .push(stop_id.clone());
            }
        }
        for aliases in by_suffix.values_mut() {
            aliases.sort_unstable();
            aliases.dedup();
        }
        Self { by_suffix }
    }

    pub fn resolve(&self, value: &str) -> Vec<String> {
        let Some(suffix) = value.rsplit(':').find(|part| !part.is_empty()) else {
            return Vec::new();
        };
        self.by_suffix.get(suffix).cloned().unwrap_or_default()
    }

    pub fn first(&self, value: &str) -> Option<String> {
        self.resolve(value).into_iter().next()
    }
}

#[derive(Debug, Clone)]
struct ResolvedCall {
    original_index: usize,
    aliases: Vec<String>,
    observed: i64,
}

#[derive(Debug)]
struct AdaptiveScore {
    trip_id: String,
    service_date: NaiveDate,
    matched_calls: usize,
    total_difference: i128,
    mean_difference: i64,
    max_difference: i64,
    stop_indices: Vec<usize>,
    used_parent_station_match: bool,
}

impl AdaptiveScore {
    fn is_better_than(&self, other: &Self) -> bool {
        self.matched_calls > other.matched_calls
            || (self.matched_calls == other.matched_calls
                && self.mean_difference < other.mean_difference)
            || (self.matched_calls == other.matched_calls
                && self.mean_difference == other.mean_difference
                && self.max_difference < other.max_difference)
            || (self.matched_calls == other.matched_calls
                && self.mean_difference == other.mean_difference
                && self.max_difference == other.max_difference
                && self.total_difference < other.total_difference)
    }
}

pub fn match_journey(
    journey: &EstimatedVehicleJourney,
    gtfs: &Gtfs,
    aliases: &StopAliasIndex,
) -> Option<JourneyMatch> {
    let route_id = journey
        .line_ref
        .as_ref()
        .and_then(|value| value.value.as_deref().and_then(extract_idfm_id))?;
    let call_count = journey
        .estimated_calls
        .as_ref()
        .map_or(0, |calls| calls.estimated_call.len());
    let calls = resolve_calls(journey, aliases);
    if calls.is_empty() {
        return None;
    }

    let operator_trajectory = journey
        .operator_ref
        .as_ref()
        .is_some_and(|operator| {
            operator
                .value
                .as_deref()
                .is_some_and(is_point_observation_operator)
        });
    let destination_aliases = journey
        .destination_ref
        .as_ref()
        .and_then(|value| value.value.as_deref().map(|value| aliases.resolve(value)))
        .unwrap_or_default();
    let timezone = timezone_for_route(gtfs, &route_id);
    let service_dates = candidate_service_dates(&calls, timezone);
    let route_type = gtfs.routes.get(&route_id).map(|route| format!("{:?}", route.route_type));
    let (early_seconds, late_seconds) = time_window(route_type.as_deref(), operator_trajectory);

    // Match destination in progressively weaker passes. This mirrors the Java fallback:
    // terminus first, destination anywhere second, then no destination restriction.
    for destination_mode in 0..=2 {
        let mut best: Option<AdaptiveScore> = None;

        for trip in gtfs.trips.values().filter(|trip| trip.route_id == route_id) {
            if !destination_matches(trip, &destination_aliases, gtfs, destination_mode) {
                continue;
            }

            for service_date in &service_dates {
                if !service_runs_on(gtfs, &trip.service_id, *service_date) {
                    continue;
                }

                let score = if operator_trajectory {
                    score_operator_trajectory(
                        trip,
                        &calls,
                        *service_date,
                        timezone,
                        gtfs,
                        early_seconds,
                        late_seconds,
                        call_count,
                    )
                } else {
                    score_unordered_calls(
                        trip,
                        &calls,
                        *service_date,
                        timezone,
                        gtfs,
                        early_seconds,
                        late_seconds,
                        call_count,
                    )
                };

                let Some(score) = score else { continue };
                if best.as_ref().is_none_or(|current| score.is_better_than(current)) {
                    best = Some(score);
                }
            }
        }

        if let Some(score) = best {
            return Some(to_journey_match(score));
        }
    }

    None
}

pub fn match_cached_assignment(
    journey: &EstimatedVehicleJourney,
    gtfs: &Gtfs,
    aliases: &StopAliasIndex,
    trip_id: &str,
) -> Option<JourneyMatch> {
    let trip = gtfs.trips.get(trip_id)?;
    let route_id = journey
        .line_ref
        .as_ref()
        .and_then(|value| value.value.as_deref().and_then(extract_idfm_id))?;
    if trip.route_id != route_id {
        return None;
    }

    let call_count = journey
        .estimated_calls
        .as_ref()
        .map_or(0, |calls| calls.estimated_call.len());
    let calls = resolve_calls(journey, aliases);
    let timezone = timezone_for_route(gtfs, &route_id);
    let route_type = gtfs.routes.get(&route_id).map(|route| format!("{:?}", route.route_type));
    let (early, late) = time_window(route_type.as_deref(), false);

    let mut best: Option<AdaptiveScore> = None;
    for service_date in candidate_service_dates(&calls, timezone) {
        if !service_runs_on(gtfs, &trip.service_id, service_date) {
            continue;
        }
        let Some(score) = score_unordered_calls(
            trip,
            &calls,
            service_date,
            timezone,
            gtfs,
            early,
            late,
            call_count,
        ) else {
            continue;
        };
        if score.mean_difference <= CACHE_MAX_MEAN_DIFFERENCE_SECONDS
            && best.as_ref().is_none_or(|current| score.is_better_than(current))
        {
            best = Some(score);
        }
    }
    best.map(to_journey_match)
}

fn to_journey_match(score: AdaptiveScore) -> JourneyMatch {
    JourneyMatch {
        trip_id: score.trip_id,
        service_date: Some(score.service_date),
        kind: MatchKind::DirectionAndTime,
        mean_abs_difference_seconds: Some(score.mean_difference),
        stop_indices: score.stop_indices,
        used_parent_station_match: score.used_parent_station_match,
    }
}

fn resolve_calls(
    journey: &EstimatedVehicleJourney,
    aliases: &StopAliasIndex,
) -> Vec<ResolvedCall> {
    journey
        .estimated_calls
        .as_ref()
        .into_iter()
        .flat_map(|calls| calls.estimated_call.iter().enumerate())
        .filter_map(|(index, call)| resolve_call(index, call, aliases))
        .collect()
}

fn resolve_call(
    original_index: usize,
    call: &EstimatedCall,
    aliases: &StopAliasIndex,
) -> Option<ResolvedCall> {
    let reference = call.stop_point_ref.as_ref()?;
    let stop_aliases = reference
        .value
        .as_deref()
        .map(|value| aliases.resolve(value))?;
    if stop_aliases.is_empty() {
        return None;
    }
    // Expected-first is intentional: the Java implementation sorts and matches using the
    // current prediction, while aimed time is a fallback when no expected timestamp exists.
    let observed = [
        call.expected_arrival_time.as_deref(),
        call.expected_departure_time.as_deref(),
        call.aimed_arrival_time.as_deref(),
        call.aimed_departure_time.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(parse_timestamp)?;
    Some(ResolvedCall {
        original_index,
        aliases: stop_aliases,
        observed,
    })
}

fn score_unordered_calls(
    trip: &Trip,
    calls: &[ResolvedCall],
    service_date: NaiveDate,
    timezone: Tz,
    gtfs: &Gtfs,
    early_seconds: i64,
    late_seconds: i64,
    call_count: usize,
) -> Option<AdaptiveScore> {
    let required = 2_usize.max(calls.len().div_ceil(2));
    let mut matched = 0_usize;
    let mut total = 0_i128;
    let mut max_difference = 0_i64;
    let mut indices = vec![usize::MAX; call_count];
    let mut parent_match = false;

    for call in calls {
        let mut best: Option<(usize, i64, bool)> = None;
        for (index, stop_time) in trip.stop_times.iter().enumerate() {
            let (matches, used_parent) = call_matches_stop(call, &stop_time.stop.id, gtfs);
            if !matches {
                continue;
            }
            let Some(seconds) = stop_time.departure_time.or(stop_time.arrival_time) else {
                continue;
            };
            let Some(difference) = signed_time_difference(call.observed, service_date, seconds, timezone) else {
                continue;
            };
            if difference < early_seconds || difference > late_seconds {
                continue;
            }
            let absolute = difference.abs();
            if best.is_none_or(|(_, current, _)| absolute < current) {
                best = Some((index, absolute, used_parent));
            }
        }

        if let Some((index, difference, used_parent)) = best {
            matched += 1;
            total += i128::from(difference);
            max_difference = max_difference.max(difference);
            indices[call.original_index] = index;
            parent_match |= used_parent;
        }
    }

    if matched < required {
        return None;
    }
    Some(AdaptiveScore {
        trip_id: trip.id.clone(),
        service_date,
        matched_calls: matched,
        total_difference: total,
        mean_difference: (total / matched as i128) as i64,
        max_difference,
        stop_indices: indices,
        used_parent_station_match: parent_match,
    })
}

fn score_operator_trajectory(
    trip: &Trip,
    calls: &[ResolvedCall],
    service_date: NaiveDate,
    timezone: Tz,
    gtfs: &Gtfs,
    early_seconds: i64,
    late_seconds: i64,
    call_count: usize,
) -> Option<AdaptiveScore> {
    // RATP-SIV and MeC_Bus_PC may expose observations rather than a clean ordered
    // trajectory. Match overlapping points and rank by delay consistency (MAD), not order.
    let mut delays = Vec::<(usize, i64, bool)>::new();
    let mut output_indices = vec![usize::MAX; call_count];
    for call in calls {
        let mut best: Option<(usize, i64, bool)> = None;
        for (stop_index, stop_time) in trip.stop_times.iter().enumerate() {
            let (matches, used_parent) = call_matches_stop(call, &stop_time.stop.id, gtfs);
            if !matches {
                continue;
            }
            let Some(seconds) = stop_time.departure_time.or(stop_time.arrival_time) else {
                continue;
            };
            let Some(delay) = signed_time_difference(call.observed, service_date, seconds, timezone) else {
                continue;
            };
            if delay < early_seconds || delay > late_seconds {
                continue;
            }
            if best.is_none_or(|(_, current, _)| delay.abs() < current.abs()) {
                best = Some((stop_index, delay, used_parent));
            }
        }
        if let Some((stop_index, delay, used_parent)) = best {
            output_indices[call.original_index] = stop_index;
            delays.push((stop_index, delay, used_parent));
        }
    }
    if delays.is_empty() {
        return None;
    }
    let mut sorted = delays.iter().map(|(_, delay, _)| *delay).collect::<Vec<_>>();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let mut deviations = sorted
        .iter()
        .map(|delay| (*delay - median).abs())
        .collect::<Vec<_>>();
    deviations.sort_unstable();
    let mad = deviations[deviations.len() / 2];
    let mad_limit = if trip.route_id.contains("Metro") { 300 } else { 480 };
    if mad > mad_limit {
        return None;
    }
    let total = delays.iter().map(|(_, delay, _)| i128::from(delay.abs())).sum();
    let max_difference = delays.iter().map(|(_, delay, _)| delay.abs()).max().unwrap_or(0);
    Some(AdaptiveScore {
        trip_id: trip.id.clone(),
        service_date,
        matched_calls: delays.len(),
        total_difference: total,
        mean_difference: (total / delays.len() as i128) as i64,
        max_difference,
        stop_indices: output_indices,
        used_parent_station_match: delays.iter().any(|(_, _, parent)| *parent),
    })
}

fn call_matches_stop(call: &ResolvedCall, stop_id: &str, gtfs: &Gtfs) -> (bool, bool) {
    if call.aliases.iter().any(|alias| alias == stop_id) {
        return (true, false);
    }
    let stop_parent = top_parent(stop_id, gtfs);
    let parent_match = call
        .aliases
        .iter()
        .any(|alias| top_parent(alias, gtfs) == stop_parent);
    (parent_match, parent_match)
}

fn top_parent(stop_id: &str, gtfs: &Gtfs) -> String {
    let mut current = stop_id.to_string();
    let mut visited = HashSet::new();
    while visited.insert(current.clone()) {
        let Some(parent) = gtfs
            .stops
            .get(&current)
            .and_then(|stop| stop.parent_station.as_ref())
        else {
            break;
        };
        if parent == &current {
            break;
        }
        current = parent.clone();
    }
    current
}

fn destination_matches(
    trip: &Trip,
    destination_aliases: &[String],
    gtfs: &Gtfs,
    mode: usize,
) -> bool {
    if destination_aliases.is_empty() || mode == 2 {
        return true;
    }
    let stop_matches = |stop_id: &str| {
        destination_aliases.iter().any(|alias| {
            alias == stop_id || top_parent(alias, gtfs) == top_parent(stop_id, gtfs)
        })
    };
    match mode {
        0 => trip.stop_times.last().is_some_and(|stop| stop_matches(&stop.stop.id)),
        1 => trip.stop_times.iter().any(|stop| stop_matches(&stop.stop.id)),
        _ => true,
    }
}

fn time_window(route_type: Option<&str>, operator_trajectory: bool) -> (i64, i64) {
    if operator_trajectory {
        return (-20 * 60, 30 * 60);
    }
    match route_type {
        Some("Tramway" | "Subway" | "Metro" | "Tram") => (-5 * 60, 15 * 60),
        Some("Bus") => (-5 * 60, 30 * 60),
        _ => (-5 * 60, 20 * 60),
    }
}

fn is_point_observation_operator(value: &str) -> bool {
    value.starts_with("RATP-SIV:") || value.starts_with("MeC_Bus_PC:Operator:")
}

fn extract_idfm_id(value: &str) -> Option<String> {
    let id = value.rsplit(':').find(|part| !part.is_empty())?;
    Some(format!("IDFM:{id}"))
}

fn parse_timestamp(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value).ok().map(|value| value.timestamp())
}

fn timezone_for_route(gtfs: &Gtfs, route_id: &str) -> Tz {
    let route = gtfs.routes.get(route_id);
    let agency_id = route.and_then(|route| route.agency_id.as_ref());
    gtfs.agencies
        .iter()
        .filter(|agency| agency_id.is_none() || agency.id.as_ref() == agency_id)
        .find_map(|agency| agency.timezone.parse::<Tz>().ok())
        .unwrap_or(Paris)
}

fn candidate_service_dates(calls: &[ResolvedCall], timezone: Tz) -> Vec<NaiveDate> {
    let local_date = calls
        .iter()
        .map(|call| call.observed)
        .min()
        .and_then(|timestamp| Utc.timestamp_opt(timestamp, 0).single())
        .map(|timestamp| timestamp.with_timezone(&timezone).date_naive())
        .unwrap_or_else(|| Utc::now().with_timezone(&timezone).date_naive());
    (0..=2)
        .filter_map(|days| local_date.checked_sub_signed(Duration::days(days)))
        .collect()
}

fn service_runs_on(gtfs: &Gtfs, service_id: &str, date: NaiveDate) -> bool {
    if let Some(exception) = gtfs
        .calendar_dates
        .get(service_id)
        .and_then(|dates| dates.iter().find(|exception| exception.date == date))
    {
        return exception.exception_type == Exception::Added;
    }
    gtfs.calendar.get(service_id).is_some_and(|calendar| {
        calendar.start_date <= date
            && date <= calendar.end_date
            && match date.weekday() {
                chrono::Weekday::Mon => calendar.monday,
                chrono::Weekday::Tue => calendar.tuesday,
                chrono::Weekday::Wed => calendar.wednesday,
                chrono::Weekday::Thu => calendar.thursday,
                chrono::Weekday::Fri => calendar.friday,
                chrono::Weekday::Sat => calendar.saturday,
                chrono::Weekday::Sun => calendar.sunday,
            }
    })
}

fn signed_time_difference(
    observed: i64,
    service_date: NaiveDate,
    scheduled_seconds: u32,
    timezone: Tz,
) -> Option<i64> {
    scheduled_timestamps(service_date, scheduled_seconds, timezone)
        .into_iter()
        .map(|scheduled| observed - scheduled)
        .min_by_key(|difference| difference.abs())
}

fn scheduled_timestamps(service_date: NaiveDate, seconds: u32, timezone: Tz) -> Vec<i64> {
    let day_offset = i64::from(seconds / 86_400);
    let seconds_in_day = seconds % 86_400;
    let Some(date) = service_date.checked_add_signed(Duration::days(day_offset)) else {
        return Vec::new();
    };
    let Some(time) = NaiveTime::from_num_seconds_from_midnight_opt(seconds_in_day, 0) else {
        return Vec::new();
    };
    match timezone.from_local_datetime(&date.and_time(time)) {
        LocalResult::Single(value) => vec![value.timestamp()],
        LocalResult::Ambiguous(first, second) => vec![first.timestamp(), second.timestamp()],
        LocalResult::None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_point_observation_operators() {
        assert!(is_point_observation_operator("RATP-SIV:Operator:foo"));
        assert!(is_point_observation_operator("MeC_Bus_PC:Operator:foo"));
        assert!(!is_point_observation_operator("SNCF:Operator:foo"));
    }

    #[test]
    fn route_windows_are_mode_specific() {
        assert_eq!(time_window(Some("Subway"), false), (-300, 900));
        assert_eq!(time_window(Some("Bus"), false), (-300, 1800));
        assert_eq!(time_window(Some("Rail"), false), (-300, 1200));
    }
}
