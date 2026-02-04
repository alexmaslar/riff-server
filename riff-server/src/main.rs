pub mod error;
mod middleware;
mod routes;

use anyhow::Result;
use axum::{
    extract::State,
    middleware as axum_mw,
    routing::{delete, get, post},
    Json, Router,
};
use riff_core::{ai, analysis, auth, config::Config, db, discogs, scanner};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

pub struct AppState {
    pub db: SqlitePool,
    pub config: Config,
    pub enrichment_running: AtomicBool,
    pub analysis_running: AtomicBool,
    pub summarization_running: AtomicBool,
    pub rating_running: AtomicBool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::load()?;
    tracing::info!("loaded config, port={}", config.server.port);

    if config.metadata.discogs.api_token.is_some() {
        tracing::info!("discogs api token loaded");
    } else {
        tracing::warn!("no discogs api token configured");
    }

    if config.metadata.ai.enabled {
        tracing::info!("AI summarization enabled (provider: {:?})", config.metadata.ai.provider);
    } else {
        tracing::debug!("AI summarization not configured");
    }

    let pool = db::init_pool().await?;
    tracing::info!("database initialized");

    // Bootstrap admin user on first run
    auth::bootstrap_admin(&pool, &config).await?;

    // Scan library on startup
    if let Some(library_path) = &config.library.path {
        tracing::info!("scanning library: {}", library_path);
        match scanner::scan_library(&pool, library_path).await {
            Ok(result) => {
                tracing::info!(
                    "scan complete: {} artists, {} albums, {} tracks",
                    result.artists_added,
                    result.albums_added,
                    result.tracks_added,
                );
            }
            Err(e) => tracing::warn!("library scan failed: {}", e),
        }
    }

    let state = Arc::new(AppState {
        db: pool,
        config: config.clone(),
        enrichment_running: AtomicBool::new(false),
        analysis_running: AtomicBool::new(false),
        summarization_running: AtomicBool::new(false),
        rating_running: AtomicBool::new(false),
    });

    // Auto-enrich from Discogs in background, then chain analysis
    if config.metadata.discogs.api_token.is_some() && config.metadata.discogs.auto_enrich {
        if state.enrichment_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            let enrich_state = state.clone();
            tokio::spawn(async move {
                match discogs::enrich_library(&enrich_state.db, &enrich_state.config.metadata.discogs).await {
                    Ok(result) => {
                        tracing::info!(
                            "enrichment complete: {} albums, {} artists, {} covers",
                            result.albums_enriched,
                            result.artists_enriched,
                            result.covers_downloaded,
                        );
                    }
                    Err(e) => tracing::warn!("background enrichment failed: {}", e),
                }
                enrich_state.enrichment_running.store(false, Ordering::SeqCst);

                // Chain summarization after enrichment
                if enrich_state.config.metadata.ai.enabled {
                    if enrich_state.summarization_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                        match ai::summarize_library(&enrich_state.db, &enrich_state.config.metadata.ai).await {
                            Ok(result) => {
                                tracing::info!(
                                    "summarization complete: {} summarized, {} errors",
                                    result.albums_summarized,
                                    result.errors.len(),
                                );
                            }
                            Err(e) => tracing::warn!("background summarization failed: {}", e),
                        }
                        enrich_state.summarization_running.store(false, Ordering::SeqCst);
                    }

                    // Chain rating after summarization
                    if enrich_state.rating_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                        match ai::rate_library(&enrich_state.db, &enrich_state.config.metadata.ai).await {
                            Ok(result) => {
                                tracing::info!(
                                    "rating complete: {} rated, {} errors",
                                    result.albums_rated,
                                    result.errors.len(),
                                );
                            }
                            Err(e) => tracing::warn!("background rating failed: {}", e),
                        }
                        enrich_state.rating_running.store(false, Ordering::SeqCst);
                    }
                }

                // Chain analysis after rating
                if enrich_state.analysis_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                    match analysis::analyze_library(&enrich_state.db).await {
                        Ok(result) => {
                            tracing::info!(
                                "analysis complete: {} analyzed, {} failed, {} skipped",
                                result.tracks_analyzed,
                                result.tracks_failed,
                                result.tracks_skipped,
                            );
                        }
                        Err(e) => tracing::warn!("background analysis failed: {}", e),
                    }
                    enrich_state.analysis_running.store(false, Ordering::SeqCst);
                }
            });
        }
    } else {
        // No Discogs configured — run summarization first, then analysis
        let startup_state = state.clone();
        tokio::spawn(async move {
            // Summarization first (user-visible)
            if startup_state.config.metadata.ai.enabled {
                if startup_state.summarization_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                    match ai::summarize_library(&startup_state.db, &startup_state.config.metadata.ai).await {
                        Ok(result) => {
                            tracing::info!(
                                "summarization complete: {} summarized, {} errors",
                                result.albums_summarized,
                                result.errors.len(),
                            );
                        }
                        Err(e) => tracing::warn!("background summarization failed: {}", e),
                    }
                    startup_state.summarization_running.store(false, Ordering::SeqCst);
                }

                // Then rating
                if startup_state.rating_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                    match ai::rate_library(&startup_state.db, &startup_state.config.metadata.ai).await {
                        Ok(result) => {
                            tracing::info!(
                                "rating complete: {} rated, {} errors",
                                result.albums_rated,
                                result.errors.len(),
                            );
                        }
                        Err(e) => tracing::warn!("background rating failed: {}", e),
                    }
                    startup_state.rating_running.store(false, Ordering::SeqCst);
                }
            }

            // Then analysis (background-only)
            if startup_state.analysis_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                match analysis::analyze_library(&startup_state.db).await {
                    Ok(result) => {
                        tracing::info!(
                            "analysis complete: {} analyzed, {} failed, {} skipped",
                            result.tracks_analyzed,
                            result.tracks_failed,
                            result.tracks_skipped,
                        );
                    }
                    Err(e) => tracing::warn!("background analysis failed: {}", e),
                }
                startup_state.analysis_running.store(false, Ordering::SeqCst);
            }
        });
    }

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Public routes (no auth required)
    let public = Router::new()
        .route("/health", get(health))
        .route("/auth/login", post(routes::auth::login))
        .route("/auth/refresh", post(routes::auth::refresh))
        .route("/albums/{id}/cover", get(routes::albums::get_cover));

    // Protected routes (require valid JWT)
    let protected = Router::new()
        .route("/artists", get(routes::artists::list_artists))
        .route("/artists/{id}", get(routes::artists::get_artist))
        .route("/albums", get(routes::albums::list_albums))
        .route("/albums/{id}", get(routes::albums::get_album))
        .route("/albums/{id}/play", post(routes::albums::increment_play_count))
        .route("/tracks/{id}/stream", get(routes::tracks::stream_track))
        .route("/tracks/{id}/download", get(routes::tracks::download_track))
        .route("/playlists", get(routes::playlists::list_playlists).post(routes::playlists::create_playlist))
        .route("/playlists/{id}", get(routes::playlists::get_playlist).delete(routes::playlists::delete_playlist))
        .route("/playlists/{id}/tracks", post(routes::playlists::add_track).put(routes::playlists::reorder_tracks))
        .route("/playlists/{id}/tracks/{track_id}", delete(routes::playlists::remove_track))
        // History
        .route("/history", post(routes::history::record_play))
        .route("/history/albums", get(routes::history::recently_played_albums))
        .route("/history/continue", get(routes::history::continue_listening))
        // Favorites
        .route("/favorites", post(routes::favorites::toggle_favorite).get(routes::favorites::list_favorites))
        .route("/favorites/check", get(routes::favorites::check_favorite))
        .route_layer(axum_mw::from_fn_with_state(
            state.clone(),
            middleware::require_auth,
        ));

    // Admin routes (require auth + admin role)
    let admin = Router::new()
        .route("/library/scan", post(routes::library::trigger_scan))
        .route("/library/enrich", post(routes::library::trigger_enrichment))
        .route("/library/enrich/{album_id}", post(routes::library::enrich_album))
        .route("/library/analyze", post(routes::library::trigger_analysis))
        .route("/library/summarize", post(routes::library::trigger_summarization))
        .route("/library/summarize/{album_id}", post(routes::library::summarize_album))
        .route("/library/rate", post(routes::library::trigger_rating))
        .route("/library/rate/{album_id}", post(routes::library::rate_album))
        .route("/library/stats", get(routes::library::library_stats))
        .route("/users", get(routes::users::list_users))
        .route("/users", post(routes::users::create_user))
        .route("/users/{id}", delete(routes::users::delete_user))
        .route_layer(axum_mw::from_fn(middleware::require_admin))
        .route_layer(axum_mw::from_fn_with_state(
            state.clone(),
            middleware::require_auth,
        ));

    let app = Router::new()
        .merge(public)
        .merge(protected)
        .merge(admin)
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state);

    let addr = format!("0.0.0.0:{}", config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on {}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(_state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
