pub mod error;
mod middleware;
mod routes;

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    middleware as axum_mw,
    routing::{delete, get, post},
    BoxError, Json, Router,
};
use riff_core::{ai, analysis, auth, config::Config, daily_mixes, db, discogs, scanner};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceBuilder;
use tower::timeout::TimeoutLayer;
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
    pub recommendation_running: AtomicBool,
    pub artist_bio_running: AtomicBool,
    pub artist_recommendation_running: AtomicBool,
}

async fn run_background_pipeline(state: Arc<AppState>) {
    // Step 1: Enrichment (if Discogs configured)
    if state.config.metadata.discogs.api_token.is_some()
        && state.config.metadata.discogs.auto_enrich
    {
        run_stage(&state.enrichment_running, "enrichment", async {
            discogs::enrich_library(&state.db, &state.config.metadata.discogs).await
        })
        .await;
    }

    // Step 2: Summarization + Rating (if AI configured)
    if state.config.metadata.ai.enabled {
        run_stage(&state.summarization_running, "summarization", async {
            ai::summarize_library(&state.db, &state.config.metadata.ai).await
        })
        .await;

        run_stage(&state.rating_running, "rating", async {
            ai::rate_library(&state.db, &state.config.metadata.ai).await
        })
        .await;

        run_stage(&state.recommendation_running, "recommendations", async {
            ai::recommend_library(&state.db, &state.config.metadata.ai).await
        })
        .await;

        run_stage(&state.artist_recommendation_running, "artist recommendations", async {
            ai::recommend_artists(&state.db, &state.config.metadata.ai).await
        })
        .await;

        run_stage(&state.artist_bio_running, "artist bios", async {
            ai::bio_artists(&state.db, &state.config.metadata.ai).await
        })
        .await;
    }

    // Step 3: Analysis (always)
    run_stage(&state.analysis_running, "analysis", async {
        analysis::analyze_library(&state.db).await
    })
    .await;

    // Step 4: Daily mixes (always, after analysis so ratings/BPM are available)
    match daily_mixes::generate_all_daily_mixes(&state.db).await {
        Ok(_) => tracing::info!("daily mixes complete"),
        Err(e) => tracing::warn!("daily mixes failed: {e}"),
    }
}

async fn run_stage<F, R>(flag: &AtomicBool, name: &str, f: F)
where
    F: std::future::Future<Output = anyhow::Result<R>>,
{
    if flag
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        match f.await {
            Ok(_) => tracing::info!("{name} complete"),
            Err(e) => tracing::warn!("{name} failed: {e}"),
        }
        flag.store(false, Ordering::SeqCst);
    }
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

    // Detect library path changes and wipe stale data
    if let Some(library_path) = &config.library.path {
        db::check_library_path(&pool, library_path).await?;
    }

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
        recommendation_running: AtomicBool::new(false),
        artist_bio_running: AtomicBool::new(false),
        artist_recommendation_running: AtomicBool::new(false),
    });

    // Background pipeline: enrich → summarize → rate → analyze
    {
        let pipeline_state = state.clone();
        tokio::spawn(async move {
            run_background_pipeline(pipeline_state).await;
        });
    }

    let cors = if let Some(ref origins) = config.server.cors_origins {
        let allowed: Vec<_> = origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();
        CorsLayer::new()
            .allow_origin(allowed)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        CorsLayer::very_permissive()
    };

    // Public routes (no auth required)
    let public = Router::new()
        .route("/health", get(health))
        .route("/auth/login", post(routes::auth::login))
        .route("/auth/refresh", post(routes::auth::refresh))
        .route("/albums/{id}/cover", get(routes::albums::get_cover))
        .route("/mixes/daily/{id}/cover", get(routes::daily_mixes::get_mix_cover));

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
        .route("/history/stats", get(routes::history::listening_stats))
        // Favorites
        .route("/favorites", post(routes::favorites::toggle_favorite).get(routes::favorites::list_favorites))
        .route("/favorites/check", get(routes::favorites::check_favorite))
        // Featured
        .route("/featured-album", get(routes::featured::get_featured_album))
        .route("/featured-artist", get(routes::featured::get_featured_artist))
        // Daily Mixes
        .route("/mixes/daily", get(routes::daily_mixes::list_daily_mixes))
        .route("/mixes/daily/{id}", get(routes::daily_mixes::get_daily_mix))
        .route("/mixes/daily/{id}/save", post(routes::daily_mixes::save_mix_as_playlist))
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
        .route("/library/recommend", post(routes::library::trigger_recommendations))
        .route("/library/artist-recommendations", post(routes::library::trigger_artist_recommendations))
        .route("/library/artist-bios", post(routes::library::trigger_artist_bios))
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
        .layer(
            ServiceBuilder::new()
                .layer(axum::error_handling::HandleErrorLayer::new(|_: BoxError| async {
                    StatusCode::REQUEST_TIMEOUT
                }))
                .layer(TimeoutLayer::new(Duration::from_secs(30)))
        )
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
