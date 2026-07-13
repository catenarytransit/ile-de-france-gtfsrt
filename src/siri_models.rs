use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct SiriResponse {
    #[serde(rename = "Siri")]
    pub siri: Siri,
}

#[derive(Debug, Deserialize)]
pub struct Siri {
    #[serde(rename = "ServiceDelivery")]
    pub service_delivery: ServiceDelivery,
}

#[derive(Debug, Deserialize)]
pub struct ServiceDelivery {
    #[serde(rename = "EstimatedTimetableDelivery")]
    pub estimated_timetable_delivery: Vec<EstimatedTimetableDelivery>,
}

#[derive(Debug, Deserialize)]
pub struct EstimatedTimetableDelivery {
    #[serde(rename = "EstimatedJourneyVersionFrame")]
    pub estimated_journey_version_frame: Vec<EstimatedJourneyVersionFrame>,
}

#[derive(Debug, Deserialize)]
pub struct EstimatedJourneyVersionFrame {
    #[serde(rename = "EstimatedVehicleJourney")]
    pub estimated_vehicle_journey: Vec<EstimatedVehicleJourney>,
}

#[derive(Debug, Deserialize)]
pub struct EstimatedVehicleJourney {
    #[serde(rename = "DatedVehicleJourneyRef")]
    pub dated_vehicle_journey_ref: Option<ValueWrapper>,
    #[serde(rename = "LineRef")]
    pub line_ref: Option<ValueWrapper>,
    #[serde(rename = "DirectionName")]
    pub direction_name: Option<Vec<ValueWrapper>>,
    #[serde(rename = "DestinationRef")]
    pub destination_ref: Option<ValueWrapper>,
    #[serde(rename = "EstimatedCalls")]
    pub estimated_calls: Option<EstimatedCalls>,
}

#[derive(Debug, Deserialize)]
pub struct EstimatedCalls {
    #[serde(rename = "EstimatedCall")]
    pub estimated_call: Vec<EstimatedCall>,
}

#[derive(Debug, Deserialize)]
pub struct EstimatedCall {
    #[serde(rename = "StopPointRef")]
    pub stop_point_ref: Option<ValueWrapper>,
    #[serde(rename = "AimedArrivalTime")]
    pub aimed_arrival_time: Option<String>,
    #[serde(rename = "AimedDepartureTime")]
    pub aimed_departure_time: Option<String>,
    #[serde(rename = "ExpectedArrivalTime")]
    pub expected_arrival_time: Option<String>,
    #[serde(rename = "ExpectedDepartureTime")]
    pub expected_departure_time: Option<String>,
    #[serde(rename = "ArrivalStatus")]
    pub arrival_status: Option<String>,
    #[serde(rename = "DepartureStatus")]
    pub departure_status: Option<String>,
    #[serde(rename = "ArrivalPlatformName")]
    pub arrival_platform_name: Option<ValueWrapper>,
    #[serde(rename = "DeparturePlatformName")]
    pub departure_platform_name: Option<ValueWrapper>,
}

#[derive(Debug, Deserialize)]
pub struct ValueWrapper {
    pub value: String,
}
