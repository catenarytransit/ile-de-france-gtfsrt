use crate::state::{AppState, LoadedGtfs};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use gtfs_structures::Gtfs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

pub async fn start_gtfs_updater(state: Arc<AppState>, url: String) -> Result<()> {
    let cache_path = std::env::var("GTFS_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./idfm-gtfs.zip"));

    if let Some(parent) = cache_path.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!("Failed to create GTFS cache directory {}", parent.display())
        })?;
    }

    let loaded_existing_cache = match load_gtfs_from_disk(&cache_path).await {
        Ok(gtfs) => {
            println!(
                "Loaded cached GTFS from {}: {} trips, {} stops, {} routes",
                cache_path.display(),
                gtfs.trips.len(),
                gtfs.stops.len(),
                gtfs.routes.len(),
            );

            publish_gtfs(&state, gtfs).await?;
            true
        }

        Err(error) => {
            println!(
                "No usable GTFS cache at {}: {error:#}",
                cache_path.display()
            );
            println!("Downloading initial GTFS dataset...");

            let gtfs = download_validate_and_replace(&url, &cache_path)
                .await
                .context("Failed to obtain initial GTFS dataset")?;

            println!(
                "Downloaded initial GTFS: {} trips, {} stops, {} routes",
                gtfs.trips.len(),
                gtfs.stops.len(),
                gtfs.routes.len(),
            );

            publish_gtfs(&state, gtfs).await?;
            false
        }
    };

    // At this point state.gtfs is guaranteed to be populated.
    // The SIRI downloader can now safely start.
    tokio::spawn(async move {
        // A cached feed may be old, so refresh it immediately in the
        // background. If we just downloaded it, wait until the next interval.
        if !loaded_existing_cache {
            tokio::time::sleep(REFRESH_INTERVAL).await;
        }

        loop {
            println!("Checking for an updated GTFS dataset...");

            match download_validate_and_replace(&url, &cache_path).await {
                Ok(gtfs) => {
                    println!(
                        "GTFS refreshed: {} trips, {} stops, {} routes",
                        gtfs.trips.len(),
                        gtfs.stops.len(),
                        gtfs.routes.len(),
                    );

                    if let Err(error) = publish_gtfs(&state, gtfs).await {
                        eprintln!(
                            "Failed to build GTFS matching index; retaining existing dataset: {error:#}"
                        );
                    }
                }

                Err(error) => {
                    // Keep serving the existing in-memory GTFS.
                    // Do not replace the known-good disk cache.
                    eprintln!("GTFS refresh failed; retaining existing dataset: {error:#}");
                }
            }

            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    });

    Ok(())
}

async fn load_gtfs_from_disk(path: &Path) -> Result<Gtfs> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("GTFS cache does not exist: {}", path.display()))?;

    if metadata.len() == 0 {
        anyhow::bail!("GTFS cache is empty");
    }

    let path_buf = path.to_path_buf();
    let parse_path = path_buf.clone();

    tokio::task::spawn_blocking(move || Gtfs::from_path(&parse_path))
        .await
        .context("GTFS parsing task panicked")?
        .with_context(|| format!("Failed to parse cached GTFS {}", path_buf.display()))
}

async fn download_validate_and_replace(url: &str, cache_path: &Path) -> Result<Gtfs> {
    // Keep the temporary file next to the final file so rename remains
    // atomic on Linux.
    let temporary_path = cache_path.with_extension("zip.tmp");

    let client = reqwest::Client::new();

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to download GTFS from {url}"))?
        .error_for_status()
        .with_context(|| format!("GTFS server returned an error for {url}"))?;

    let expected_length = response.content_length();
    let mut stream = response.bytes_stream();

    let mut file = tokio::fs::File::create(&temporary_path)
        .await
        .with_context(|| {
            format!(
                "Failed to create temporary GTFS file {}",
                temporary_path.display()
            )
        })?;

    let mut downloaded_bytes: u64 = 0;

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.context("Failed while downloading GTFS body")?;

        file.write_all(&chunk)
            .await
            .context("Failed to write GTFS download to disk")?;

        downloaded_bytes += chunk.len() as u64;
    }

    file.flush()
        .await
        .context("Failed to flush temporary GTFS file")?;

    file.sync_all()
        .await
        .context("Failed to synchronize temporary GTFS file")?;

    drop(file);

    if downloaded_bytes == 0 {
        let _ = tokio::fs::remove_file(&temporary_path).await;
        anyhow::bail!("Downloaded GTFS file was empty");
    }

    if let Some(expected_length) = expected_length {
        if downloaded_bytes != expected_length {
            let _ = tokio::fs::remove_file(&temporary_path).await;

            anyhow::bail!(
                "Incomplete GTFS download: expected {} bytes, received {} bytes",
                expected_length,
                downloaded_bytes
            );
        }
    }

    println!(
        "Downloaded {} bytes to {}",
        downloaded_bytes,
        temporary_path.display()
    );

    // Validate and fully parse the new ZIP before replacing the known-good
    // cache. A broken download can therefore never destroy the old cache.
    let parse_path = temporary_path.clone();

    let gtfs = tokio::task::spawn_blocking(move || Gtfs::from_path(&parse_path))
        .await
        .context("GTFS parsing task panicked")?
        .with_context(|| {
            format!(
                "Downloaded file is not a valid GTFS dataset: {}",
                temporary_path.display()
            )
        })?;

    // Atomic replacement on the Linux server, because both files are in the
    // same directory/filesystem.
    tokio::fs::rename(&temporary_path, cache_path)
        .await
        .with_context(|| {
            format!(
                "Failed to move {} to {}",
                temporary_path.display(),
                cache_path.display()
            )
        })?;

    println!("Saved GTFS cache to {}", cache_path.display());

    Ok(gtfs)
}

async fn publish_gtfs(state: &Arc<AppState>, gtfs: Gtfs) -> Result<()> {
    let loaded_gtfs = tokio::task::spawn_blocking(move || LoadedGtfs::new(gtfs))
        .await
        .context("GTFS matching-index task panicked")?;

    println!(
        "Built GTFS matching index with {} unique directions",
        loaded_gtfs.match_index.direction_count()
    );

    let mut lock = state.gtfs.write().await;
    *lock = Some(Arc::new(loaded_gtfs));

    Ok(())
}
