use crate::matching_diagnostics::{
    StopAlignmentDiagnostics, diagnose_stop_alignment as analyze_stop_alignment,
};
use crate::siri_models::EstimatedVehicleJourney;
use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::{Europe::Paris, Tz};
use gtfs_structures::{Exception, Gtfs, Trip};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    ExactId,
    DirectionAndTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatchMissReason {
    NoObservedCalls,
    NoUsableTimes,
    MissingLineRef,
    RouteNotFound,
    NoStopAlignment,
    InactiveServiceDate,
    NoTimeComparisons,
}

impl MatchMissReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoObservedCalls => "no_observed_calls",
            Self::NoUsableTimes => "no_usable_times",
            Self::MissingLineRef => "missing_or_invalid_line_ref",
            Self::RouteNotFound => "route_not_found",
            Self::NoStopAlignment => "no_stop_alignment",
            Self::InactiveServiceDate => "inactive_service_date",
            Self::NoTimeComparisons => "no_time_comparisons",
        }
    }
}

#[derive(Debug, Clone)]
pub struct JourneyMatch {
    pub trip_id: String,
    pub service_date: Option<NaiveDate>,
    pub kind: MatchKind,
    pub mean_abs_difference_seconds: Option<i64>,
    /// GTFS stop_time indices corresponding to each SIRI call that had a StopPointRef.
    pub stop_indices: Vec<usize>,
    /// True when exact stop IDs did not align and station hierarchy was used.
    pub used_parent_station_match: bool,
}

#[derive(Debug)]
struct DirectionPattern {
    stop_ids: Vec<String>,
    destination_stop_id: String,
    trip_ids: Vec<String>,
}

#[derive(Debug, Default)]
pub struct GtfsMatchIndex {
    directions: HashMap<String, Vec<DirectionPattern>>,
    exact_trip_ids: HashMap<String, Vec<String>>,
    trips_by_route: HashMap<String, Vec<String>>,
    route_timezones: HashMap<String, Tz>,
    service_exceptions: HashMap<String, HashMap<NaiveDate, bool>>,
    canonical_stop_ids: HashMap<String, String>,
    stop_ids_by_suffix: HashMap<String, Vec<String>>,
}

#[derive(Debug)]
struct ObservedCall {
    stop_aliases: Vec<String>,
    aimed_arrival: Option<i64>,
    aimed_departure: Option<i64>,
    expected_arrival: Option<i64>,
    expected_departure: Option<i64>,
}

impl ObservedCall {
    fn matching_arrival(&self) -> Option<i64> {
        self.aimed_arrival.or(self.expected_arrival)
    }

    fn matching_departure(&self) -> Option<i64> {
        self.aimed_departure.or(self.expected_departure)
    }
}

#[derive(Debug)]
struct CandidateScore {
    trip_id: String,
    service_date: NaiveDate,
    mean_abs_difference_seconds: i64,
    max_abs_difference_seconds: i64,
    comparisons: usize,
    stop_indices: Vec<usize>,
    used_parent_station_match: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopMatchMode {
    Exact,
    ParentStation,
}

impl CandidateScore {
    fn is_better_than(&self, other: &Self) -> bool {
        self.mean_abs_difference_seconds < other.mean_abs_difference_seconds
            || (self.mean_abs_difference_seconds == other.mean_abs_difference_seconds
                && self.max_abs_difference_seconds < other.max_abs_difference_seconds)
            || (self.mean_abs_difference_seconds == other.mean_abs_difference_seconds
                && self.max_abs_difference_seconds == other.max_abs_difference_seconds
                && self.comparisons > other.comparisons)
            || (self.mean_abs_difference_seconds == other.mean_abs_difference_seconds
                && self.max_abs_difference_seconds == other.max_abs_difference_seconds
                && self.comparisons == other.comparisons
                && self.service_date > other.service_date)
            || (self.mean_abs_difference_seconds == other.mean_abs_difference_seconds
                && self.max_abs_difference_seconds == other.max_abs_difference_seconds
                && self.comparisons == other.comparisons
                && self.service_date == other.service_date
                && self.trip_id < other.trip_id)
    }
}

impl GtfsMatchIndex {
    pub fn build(gtfs: &Gtfs) -> Self {
        let default_timezone = gtfs
            .agencies
            .iter()
            .find_map(|agency| agency.timezone.parse::<Tz>().ok())
            .unwrap_or(Paris);

        let agency_timezones = gtfs
            .agencies
            .iter()
            .filter_map(|agency| {
                Some((
                    agency.id.as_ref()?.clone(),
                    agency.timezone.parse::<Tz>().ok()?,
                ))
            })
            .collect::<HashMap<_, _>>();

        let route_timezones = gtfs
            .routes
            .iter()
            .map(|(route_id, route)| {
                let timezone = route
                    .agency_id
                    .as_ref()
                    .and_then(|agency_id| agency_timezones.get(agency_id))
                    .copied()
                    .unwrap_or(default_timezone);

                (route_id.clone(), timezone)
            })
            .collect::<HashMap<_, _>>();

        let canonical_stop_ids = build_canonical_stop_ids(gtfs);

        let mut stop_ids_by_suffix = HashMap::<String, Vec<String>>::new();
        for stop_id in gtfs.stops.keys() {
            if let Some(suffix) = stop_id.rsplit(':').next() {
                stop_ids_by_suffix
                    .entry(suffix.to_owned())
                    .or_default()
                    .push(stop_id.clone());
            }
        }
        for ids in stop_ids_by_suffix.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }

        let mut grouped_directions = HashMap::<(String, Vec<String>), Vec<String>>::new();
        let mut exact_trip_ids = HashMap::<String, Vec<String>>::new();
        let mut trips_by_route = HashMap::<String, Vec<String>>::new();

        for (trip_id, trip) in &gtfs.trips {
            trips_by_route
                .entry(trip.route_id.clone())
                .or_default()
                .push(trip_id.clone());

            if !trip.stop_times.is_empty() {
                let stop_ids = trip
                    .stop_times
                    .iter()
                    .map(|stop_time| stop_time.stop.id.clone())
                    .collect::<Vec<_>>();

                grouped_directions
                    .entry((trip.route_id.clone(), stop_ids))
                    .or_default()
                    .push(trip_id.clone());
            }

            insert_exact_key(&mut exact_trip_ids, trip_id, trip_id);

            if let Some(suffix) = trip_id.rsplit(':').next() {
                if suffix != trip_id {
                    insert_exact_key(&mut exact_trip_ids, suffix, trip_id);
                }
            }
        }

        for ids in trips_by_route.values_mut() {
            ids.sort_unstable();
        }

        let mut directions = HashMap::<String, Vec<DirectionPattern>>::new();

        for ((route_id, stop_ids), mut trip_ids) in grouped_directions {
            let Some(destination_stop_id) = stop_ids.last().cloned() else {
                continue;
            };

            trip_ids.sort_unstable();

            directions
                .entry(route_id)
                .or_default()
                .push(DirectionPattern {
                    stop_ids,
                    destination_stop_id,
                    trip_ids,
                });
        }

        for patterns in directions.values_mut() {
            patterns.sort_unstable_by(|left, right| {
                left.destination_stop_id
                    .cmp(&right.destination_stop_id)
                    .then_with(|| left.stop_ids.cmp(&right.stop_ids))
            });
        }

        let service_exceptions = gtfs
            .calendar_dates
            .iter()
            .map(|(service_id, dates)| {
                let dates = dates
                    .iter()
                    .map(|date| (date.date, date.exception_type == Exception::Added))
                    .collect::<HashMap<_, _>>();

                (service_id.clone(), dates)
            })
            .collect::<HashMap<_, _>>();

        Self {
            directions,
            exact_trip_ids,
            trips_by_route,
            route_timezones,
            service_exceptions,
            canonical_stop_ids,
            stop_ids_by_suffix,
        }
    }

    pub fn resolve_siri_stop_ids(&self, value: &str) -> Vec<String> {
        let Some(suffix) = siri_reference_suffix(value) else {
            return Vec::new();
        };

        self.stop_ids_by_suffix
            .get(suffix)
            .cloned()
            .unwrap_or_default()
    }

    pub fn trip_ids_for_route(&self, route_id: &str) -> &[String] {
        self.trips_by_route
            .get(route_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn direction_count(&self) -> usize {
        self.directions.values().map(Vec::len).sum()
    }

    /// Explain why the ordered stop sequence could not be aligned.
    ///
    /// This is intentionally called only after normal exact and parent-station
    /// matching fail. The LCS work performed by the diagnostic path is more
    /// expensive than the normal matcher and must not be part of every match.
    pub fn diagnose_stop_alignment(
        &self,
        journey: &EstimatedVehicleJourney,
    ) -> Option<StopAlignmentDiagnostics> {
        let observed_calls = self.observed_calls(journey);
        if observed_calls.is_empty() {
            return None;
        }

        let target_route_id = journey
            .line_ref
            .as_ref()
            .and_then(|reference| reference.value.as_deref().and_then(extract_idfm_id))?;
        let patterns = self.directions.get(&target_route_id)?;
        let target_destination = journey
            .destination_ref
            .as_ref()
            .and_then(|reference| reference.value.as_deref())
            .map(|dest_ref| self.resolve_siri_stop_ids(dest_ref))
            .unwrap_or_default();

        let mut candidate_patterns = patterns.iter().collect::<Vec<_>>();
        let mut destination_filter_applied = false;

        if !target_destination.is_empty() {
            let exact = patterns
                .iter()
                .filter(|pattern| target_destination.contains(&pattern.destination_stop_id))
                .collect::<Vec<_>>();

            if !exact.is_empty() {
                destination_filter_applied = exact.len() < patterns.len();
                candidate_patterns = exact;
            } else {
                let parent = patterns
                    .iter()
                    .filter(|pattern| {
                        target_destination.iter().any(|dest| {
                            self.stops_equivalent(&pattern.destination_stop_id, dest)
                        })
                    })
                    .collect::<Vec<_>>();

                if !parent.is_empty() {
                    destination_filter_applied = parent.len() < patterns.len();
                    candidate_patterns = parent;
                }
            }
        }

        let observed_stop_ids = observed_calls
            .iter()
            .map(|call| call.stop_aliases.first().cloned().unwrap_or_default())
            .collect::<Vec<_>>();
        let candidate_stop_patterns = candidate_patterns
            .iter()
            .map(|pattern| pattern.stop_ids.as_slice())
            .collect::<Vec<_>>();
        let all_stop_patterns = patterns
            .iter()
            .map(|pattern| pattern.stop_ids.as_slice())
            .collect::<Vec<_>>();

        Some(analyze_stop_alignment(
            &observed_stop_ids,
            &candidate_stop_patterns,
            &all_stop_patterns,
            &self.canonical_stop_ids,
            destination_filter_applied,
        ))
    }

    pub fn match_journey(
        &self,
        journey: &EstimatedVehicleJourney,
        gtfs: &Gtfs,
    ) -> Result<JourneyMatch, MatchMissReason> {
        let observed_calls = self.observed_calls(journey);

        if let Some(exact_match) = self.match_exact_id(journey, gtfs, &observed_calls) {
            return Ok(exact_match);
        }

        self.match_direction_and_time(journey, gtfs, &observed_calls)
    }

    fn match_exact_id(
        &self,
        journey: &EstimatedVehicleJourney,
        gtfs: &Gtfs,
        observed_calls: &[ObservedCall],
    ) -> Option<JourneyMatch> {
        let exact_key = journey
            .dated_vehicle_journey_ref
            .as_ref()
            .and_then(|reference| reference.value.as_deref().and_then(extract_siri_trip_key))?;
        let indexed_trip_ids = self.exact_trip_ids.get(exact_key)?;
        let target_route_id = journey
            .line_ref
            .as_ref()
            .and_then(|reference| reference.value.as_deref().and_then(extract_idfm_id));
        let target_destination = journey
            .destination_ref
            .as_ref()
            .and_then(|reference| reference.value.as_deref())
            .map(|dest_ref| self.resolve_siri_stop_ids(dest_ref))
            .unwrap_or_default();

        let mut candidates = indexed_trip_ids
            .iter()
            .filter(|trip_id| {
                target_route_id.as_ref().is_none_or(|route_id| {
                    gtfs.trips
                        .get(*trip_id)
                        .is_some_and(|trip| trip.route_id == *route_id)
                })
            })
            .collect::<Vec<_>>();

        // When LineRef is present, never accept an exact-ID candidate from another route.
        if candidates.is_empty() {
            return None;
        }

        if !target_destination.is_empty() {
            let destination_candidates = candidates
                .iter()
                .copied()
                .filter(|trip_id| {
                    gtfs.trips
                        .get(*trip_id)
                        .and_then(|trip| trip.stop_times.last())
                        .is_some_and(|stop_time| {
                            target_destination.iter().any(|dest| {
                                self.stops_equivalent(&stop_time.stop.id, dest)
                            })
                        })
                })
                .collect::<Vec<_>>();

            if !destination_candidates.is_empty() {
                candidates = destination_candidates;
            }
        }

        let mut best_score: Option<CandidateScore> = None;

        for trip_id in &candidates {
            let Some(trip) = gtfs.trips.get(*trip_id) else {
                continue;
            };
            let timezone = self.timezone_for_route(&trip.route_id);

            if let Some(score) = self.score_trip(trip, observed_calls, timezone, gtfs) {
                if best_score
                    .as_ref()
                    .is_none_or(|best| score.is_better_than(best))
                {
                    best_score = Some(score);
                }
            }
        }

        if let Some(score) = best_score {
            return Some(JourneyMatch {
                trip_id: score.trip_id,
                service_date: Some(score.service_date),
                kind: MatchKind::ExactId,
                mean_abs_difference_seconds: Some(score.mean_abs_difference_seconds),
                stop_indices: score.stop_indices,
                used_parent_station_match: score.used_parent_station_match,
            });
        }

        let trip_id = (*candidates.first()?).clone();
        let trip = gtfs.trips.get(&trip_id)?;
        let timezone = self.timezone_for_route(&trip.route_id);
        let service_date = self.infer_service_date(trip, observed_calls, timezone, gtfs);

        let (stop_indices, used_parent_station_match) = self
            .first_alignment_for_trip(trip, observed_calls)?;

        Some(JourneyMatch {
            trip_id,
            service_date,
            kind: MatchKind::ExactId,
            mean_abs_difference_seconds: None,
            stop_indices,
            used_parent_station_match,
        })
    }

    fn match_direction_and_time(
        &self,
        journey: &EstimatedVehicleJourney,
        gtfs: &Gtfs,
        observed_calls: &[ObservedCall],
    ) -> Result<JourneyMatch, MatchMissReason> {
        if observed_calls.is_empty() {
            return Err(MatchMissReason::NoObservedCalls);
        }
        if !has_matching_time(observed_calls) {
            return Err(MatchMissReason::NoUsableTimes);
        }

        let target_route_id = journey
            .line_ref
            .as_ref()
            .and_then(|reference| reference.value.as_deref().and_then(extract_idfm_id))
            .ok_or(MatchMissReason::MissingLineRef)?;
        let patterns = self
            .directions
            .get(&target_route_id)
            .ok_or(MatchMissReason::RouteNotFound)?;
        let target_destination = journey
            .destination_ref
            .as_ref()
            .and_then(|reference| reference.value.as_deref())
            .map(|dest_ref| self.resolve_siri_stop_ids(dest_ref))
            .unwrap_or_default();

        let exact_destination_patterns = if !target_destination.is_empty() {
            let exact = patterns
                .iter()
                .filter(|pattern| target_destination.contains(&pattern.destination_stop_id))
                .collect::<Vec<_>>();
            (!exact.is_empty()).then_some(exact)
        } else {
            None
        };

        let parent_destination_patterns = if exact_destination_patterns.is_none() && !target_destination.is_empty() {
            let parent = patterns
                .iter()
                .filter(|pattern| {
                    target_destination.iter().any(|dest| {
                        self.stops_equivalent(&pattern.destination_stop_id, dest)
                    })
                })
                .collect::<Vec<_>>();
            (!parent.is_empty()).then_some(parent)
        } else {
            None
        };

        let candidate_patterns = match exact_destination_patterns {
            Some(exact) => exact,
            None => match parent_destination_patterns {
                Some(parent) => parent,
                None => patterns.iter().collect::<Vec<_>>(),
            },
        };

        let timezone = self.timezone_for_route(&target_route_id);
        let service_dates = candidate_service_dates(observed_calls, timezone)
            .ok_or(MatchMissReason::NoUsableTimes)?;
        let mut saw_alignment = false;
        let mut saw_active_service = false;

        // Preserve platform-level precision: only use parent-station equivalence
        // after every exact-ID alignment attempt has failed to produce a match.
        for mode in [StopMatchMode::Exact, StopMatchMode::ParentStation] {
            let mut best_score: Option<CandidateScore> = None;

            for pattern in &candidate_patterns {
                if !has_valid_alignment(
                    &pattern.stop_ids,
                    observed_calls,
                    &self.canonical_stop_ids,
                    mode,
                ) {
                    continue;
                }
                saw_alignment = true;

                for trip_id in &pattern.trip_ids {
                    let Some(trip) = gtfs.trips.get(trip_id) else {
                        continue;
                    };

                    for service_date in &service_dates {
                        if !self.service_runs_on(gtfs, &trip.service_id, *service_date) {
                            continue;
                        }
                        saw_active_service = true;

                        if let Some(score) = find_best_alignment(
                            trip,
                            observed_calls,
                            Some(*service_date),
                            Some(timezone),
                            &self.canonical_stop_ids,
                            mode,
                        ) {
                            if best_score
                                .as_ref()
                                .is_none_or(|best| score.is_better_than(best))
                            {
                                best_score = Some(score);
                            }
                        }
                    }
                }
            }

            if let Some(score) = best_score {
                return Ok(JourneyMatch {
                    trip_id: score.trip_id,
                    service_date: Some(score.service_date),
                    kind: MatchKind::DirectionAndTime,
                    mean_abs_difference_seconds: Some(score.mean_abs_difference_seconds),
                    stop_indices: score.stop_indices,
                    used_parent_station_match: score.used_parent_station_match,
                });
            }
        }

        if !saw_alignment {
            Err(MatchMissReason::NoStopAlignment)
        } else if !saw_active_service {
            Err(MatchMissReason::InactiveServiceDate)
        } else {
            Err(MatchMissReason::NoTimeComparisons)
        }
    }

    fn score_trip(
        &self,
        trip: &Trip,
        observed_calls: &[ObservedCall],
        timezone: Tz,
        gtfs: &Gtfs,
    ) -> Option<CandidateScore> {
        if observed_calls.is_empty() || !has_matching_time(observed_calls) {
            return None;
        }

        let service_dates = candidate_service_dates(observed_calls, timezone)?;

        for mode in [StopMatchMode::Exact, StopMatchMode::ParentStation] {
            let mut best_score: Option<CandidateScore> = None;
            for service_date in &service_dates {
                if !self.service_runs_on(gtfs, &trip.service_id, *service_date) {
                    continue;
                }

                if let Some(score) = find_best_alignment(
                    trip,
                    observed_calls,
                    Some(*service_date),
                    Some(timezone),
                    &self.canonical_stop_ids,
                    mode,
                ) {
                    if best_score
                        .as_ref()
                        .is_none_or(|best| score.is_better_than(best))
                    {
                        best_score = Some(score);
                    }
                }
            }

            if best_score.is_some() {
                return best_score;
            }
        }

        None
    }

    fn first_alignment_for_trip(
        &self,
        trip: &Trip,
        observed_calls: &[ObservedCall],
    ) -> Option<(Vec<usize>, bool)> {
        if observed_calls.is_empty() {
            return None;
        }

        for mode in [StopMatchMode::Exact, StopMatchMode::ParentStation] {
            if let Some(score) = find_best_alignment(
                trip,
                observed_calls,
                None,
                None,
                &self.canonical_stop_ids,
                mode,
            ) {
                return Some((score.stop_indices, mode == StopMatchMode::ParentStation));
            }
        }

        None
    }

    fn infer_service_date(
        &self,
        trip: &Trip,
        observed_calls: &[ObservedCall],
        timezone: Tz,
        gtfs: &Gtfs,
    ) -> Option<NaiveDate> {
        candidate_service_dates(observed_calls, timezone)?
            .into_iter()
            .find(|date| self.service_runs_on(gtfs, &trip.service_id, *date))
    }

    fn service_runs_on(&self, gtfs: &Gtfs, service_id: &str, date: NaiveDate) -> bool {
        if let Some(added) = self
            .service_exceptions
            .get(service_id)
            .and_then(|exceptions| exceptions.get(&date))
        {
            return *added;
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

    fn stops_equivalent(&self, left: &str, right: &str) -> bool {
        left == right || self.canonical_stop_id(left) == self.canonical_stop_id(right)
    }

    fn canonical_stop_id<'a>(&'a self, stop_id: &'a str) -> &'a str {
        self.canonical_stop_ids
            .get(stop_id)
            .map(String::as_str)
            .unwrap_or(stop_id)
    }

    fn timezone_for_route(&self, route_id: &str) -> Tz {
        self.route_timezones.get(route_id).copied().unwrap_or(Paris)
    }

    fn observed_calls(&self, journey: &EstimatedVehicleJourney) -> Vec<ObservedCall> {
        journey
            .estimated_calls
            .as_ref()
            .into_iter()
            .flat_map(|calls| &calls.estimated_call)
            .filter_map(|call| {
                let siri_ref = call
                    .stop_point_ref
                    .as_ref()
                    .and_then(|reference| reference.value.as_deref())?;

                let stop_aliases = self.resolve_siri_stop_ids(siri_ref);
                if stop_aliases.is_empty() {
                    return None;
                }

                Some(ObservedCall {
                    stop_aliases,
                    aimed_arrival: call.aimed_arrival_time.as_deref().and_then(parse_timestamp),
                    aimed_departure: call
                        .aimed_departure_time
                        .as_deref()
                        .and_then(parse_timestamp),
                    expected_arrival: call
                        .expected_arrival_time
                        .as_deref()
                        .and_then(parse_timestamp),
                    expected_departure: call
                        .expected_departure_time
                        .as_deref()
                        .and_then(parse_timestamp),
                })
            })
            .collect()
    }
}

fn build_canonical_stop_ids(gtfs: &Gtfs) -> HashMap<String, String> {
    let mut canonical = HashMap::with_capacity(gtfs.stops.len());

    for stop_id in gtfs.stops.keys() {
        let mut current = stop_id.clone();
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

        canonical.insert(stop_id.clone(), current);
    }

    canonical
}

fn insert_exact_key(index: &mut HashMap<String, Vec<String>>, key: &str, trip_id: &str) {
    index
        .entry(key.to_string())
        .or_default()
        .push(trip_id.to_string());
}

fn extract_siri_trip_key(value: &str) -> Option<&str> {
    let (_, value) = value.split_once("::")?;
    let value = value.trim_end_matches(":LOC").trim_end_matches(':');
    (!value.is_empty()).then_some(value)
}

fn extract_idfm_id(value: &str) -> Option<String> {
    let id = value.rsplit(':').find(|part| !part.is_empty())?;
    Some(format!("IDFM:{id}"))
}


fn parse_timestamp(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date_time| date_time.timestamp())
}

fn has_matching_time(observed_calls: &[ObservedCall]) -> bool {
    observed_calls
        .iter()
        .any(|call| call.matching_arrival().is_some() || call.matching_departure().is_some())
}

fn candidate_service_dates(
    observed_calls: &[ObservedCall],
    timezone: Tz,
) -> Option<Vec<NaiveDate>> {
    let earliest_timestamp = observed_calls
        .iter()
        .flat_map(|call| [call.matching_arrival(), call.matching_departure()])
        .flatten()
        .min()?;
    let local_date = Utc
        .timestamp_opt(earliest_timestamp, 0)
        .single()?
        .with_timezone(&timezone)
        .date_naive();

    Some(
        (0..=2)
            .filter_map(|days| local_date.checked_sub_signed(Duration::days(days)))
            .collect(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DpScore {
    total_difference: i128,
    max_difference: i64,
    comparisons: usize,
}

impl DpScore {
    fn is_better_than(&self, other: &Self) -> bool {
        match (self.comparisons, other.comparisons) {
            (0, 0) => false,
            (0, _) => false,
            (_, 0) => true,
            (self_comp, other_comp) => {
                let self_total = self.total_difference;
                let other_total = other.total_difference;
                let self_mean_less = self_total * (other_comp as i128) < other_total * (self_comp as i128);
                let self_mean_eq = self_total * (other_comp as i128) == other_total * (self_comp as i128);

                if self_mean_less {
                    true
                } else if self_mean_eq {
                    if self.max_difference < other.max_difference {
                        true
                    } else if self.max_difference == other.max_difference {
                        self_comp > other_comp
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
        }
    }
}

fn find_best_alignment(
    trip: &Trip,
    observed_calls: &[ObservedCall],
    service_date: Option<NaiveDate>,
    timezone: Option<Tz>,
    canonical_stop_ids: &HashMap<String, String>,
    mode: StopMatchMode,
) -> Option<CandidateScore> {
    let num_calls = observed_calls.len();
    let num_stops = trip.stop_times.len();
    if num_calls == 0 || num_stops == 0 || num_stops < num_calls {
        return None;
    }

    let mut dp = vec![vec![None; num_stops + 1]; num_calls + 1];
    let mut predecessor = vec![vec![None; num_stops + 1]; num_calls + 1];

    for s in 0..=num_stops {
        dp[0][s] = Some(DpScore {
            total_difference: 0,
            max_difference: 0,
            comparisons: 0,
        });
    }

    for c in 1..=num_calls {
        let observed_call = &observed_calls[c - 1];

        for s in 1..=num_stops {
            let mut best_score = dp[c][s - 1];
            let mut best_pred = predecessor[c][s - 1];

            if let Some(prev_score) = dp[c - 1][s - 1] {
                let stop_time = &trip.stop_times[s - 1];
                let gtfs_stop_id = stop_time.stop.id.as_str();

                let matches = observed_call.stop_aliases.iter().any(|siri_stop_id| {
                    match mode {
                        StopMatchMode::Exact => gtfs_stop_id == siri_stop_id,
                        StopMatchMode::ParentStation => {
                            canonical_stop_ids
                                .get(gtfs_stop_id)
                                .map(String::as_str)
                                .unwrap_or(gtfs_stop_id)
                                == canonical_stop_ids
                                    .get(siri_stop_id)
                                    .map(String::as_str)
                                    .unwrap_or(siri_stop_id)
                        }
                    }
                });

                if matches {
                    let mut match_comparisons = 0;
                    let mut match_total_diff = 0;
                    let mut match_max_diff = 0;

                    if let (Some(date), Some(tz)) = (service_date, timezone) {
                        if let (Some(observed), Some(scheduled_seconds)) = (
                            observed_call.matching_arrival(),
                            stop_time.arrival_time.or(stop_time.departure_time),
                        ) {
                            if let Some(difference) =
                                minimum_time_difference(observed, date, scheduled_seconds, tz)
                            {
                                match_total_diff += difference;
                                match_max_diff = match_max_diff.max(difference);
                                match_comparisons += 1;
                            }
                        }

                        if let (Some(observed), Some(scheduled_seconds)) = (
                            observed_call.matching_departure(),
                            stop_time.departure_time.or(stop_time.arrival_time),
                        ) {
                            if let Some(difference) =
                                minimum_time_difference(observed, date, scheduled_seconds, tz)
                            {
                                match_total_diff += difference;
                                match_max_diff = match_max_diff.max(difference);
                                match_comparisons += 1;
                            }
                        }
                    }

                    let new_score = DpScore {
                        total_difference: prev_score.total_difference + i128::from(match_total_diff),
                        max_difference: prev_score.max_difference.max(match_max_diff),
                        comparisons: prev_score.comparisons + match_comparisons,
                    };

                    if best_score.is_none() || new_score.is_better_than(&best_score.unwrap()) {
                        best_score = Some(new_score);
                        best_pred = Some(s - 1);
                    }
                }
            }

            dp[c][s] = best_score;
            predecessor[c][s] = best_pred;
        }
    }

    let final_score = dp[num_calls][num_stops]?;

    if service_date.is_some() && final_score.comparisons == 0 {
        return None;
    }

    let mut stop_indices = vec![0; num_calls];
    let mut curr_s = num_stops;
    for c in (1..=num_calls).rev() {
        let stop_idx = predecessor[c][curr_s]?;
        stop_indices[c - 1] = stop_idx;
        curr_s = stop_idx;
    }

    Some(CandidateScore {
        trip_id: trip.id.clone(),
        service_date: service_date.unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
        mean_abs_difference_seconds: if final_score.comparisons > 0 {
            (final_score.total_difference / final_score.comparisons as i128) as i64
        } else {
            0
        },
        max_abs_difference_seconds: final_score.max_difference,
        comparisons: final_score.comparisons,
        stop_indices,
        used_parent_station_match: mode == StopMatchMode::ParentStation,
    })
}

fn has_valid_alignment(
    stop_ids: &[String],
    observed_calls: &[ObservedCall],
    canonical_stop_ids: &HashMap<String, String>,
    mode: StopMatchMode,
) -> bool {
    let num_calls = observed_calls.len();
    let num_stops = stop_ids.len();
    if num_stops < num_calls {
        return false;
    }
    let mut dp = vec![vec![false; num_stops + 1]; num_calls + 1];
    for s in 0..=num_stops {
        dp[0][s] = true;
    }
    for c in 1..=num_calls {
        let observed_call = &observed_calls[c - 1];
        for s in 1..=num_stops {
            let mut possible = dp[c][s - 1];
            if dp[c - 1][s - 1] {
                let gtfs_stop_id = stop_ids[s - 1].as_str();
                let matches = observed_call.stop_aliases.iter().any(|siri_stop_id| {
                    match mode {
                        StopMatchMode::Exact => gtfs_stop_id == siri_stop_id,
                        StopMatchMode::ParentStation => {
                            canonical_stop_ids
                                .get(gtfs_stop_id)
                                .map(String::as_str)
                                .unwrap_or(gtfs_stop_id)
                                == canonical_stop_ids
                                    .get(siri_stop_id)
                                    .map(String::as_str)
                                    .unwrap_or(siri_stop_id)
                        }
                    }
                });
                if matches {
                    possible = true;
                }
            }
            dp[c][s] = possible;
        }
    }
    dp[num_calls][num_stops]
}

fn minimum_time_difference(
    expected_timestamp: i64,
    service_date: NaiveDate,
    scheduled_seconds: u32,
    timezone: Tz,
) -> Option<i64> {
    scheduled_timestamps(service_date, scheduled_seconds, timezone)
        .into_iter()
        .map(|scheduled_timestamp| expected_timestamp.abs_diff(scheduled_timestamp) as i64)
        .min()
}

fn scheduled_timestamps(service_date: NaiveDate, scheduled_seconds: u32, timezone: Tz) -> Vec<i64> {
    let day_offset = i64::from(scheduled_seconds / 86_400);
    let seconds_in_day = scheduled_seconds % 86_400;
    let Some(date) = service_date.checked_add_signed(Duration::days(day_offset)) else {
        return Vec::new();
    };
    let Some(time) = NaiveTime::from_num_seconds_from_midnight_opt(seconds_in_day, 0) else {
        return Vec::new();
    };
    let local_date_time = date.and_time(time);

    match timezone.from_local_datetime(&local_date_time) {
        LocalResult::Single(date_time) => vec![date_time.timestamp()],
        LocalResult::Ambiguous(first, second) => {
            vec![first.timestamp(), second.timestamp()]
        }
        LocalResult::None => Vec::new(),
    }
}

fn siri_reference_suffix(value: &str) -> Option<&str> {
    value.rsplit(':').find(|part| !part.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtfs_structures::{StopTime, Stop};
    use std::sync::Arc;

    fn make_dummy_trip<S: AsRef<str>>(stop_ids: &[S]) -> Trip {
        let stop_times = stop_ids
            .iter()
            .map(|stop_id| StopTime {
                stop: Arc::new(Stop {
                    id: stop_id.as_ref().to_string(),
                    ..Default::default()
                }),
                arrival_time: None,
                departure_time: None,
                ..Default::default()
            })
            .collect();

        Trip {
            id: "dummy".to_string(),
            service_id: "dummy".to_string(),
            route_id: "dummy".to_string(),
            stop_times,
            ..Default::default()
        }
    }

    fn exact_alignments<S: AsRef<str>>(
        stop_ids: &[S],
        observed_calls: &[ObservedCall],
    ) -> Vec<Vec<usize>> {
        let trip = make_dummy_trip(stop_ids);
        find_best_alignment(
            &trip,
            observed_calls,
            None,
            None,
            &HashMap::new(),
            StopMatchMode::Exact,
        )
        .map(|score| vec![score.stop_indices])
        .unwrap_or_default()
    }

    fn parent_alignments<S: AsRef<str>>(
        stop_ids: &[S],
        observed_calls: &[ObservedCall],
        canonical: &HashMap<String, String>,
    ) -> Vec<Vec<usize>> {
        let trip = make_dummy_trip(stop_ids);
        find_best_alignment(
            &trip,
            observed_calls,
            None,
            None,
            canonical,
            StopMatchMode::ParentStation,
        )
        .map(|score| vec![score.stop_indices])
        .unwrap_or_default()
    }

    fn observed_call(stop_id: &str) -> ObservedCall {
        ObservedCall {
            stop_aliases: vec![stop_id.to_string()],
            aimed_arrival: None,
            aimed_departure: None,
            expected_arrival: Some(0),
            expected_departure: None,
        }
    }

    #[test]
    fn aligns_omitted_calls_as_an_ordered_subsequence() {
        let stop_ids = ["A", "B", "C", "D", "E", "F"];
        let observed_calls = [observed_call("B"), observed_call("D"), observed_call("F")];

        assert_eq!(
            exact_alignments(&stop_ids, &observed_calls),
            vec![vec![1, 3, 5]]
        );
    }

    #[test]
    fn keeps_all_ordered_alignments_for_repeated_stops() {
        let stop_ids = ["A", "B", "C", "B", "D"];
        let observed_calls = [observed_call("B"), observed_call("D")];

        assert_eq!(
            exact_alignments(&stop_ids, &observed_calls),
            vec![vec![1, 4]]
        );
    }

    #[test]
    fn rejects_calls_whose_order_does_not_match_the_trip() {
        let stop_ids = ["A", "B", "C", "D"];
        let observed_calls = [observed_call("D"), observed_call("B")];

        assert!(exact_alignments(&stop_ids, &observed_calls).is_empty());
    }

    #[test]
    fn parent_station_alignment_matches_sibling_platforms() {
        let stop_ids = ["platform-a", "platform-c"];
        let observed_calls = [observed_call("platform-b"), observed_call("platform-c")];
        let canonical = HashMap::from([
            ("platform-a".to_string(), "station".to_string()),
            ("platform-b".to_string(), "station".to_string()),
            ("platform-c".to_string(), "platform-c".to_string()),
        ]);

        assert!(
            exact_alignments(&stop_ids, &observed_calls).is_empty()
        );
        assert_eq!(
            parent_alignments(
                &stop_ids,
                &observed_calls,
                &canonical,
            ),
            vec![vec![0, 1]]
        );
    }

    #[test]
    fn aimed_times_take_precedence_over_expected_times() {
        let call = ObservedCall {
            stop_aliases: vec!["B".to_string()],
            aimed_arrival: Some(100),
            aimed_departure: Some(110),
            expected_arrival: Some(130),
            expected_departure: Some(140),
        };

        assert_eq!(call.matching_arrival(), Some(100));
        assert_eq!(call.matching_departure(), Some(110));
    }

    #[test]
    fn resolves_sncf_stop_area_to_namespaced_gtfs_stop() {
        use gtfs_structures::Stop;
        use std::collections::BTreeMap;
        let mut stops = BTreeMap::new();
        stops.insert("IDFM:monomodalStopPlace:47874".to_string(), Stop {
            id: "IDFM:monomodalStopPlace:47874".to_string(),
            ..Default::default()
        });
        let gtfs = Gtfs {
            stops,
            ..Default::default()
        };
        let index = GtfsMatchIndex::build(&gtfs);
        let resolved = index.resolve_siri_stop_ids("STIF:StopArea:SP:47874:");
        assert_eq!(resolved, vec!["IDFM:monomodalStopPlace:47874".to_string()]);
    }

    #[test]
    fn exact_trip_without_stop_alignment_is_rejected() {
        use gtfs_structures::StopTime;
        use std::collections::BTreeMap;
        use crate::siri_models::{EstimatedVehicleJourney, EstimatedCalls, EstimatedCall, ValueWrapper};

        let stop = Arc::new(Stop {
            id: "IDFM:monomodalStopPlace:12345".to_string(),
            ..Default::default()
        });

        let mut trips = BTreeMap::new();
        trips.insert("some-uuid-trip-id".to_string(), Trip {
            id: "some-uuid-trip-id".to_string(),
            service_id: "service-id".to_string(),
            route_id: "IDFM:route-id".to_string(),
            stop_times: vec![StopTime {
                stop,
                arrival_time: None,
                departure_time: None,
                ..Default::default()
            }],
            ..Default::default()
        });

        let mut stops = BTreeMap::new();
        stops.insert("IDFM:monomodalStopPlace:12345".to_string(), Stop {
            id: "IDFM:monomodalStopPlace:12345".to_string(),
            ..Default::default()
        });
        stops.insert("IDFM:monomodalStopPlace:47874".to_string(), Stop {
            id: "IDFM:monomodalStopPlace:47874".to_string(),
            ..Default::default()
        });

        let gtfs = Gtfs {
            trips,
            stops,
            ..Default::default()
        };
        let index = GtfsMatchIndex::build(&gtfs);

        let journey = EstimatedVehicleJourney {
            dated_vehicle_journey_ref: Some(ValueWrapper {
                value: Some("SIRI::some-uuid-trip-id".to_string()),
            }),
            line_ref: Some(ValueWrapper {
                value: Some("STIF:Line::route-id:".to_string()),
            }),
            operator_ref: None,
            direction_ref: None,
            direction_name: None,
            destination_ref: None,
            journey_note: None,
            estimated_calls: Some(EstimatedCalls {
                estimated_call: vec![EstimatedCall {
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
                }],
            }),
        };

        let result = index.match_journey(&journey, &gtfs);
        assert!(result.is_err());
    }
}
