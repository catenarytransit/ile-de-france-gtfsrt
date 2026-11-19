//! Persist the platform information exposed by GET /platforms across restarts.
//!
//! The on-disk JSON has the same shape as the existing /platforms response:
//! a mapping from GTFS trip ID to a list of stop/platform pairs.

use crate::state::{AppState, PlatformInfo};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

pub fn cache_path() -> PathBuf {
    std::env::var_os("PLATFORMS_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./platforms.json"))
}

/// Restore a complete cache snapshot. A missing file is normal on first boot.
/// Callers can log other errors and continue with an empty in-memory cache.
pub async fn restore(state: &AppState, path: &Path) -> Result<usize> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read platform cache {}", path.display()));
        }
    };

    // Parse before changing state so a corrupt cache cannot discard good data.
    let platforms: HashMap<String, Vec<PlatformInfo>> = serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid platform cache JSON at {}", path.display()))?;
    let count = platforms.len();

    state.trip_platforms.clear();
    for (trip_id, stops) in platforms {
        state.trip_platforms.insert(trip_id, stops);
    }

    Ok(count)
}

/// Save the current platform map using a same-directory temporary file and
/// atomic rename, so a failed write cannot truncate the last good cache.
pub async fn persist(state: &AppState, path: &Path) -> Result<usize> {
    let platforms: HashMap<String, Vec<PlatformInfo>> = state
        .trip_platforms
        .iter()
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect();
    let count = platforms.len();
    let bytes = serde_json::to_vec(&platforms).context("Failed to serialize platform cache")?;

    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create platform cache directory {}", parent.display()))?;
    }

    let temporary_path = path.with_extension("json.tmp");
    let mut file = tokio::fs::File::create(&temporary_path)
        .await
        .with_context(|| format!("Failed to create {}", temporary_path.display()))?;
    file.write_all(&bytes)
        .await
        .with_context(|| format!("Failed to write {}", temporary_path.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("Failed to sync {}", temporary_path.display()))?;
    drop(file);

    tokio::fs::rename(&temporary_path, path)
        .await
        .with_context(|| {
            format!(
                "Failed to replace platform cache {} with {}",
                path.display(),
                temporary_path.display()
            )
        })?;

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_path() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "idfm-platform-cache-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn platform(name: &str) -> PlatformInfo {
        PlatformInfo {
            stop_id: "stop-123".to_string(),
            platform_name: name.to_string(),
        }
    }

    #[tokio::test]
    async fn restores_saved_platforms_on_reboot() {
        let path = test_path();
        let before = AppState::new(Default::default());
        before
            .trip_platforms
            .insert("trip-123".to_string(), vec![platform("Voie 4")]);

        assert_eq!(persist(&before, &path).await.unwrap(), 1);

        let after = AppState::new(Default::default());
        assert_eq!(restore(&after, &path).await.unwrap(), 1);
        let stops = after.trip_platforms.get("trip-123").unwrap();
        assert_eq!(stops[0].stop_id, "stop-123");
        assert_eq!(stops[0].platform_name, "Voie 4");
        drop(stops);

        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn persists_new_platform_values_over_previous_snapshot() {
        let path = test_path();
        let state = AppState::new(Default::default());
        state
            .trip_platforms
            .insert("trip-123".to_string(), vec![platform("Voie 4")]);
        persist(&state, &path).await.unwrap();

        state
            .trip_platforms
            .insert("trip-123".to_string(), vec![platform("Voie 7")]);
        persist(&state, &path).await.unwrap();

        let restarted = AppState::new(Default::default());
        restore(&restarted, &path).await.unwrap();
        assert_eq!(
            restarted.trip_platforms.get("trip-123").unwrap()[0].platform_name,
            "Voie 7"
        );

        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn missing_cache_is_not_an_error() {
        let state = AppState::new(Default::default());
        assert_eq!(restore(&state, &test_path()).await.unwrap(), 0);
        assert!(state.trip_platforms.is_empty());
    }

    #[tokio::test]
    async fn corrupt_cache_does_not_clear_existing_entries() {
        let path = test_path();
        tokio::fs::write(&path, b"{invalid-json").await.unwrap();

        let state = AppState::new(Default::default());
        state
            .trip_platforms
            .insert("trip-123".to_string(), vec![platform("Voie 4")]);
        assert!(restore(&state, &path).await.is_err());
        assert_eq!(
            state.trip_platforms.get("trip-123").unwrap()[0].platform_name,
            "Voie 4"
        );

        tokio::fs::remove_file(path).await.unwrap();
    }
}
