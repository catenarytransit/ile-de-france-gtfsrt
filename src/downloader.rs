use crate::keys::KeyManager;
use crate::processor::process_siri;
use crate::siri_models::SiriResponse;
use crate::state::AppState;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;

pub async fn start_downloader(state: Arc<AppState>, key_manager: Arc<KeyManager>, url: String) {
    let client = Client::new();

    tokio::spawn(async move {
        loop {
            let api_key = key_manager.get_next_key();
            println!("Fetching SIRI feed with key {}...", &api_key[0..5]); // debug print part of key

            let response_result = client.get(&url).header("apikey", api_key).send().await;

            match response_result {
                Ok(response) if response.status().is_success() => {
                    match response.json::<SiriResponse>().await {
                        Ok(siri_payload) => {
                            println!("Successfully fetched and parsed SIRI payload.");
                            // Process payload into state
                            process_siri(state.clone(), siri_payload).await;
                        }
                        Err(e) => {
                            eprintln!("Failed to parse SIRI JSON: {}", e);
                        }
                    }
                }
                Ok(response) => {
                    eprintln!("SIRI fetch failed with status: {}", response.status());
                }
                Err(e) => {
                    eprintln!("Failed to fetch SIRI payload: {}", e);
                }
            }

            // The user requested: "fetch this url every  100s"
            tokio::time::sleep(Duration::from_secs(100)).await;
        }
    });
}
