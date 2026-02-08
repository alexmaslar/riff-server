pub mod error;
mod discovery;
mod middleware;
mod routes;
mod tls;
mod upnp;

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    middleware as axum_mw,
    routing::{delete, get, post, put},
    BoxError, Json, Router,
};
use riff_core::{ai, analysis, auth, config::Config, daily_mixes, db, discogs, scanner};
use serde_json::{json, Value};
use socket2::{Domain, Protocol, Socket, TcpKeepalive, Type};
use sqlx::SqlitePool;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tower::ServiceBuilder;
use tower::timeout::TimeoutLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

pub struct AppState {
    pub db: SqlitePool,
    pub config: RwLock<Config>,
    pub enrichment_running: AtomicBool,
    pub analysis_running: AtomicBool,
    pub summarization_running: AtomicBool,
    pub rating_running: AtomicBool,
    pub recommendation_running: AtomicBool,
    pub artist_bio_running: AtomicBool,
    pub artist_recommendation_running: AtomicBool,
    pub remote_access: upnp::RemoteAccessManager,
}

async fn run_background_pipeline(state: Arc<AppState>) {
    // Step 1: Enrichment (if Discogs configured)
    {
        let config = state.config.read().await;
        if config.metadata.discogs.api_token.is_some() && config.metadata.discogs.auto_enrich {
            let discogs_config = config.metadata.discogs.clone();
            let db = state.db.clone();
            drop(config);
            run_stage(&state.enrichment_running, "enrichment", async move {
                discogs::enrich_library(&db, &discogs_config).await
            })
            .await;
        }
    }

    // Step 2: AI tasks (if AI configured, gated per-task)
    {
        let config = state.config.read().await;
        if config.metadata.ai.enabled {
            let ai_config = config.metadata.ai.clone();
            drop(config);

            let db = state.db.clone();

            if ai_config.album_summaries {
                run_stage(&state.summarization_running, "summarization", {
                    let ai_config = ai_config.clone();
                    let db = db.clone();
                    async move { ai::summarize_library(&db, &ai_config).await }
                })
                .await;
            }

            if ai_config.album_ratings {
                run_stage(&state.rating_running, "rating", {
                    let ai_config = ai_config.clone();
                    let db = db.clone();
                    async move { ai::rate_library(&db, &ai_config).await }
                })
                .await;
            }

            if ai_config.album_recommendations {
                run_stage(&state.recommendation_running, "recommendations", {
                    let ai_config = ai_config.clone();
                    let db = db.clone();
                    async move { ai::recommend_library(&db, &ai_config).await }
                })
                .await;
            }

            if ai_config.artist_recommendations {
                run_stage(&state.artist_recommendation_running, "artist recommendations", {
                    let ai_config = ai_config.clone();
                    let db = db.clone();
                    async move { ai::recommend_artists(&db, &ai_config).await }
                })
                .await;
            }

            if ai_config.artist_bios {
                run_stage(&state.artist_bio_running, "artist bios", {
                    let ai_config = ai_config.clone();
                    let db = db.clone();
                    async move { ai::bio_artists(&db, &ai_config).await }
                })
                .await;
            }
        }
    }

    // Step 3: Analysis (always)
    run_stage(&state.analysis_running, "analysis", {
        let db = state.db.clone();
        async move { analysis::analyze_library(&db).await }
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

/// Create a TCP listener with keepalive probes to survive cellular NAT timeouts.
/// Probes start after 15s idle, repeat every 5s, and give up after 3 failures (~30s total).
fn create_keepalive_listener(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.set_nodelay(true)?;
    socket.set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(Duration::from_secs(15))
            .with_interval(Duration::from_secs(5))
            .with_retries(3),
    )?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(std::net::TcpListener::from(socket))
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls crypto provider before anything uses TLS.
    // Both ring and aws-lc-rs features get activated via Cargo feature unification,
    // so we must explicitly pick one. This covers reqwest's outbound HTTPS calls.
    let _ = rustls::crypto::ring::default_provider().install_default();

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

    // Generate TLS certificate for HTTPS
    let (cert_path, key_path) = tls::ensure_certificate()?;
    let cert_fingerprint = tls::cert_fingerprint(&cert_path)?;
    tracing::info!("TLS cert fingerprint: {}", cert_fingerprint);

    let state = Arc::new(AppState {
        db: pool,
        config: RwLock::new(config.clone()),
        enrichment_running: AtomicBool::new(false),
        analysis_running: AtomicBool::new(false),
        summarization_running: AtomicBool::new(false),
        rating_running: AtomicBool::new(false),
        recommendation_running: AtomicBool::new(false),
        artist_bio_running: AtomicBool::new(false),
        artist_recommendation_running: AtomicBool::new(false),
        remote_access: upnp::RemoteAccessManager::new(),
    });

    // Set cert fingerprint on the remote access manager
    state
        .remote_access
        .set_cert_fingerprint(cert_fingerprint)
        .await;

    // Background pipeline: enrich → summarize → rate → analyze
    {
        let pipeline_state = state.clone();
        tokio::spawn(async move {
            run_background_pipeline(pipeline_state).await;
        });
    }

    // Start mDNS/Bonjour service advertisement
    let _mdns = match discovery::start_discovery(config.server.port) {
        Ok(mdns) => Some(mdns),
        Err(e) => {
            tracing::warn!("mDNS advertisement failed: {e}");
            None
        }
    };

    // Start remote access if enabled
    if config.remote_access.enabled {
        let ra_state = state.clone();
        let external_url = config.remote_access.external_url.clone();
        let method = config.remote_access.method.clone();
        tokio::spawn(async move {
            if let Err(e) = ra_state.remote_access.start(external_url, &method).await {
                tracing::warn!("remote access setup failed: {e}");
            }
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

    // Streaming routes — no timeout, no gzip (audio is already compressed;
    // gzip switches to chunked encoding which breaks Content-Length on iOS)
    let streaming = Router::new()
        .route("/tracks/{id}/stream", get(routes::tracks::stream_track))
        .route("/tracks/{id}/download", get(routes::tracks::download_track))
        .route_layer(axum_mw::from_fn_with_state(
            state.clone(),
            middleware::require_auth,
        ));

    // Protected routes (require valid JWT)
    let protected = Router::new()
        .route("/remote-access/status", get(routes::remote_access::get_status))
        .route("/artists", get(routes::artists::list_artists))
        .route("/artists/{id}", get(routes::artists::get_artist))
        .route("/albums", get(routes::albums::list_albums))
        .route("/albums/{id}", get(routes::albums::get_album))
        .route("/albums/{id}/play", post(routes::albums::increment_play_count))
        .route("/playlists", get(routes::playlists::list_playlists).post(routes::playlists::create_playlist))
        .route("/playlists/{id}", get(routes::playlists::get_playlist).delete(routes::playlists::delete_playlist))
        .route("/playlists/{id}/tracks", post(routes::playlists::add_track).put(routes::playlists::reorder_tracks))
        .route("/playlists/{id}/tracks/{track_id}", delete(routes::playlists::remove_track))
        // Smart Playlists (AI)
        .route("/playlists/ai/suggestions", get(routes::smart_playlist::get_suggestions))
        .route("/playlists/ai/generate", post(routes::smart_playlist::generate))
        .route("/playlists/ai/refine", post(routes::smart_playlist::refine))
        .route("/playlists/ai/save", post(routes::smart_playlist::save))
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
        .route("/featured-albums", get(routes::featured::get_featured_albums))
        .route("/featured-artists", get(routes::featured::get_featured_artists))
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
        .route("/remote-access/enable", post(routes::remote_access::enable))
        .route("/remote-access/disable", post(routes::remote_access::disable))
        .route("/remote-access/configure", put(routes::remote_access::configure))
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
        .route("/library/ai/clear", post(routes::library::clear_ai_data))
        .route("/config", get(routes::config::get_config).put(routes::config::update_config))
        .route("/users", get(routes::users::list_users))
        .route("/users", post(routes::users::create_user))
        .route("/users/{id}", delete(routes::users::delete_user))
        .route_layer(axum_mw::from_fn(middleware::require_admin))
        .route_layer(axum_mw::from_fn_with_state(
            state.clone(),
            middleware::require_auth,
        ));

    // Apply timeout + gzip compression only to JSON API routes (not streaming)
    let timed_routes = Router::new()
        .merge(protected)
        .merge(admin)
        .layer(
            ServiceBuilder::new()
                .layer(axum::error_handling::HandleErrorLayer::new(|_: BoxError| async {
                    StatusCode::REQUEST_TIMEOUT
                }))
                .layer(TimeoutLayer::new(Duration::from_secs(120)))
        )
        .layer(CompressionLayer::new().gzip(true));

    let app = Router::new()
        .merge(public)
        .merge(streaming)      // no timeout, no gzip — raw bytes with accurate Content-Length
        .merge(timed_routes)   // 120s timeout + gzip for JSON API responses
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state.clone());

    // HTTP server (local LAN access) — with TCP keepalive for cellular resilience
    let http_addr: SocketAddr = format!("0.0.0.0:{}", config.server.port).parse()?;
    let std_listener = create_keepalive_listener(http_addr)?;
    std_listener.set_nonblocking(true)?;
    let http_listener = tokio::net::TcpListener::from_std(std_listener)?;
    tracing::info!("HTTP listening on {}", http_addr);

    // HTTPS server (remote access via UPnP) — with TCP keepalive for cellular resilience
    // Build rustls ServerConfig with explicit ring provider to avoid
    // CryptoProvider auto-detection failure when both ring and aws-lc-rs
    // features are enabled via Cargo feature unification.
    let https_addr: SocketAddr = "0.0.0.0:8443".parse()?;
    let https_listener = create_keepalive_listener(https_addr)?;
    let tls_config = tls::build_rustls_config(&cert_path, &key_path)?;
    tracing::info!("HTTPS listening on {}", https_addr);

    let http_app = app.clone();

    // Run HTTP and HTTPS listeners as independent tasks so one failing
    // doesn't take down the other.
    let http_task = tokio::spawn(async move {
        if let Err(e) = axum::serve(http_listener, http_app).await {
            tracing::error!("HTTP server error: {e}");
        }
    });

    let https_task = tokio::spawn(async move {
        if let Err(e) = axum_server::from_tcp_rustls(https_listener, tls_config)
            .serve(app.into_make_service())
            .await
        {
            tracing::error!("HTTPS server error: {e}");
        }
    });

    // Wait for shutdown signal
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down...");

    // Cleanup UPnP
    state.remote_access.stop().await;

    // Abort both listeners
    http_task.abort();
    https_task.abort();

    Ok(())
}

async fn health(State(_state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
