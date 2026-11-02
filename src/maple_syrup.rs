//! Maple Syrup: share a stop-time itinerary plus per-trip start time.
//!
//! The source GTFS parser expands stop_times.txt into a Vec<StopTime> per trip.
//! Paris has many journeys whose stop sequence, relative times and boarding
//! attributes are identical. We drain those vectors and retain each unique
//! normalized itinerary once, while keeping only (pattern, start) for each trip.
//!
//! Preserve *every* StopTime field, including untimed stops, sequences, pickup,
//! drop-off, headsigns and shape distances. Never mutate the source ordering.
//! Unusual schedules (including >24h GTFS times) round-trip exactly.
use gtfs_structures::{Gtfs, StopTime, Trip};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::mem::discriminant;

#[derive(Clone, Copy, Debug)]
struct TripRef {
    pattern: usize,
    start: u32,
}

#[derive(Debug, Default)]
pub struct MapleSyrup {
    patterns: Vec<Vec<StopTime>>,
    by_hash: HashMap<u64, Vec<usize>>,
    by_trip: HashMap<String, TripRef>,
    raw_stop_times: usize,
}

impl MapleSyrup {
    pub fn compress(gtfs: &mut Gtfs) -> Self {
        let mut result = Self::default();
        result.by_trip.reserve(gtfs.trips.len());
        for (id, trip) in &mut gtfs.trips {
            let mut stops = std::mem::take(&mut trip.stop_times);
            result.raw_stop_times += stops.len();
            // The minimum time also handles missing arrival/departure at the
            // first stop, unsorted feeds, and extended (>24h) service times.
            let start = stops
                .iter()
                .flat_map(|stop| [stop.arrival_time, stop.departure_time])
                .flatten()
                .min()
                .unwrap_or(0);
            for stop in &mut stops {
                stop.arrival_time = stop.arrival_time.map(|time| time - start);
                stop.departure_time = stop.departure_time.map(|time| time - start);
            }
            let hash = fingerprint(&stops);
            let pattern = result
                .by_hash
                .get(&hash)
                .and_then(|candidates| candidates.iter().copied().find(|&candidate| {
                    same_pattern(&result.patterns[candidate], &stops)
                }));
            let pattern = match pattern {
                Some(pattern) => pattern,
                None => {
                    let index = result.patterns.len();
                    result.patterns.push(stops);
                    result.by_hash.entry(hash).or_default().push(index);
                    index
                }
            };
            result.by_trip.insert(id.clone(), TripRef { pattern, start });
        }
        result
    }

    pub fn stats(&self) -> (usize, usize, usize) {
        (self.by_trip.len(), self.patterns.len(), self.raw_stop_times)
    }

    pub fn start_time(&self, trip_id: &str) -> Option<u32> {
        Some(self.by_trip.get(trip_id)?.start)
    }

    pub fn max_offset(&self, trip_id: &str) -> Option<u32> {
        let stops = self.patterns.get(self.by_trip.get(trip_id)?.pattern)?;
        stops.iter().flat_map(|s| [s.arrival_time, s.departure_time]).flatten().max()
    }

    pub fn stop_id(&self, trip_id: &str, index: usize) -> Option<&str> {
        let stops = self.patterns.get(self.by_trip.get(trip_id)?.pattern)?;
        Some(stops.get(index)?.stop.id.as_str())
    }

    pub fn last_stop_id(&self, trip_id: &str) -> Option<&str> {
        let stops = self.patterns.get(self.by_trip.get(trip_id)?.pattern)?;
        Some(stops.last()?.stop.id.as_str())
    }

    pub fn stop_ids(&self, trip_id: &str) -> Option<Vec<String>> {
        Some(self.patterns.get(self.by_trip.get(trip_id)?.pattern)?
            .iter().map(|s| s.stop.id.clone()).collect())
    }

    /// Materialize only a *candidate* or matched trip, never all GTFS trips.
    pub fn trip(&self, gtfs: &Gtfs, trip_id: &str) -> Option<Trip> {
        let mut trip = gtfs.trips.get(trip_id)?.clone();
        let reference = self.by_trip.get(trip_id)?;
        let template = self.patterns.get(reference.pattern)?;
        trip.stop_times = template
            .iter()
            .map(|stop| {
                let mut stop = stop.clone();
                stop.arrival_time = stop.arrival_time.map(|offset| reference.start + offset);
                stop.departure_time = stop.departure_time.map(|offset| reference.start + offset);
                stop
            })
            .collect();
        Some(trip)
    }
}

fn fingerprint(stops: &[StopTime]) -> u64 {
    let mut hasher = DefaultHasher::new();
    stops.len().hash(&mut hasher);
    for stop in stops {
        stop.stop.id.hash(&mut hasher);
        stop.arrival_time.hash(&mut hasher);
        stop.departure_time.hash(&mut hasher);
        stop.stop_sequence.hash(&mut hasher);
        stop.stop_headsign.hash(&mut hasher);
        discriminant(&stop.pickup_type).hash(&mut hasher);
        discriminant(&stop.drop_off_type).hash(&mut hasher);
        discriminant(&stop.continuous_pickup).hash(&mut hasher);
        discriminant(&stop.continuous_drop_off).hash(&mut hasher);
        discriminant(&stop.timepoint).hash(&mut hasher);
        stop.shape_dist_traveled.map(f32::to_bits).hash(&mut hasher);
    }
    hasher.finish()
}

fn same_pattern(a: &[StopTime], b: &[StopTime]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| {
        a.stop.id == b.stop.id
            && a.arrival_time == b.arrival_time
            && a.departure_time == b.departure_time
            && a.stop_sequence == b.stop_sequence
            && a.stop_headsign == b.stop_headsign
            && discriminant(&a.pickup_type) == discriminant(&b.pickup_type)
            && discriminant(&a.drop_off_type) == discriminant(&b.drop_off_type)
            && discriminant(&a.continuous_pickup) == discriminant(&b.continuous_pickup)
            && discriminant(&a.continuous_drop_off) == discriminant(&b.continuous_drop_off)
            && discriminant(&a.timepoint) == discriminant(&b.timepoint)
            && a.shape_dist_traveled.map(f32::to_bits)
                == b.shape_dist_traveled.map(f32::to_bits)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtfs_structures::{Stop, StopTime};
    use std::sync::Arc;

    fn trip(id: &str, base: u32, stop: Arc<Stop>) -> Trip {
        Trip {
            id: id.to_owned(),
            service_id: "daily".to_owned(),
            route_id: "metro".to_owned(),
            stop_times: vec![
                StopTime { stop: stop.clone(), arrival_time: Some(base), departure_time: Some(base + 20), stop_sequence: 1, ..Default::default() },
                StopTime { stop, arrival_time: Some(base + 90), departure_time: None, stop_sequence: 2, ..Default::default() },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn deduplicates_shifted_schedules_and_round_trips() {
        let stop = Arc::new(Stop { id: "station".into(), ..Default::default() });
        let mut gtfs = Gtfs::default();
        let one = trip("one", 23 * 3600, stop.clone());
        let two = trip("two", 27 * 3600, stop.clone());
        gtfs.trips.insert(one.id.clone(), one.clone());
        gtfs.trips.insert(two.id.clone(), two.clone());
        let compressed = MapleSyrup::compress(&mut gtfs);
        assert_eq!(compressed.stats(), (2, 1, 4));
        assert!(gtfs.trips.values().all(|t| t.stop_times.is_empty()));
        for original in [&one, &two] {
            let expanded = compressed.trip(&gtfs, &original.id).unwrap();
            for (a, b) in expanded.stop_times.iter().zip(&original.stop_times) {
                assert_eq!(a.stop.id, b.stop.id);
                assert_eq!(a.arrival_time, b.arrival_time);
                assert_eq!(a.departure_time, b.departure_time);
                assert_eq!(a.stop_sequence, b.stop_sequence);
            }
        }
        assert_eq!(compressed.stop_id("two", 1), Some("station"));
        assert_eq!(compressed.start_time("two"), Some(27 * 3600));
    }

    #[test]
    fn distinct_stop_metadata_is_not_merged() {
        let stop = Arc::new(Stop { id: "station".into(), ..Default::default() });
        let mut gtfs = Gtfs::default();
        let first = trip("first", 200, stop.clone());
        let mut second = trip("second", 400, stop);
        second.stop_times[0].stop_headsign = Some("different".into());
        gtfs.trips.insert(first.id.clone(), first);
        gtfs.trips.insert(second.id.clone(), second);
        let compressed = MapleSyrup::compress(&mut gtfs);
        assert_eq!(compressed.stats().1, 2);
    }
}
