use gtfs_structures::Gtfs;
use serde_json::Value;
use std::fs::File;
use std::io::BufReader;

fn main() {
    println!("Loading GTFS...");
    let gtfs = Gtfs::from_url("https://eu.ftp.opendatasoft.com/stif/GTFS/IDFM-gtfs.zip")
        .expect("Failed to load GTFS");
    println!("GTFS loaded. Total trips: {}", gtfs.trips.len());

    let mut with_short_name = 0;
    for (_id, trip) in &gtfs.trips {
        if trip.trip_short_name.is_some() {
            with_short_name += 1;
            if with_short_name <= 10 {
                println!(
                    "Trip ID: {}, Route: {}, Short Name: {:?}",
                    trip.id, trip.route_id, trip.trip_short_name
                );
            }
        }
    }
    println!("Trips with short name: {}", with_short_name);

    println!("Loading example.json...");
    let file = File::open("example.json").unwrap();
    let reader = BufReader::new(file);
    let v: Value = serde_json::from_reader(reader).unwrap();

    let deliveries = v["Siri"]["ServiceDelivery"]["EstimatedTimetableDelivery"]
        .as_array()
        .unwrap();

    let mut found_matches = 0;
    let mut total_with_name = 0;

    for del in deliveries {
        let frames = del["EstimatedJourneyVersionFrame"].as_array().unwrap();
        for frame in frames {
            let journeys = frame["EstimatedVehicleJourney"].as_array().unwrap();
            for journey in journeys {
                if let Some(names) = journey["VehicleJourneyName"].as_array() {
                    if !names.is_empty() {
                        total_with_name += 1;
                        let val = names[0]["value"].as_str().unwrap();

                        // Try to find a matching trip by trip_short_name
                        let mut matched = false;
                        for (_id, trip) in &gtfs.trips {
                            if let Some(short_name) = &trip.trip_short_name {
                                if short_name == val {
                                    matched = true;
                                    break;
                                }
                            }
                        }
                        if matched {
                            found_matches += 1;
                        }
                    }
                }
            }
        }
    }

    println!(
        "Total journeys with VehicleJourneyName: {}, Matched trips: {}",
        total_with_name, found_matches
    );
}
