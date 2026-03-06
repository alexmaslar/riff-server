use std::sync::Arc;
use std::time::Duration;

use riff_core::{analysis, daily_mixes, db, musicbrainz, plugin, scanner};
use tracing::Instrument;

use crate::routes;
use crate::transcode;
use crate::AppState;

pub async fn run_background_pipeline(state: Arc<AppState>) {
    let mut first_run = true;
    loop {
        if !first_run {
            state.pipeline_notify.notified().await;
            // Debounce: coalesce rapid triggers (e.g. multiple downloads completing)
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        first_run = false;

        run_pipeline_iteration(&state)
            .instrument(tracing::info_span!("pipeline"))
            .await;
    }
}

async fn run_pipeline_iteration(state: &Arc<AppState>) {
        tracing::info!("starting");

        let mut album_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut artist_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Step 1: Enrichment (MusicBrainz — no API key needed)
        {
            let config = state.config.read().await;
            if config.metadata.enrichment.auto_enrich {
                let db = state.db.clone();
                drop(config);
                if state.stage_manager.try_start("enrichment") {
                        match musicbrainz::enrich_library(&db).await {
                        Ok(r) => {
                            tracing::info!(stage = "enrichment", "complete");
                            album_ids.extend(r.enriched_album_ids);
                            artist_ids.extend(r.enriched_artist_ids);
                        }
                        Err(e) => tracing::warn!(stage = "enrichment", error = %e, "failed"),
                    }
                    state.stage_manager.finish("enrichment");
                }
            }
        }

        // Step 1b: Editorial enrichment (editorial plugins)
        {
            let db = state.db.clone();
            let editorial_providers = state
                .plugin_registry
                .read()
                .await
                .editorial_providers()
                .to_vec();
            if state.stage_manager.try_start("editorial") {
                    match riff_core::editorial::enrich_library_editorial(&db, &editorial_providers)
                    .await
                {
                    Ok(r) => {
                        tracing::info!(stage = "editorial", "complete");
                        album_ids.extend(r.enriched_album_ids);
                    }
                    Err(e) => tracing::warn!(stage = "editorial", error = %e, "failed"),
                }
                state.stage_manager.finish("editorial");
            }
        }

        // Step 2: Album recommendations (metadata-based, always run)
        run_stage(&state, "recommendation", {
            let db = state.db.clone();
            async move { riff_core::recommendations::generate_recommendations(&db).await }
        })
        .await;

        // Step 2a: Artist recommendations (metadata-based, always run)
        run_stage(&state, "artist_recommendation", {
            let db = state.db.clone();
            async move {
                riff_core::recommendations::generate_artist_recommendations(&db).await
            }
        })
        .await;

        // Step 2b: Spotify artist images + streaming provider fallback
        {
            let db = state.db.clone();
            let providers = state
                .plugin_registry
                .read()
                .await
                .streaming_providers()
                .to_vec();
            match musicbrainz::enrich_artist_images_spotify(&db, &providers).await {
                Ok(ids) if !ids.is_empty() => {
                    tracing::info!(artists_updated = ids.len(), "artist images enriched");
                    artist_ids.extend(ids);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "artist image enrichment failed"),
            }
        }

        // Step 2c: Deezer top tracks (no API key needed)
        if state.stage_manager.try_start("top_tracks") {
            let result = musicbrainz::enrich_artist_top_tracks(&state.db, &state.http_client).await;
            match result {
                Ok(ids) => {
                    tracing::info!(stage = "top_tracks", "complete");
                    artist_ids.extend(ids);
                }
                Err(e) => tracing::warn!(stage = "top_tracks", error = %e, "failed"),
            }
            state.stage_manager.finish("top_tracks");
        }

        // Step 2d: Wikipedia bios (no API key needed)
        {
            let wiki_ids =
                musicbrainz::enrich_artist_bios_wikipedia(&state.db, &state.http_client)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "Wikipedia bio enrichment failed");
                        vec![]
                    });
            artist_ids.extend(wiki_ids);
        }

        // Step 3: Analysis (always)
        if state.stage_manager.try_start("analysis") {
            let result = analysis::analyze_library(&state.db).await;
            match result {
                Ok(r) => {
                    tracing::info!(stage = "analysis", "complete");
                    album_ids.extend(r.enriched_album_ids);
                }
                Err(e) => tracing::warn!(stage = "analysis", error = %e, "failed"),
            }
            state.stage_manager.finish("analysis");
        }

        // Emit consolidated enrichment event for SSE clients
        if !album_ids.is_empty() || !artist_ids.is_empty() {
            state
                .event_bus
                .emit(plugin::events::ServerEvent::EnrichmentCompleted {
                    album_ids: album_ids.into_iter().collect(),
                    artist_ids: artist_ids.into_iter().collect(),
                });
        }

        // Daily mixes are NOT run here — they regenerate only at midnight UTC via run_daily_refresh()

        // Step 4: Clean up stale caches (not accessed in 24h)
        routes::hls::cleanup_hls_cache().await;
        transcode::cleanup_cache(std::time::Duration::from_secs(86400)).await;

        // Step 5: WAL checkpoint — reclaim disk space after bulk writes
        if let Err(e) = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&state.db)
            .await
        {
            tracing::warn!(error = %e, "WAL checkpoint failed");
        }

        // Step 6: Update query planner statistics for long-running server
        let _ = sqlx::query("PRAGMA analysis_limit=400")
            .execute(&state.db)
            .await;
        if let Err(e) = sqlx::query("PRAGMA optimize")
            .execute(&state.db)
            .await
        {
            tracing::warn!(error = %e, "PRAGMA optimize failed");
        }

        tracing::info!("complete, waiting for next trigger");
}

async fn run_stage<F, R>(state: &AppState, stage: &str, f: F)
where
    F: std::future::Future<Output = anyhow::Result<R>>,
{
    if state.stage_manager.try_start(stage) {
        match f.await {
            Ok(_) => tracing::info!(stage, "complete"),
            Err(e) => tracing::warn!(stage, error = %e, "failed"),
        }
        state.stage_manager.finish(stage);
    }
}

pub async fn run_periodic_scanner(state: Arc<AppState>) {
    loop {
        // Read the minimum scan interval across all libraries
        let interval_secs = {
            let config = state.config.read().await;
            let global = config.library.scan_interval;
            config
                .resolved_libraries()
                .iter()
                .filter_map(|lib| lib.scan_interval)
                .min()
                .unwrap_or(global)
        };

        // scan_interval = 0 means disabled; re-check every 5 minutes
        if interval_secs == 0 {
            tokio::time::sleep(Duration::from_secs(300)).await;
            continue;
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        // Guard: skip if a scan is already in progress
        if !state.stage_manager.try_start("scan") {
            tracing::info!("periodic scan skipped — scan already running");
            continue;
        }

        let config = state.config.read().await;
        let libs = config.resolved_libraries();
        let global_interval = config.library.scan_interval;
        drop(config);

        let mut new_content_found = false;

        for lib_entry in &libs {
            let effective_interval = lib_entry.scan_interval.unwrap_or(global_interval);
            if effective_interval == 0 {
                continue;
            }

            let library_id = match db::get_library_id_by_path(&state.db, &lib_entry.path).await {
                Ok(Some(id)) => id,
                _ => continue,
            };

            tracing::info!(library = %lib_entry.name, path = %lib_entry.path, "periodic scan");
            match scanner::scan_library(&state.db, &lib_entry.path, &library_id).await {
                Ok(result) => {
                    if result.tracks_added > 0
                        || result.albums_added > 0
                        || result.artists_added > 0
                        || result.tracks_removed > 0
                        || result.albums_removed > 0
                        || result.artists_removed > 0
                    {
                        new_content_found = true;
                        tracing::info!(
                            library = %lib_entry.name,
                            artists_added = result.artists_added,
                            albums_added = result.albums_added,
                            tracks_added = result.tracks_added,
                            tracks_removed = result.tracks_removed,
                            albums_removed = result.albums_removed,
                            artists_removed = result.artists_removed,
                            "periodic scan complete",
                        );
                    }
                }
                Err(e) => tracing::warn!(library = %lib_entry.name, error = %e, "periodic scan failed"),
            }
        }

        if new_content_found {
            tracing::info!("periodic scan found new content, triggering pipeline");
            state.pipeline_notify.notify_one();
        }

        state.stage_manager.finish("scan");
    }
}

pub async fn run_daily_refresh(state: Arc<AppState>) {
    loop {
        // Compute duration until next midnight UTC
        let now = chrono::Utc::now();
        let tomorrow = (now.date_naive() + chrono::Days::new(1))
            .and_hms_opt(0, 0, 0)
            .expect("midnight is always a valid time");
        let until_midnight = tomorrow
            .signed_duration_since(now.naive_utc())
            .to_std()
            .unwrap_or(Duration::from_secs(60));

        tokio::time::sleep(until_midnight).await;

        // Generate daily mixes for all users
        match daily_mixes::generate_all_daily_mixes(&state.db).await {
            Ok(_) => tracing::info!("daily mix refresh complete"),
            Err(e) => tracing::warn!(error = %e, "daily mix refresh failed"),
        }
    }
}
