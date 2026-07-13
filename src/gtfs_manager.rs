use crate::state::AppState;
use anyhow::Result;
use gtfs_structures::Gtfs;
use std::sync::Arc;
use std::time::Duration;

pub async fn start_gtfs_updater(state: Arc<AppState>, url: String) {
    tokio::spawn(async move {
        loop {
            println!("Fetching GTFS from {}...", url);
            match load_gtfs(&url).await {
                Ok(gtfs) => {
                    println!("Successfully loaded GTFS dataset");
                    let mut lock = state.gtfs.write().await;
                    *lock = Some(Arc::new(gtfs));
                }
                Err(e) => {
                    eprintln!("Failed to load GTFS dataset: {}", e);
                }
            }
            
            // Wait 24 hours before reloading the GTFS
            tokio::time::sleep(Duration::from_secs(86400)).await;
        }
    });
}

async fn load_gtfs(url: &str) -> Result<Gtfs> {
    // using spawn_blocking as gtfs_structures parsing is CPU-bound
    let url = url.to_string();
    let result = tokio::task::spawn_blocking(move || {
        Gtfs::from_url(&url)
    }).await??;
    
    Ok(result)
}
