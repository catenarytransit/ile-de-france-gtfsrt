mod adaptive_matching;
mod downloader;
mod enhanced_processor;
mod gtfs_manager;
mod keys;
mod matching;
mod matching_diagnostics;
mod processor;
mod siri_models;
mod state;

use anyhow::Result;
use gtfs_realtime::FeedMessage;
use keys::KeyManager;
use prost::Message;
use state::AppState;
use std::sync::Arc;
use warp::Filter;

const GTFS_URL: &str = "https://eu.ftp.opendatasoft.com/stif/GTFS/IDFM-gtfs.zip";
const SIRI_URL: &str =
    "https://prim.iledefrance-mobilites.fr/marketplace/estimated-timetable?LineRef=ALL";
const KEYS_FILE: &str = "keys.txt";

#[tokio::main]
async fn main() -> Result<()> {
    println!("Initializing Ile-de-France GTFS-RT Server...");

    let key_manager = Arc::new(KeyManager::new(KEYS_FILE)?);
    let state = {
        let rt_cache_path = std::env::var("GTFS_RT_CACHE_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("./gtfs-rt.pb"));

        let initial_feed = match tokio::fs::read(&rt_cache_path).await {
            Ok(bytes) => {
                match FeedMessage::decode(&bytes[..]) {
                    Ok(feed) => {
                        println!(
                            "Recovered cached GTFS-RT feed from {}: {} entities",
                            rt_cache_path.display(),
                            feed.entity.len()
                        );
                        feed
                    }
                    Err(e) => {
                        eprintln!("Failed to decode cached GTFS-RT feed: {e}");
                        FeedMessage::default()
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("No cached GTFS-RT feed found at {}", rt_cache_path.display());
                FeedMessage::default()
            }
            Err(e) => {
                eprintln!("Failed to read cached GTFS-RT feed: {e}");
                FeedMessage::default()
            }
        };

        Arc::new(AppState::new(initial_feed))
    };

    // Start background GTFS update loop
    gtfs_manager::start_gtfs_updater(state.clone(), GTFS_URL.to_string()).await?;

    // Start background SIRI downloader loop
    downloader::start_downloader(state.clone(), key_manager, SIRI_URL.to_string()).await;

    // HTTP Server Setup
    let state_filter = warp::any().map(move || state.clone());

    // GET /gtfs-rt
    let gtfs_rt_route = warp::path("gtfs-rt")
        .and(warp::get())
        .and(state_filter.clone())
        .then(|state: Arc<AppState>| async move {
            let msg = {
                let lock = state.gtfs_rt_feed.read().await;
                Arc::clone(&*lock)
            };
            let mut buf = Vec::new();
            msg.encode(&mut buf).unwrap();
            warp::reply::with_header(buf, "content-type", "application/x-protobuf")
        });

    // GET /platforms
    let platforms_route = warp::path("platforms")
        .and(warp::get())
        .and(state_filter.clone())
        .then(|state: Arc<AppState>| async move {
            let mut data = std::collections::HashMap::new();
            for item in state.trip_platforms.iter() {
                data.insert(item.key().clone(), item.value().clone());
            }
            warp::reply::json(&data)
        });

    let routes = gtfs_rt_route.or(platforms_route);

    let server_port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .expect("Invalid PORT env variable");

    println!("Server running at http://localhost:{}", server_port);
    warp::serve(routes).run(([0, 0, 0, 0], server_port)).await;

    Ok(())
}
