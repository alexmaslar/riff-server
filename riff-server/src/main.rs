pub mod error;
mod middleware;
mod routes;

use anyhow::Result;
use axum::{
    extract::State,
    middleware as axum_mw,
    routing::{get, post},
    Json, Router,
};
use riff_core::{auth, config::Config, db, discogs, scanner};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

pub struct AppState {
    pub db: SqlitePool,
    pub config: Config,
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

    // Auto-enrich from Discogs in background
    if config.metadata.discogs.api_token.is_some() && config.metadata.discogs.auto_enrich {
        let enrich_pool = pool.clone();
        let discogs_config = config.metadata.discogs.clone();
        tokio::spawn(async move {
            match discogs::enrich_library(&enrich_pool, &discogs_config).await {
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
        });
    }

    let state = Arc::new(AppState {
        db: pool,
        config: config.clone(),
    });

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
        .route("/tracks/{id}/stream", get(routes::tracks::stream_track))
        .route("/tracks/{id}/download", get(routes::tracks::download_track))
        .route_layer(axum_mw::from_fn_with_state(
            state.clone(),
            middleware::require_auth,
        ));

    // Admin routes (require auth + admin role)
    let admin = Router::new()
        .route("/library/scan", post(routes::library::trigger_scan))
        .route("/library/enrich", post(routes::library::trigger_enrichment))
        .route("/library/enrich/{album_id}", post(routes::library::enrich_album))
        .route("/users", get(routes::users::list_users))
        .route("/users", post(routes::users::create_user))
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
