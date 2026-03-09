pub mod error;
mod discovery;
mod middleware;
pub mod pipeline;
mod plugin_reload;
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
use riff_core::plugin::catalog::RemotePluginEntry;
use riff_core::{auth, config::Config, db, musicbrainz, plugin, scanner::{self, metadata::{detect_library_convention, build_album_dir, build_track_filename, write_flac_tags}}};
use serde_json::{json, Value};
use socket2::{Domain, Protocol, Socket, TcpKeepalive, Type};
use sqlx::SqlitePool;
use std::net::SocketAddr;
use std::path::Path;
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
    pub remote_catalog: RwLock<Vec<RemotePluginEntry>>,
    pub event_bus: plugin::events::EventBus,
    /// Wakes the background pipeline when new content arrives (scan, download, manual trigger).
    pub pipeline_notify: Notify,
    /// Names of plugins loaded from dev_plugins config (invisible to non-admin users).
    pub dev_plugin_names: RwLock<std::collections::HashSet<String>>,
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
    let plugin_registry = plugin::registry::PluginRegistry::new();

    // Fetch community plugin catalog (non-blocking, empty on failure)
    let remote_catalog = plugin::catalog::fetch_remote_catalog().await;

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
        remote_catalog: RwLock::new(remote_catalog),
        event_bus,
        pipeline_notify: Notify::new(),
        dev_plugin_names: RwLock::new(std::collections::HashSet::new()),
        library_ids_cache: RwLock::new(None),
        cert_fingerprint: cert_fingerprint.clone(),
        relay_connected: AtomicBool::new(false),
    });


    // Download and load enabled WASM plugins from the remote catalog
    let plugin_results = plugin_reload::reload_wasm_plugins(&state).await;
    for (name, result) in &plugin_results {
        if !result.healthy {
            tracing::warn!(
                plugin = %name,
                error = result.message.as_deref().unwrap_or("unknown error"),
                "plugin unhealthy",
            );
        }
    }

    // Load dev plugins from local paths
    let (dev_results, dev_names) = plugin_reload::reload_dev_plugins(&state).await;
    for (name, result) in &dev_results {
        if result.loaded {
            tracing::info!(plugin = %name, healthy = result.healthy, "dev plugin loaded");
        } else {
            tracing::warn!(
                plugin = %name,
                error = result.message.as_deref().unwrap_or("failed to load"),
                "dev plugin failed",
            );
        }
    }
    *state.dev_plugin_names.write().await = dev_names;

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

    // Download queue processor (streaming provider downloads)
    {
        let dl_state = state.clone();
        tokio::spawn(async move {
            run_download_processor(dl_state).await;
        });
    }

    // Periodic remote catalog refresh (every 6 hours)
    {
        let catalog_state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
                let entries = plugin::catalog::fetch_remote_catalog().await;
                *catalog_state.remote_catalog.write().await = entries;
            }
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
        // Streaming provider proxy (Tidal/Qobuz CDN)
        .route("/streaming/tracks/{provider}/{id}/stream", get(routes::streaming::stream_track))
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
        // Streaming providers (Tidal/Qobuz)
        .route("/streaming/search", get(routes::streaming::search))
        .route("/streaming/albums/{provider}/{id}", get(routes::streaming::get_album))
        .route("/streaming/artists/{provider}/{id}/albums", get(routes::streaming::get_artist_albums))
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
        .route("/plugins/{name}/reload", post(routes::plugins::reload_dev_plugin))
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

async fn cleanup_cancelled_download(db: &SqlitePool, album_dir: &Path, local_album_id: Option<&str>) {
    // Delete album directory from disk
    if album_dir.exists() {
        if let Err(e) = tokio::fs::remove_dir_all(album_dir).await {
            tracing::warn!("cleanup: failed to delete {}: {e}", album_dir.display());
        }
    }
    // Delete DB records if album was already scanned
    if let Some(album_id) = local_album_id {
        let artist: Option<(String,)> =
            sqlx::query_as("SELECT artist_id FROM albums WHERE id = ?")
                .bind(album_id)
                .fetch_optional(db)
                .await
                .unwrap_or(None);

        let track_ids: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM tracks WHERE album_id = ?")
                .bind(album_id)
                .fetch_all(db)
                .await
                .unwrap_or_default();

        // Delete track-referencing rows
        for (tid,) in &track_ids {
            sqlx::query("DELETE FROM play_history WHERE track_id = ?")
                .bind(tid).execute(db).await.ok();
            sqlx::query("DELETE FROM playlist_tracks WHERE track_id = ?")
                .bind(tid).execute(db).await.ok();
            sqlx::query("DELETE FROM daily_mix_tracks WHERE track_id = ?")
                .bind(tid).execute(db).await.ok();
            sqlx::query("DELETE FROM favorites WHERE entity_type = 'track' AND entity_id = ?")
                .bind(tid).execute(db).await.ok();
        }

        // Delete album-referencing rows (FK cascades not enforced without PRAGMA foreign_keys)
        sqlx::query("DELETE FROM album_credits WHERE album_id = ?")
            .bind(album_id).execute(db).await.ok();
        sqlx::query("DELETE FROM album_recommendations WHERE album_id = ? OR recommended_album_id = ?")
            .bind(album_id).bind(album_id).execute(db).await.ok();
        sqlx::query("DELETE FROM favorites WHERE entity_type = 'album' AND entity_id = ?")
            .bind(album_id).execute(db).await.ok();

        sqlx::query("DELETE FROM tracks WHERE album_id = ?")
            .bind(album_id).execute(db).await.ok();
        sqlx::query("DELETE FROM albums WHERE id = ?")
            .bind(album_id).execute(db).await.ok();

        // Delete artist if orphaned (no remaining albums)
        if let Some((artist_id,)) = artist {
            let count: Option<(i64,)> =
                sqlx::query_as("SELECT COUNT(*) FROM albums WHERE artist_id = ?")
                    .bind(&artist_id)
                    .fetch_optional(db)
                    .await
                    .unwrap_or(None);
            if count.map(|c| c.0).unwrap_or(0) == 0 {
                sqlx::query("DELETE FROM artist_recommendations WHERE artist_id = ? OR recommended_artist_id = ?")
                    .bind(&artist_id).bind(&artist_id).execute(db).await.ok();
                sqlx::query("DELETE FROM artist_top_tracks WHERE artist_id = ?")
                    .bind(&artist_id).execute(db).await.ok();
                sqlx::query("DELETE FROM favorites WHERE entity_type = 'artist' AND entity_id = ?")
                    .bind(&artist_id).execute(db).await.ok();
                sqlx::query("DELETE FROM artists WHERE id = ?")
                    .bind(&artist_id).execute(db).await.ok();
            }
        }
    }
}

async fn run_download_processor(state: Arc<AppState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;

        // Reset stuck processing items (>30 min without completing)
        let _ = sqlx::query(
            "UPDATE download_queue SET status = 'completed', processing_stage = 'complete', completed_at = datetime('now') \
             WHERE status = 'processing' AND completed_at IS NULL AND created_at < datetime('now', '-30 minutes')"
        )
        .execute(&state.db)
        .await;

        // Re-queue downloads stuck in 'downloading' for >10 min (e.g. server crashed mid-download)
        let recovered = sqlx::query(
            "UPDATE download_queue SET status = 'queued', tracks_completed = 0, current_track = NULL, error = NULL \
             WHERE status = 'downloading' AND created_at < datetime('now', '-10 minutes')"
        )
        .execute(&state.db)
        .await;
        if let Ok(r) = recovered {
            if r.rows_affected() > 0 {
                tracing::info!(count = r.rows_affected(), "re-queued interrupted downloads");
            }
        }

        // Fetch next queued download
        let row: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT id, provider, provider_album_id, quality, library_id FROM download_queue WHERE status = 'queued' ORDER BY created_at ASC LIMIT 1"
        )
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);

        let Some((dl_id, provider_name, album_id, quality, library_id)) = row else {
            continue;
        };

        // Mark as downloading
        let _ = sqlx::query("UPDATE download_queue SET status = 'downloading' WHERE id = ?")
            .bind(&dl_id)
            .execute(&state.db)
            .await;

        // Find the provider
        let registry = state.plugin_registry.read().await;
        let provider = registry
            .streaming_providers()
            .iter()
            .find(|p| p.provider_name() == provider_name)
            .cloned();
        drop(registry);

        let Some(provider) = provider else {
            let _ = sqlx::query("UPDATE download_queue SET status = 'failed', error = ? WHERE id = ?")
                .bind(format!("provider '{provider_name}' not loaded"))
                .bind(&dl_id)
                .execute(&state.db)
                .await;
            continue;
        };

        // Fetch album detail
        let detail = match provider.get_album(&album_id).await {
            Ok(d) => d,
            Err(e) => {
                let _ = sqlx::query("UPDATE download_queue SET status = 'failed', error = ? WHERE id = ?")
                    .bind(format!("album fetch failed: {e}"))
                    .bind(&dl_id)
                    .execute(&state.db)
                    .await;
                continue;
            }
        };

        // Determine library (id, path) — use the requested library, fall back to largest
        let resolved_lib: Option<(String, String)> = if let Some(ref lib_id) = library_id {
            sqlx::query_as::<_, (String, String)>("SELECT id, path FROM libraries WHERE id = ?")
                .bind(lib_id)
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        let (resolved_library_id, library_path) = match resolved_lib {
            Some((id, path)) => (id, path),
            None => {
                sqlx::query_as::<_, (String, String)>(
                    "SELECT l.id, l.path FROM libraries l
                     LEFT JOIN albums a ON a.library_id = l.id
                     GROUP BY l.id ORDER BY COUNT(a.id) DESC LIMIT 1"
                )
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| ("".to_string(), "/tmp/riff-downloads".to_string()))
            }
        };

        let streaming_quality = match quality.to_lowercase().as_str() {
            "hires" | "hi_res" | "hi_res_lossless" | "27" => riff_core::plugin::capabilities::StreamingQuality::HiRes,
            "lossless" | "6" => riff_core::plugin::capabilities::StreamingQuality::Lossless,
            "high" | "5" => riff_core::plugin::capabilities::StreamingQuality::High,
            _ => riff_core::plugin::capabilities::StreamingQuality::Lossless,
        };

        let lib_path = std::path::Path::new(&library_path);
        let convention = detect_library_convention(lib_path);
        let album_dir = build_album_dir(lib_path, &detail.album.artist.name, &detail.album.title, convention);

        let mut completed = 0i64;
        let mut failed = false;

        for track in &detail.tracks {
            // Check if cancelled
            let status: Option<(String,)> =
                sqlx::query_as("SELECT status FROM download_queue WHERE id = ?")
                    .bind(&dl_id)
                    .fetch_optional(&state.db)
                    .await
                    .unwrap_or(None);
            if status.as_ref().map(|s| s.0.as_str()) == Some("cancelled") {
                break;
            }

            let filename = build_track_filename(
                &detail.album.artist.name,
                &detail.album.title,
                track.track_number as i32,
                &track.title,
                "flac",
                convention,
            );
            let dest = album_dir.join(&filename);

            // Update current track
            let _ = sqlx::query("UPDATE download_queue SET current_track = ? WHERE id = ?")
                .bind(&track.title)
                .bind(&dl_id)
                .execute(&state.db)
                .await;

            match provider.download_track(&track.provider_id, streaming_quality, &dest).await {
                Ok(()) => {
                    // Write metadata tags so the scanner reads proper title/track numbers
                    if let Err(e) = write_flac_tags(
                        &dest,
                        &detail.album.artist.name,
                        &detail.album.title,
                        &track.title,
                        track.track_number,
                        track.disc_number,
                        detail.album.year,
                    ) {
                        tracing::warn!(track = %track.title, error = %e, "failed to write tags");
                    }
                    completed += 1;
                    let _ = sqlx::query("UPDATE download_queue SET tracks_completed = ? WHERE id = ?")
                        .bind(completed)
                        .bind(&dl_id)
                        .execute(&state.db)
                        .await;
                }
                Err(e) => {
                    tracing::warn!(track = %track.title, error = %e, "download track failed");
                    let _ = sqlx::query("UPDATE download_queue SET status = 'failed', error = ? WHERE id = ?")
                        .bind(format!("track '{}' failed: {e}", track.title))
                        .bind(&dl_id)
                        .execute(&state.db)
                        .await;
                    failed = true;
                    break;
                }
            }
        }

        if failed {
            continue;
        }

        // Check if cancelled during track downloads
        let was_cancelled = {
            let st: Option<(String,)> =
                sqlx::query_as("SELECT status FROM download_queue WHERE id = ?")
                    .bind(&dl_id)
                    .fetch_optional(&state.db)
                    .await
                    .unwrap_or(None);
            st.as_ref().map(|s| s.0.as_str()) == Some("cancelled")
        };
        if was_cancelled {
            tracing::info!(download_id = %dl_id, tracks = completed, "download cancelled, cleaning up");
            cleanup_cancelled_download(&state.db, &album_dir, None).await;
            continue;
        }

        // Download cover art
        if let Some(ref cover_url) = detail.album.cover_url {
            let cover_dest = album_dir.join("cover.jpg");
            if let Ok(resp) = state.http_client.get(cover_url).send().await {
                if let Ok(bytes) = resp.bytes().await {
                    let _ = tokio::fs::create_dir_all(&album_dir).await;
                    let _ = tokio::fs::write(&cover_dest, &bytes).await;
                }
            }
        }

        // Mark as processing → scanning stage
        let _ = sqlx::query(
            "UPDATE download_queue SET status = 'processing', processing_stage = 'scanning', current_track = NULL WHERE id = ?"
        )
        .bind(&dl_id)
        .execute(&state.db)
        .await;

        tracing::info!(
            artist = %detail.album.artist.name,
            album = %detail.album.title,
            tracks = completed,
            "download complete, starting post-processing",
        );

        // Scan library so the new album appears in the DB immediately
        let mut new_album: Option<(String,)> = None;
        if !resolved_library_id.is_empty() {
            match scanner::scan_library(&state.db, &library_path, &resolved_library_id).await {
                Ok(result) => {
                    tracing::info!(
                        artists_added = result.artists_added,
                        albums_added = result.albums_added,
                        tracks_added = result.tracks_added,
                        "post-download scan complete",
                    );
                    state.event_bus.emit(plugin::events::ServerEvent::ScanCompleted {
                        library_id: resolved_library_id.clone(),
                        tracks_added: result.tracks_added,
                        tracks_removed: result.tracks_removed,
                    });

                    // Find the newly created album
                    new_album = sqlx::query_as(
                        "SELECT a.id FROM albums a JOIN artists ar ON a.artist_id = ar.id \
                         WHERE LOWER(ar.name) = LOWER(?) AND LOWER(a.title) = LOWER(?) \
                         AND a.library_id = ? ORDER BY a.added_at DESC LIMIT 1"
                    )
                    .bind(&detail.album.artist.name)
                    .bind(&detail.album.title)
                    .bind(&resolved_library_id)
                    .fetch_optional(&state.db)
                    .await
                    .unwrap_or(None);

                    if let Some((ref local_album_id,)) = new_album {
                        // Update local_album_id on the download queue entry
                        let _ = sqlx::query("UPDATE download_queue SET local_album_id = ? WHERE id = ?")
                            .bind(local_album_id)
                            .bind(&dl_id)
                            .execute(&state.db)
                            .await;

                        // Set provider info on the album
                        let _ = sqlx::query("UPDATE albums SET provider = ?, provider_album_id = ? WHERE id = ?")
                            .bind(&provider_name)
                            .bind(&album_id)
                            .bind(local_album_id)
                            .execute(&state.db)
                            .await;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "post-download scan failed"),
            }
        }

        // Run post-download enrichment if we found the album
        if let Some((ref local_album_id,)) = new_album {
            // MusicBrainz enrichment
            let status: Option<(String,)> = sqlx::query_as("SELECT status FROM download_queue WHERE id = ?")
                .bind(&dl_id).fetch_optional(&state.db).await.unwrap_or(None);
            if status.as_ref().map(|s| s.0.as_str()) != Some("cancelled") {
                let _ = sqlx::query("UPDATE download_queue SET processing_stage = 'enriching' WHERE id = ?")
                    .bind(&dl_id).execute(&state.db).await;
                if let Err(e) = musicbrainz::enrichment::enrich_album(&state.db, local_album_id).await {
                    tracing::warn!(album_id = %local_album_id, error = %e, "post-download enrichment failed");
                }
            }

            // Editorial review enrichment
            let status: Option<(String,)> = sqlx::query_as("SELECT status FROM download_queue WHERE id = ?")
                .bind(&dl_id).fetch_optional(&state.db).await.unwrap_or(None);
            if status.as_ref().map(|s| s.0.as_str()) != Some("cancelled") {
                let editorial_providers = state.plugin_registry.read().await
                    .editorial_providers().to_vec();
                if !editorial_providers.is_empty() {
                    let _ = sqlx::query("UPDATE download_queue SET processing_stage = 'editorial' WHERE id = ?")
                        .bind(&dl_id).execute(&state.db).await;

                    // Re-fetch album metadata (MusicBrainz may have updated title/artist)
                    let album_meta: Option<(String, String, Option<String>)> = sqlx::query_as(
                        "SELECT a.title, ar.name, a.release_date FROM albums a \
                         JOIN artists ar ON a.artist_id = ar.id WHERE a.id = ?"
                    )
                    .bind(local_album_id)
                    .fetch_optional(&state.db)
                    .await
                    .unwrap_or(None);

                    if let Some((title, artist_name, release_date)) = &album_meta {
                        match riff_core::editorial::enrich_album(
                            &state.db,
                            &editorial_providers,
                            local_album_id,
                            title,
                            artist_name,
                            release_date.as_deref(),
                        ).await {
                            Ok(count) => tracing::info!(album_id = %local_album_id, reviews = count, "post-download editorial complete"),
                            Err(e) => tracing::warn!(album_id = %local_album_id, error = %e, "post-download editorial failed"),
                        }
                    }
                }
            }

            // Emit immediate enrichment event for this album
            state.event_bus.emit(plugin::events::ServerEvent::EnrichmentCompleted {
                album_ids: vec![local_album_id.clone()],
                artist_ids: Vec::new(),
            });

            // Trigger full pipeline for recommendations, artist images, and analysis
            state.pipeline_notify.notify_one();
        }

        // Check if cancelled during post-processing
        let was_cancelled = {
            let st: Option<(String,)> =
                sqlx::query_as("SELECT status FROM download_queue WHERE id = ?")
                    .bind(&dl_id)
                    .fetch_optional(&state.db)
                    .await
                    .unwrap_or(None);
            st.as_ref().map(|s| s.0.as_str()) == Some("cancelled")
        };
        if was_cancelled {
            tracing::info!(download_id = %dl_id, "download cancelled during processing, cleaning up");
            cleanup_cancelled_download(
                &state.db,
                &album_dir,
                new_album.as_ref().map(|(id,)| id.as_str()),
            )
            .await;
            continue;
        }

        // Mark fully completed (guard against race with cancellation)
        let _ = sqlx::query(
            "UPDATE download_queue SET status = 'completed', processing_stage = 'complete', completed_at = datetime('now') \
             WHERE id = ? AND status != 'cancelled'"
        )
        .bind(&dl_id)
        .execute(&state.db)
        .await;
    }
}

async fn health(State(_state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
