pub mod error;
mod discovery;
mod middleware;
pub mod pipeline;
mod routes;
mod relay;
mod relay_protocol;
mod tls;
mod transcode;

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    middleware as axum_mw,
    routing::{delete, get, post, put},
    BoxError, Json, Router,
};
use riff_core::{auth, config::Config, db, plugin, scanner};
use serde_json::{json, Value};
use socket2::{Domain, Protocol, Socket, TcpKeepalive, Type};
use sqlx::SqlitePool;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, RwLock};
use tower::ServiceBuilder;
use tower::timeout::TimeoutLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;
use tracing_subscriber::{fmt, EnvFilter, prelude::*};

pub struct StageManager {
    enrichment: AtomicBool,
    editorial: AtomicBool,
    analysis: AtomicBool,
    recommendation: AtomicBool,
    artist_recommendation: AtomicBool,
    top_tracks: AtomicBool,
    listenbrainz: AtomicBool,
    scan: AtomicBool,
}

impl Default for StageManager {
    fn default() -> Self {
        Self::new()
    }
}

impl StageManager {
    pub fn new() -> Self {
        Self {
            enrichment: AtomicBool::new(false),
            editorial: AtomicBool::new(false),
            analysis: AtomicBool::new(false),
            recommendation: AtomicBool::new(false),
            artist_recommendation: AtomicBool::new(false),
            top_tracks: AtomicBool::new(false),
            listenbrainz: AtomicBool::new(false),
            scan: AtomicBool::new(false),
        }
    }

    pub fn try_start(&self, stage: &str) -> bool {
        let flag = match stage {
            "enrichment" => &self.enrichment,
            "editorial" => &self.editorial,
            "analysis" => &self.analysis,
            "recommendation" => &self.recommendation,
            "artist_recommendation" => &self.artist_recommendation,
            "top_tracks" => &self.top_tracks,
            "listenbrainz" => &self.listenbrainz,
            "scan" => &self.scan,
            _ => return false,
        };
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn finish(&self, stage: &str) {
        let flag = match stage {
            "enrichment" => &self.enrichment,
            "editorial" => &self.editorial,
            "analysis" => &self.analysis,
            "recommendation" => &self.recommendation,
            "artist_recommendation" => &self.artist_recommendation,
            "top_tracks" => &self.top_tracks,
            "listenbrainz" => &self.listenbrainz,
            "scan" => &self.scan,
            _ => return,
        };
        flag.store(false, Ordering::SeqCst);
    }
}

pub struct AppState {
    pub db: SqlitePool,
    pub config: RwLock<Config>,
    pub jwt_secret: String,
    pub stage_manager: StageManager,
    pub http_client: reqwest::Client,
    pub restart: Notify,
    pub ffmpeg_available: bool,
    pub ffmpeg_has_fdk_aac: bool,
    pub transcode_semaphore: Arc<tokio::sync::Semaphore>,
    pub hls_generating: tokio::sync::Mutex<std::collections::HashSet<String>>,
    pub login_guard: tokio::sync::Mutex<routes::auth::LoginGuard>,
    pub plugin_registry: RwLock<plugin::registry::PluginRegistry>,
    pub event_bus: plugin::events::EventBus,
    /// Wakes the background pipeline when new content arrives (scan, download, manual trigger).
    pub pipeline_notify: Notify,
    /// Cache for resolved library IDs (invalidated on library add/remove/modify).
    pub library_ids_cache: RwLock<Option<Vec<String>>>,
    /// TLS certificate fingerprint (SHA-256, base64).
    pub cert_fingerprint: String,
    /// Whether the relay tunnel is currently connected.
    pub relay_connected: AtomicBool,
}

// Background pipeline functions are in pipeline.rs
use pipeline::{run_background_pipeline, run_periodic_scanner, run_daily_refresh};

/// Create a TCP listener with keepalive probes to survive cellular NAT timeouts.
/// Probes start after 5s idle, repeat every 5s, and give up after 6 failures (~35s total).
/// Early probes (5s) keep cellular NAT mappings alive — waiting 15s risks the NAT
/// dropping the mapping before the first probe even fires.
fn create_keepalive_listener(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.set_nodelay(true)?;
    socket.set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(Duration::from_secs(5))
            .with_interval(Duration::from_secs(5))
            .with_retries(6),
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

    let fmt_layer = fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .compact();

    let filter_layer = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,hyper=warn,tower_http=debug"));

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .init();

    let config = Config::load()?;
    tracing::info!(port = config.server.port, "config loaded");

    let pool = db::init_pool().await?;
    tracing::info!("database initialized");

    // Sync libraries from config to DB
    let resolved_libs = config.resolved_libraries();
    if !resolved_libs.is_empty() {
        db::sync_libraries(&pool, &resolved_libs).await?;
    }

    // Bootstrap admin user on first run
    auth::bootstrap_admin(&pool, &config).await?;

    // Scan all libraries on startup
    for lib_entry in &resolved_libs {
        let library_id = db::get_library_id_by_path(&pool, &lib_entry.path)
            .await?
            .unwrap_or_default();
        if library_id.is_empty() {
            tracing::warn!(path = %lib_entry.path, "no library_id found, skipping scan");
            continue;
        }
        tracing::info!(library = %lib_entry.name, path = %lib_entry.path, "scanning library");
        match scanner::scan_library(&pool, &lib_entry.path, &library_id).await {
            Ok(result) => {
                tracing::info!(
                    library = %lib_entry.name,
                    artists_added = result.artists_added,
                    albums_added = result.albums_added,
                    tracks_added = result.tracks_added,
                    tracks_removed = result.tracks_removed,
                    albums_removed = result.albums_removed,
                    artists_removed = result.artists_removed,
                    "scan complete",
                );
            }
            Err(e) => tracing::warn!(library = %lib_entry.name, error = %e, "library scan failed"),
        }
    }

    // Check for FFmpeg availability and codec support
    let ffmpeg_available = tokio::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);

    let ffmpeg_has_fdk_aac = if ffmpeg_available {
        // Probe for libfdk_aac — substantially better quality than native aac below 192kbps
        let output = tokio::process::Command::new("ffmpeg")
            .args(["-encoders"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .ok();
        output
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.contains("libfdk_aac"))
            .unwrap_or(false)
    } else {
        false
    };

    if ffmpeg_available {
        let codec = if ffmpeg_has_fdk_aac { "libfdk_aac" } else { "native aac" };
        tracing::info!(codec = codec, "ffmpeg found, remote transcoding enabled");
    } else {
        tracing::warn!("ffmpeg not found — remote clients will receive lossless files");
    }

    // Generate TLS certificate for HTTPS
    let (cert_path, key_path) = tls::ensure_certificate()?;
    let cert_fingerprint = tls::cert_fingerprint(&cert_path)?;
    tracing::info!(fingerprint = %cert_fingerprint, "TLS cert loaded");

    let max_transcodes = config.streaming.max_transcode_processes;
    let event_bus = plugin::events::EventBus::new(512);

    // Register built-in editorial providers
    let mut plugin_registry = plugin::registry::PluginRegistry::new();
    plugin_registry.register_editorial(Arc::new(riff_core::editorial::PitchforkProvider::new()));
    plugin_registry.register_editorial(Arc::new(riff_core::editorial::AllMusicProvider::new()));
    plugin_registry.register_editorial(Arc::new(riff_core::editorial::NorthernTransmissionsProvider::new()));
    plugin_registry.register_editorial(Arc::new(riff_core::editorial::TheLineOfBestFitProvider::new()));
    tracing::info!("registered {} editorial providers", plugin_registry.editorial_providers().len());

    let http_client = reqwest::Client::builder().build().expect("reqwest client build with default config");

    let state = Arc::new(AppState {
        db: pool,
        config: RwLock::new(config.clone()),
        jwt_secret: config.auth.jwt_secret.clone(),
        stage_manager: StageManager::new(),
        http_client,
        restart: Notify::new(),
        ffmpeg_available,
        ffmpeg_has_fdk_aac,
        transcode_semaphore: Arc::new(tokio::sync::Semaphore::new(max_transcodes)),
        hls_generating: tokio::sync::Mutex::new(std::collections::HashSet::new()),
        login_guard: tokio::sync::Mutex::new(routes::auth::LoginGuard::new()),
        plugin_registry: RwLock::new(plugin_registry),
        event_bus,
        pipeline_notify: Notify::new(),
        library_ids_cache: RwLock::new(None),
        cert_fingerprint: cert_fingerprint.clone(),
        relay_connected: AtomicBool::new(false),
    });

    // Background pipeline: enrich → recommend → analyze → daily mixes
    {
        let pipeline_state = state.clone();
        tokio::spawn(async move {
            run_background_pipeline(pipeline_state).await;
        });
    }

    // Periodic library scanner
    {
        let scanner_state = state.clone();
        tokio::spawn(async move {
            run_periodic_scanner(scanner_state).await;
        });
    }

    // Daily mix refresh at midnight UTC
    {
        let daily_state = state.clone();
        tokio::spawn(async move {
            run_daily_refresh(daily_state).await;
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
        // Restrictive by default: no cross-origin requests unless configured.
        // The iOS app uses native HTTP (not CORS), so this has no impact on normal usage.
        CorsLayer::new()
    };

    // Public routes (no auth required)
    let public = Router::new()
        .route("/health", get(health))
        .route("/auth/login", post(routes::auth::login))
        .route("/auth/refresh", post(routes::auth::refresh))
        .route("/albums/{id}/cover", get(routes::albums::get_cover))
        .route("/mixes/daily/{id}/cover", get(routes::daily_mixes::get_mix_cover))
        .route("/playlists/{id}/cover", get(routes::playlists::get_cover))
        .route("/plugins/{name}/icon", get(routes::plugins::get_plugin_icon));

    // SSE event stream — no timeout (long-lived), no gzip, auth required
    let sse = Router::new()
        .route("/events", get(routes::events::event_stream))
        .route_layer(axum_mw::from_fn_with_state(
            state.clone(),
            middleware::require_auth,
        ));

    // Streaming routes — generous timeout, no gzip (audio is already compressed;
    // gzip switches to chunked encoding which breaks Content-Length on iOS)
    let streaming = Router::new()
        .route("/tracks/{id}/stream", get(routes::tracks::stream_track))
        .route("/tracks/{id}/download", get(routes::tracks::download_track))
        .route("/tracks/{id}/hls/playlist.m3u8", get(routes::hls::master_playlist))
        .route("/tracks/{id}/hls/{variant}/playlist.m3u8", get(routes::hls::variant_playlist))
        .route("/tracks/{id}/hls/{variant}/{segment}", get(routes::hls::serve_segment))
        .route_layer(axum_mw::from_fn_with_state(
            state.clone(),
            middleware::require_auth,
        ))
        .layer(
            ServiceBuilder::new()
                .layer(axum::error_handling::HandleErrorLayer::new(|_: BoxError| async {
                    StatusCode::REQUEST_TIMEOUT
                }))
                .layer(TimeoutLayer::new(Duration::from_secs(600)))
        );

    // Protected routes (require valid JWT)
    let protected = Router::new()
        .route("/artists", get(routes::artists::list_artists))
        .route("/artists/stories", get(routes::artists::artist_stories))
        .route("/artists/{id}", get(routes::artists::get_artist))
        .route("/artists/{id}/streaming-albums", get(routes::artists::get_streaming_albums))
        .route("/albums", get(routes::albums::list_albums))
        .route("/albums/filters", get(routes::albums::list_filters))
        .route("/genres", get(routes::albums::list_genres))
        .route("/albums/{id}", get(routes::albums::get_album))
        .route("/albums/{id}/play", post(routes::albums::increment_play_count))
        .route("/albums/{id}/reviews/hidden", get(routes::albums::get_hidden_reviews))
        .route("/albums/{id}/reviews/{source}/toggle-hidden", post(routes::albums::toggle_review_visibility))
        .route("/albums/{id}/consensus", put(routes::albums::save_consensus))
        .route("/tracks/{id}/report-decode-error", post(routes::tracks::report_decode_error))
        .route("/playlists", get(routes::playlists::list_playlists).post(routes::playlists::create_playlist))
        .route("/playlists/{id}", get(routes::playlists::get_playlist).delete(routes::playlists::delete_playlist))
        .route("/playlists/{id}/tracks", post(routes::playlists::add_track).put(routes::playlists::reorder_tracks))
        .route("/playlists/{id}/tracks/{track_id}", delete(routes::playlists::remove_track))
        // Smart Playlists (AI)
        .route("/playlists/ai/suggestions", get(routes::smart_playlist::get_suggestions))
        .route("/playlists/ai/save", post(routes::smart_playlist::save))
        .route("/playlists/generate", post(routes::smart_playlist::generate))
        // History
        .route("/history", post(routes::history::record_play))
        .route("/history/albums", get(routes::history::recently_played_albums))
        .route("/history/continue", get(routes::history::continue_listening))
        .route("/history/stats", get(routes::history::listening_stats))
        // Download recommendations
        .route("/recommendations/downloads", get(routes::history::download_recommendations))
        // Favorites
        .route("/favorites", post(routes::favorites::toggle_favorite).get(routes::favorites::list_favorites))
        .route("/favorites/check", get(routes::favorites::check_favorite))
        // Featured
        .route("/featured-album", get(routes::featured::get_featured_album))
        .route("/featured-artist", get(routes::featured::get_featured_artist))
        .route("/featured-albums", get(routes::featured::get_featured_albums))
        .route("/featured-artists", get(routes::featured::get_featured_artists))
        // Podcast Backup
        .route("/podcasts/backup", get(routes::podcasts::get_backup).put(routes::podcasts::save_backup))
        // Autoqueue
        .route("/autoqueue", get(routes::autoqueue::get_autoqueue))
        // Daily Mixes
        .route("/mixes/daily", get(routes::daily_mixes::list_daily_mixes))
        .route("/mixes/daily/{id}", get(routes::daily_mixes::get_daily_mix))
        .route("/mixes/daily/{id}/save", post(routes::daily_mixes::save_mix_as_playlist))
        // Downloads
        .route("/downloads", get(routes::downloads::list_downloads).post(routes::downloads::add_download))
        .route("/downloads/{id}", delete(routes::downloads::cancel_download))
        // User account (self-service)
        .route("/user/account", put(routes::users::update_account))
        // Libraries (read-only for non-admin)
        .route("/libraries", get(routes::libraries::list_libraries))
        // Relay info
        .route("/relay-info", get(routes::relay::relay_info))
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
        .route("/library/editorial", post(routes::library::trigger_editorial))
        .route("/library/recommend", post(routes::library::trigger_recommendations))
        .route("/library/artist-recommendations", post(routes::library::trigger_artist_recommendations))
        .route("/library/stats", get(routes::library::library_stats))
        .route("/library/clear-data", post(routes::library::clear_data))
        // Libraries (admin CRUD)
        .route("/libraries", post(routes::libraries::add_library))
        .route("/libraries/{id}", put(routes::libraries::update_library).delete(routes::libraries::remove_library))
        .route("/libraries/{id}/scan", post(routes::libraries::scan_single_library))
        .route("/albums/{id}", delete(routes::albums::delete_album))
        .route("/albums/{id}/refresh-cover", post(routes::albums::refresh_cover))
        .route("/config", get(routes::config::get_config).put(routes::config::update_config))
        .route("/plugins/catalog", get(routes::plugins::catalog))
        .route("/plugins/status", get(routes::plugins::status))
        .route("/users", get(routes::users::list_users))
        .route("/users", post(routes::users::create_user))
        .route("/users/{id}", delete(routes::users::delete_user))
        .route_layer(axum_mw::from_fn(middleware::require_admin))
        .route_layer(axum_mw::from_fn_with_state(
            state.clone(),
            middleware::require_auth,
        ));

    // Apply timeout + gzip compression + body limit only to JSON API routes (not streaming)
    let timed_routes = Router::new()
        .merge(protected)
        .merge(admin)
        .layer(
            ServiceBuilder::new()
                .layer(axum::error_handling::HandleErrorLayer::new(|_: BoxError| async {
                    StatusCode::REQUEST_TIMEOUT
                }))
                .layer(TimeoutLayer::new(Duration::from_secs(30)))
        )
        .layer(RequestBodyLimitLayer::new(10 * 1024 * 1024)) // 10MB body limit for API routes
        .layer(CompressionLayer::new().gzip(true));

    let app = Router::new()
        .merge(public)
        .merge(sse)            // no timeout — long-lived SSE connections
        .merge(streaming)      // no timeout, no gzip — raw bytes with accurate Content-Length
        .merge(timed_routes)   // 30s timeout + gzip for JSON API responses
        .layer(CatchPanicLayer::new())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(DefaultOnRequest::new().level(Level::INFO))
                .on_response(
                    DefaultOnResponse::new()
                        .level(Level::INFO)
                        .latency_unit(tower_http::LatencyUnit::Millis),
                ),
        )
        .layer(cors)
        .with_state(state.clone());

    // Relay tunnel (outbound WS to relay service for remote access)
    {
        let relay_state = state.clone();
        let relay_router = app.clone();
        tokio::spawn(async move {
            relay::run_relay_tunnel(relay_state, relay_router).await;
        });
    }

    // HTTP server (local LAN access) — with TCP keepalive for cellular resilience
    let http_addr: SocketAddr = format!("0.0.0.0:{}", config.server.port).parse()?;
    let std_listener = create_keepalive_listener(http_addr)?;
    std_listener.set_nonblocking(true)?;
    let http_listener = tokio::net::TcpListener::from_std(std_listener)?;
    tracing::info!(addr = %http_addr, "HTTP listening");

    // HTTPS server (remote access via UPnP) — with TCP keepalive for cellular resilience
    // Build rustls ServerConfig with explicit ring provider to avoid
    // CryptoProvider auto-detection failure when both ring and aws-lc-rs
    // features are enabled via Cargo feature unification.
    let https_addr: SocketAddr = format!("0.0.0.0:{}", config.server.https_port).parse()?;
    let https_listener = create_keepalive_listener(https_addr)?;
    https_listener.set_nonblocking(true)?;
    let tls_config = tls::build_rustls_config(&cert_path, &key_path)?;
    tracing::info!(addr = %https_addr, "HTTPS listening");

    let http_app = app.clone().layer(axum_mw::from_fn(middleware::mark_local));

    let https_app = app.layer(axum_mw::from_fn(middleware::mark_remote));

    // Run HTTP and HTTPS listeners as independent tasks so one failing
    // doesn't take down the other.
    let http_task = tokio::spawn(async move {
        if let Err(e) = axum::serve(http_listener, http_app).await {
            tracing::error!("HTTP server error: {e}");
        }
    });

    let https_port = config.server.https_port;
    let https_task = tokio::spawn(async move {
        // Build acceptor with handshake timeout — bots that connect but never
        // send a TLS ClientHello will be dropped after 10s instead of blocking
        // the acceptor indefinitely.
        let acceptor = axum_server::tls_rustls::RustlsAcceptor::new(tls_config)
            .handshake_timeout(Duration::from_secs(10));
        let mut server = axum_server::Server::from_tcp(https_listener).acceptor(acceptor);
        server.http_builder().http1().keep_alive(true);
        match server.serve(https_app.into_make_service_with_connect_info::<SocketAddr>()).await {
            Ok(()) => tracing::error!("HTTPS serve loop exited without error (should not happen)"),
            Err(e) => tracing::error!("HTTPS serve loop exited unexpectedly: {e}"),
        }
    });

    // Periodic self-test: TCP-connect to the HTTPS port every 60s to detect silent acceptor stalls
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            match tokio::net::TcpStream::connect(("127.0.0.1", https_port)).await {
                Ok(_) => {}
                Err(e) => tracing::error!("HTTPS self-test FAILED on port {https_port}: {e}"),
            }
        }
    });

    // Wait for shutdown or restart signal
    let restarting = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down...");
            false
        }
        _ = state.restart.notified() => {
            tracing::info!("restarting server...");
            true
        }
    };

    // Abort both listeners
    http_task.abort();
    https_task.abort();

    if restarting {
        // Kill dns-sd child process before exec() replaces us
        drop(_mdns);

        // Re-exec the same binary so it picks up the new config
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe().expect("failed to get current executable path");
        let args: Vec<String> = std::env::args().skip(1).collect();
        let err = std::process::Command::new(&exe).args(&args).exec();
        // exec() only returns on failure
        tracing::error!("exec failed: {err}");
        std::process::exit(1);
    }

    Ok(())
}

async fn health(State(_state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
