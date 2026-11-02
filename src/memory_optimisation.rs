//! Strip parsed GTFS objects down to the data needed for SIRI matching.
//!
//! We retain stop identifiers, station parents and coordinates because the
//! adaptive matcher uses them for station equivalence and timezone fallback.

use gtfs_structures::{Gtfs, Stop};
use std::collections::HashMap;
use std::sync::Arc;

pub fn prune_gtfs_for_matching(gtfs: &mut Gtfs) {
    // These collections are not consulted by either SIRI matcher.
    gtfs.shapes.clear();
    gtfs.fare_attributes.clear();
    gtfs.fare_rules.clear();
    gtfs.fare_products.clear();
    gtfs.fare_media.clear();
    gtfs.rider_categories.clear();
    gtfs.feed_info.clear();
    gtfs.levels.clear();

    for route in gtfs.routes.values_mut() {
        route.short_name = None;
        route.long_name = None;
        route.desc = None;
        route.url = None;
        route.ticketing_deep_link_id = None;
    }

    for trip in gtfs.trips.values_mut() {
        trip.shape_id = None;
        trip.trip_headsign = None;
        trip.trip_short_name = None;
        trip.block_id = None;
        trip.frequencies.clear();
        for stop_time in &mut trip.stop_times {
            stop_time.stop_headsign = None;
            stop_time.shape_dist_traveled = None;
        }
    }

    // Build exactly one lightweight Arc<Stop> per stop. Arc::make_mut would
    // detach each stop-time's reference and allocate many independent copies.
    let compact_stops: HashMap<String, Arc<Stop>> = gtfs.stops.drain()
        .map(|(key, original)| {
            let compact = Stop {
                id: original.id.clone(),
                parent_station: original.parent_station.clone(),
                latitude: original.latitude,
                longitude: original.longitude,
                ..Stop::default()
            };
            (key, Arc::new(compact))
        })
        .collect();

    for trip in gtfs.trips.values_mut() {
        for stop_time in &mut trip.stop_times {
            if let Some(compact) = compact_stops.get(&stop_time.stop.id) {
                stop_time.stop = Arc::clone(compact);
            }
        }
    }
    gtfs.stops = compact_stops;
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtfs_structures::{StopTime, Trip};

    #[test]
    fn pruning_preserves_stops_and_schedules_needed_for_matching() {
        let station = Arc::new(Stop {
            id: "stop-1".into(),
            name: Some("Large unused station name".into()),
            parent_station: Some("parent-1".into()),
            latitude: Some(48.85),
            longitude: Some(2.35),
            ..Default::default()
        });
        let mut gtfs = Gtfs::default();
        gtfs.stops.insert(station.id.clone(), Arc::clone(&station));
        gtfs.trips.insert("trip-1".into(), Trip {
            id: "trip-1".into(),
            route_id: "route-1".into(),
            service_id: "service-1".into(),
            trip_headsign: Some("unused".into()),
            stop_times: vec![StopTime {
                stop: station,
                arrival_time: Some(86_500),
                departure_time: Some(86_530),
                stop_sequence: 7,
                stop_headsign: Some("unused".into()),
                ..Default::default()
            }],
            ..Default::default()
        });
        prune_gtfs_for_matching(&mut gtfs);
        let stop_time = &gtfs.trips["trip-1"].stop_times[0];
        assert_eq!(stop_time.arrival_time, Some(86_500));
        assert_eq!(stop_time.departure_time, Some(86_530));
        assert_eq!(stop_time.stop_sequence, 7);
        assert_eq!(stop_time.stop.parent_station.as_deref(), Some("parent-1"));
        assert_eq!(stop_time.stop.latitude, Some(48.85));
        assert!(stop_time.stop.name.is_none());
        assert!(stop_time.stop_headsign.is_none());
        assert!(gtfs.trips["trip-1"].trip_headsign.is_none());
        assert!(Arc::ptr_eq(&stop_time.stop, &gtfs.stops["stop-1"]));
    }
}
