use serde_json::Value;
use std::fs::File;
use std::io::BufReader;

fn main() {
    let file = File::open("example.json").unwrap();
    let reader = BufReader::new(file);
    let v: Value = serde_json::from_reader(reader).unwrap();
    
    // Find first EstimatedVehicleJourney
    let journey = &v["Siri"]["ServiceDelivery"]["EstimatedTimetableDelivery"][0]["EstimatedJourneyVersionFrame"][0]["EstimatedVehicleJourney"][0];
    println!("{:#?}", journey);
}
