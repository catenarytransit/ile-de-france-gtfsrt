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
    /// Route ID -> unique ordered stop patterns -> trips using that pattern.
    directions: HashMap<String, Vec<DirectionPattern>>,
    /// DatedVehicleJourneyRef suffix/full value -> possible GTFS trip IDs.
    exact_trip_ids: HashMap<String, Vec<String>>,
    route_timezones: HashMap<String, Tz>,
    service_exceptions: HashMap<String, HashMap<NaiveDate, bool>>,
    /// Stop/platform/boarding-area ID -> top-most known parent station ID.
    canonical_stop_ids: HashMap<String, String>,
}

#[derive(Debug)]
struct ObservedCall {
    stop_id: String,
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

        let mut grouped_directions = HashMap::<(String, Vec<String>), Vec<String>>::new();
        let mut exact_trip_ids = HashMap::<String, Vec<String>>::new();

        for (trip_id, trip) in &gtfs.trips {
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
            route_timezones,
            service_exceptions,
            canonical_stop_ids,
        }
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
        let observed_calls = observed_calls(journey);
        if observed_calls.is_empty() {
            return None;
        }

        let target_route_id = journey
            .line_ref
            .as_ref()
            .and_then(|reference| extract_idfm_id(&reference.value))?;
        let patterns = self.directions.get(&target_route_id)?;
        let target_destination = journey
            .destination_ref
            .as_ref()
            .and_then(|reference| extract_idfm_id(&reference.value));

        let mut candidate_patterns = patterns.iter().collect::<Vec<_>>();
        let mut destination_filter_applied = false;

        if let Some(destination) = target_destination.as_ref() {
            let exact = patterns
                .iter()
                .filter(|pattern| pattern.destination_stop_id == *destination)
                .collect::<Vec<_>>();

            if !exact.is_empty() {
                destination_filter_applied = exact.len() < patterns.len();
                candidate_patterns = exact;
            } else {
                let parent = patterns
                    .iter()
                    .filter(|pattern| {
                        self.stops_equivalent(&pattern.destination_stop_id, destination)
                    })
                    .collect::<Vec<_>>();

                if !parent.is_empty() {
                    destination_filter_applied = parent.len() < patterns.len();
                    candidate_patterns = parent;
                }
            }
        }

        let observed_stop_ids = observed_calls
            .into_iter()
            .map(|call| call.stop_id)
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
        let observed_calls = observed_calls(journey);

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
            .and_then(|reference| extract_siri_trip_key(&reference.value))?;
        let indexed_trip_ids = self.exact_trip_ids.get(exact_key)?;
        let target_route_id = journey
            .line_ref
            .as_ref()
            .and_then(|reference| extract_idfm_id(&reference.value));
        let target_destination = journey
            .destination_ref
            .as_ref()
            .and_then(|reference| extract_idfm_id(&reference.value));

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

        if let Some(destination_stop_id) = target_destination.as_ref() {
            let destination_candidates = candidates
                .iter()
                .copied()
                .filter(|trip_id| {
                    gtfs.trips
                        .get(*trip_id)
                        .and_then(|trip| trip.stop_times.last())
                        .is_some_and(|stop_time| {
                            self.stops_equivalent(&stop_time.stop.id, destination_stop_id)
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
            .first_alignment_for_trip(trip, observed_calls)
            .unwrap_or_default();

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
            .and_then(|reference| extract_idfm_id(&reference.value))
            .ok_or(MatchMissReason::MissingLineRef)?;
        let patterns = self
            .directions
            .get(&target_route_id)
            .ok_or(MatchMissReason::RouteNotFound)?;
        let target_destination = journey
            .destination_ref
            .as_ref()
            .and_then(|reference| extract_idfm_id(&reference.value));

        let exact_destination_patterns = target_destination.as_ref().map(|destination| {
            patterns
                .iter()
                .filter(|pattern| pattern.destination_stop_id == *destination)
                .collect::<Vec<_>>()
        });
        let parent_destination_patterns = target_destination.as_ref().map(|destination| {
            patterns
                .iter()
                .filter(|pattern| self.stops_equivalent(&pattern.destination_stop_id, destination))
                .collect::<Vec<_>>()
        });

        let candidate_patterns = match exact_destination_patterns {
            Some(exact) if !exact.is_empty() => exact,
            _ => match parent_destination_patterns {
                Some(parent) if !parent.is_empty() => parent,
                _ => patterns.iter().collect::<Vec<_>>(),
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
                let alignments = alignment_indices(
                    &pattern.stop_ids,
                    observed_calls,
                    &self.canonical_stop_ids,
                    mode,
                );
                if alignments.is_empty() {
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

                        for stop_indices in &alignments {
                            let Some(score) = score_alignment(
                                trip,
                                observed_calls,
                                stop_indices,
                                *service_date,
                                timezone,
                                mode == StopMatchMode::ParentStation,
                            ) else {
                                continue;
                            };

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

        let trip_stop_ids = trip
            .stop_times
            .iter()
            .map(|stop_time| stop_time.stop.id.as_str())
            .collect::<Vec<_>>();
        let service_dates = candidate_service_dates(observed_calls, timezone)?;

        for mode in [StopMatchMode::Exact, StopMatchMode::ParentStation] {
            let alignments = alignment_indices(
                &trip_stop_ids,
                observed_calls,
                &self.canonical_stop_ids,
                mode,
            );
            if alignments.is_empty() {
                continue;
            }

            let mut best_score: Option<CandidateScore> = None;
            for stop_indices in alignments {
                for service_date in &service_dates {
                    if !self.service_runs_on(gtfs, &trip.service_id, *service_date) {
                        continue;
                    }

                    let Some(score) = score_alignment(
                        trip,
                        observed_calls,
                        &stop_indices,
                        *service_date,
                        timezone,
                        mode == StopMatchMode::ParentStation,
                    ) else {
                        continue;
                    };

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

        let stop_ids = trip
            .stop_times
            .iter()
            .map(|stop_time| stop_time.stop.id.as_str())
            .collect::<Vec<_>>();

        for mode in [StopMatchMode::Exact, StopMatchMode::ParentStation] {
            if let Some(indices) =
                alignment_indices(&stop_ids, observed_calls, &self.canonical_stop_ids, mode)
                    .into_iter()
                    .next()
            {
                return Some((indices, mode == StopMatchMode::ParentStation));
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

fn observed_calls(journey: &EstimatedVehicleJourney) -> Vec<ObservedCall> {
    journey
        .estimated_calls
        .as_ref()
        .into_iter()
        .flat_map(|calls| &calls.estimated_call)
        .filter_map(|call| {
            let stop_id = call
                .stop_point_ref
                .as_ref()
                .and_then(|reference| extract_idfm_id(&reference.value))?;

            Some(ObservedCall {
                stop_id,
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

fn alignment_indices<S: AsRef<str>>(
    stop_ids: &[S],
    observed_calls: &[ObservedCall],
    canonical_stop_ids: &HashMap<String, String>,
    mode: StopMatchMode,
) -> Vec<Vec<usize>> {
    let mut alignments = Vec::new();
    let mut current = Vec::with_capacity(observed_calls.len());
    collect_alignment_indices(
        stop_ids,
        observed_calls,
        canonical_stop_ids,
        mode,
        0,
        0,
        &mut current,
        &mut alignments,
    );
    alignments
}

#[allow(clippy::too_many_arguments)]
fn collect_alignment_indices<S: AsRef<str>>(
    stop_ids: &[S],
    observed_calls: &[ObservedCall],
    canonical_stop_ids: &HashMap<String, String>,
    mode: StopMatchMode,
    call_index: usize,
    search_from: usize,
    current: &mut Vec<usize>,
    alignments: &mut Vec<Vec<usize>>,
) {
    if call_index == observed_calls.len() {
        alignments.push(current.clone());
        return;
    }

    let remaining_calls = observed_calls.len() - call_index;
    if stop_ids.len().saturating_sub(search_from) < remaining_calls {
        return;
    }

    let last_candidate = stop_ids.len() - remaining_calls;
    for stop_index in search_from..=last_candidate {
        let gtfs_stop_id = stop_ids[stop_index].as_ref();
        let siri_stop_id = observed_calls[call_index].stop_id.as_str();
        let matches = match mode {
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
        };
        if !matches {
            continue;
        }

        current.push(stop_index);
        collect_alignment_indices(
            stop_ids,
            observed_calls,
            canonical_stop_ids,
            mode,
            call_index + 1,
            stop_index + 1,
            current,
            alignments,
        );
        current.pop();
    }
}

fn score_alignment(
    trip: &Trip,
    observed_calls: &[ObservedCall],
    stop_indices: &[usize],
    service_date: NaiveDate,
    timezone: Tz,
    used_parent_station_match: bool,
) -> Option<CandidateScore> {
    if stop_indices.len() != observed_calls.len() {
        return None;
    }

    let mut total_difference: i128 = 0;
    let mut max_difference = 0_i64;
    let mut comparisons = 0_usize;

    for (observed_call, stop_index) in observed_calls.iter().zip(stop_indices) {
        let stop_time = trip.stop_times.get(*stop_index)?;

        if let (Some(observed), Some(scheduled_seconds)) = (
            observed_call.matching_arrival(),
            stop_time.arrival_time.or(stop_time.departure_time),
        ) {
            if let Some(difference) =
                minimum_time_difference(observed, service_date, scheduled_seconds, timezone)
            {
                total_difference += i128::from(difference);
                max_difference = max_difference.max(difference);
                comparisons += 1;
            }
        }

        if let (Some(observed), Some(scheduled_seconds)) = (
            observed_call.matching_departure(),
            stop_time.departure_time.or(stop_time.arrival_time),
        ) {
            if let Some(difference) =
                minimum_time_difference(observed, service_date, scheduled_seconds, timezone)
            {
                total_difference += i128::from(difference);
                max_difference = max_difference.max(difference);
                comparisons += 1;
            }
        }
    }

    if comparisons == 0 {
        return None;
    }

    Some(CandidateScore {
        trip_id: trip.id.clone(),
        service_date,
        mean_abs_difference_seconds: (total_difference / comparisons as i128) as i64,
        max_abs_difference_seconds: max_difference,
        comparisons,
        stop_indices: stop_indices.to_vec(),
        used_parent_station_match,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn exact_alignments<S: AsRef<str>>(
        stop_ids: &[S],
        observed_calls: &[ObservedCall],
    ) -> Vec<Vec<usize>> {
        alignment_indices(
            stop_ids,
            observed_calls,
            &HashMap::new(),
            StopMatchMode::Exact,
        )
    }

    fn observed_call(stop_id: &str) -> ObservedCall {
        ObservedCall {
            stop_id: stop_id.to_string(),
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
            vec![vec![1, 4], vec![3, 4]]
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
            alignment_indices(&stop_ids, &observed_calls, &canonical, StopMatchMode::Exact,)
                .is_empty()
        );
        assert_eq!(
            alignment_indices(
                &stop_ids,
                &observed_calls,
                &canonical,
                StopMatchMode::ParentStation,
            ),
            vec![vec![0, 1]]
        );
    }

    #[test]
    fn aimed_times_take_precedence_over_expected_times() {
        let call = ObservedCall {
            stop_id: "B".to_string(),
            aimed_arrival: Some(100),
            aimed_departure: Some(110),
            expected_arrival: Some(130),
            expected_departure: Some(140),
        };

        assert_eq!(call.matching_arrival(), Some(100));
        assert_eq!(call.matching_departure(), Some(110));
    }
}
