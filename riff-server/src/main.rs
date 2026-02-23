pub mod error;
mod discovery;
mod middleware;
mod plugin_reload;
mod routes;
mod tls;
mod transcode;
mod upnp;

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    middleware as axum_mw,
    routing::{delete, get, post, put},
    BoxError, Json, Router,
};
use riff_core::plugin::catalog::RemotePluginEntry;
use riff_core::{ai, analysis, auth, config::Config, daily_mixes, db, musicbrainz, plugin, scanner::{self, metadata::{detect_library_convention, build_album_dir, build_track_filename, write_flac_tags}}};
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
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

pub struct AppState {
    pub db: SqlitePool,
    pub config: RwLock<Config>,
    pub jwt_secret: String,
    pub enrichment_running: AtomicBool,
    pub analysis_running: AtomicBool,
    pub summarization_running: AtomicBool,
    pub rating_running: AtomicBool,
    pub tag_extraction_running: AtomicBool,
    pub recommendation_running: AtomicBool,
    pub artist_bio_running: AtomicBool,
    pub artist_recommendation_running: AtomicBool,
    pub top_tracks_running: AtomicBool,
    pub scan_running: AtomicBool,
    pub remote_access: upnp::RemoteAccessManager,
    pub restart: Notify,
    pub ffmpeg_available: bool,
    pub ffmpeg_has_fdk_aac: bool,
    pub transcode_semaphore: Arc<tokio::sync::Semaphore>,
    pub hls_generating: tokio::sync::Mutex<std::collections::HashSet<String>>,
    pub login_guard: tokio::sync::Mutex<routes::auth::LoginGuard>,
    pub plugin_registry: RwLock<plugin::registry::PluginRegistry>,
    pub remote_catalog: RwLock<Vec<RemotePluginEntry>>,
    pub event_bus: plugin::events::EventBus,
    /// Names of plugins loaded from dev_plugins config (invisible to non-admin users).
    pub dev_plugin_names: RwLock<std::collections::HashSet<String>>,
}

async fn run_background_pipeline(state: Arc<AppState>) {
    // Step 1: Enrichment (MusicBrainz — no API key needed)
    {
        let config = state.config.read().await;
        if config.metadata.enrichment.auto_enrich {
            let db = state.db.clone();
            drop(config);
            run_stage(&state.enrichment_running, "enrichment", async move {
                musicbrainz::enrich_library(&db).await
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

            if ai_config.album_tags {
                run_stage(&state.tag_extraction_running, "tag extraction", {
                    let ai_config = ai_config.clone();
                    let db = db.clone();
                    async move { ai::extract_tags_library(&db, &ai_config).await }
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

    // Step 3a: Discogs artist images (if API key configured)
    {
        let config = state.config.read().await;
        if let Some(ref api_key) = config.metadata.discogs_api_key {
            let api_key = api_key.clone();
            let db = state.db.clone();
            drop(config);
            match musicbrainz::enrich_artist_images_discogs(&db, &api_key).await {
                Ok(n) if n > 0 => tracing::info!("discogs artist images: {n} artists updated"),
                Ok(_) => {}
                Err(e) => tracing::warn!("discogs artist images failed: {e}"),
            }
        }
    }

    // Step 3b: Deezer top tracks + artist images fallback (no API key needed)
    run_stage(&state.top_tracks_running, "top tracks", {
        let db = state.db.clone();
        async move { musicbrainz::enrich_artist_top_tracks(&db).await }
    })
    .await;

    // Step 4: Analysis (always)
    run_stage(&state.analysis_running, "analysis", {
        let db = state.db.clone();
        async move { analysis::analyze_library(&db).await }
    })
    .await;

    // Step 5: Daily mixes (always, after analysis so ratings/BPM are available)
    match daily_mixes::generate_all_daily_mixes(&state.db).await {
        Ok(_) => tracing::info!("daily mixes complete"),
        Err(e) => tracing::warn!("daily mixes failed: {e}"),
    }

    // Step 6: Clean up stale caches (not accessed in 24h)
    routes::hls::cleanup_hls_cache().await;
    transcode::cleanup_cache(std::time::Duration::from_secs(86400)).await;
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

async fn run_periodic_scanner(state: Arc<AppState>) {
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
        if state
            .scan_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::info!("periodic scan skipped — scan already running");
            continue;
        }

        let config = state.config.read().await;
        let libs = config.resolved_libraries();
        let global_interval = config.library.scan_interval;
        drop(config);

        for lib_entry in &libs {
            let effective_interval = lib_entry.scan_interval.unwrap_or(global_interval);
            if effective_interval == 0 {
                continue;
            }

            let library_id = match db::get_library_id_by_path(&state.db, &lib_entry.path).await {
                Ok(Some(id)) => id,
                _ => continue,
            };

            tracing::info!("periodic scan: {:?} ({})", lib_entry.name, lib_entry.path);
            match scanner::scan_library(&state.db, &lib_entry.path, &library_id).await {
                Ok(result) => {
                    if result.tracks_added > 0
                        || result.albums_added > 0
                        || result.artists_added > 0
                        || result.tracks_removed > 0
                        || result.albums_removed > 0
                        || result.artists_removed > 0
                    {
                        tracing::info!(
                            "periodic scan complete for {:?}: +{} artists, +{} albums, +{} tracks, -{} tracks, -{} albums, -{} artists",
                            lib_entry.name,
                            result.artists_added,
                            result.albums_added,
                            result.tracks_added,
                            result.tracks_removed,
                            result.albums_removed,
                            result.artists_removed,
                        );
                    }
                }
                Err(e) => tracing::warn!("periodic scan failed for {:?}: {}", lib_entry.name, e),
            }
        }

        state.scan_running.store(false, Ordering::SeqCst);
    }
}

async fn run_daily_refresh(state: Arc<AppState>) {
    loop {
        // Compute duration until next midnight UTC
        let now = chrono::Utc::now();
        let tomorrow = (now.date_naive() + chrono::Days::new(1))
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let until_midnight = tomorrow
            .signed_duration_since(now.naive_utc())
            .to_std()
            .unwrap_or(Duration::from_secs(60));

        tokio::time::sleep(until_midnight).await;

        // Generate daily mixes for all users
        match daily_mixes::generate_all_daily_mixes(&state.db).await {
            Ok(_) => tracing::info!("daily mix refresh complete"),
            Err(e) => tracing::warn!("daily mix refresh failed: {e}"),
        }
    }
}

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

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::load()?;
    tracing::info!("loaded config, port={}", config.server.port);

    if config.metadata.enrichment.auto_enrich {
        tracing::info!("metadata enrichment enabled (MusicBrainz)");
    }

    if config.metadata.ai.enabled {
        tracing::info!("AI summarization enabled (provider: {:?})", config.metadata.ai.provider);
    } else {
        tracing::debug!("AI summarization not configured");
    }

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
            tracing::warn!("no library_id found for path {:?}, skipping scan", lib_entry.path);
            continue;
        }
        tracing::info!("scanning library {:?}: {}", lib_entry.name, lib_entry.path);
        match scanner::scan_library(&pool, &lib_entry.path, &library_id).await {
            Ok(result) => {
                tracing::info!(
                    "scan complete for {:?}: +{} artists, +{} albums, +{} tracks, -{} tracks, -{} albums, -{} artists",
                    lib_entry.name,
                    result.artists_added,
                    result.albums_added,
                    result.tracks_added,
                    result.tracks_removed,
                    result.albums_removed,
                    result.artists_removed,
                );
            }
            Err(e) => tracing::warn!("library scan failed for {:?}: {}", lib_entry.name, e),
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
        tracing::info!("ffmpeg found ({codec}) — remote transcoding enabled");
    } else {
        tracing::warn!("ffmpeg not found — remote clients will receive lossless files");
    }

    // Generate TLS certificate for HTTPS
    let (cert_path, key_path) = tls::ensure_certificate()?;
    let cert_fingerprint = tls::cert_fingerprint(&cert_path)?;
    tracing::info!("TLS cert fingerprint: {}", cert_fingerprint);

    let max_transcodes = config.streaming.max_transcode_processes;
    let event_bus = plugin::events::EventBus::new(512);
    let plugin_registry = plugin::registry::PluginRegistry::new();

    // Fetch community plugin catalog (non-blocking, empty on failure)
    let remote_catalog = plugin::catalog::fetch_remote_catalog().await;

    let state = Arc::new(AppState {
        db: pool,
        config: RwLock::new(config.clone()),
        jwt_secret: config.auth.jwt_secret.clone(),
        enrichment_running: AtomicBool::new(false),
        analysis_running: AtomicBool::new(false),
        summarization_running: AtomicBool::new(false),
        rating_running: AtomicBool::new(false),
        tag_extraction_running: AtomicBool::new(false),
        recommendation_running: AtomicBool::new(false),
        artist_bio_running: AtomicBool::new(false),
        artist_recommendation_running: AtomicBool::new(false),
        top_tracks_running: AtomicBool::new(false),
        scan_running: AtomicBool::new(false),
        remote_access: upnp::RemoteAccessManager::new(config.server.https_port),
        restart: Notify::new(),
        ffmpeg_available,
        ffmpeg_has_fdk_aac,
        transcode_semaphore: Arc::new(tokio::sync::Semaphore::new(max_transcodes)),
        hls_generating: tokio::sync::Mutex::new(std::collections::HashSet::new()),
        login_guard: tokio::sync::Mutex::new(routes::auth::LoginGuard::new()),
        plugin_registry: RwLock::new(plugin_registry),
        remote_catalog: RwLock::new(remote_catalog),
        event_bus,
        dev_plugin_names: RwLock::new(std::collections::HashSet::new()),
    });

    // Set cert fingerprint on the remote access manager
    state
        .remote_access
        .set_cert_fingerprint(cert_fingerprint)
        .await;

    // Download and load enabled WASM plugins from the remote catalog
    let plugin_results = plugin_reload::reload_wasm_plugins(&state).await;
    for (name, result) in &plugin_results {
        if !result.healthy {
            tracing::warn!(
                "plugin {name} is unhealthy: {}",
                result.message.as_deref().unwrap_or("unknown error")
            );
        }
    }

    // Load dev plugins from local paths
    let (dev_results, dev_names) = plugin_reload::reload_dev_plugins(&state).await;
    for (name, result) in &dev_results {
        if result.loaded {
            tracing::info!("dev plugin {name}: loaded (healthy={})", result.healthy);
        } else {
            tracing::warn!(
                "dev plugin {name}: {}",
                result.message.as_deref().unwrap_or("failed to load")
            );
        }
    }
    *state.dev_plugin_names.write().await = dev_names;

    // Background pipeline: enrich → summarize → rate → analyze
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
        .route("/mixes/daily/{id}/cover", get(routes::daily_mixes::get_mix_cover));

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
        .route("/remote-access/status", get(routes::remote_access::get_status))
        .route("/artists", get(routes::artists::list_artists))
        .route("/artists/stories", get(routes::artists::artist_stories))
        .route("/artists/{id}", get(routes::artists::get_artist))
        .route("/artists/{id}/streaming-albums", get(routes::artists::get_streaming_albums))
        .route("/albums", get(routes::albums::list_albums))
        .route("/albums/filters", get(routes::albums::list_filters))
        .route("/genres", get(routes::albums::list_genres))
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
        .route("/library/tags", post(routes::library::trigger_tag_extraction))
        .route("/library/recommend", post(routes::library::trigger_recommendations))
        .route("/library/artist-recommendations", post(routes::library::trigger_artist_recommendations))
        .route("/library/artist-bios", post(routes::library::trigger_artist_bios))
        .route("/library/stats", get(routes::library::library_stats))
        .route("/library/ai/clear", post(routes::library::clear_ai_data))
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
    let https_addr: SocketAddr = format!("0.0.0.0:{}", config.server.https_port).parse()?;
    let https_listener = create_keepalive_listener(https_addr)?;
    https_listener.set_nonblocking(true)?;
    let tls_config = tls::build_rustls_config(&cert_path, &key_path)?;
    tracing::info!("HTTPS listening on {}", https_addr);

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

    // Cleanup UPnP
    state.remote_access.stop().await;

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
                tracing::info!("re-queued {} interrupted download(s)", r.rows_affected());
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
                        tracing::warn!("failed to write tags for {}: {e}", track.title);
                    }
                    completed += 1;
                    let _ = sqlx::query("UPDATE download_queue SET tracks_completed = ? WHERE id = ?")
                        .bind(completed)
                        .bind(&dl_id)
                        .execute(&state.db)
                        .await;
                }
                Err(e) => {
                    tracing::warn!("download track {} failed: {e}", track.title);
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

        // Download cover art
        if let Some(ref cover_url) = detail.album.cover_url {
            let cover_dest = album_dir.join("cover.jpg");
            if let Ok(resp) = reqwest::get(cover_url).await {
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
            "download complete: {} - {} ({} tracks), starting post-processing",
            detail.album.artist.name,
            detail.album.title,
            completed
        );

        // Scan library so the new album appears in the DB immediately
        let mut new_album: Option<(String,)> = None;
        if !resolved_library_id.is_empty() {
            match scanner::scan_library(&state.db, &library_path, &resolved_library_id).await {
                Ok(result) => {
                    tracing::info!(
                        "post-download scan: +{} artists, +{} albums, +{} tracks",
                        result.artists_added, result.albums_added, result.tracks_added
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
                Err(e) => tracing::warn!("post-download scan failed: {e}"),
            }
        }

        // Run AI pipeline if we found the album
        if let Some((ref local_album_id,)) = new_album {
            let config = state.config.read().await;
            let ai_config = config.metadata.ai.clone();
            drop(config);

            // Stage 1: MusicBrainz enrichment
            let status: Option<(String,)> = sqlx::query_as("SELECT status FROM download_queue WHERE id = ?")
                .bind(&dl_id).fetch_optional(&state.db).await.unwrap_or(None);
            if status.as_ref().map(|s| s.0.as_str()) != Some("cancelled") {
                let _ = sqlx::query("UPDATE download_queue SET processing_stage = 'enriching' WHERE id = ?")
                    .bind(&dl_id).execute(&state.db).await;
                if let Err(e) = musicbrainz::enrichment::enrich_album(&state.db, local_album_id).await {
                    tracing::warn!("post-download enrichment failed for {}: {e}", local_album_id);
                }
            }

            // Stage 2: AI summarize (if AI configured)
            let status: Option<(String,)> = sqlx::query_as("SELECT status FROM download_queue WHERE id = ?")
                .bind(&dl_id).fetch_optional(&state.db).await.unwrap_or(None);
            if status.as_ref().map(|s| s.0.as_str()) != Some("cancelled") && ai_config.enabled {
                let _ = sqlx::query("UPDATE download_queue SET processing_stage = 'summarizing' WHERE id = ?")
                    .bind(&dl_id).execute(&state.db).await;
                if let Err(e) = ai::summarize_album(&state.db, &ai_config, local_album_id).await {
                    tracing::warn!("post-download summarization failed for {}: {e}", local_album_id);
                }
            }

            // Stage 3: AI rate
            let status: Option<(String,)> = sqlx::query_as("SELECT status FROM download_queue WHERE id = ?")
                .bind(&dl_id).fetch_optional(&state.db).await.unwrap_or(None);
            if status.as_ref().map(|s| s.0.as_str()) != Some("cancelled") && ai_config.enabled {
                let _ = sqlx::query("UPDATE download_queue SET processing_stage = 'rating' WHERE id = ?")
                    .bind(&dl_id).execute(&state.db).await;
                if let Err(e) = ai::rate_album(&state.db, &ai_config, local_album_id).await {
                    tracing::warn!("post-download rating failed for {}: {e}", local_album_id);
                }
            }

            // Stage 4: AI extract tags
            let status: Option<(String,)> = sqlx::query_as("SELECT status FROM download_queue WHERE id = ?")
                .bind(&dl_id).fetch_optional(&state.db).await.unwrap_or(None);
            if status.as_ref().map(|s| s.0.as_str()) != Some("cancelled") && ai_config.enabled {
                let _ = sqlx::query("UPDATE download_queue SET processing_stage = 'extracting_tags' WHERE id = ?")
                    .bind(&dl_id).execute(&state.db).await;
                if let Err(e) = ai::extract_tags_album(&state.db, &ai_config, local_album_id).await {
                    tracing::warn!("post-download tag extraction failed for {}: {e}", local_album_id);
                }
            }
        }

        // Mark fully completed
        let _ = sqlx::query(
            "UPDATE download_queue SET status = 'completed', processing_stage = 'complete', completed_at = datetime('now') WHERE id = ?"
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
