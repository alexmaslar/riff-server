pub mod error;
mod discovery;
mod middleware;
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
use riff_core::{ai, analysis, auth, config::Config, daily_mixes, db, musicbrainz, scanner};
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
    pub recommendation_running: AtomicBool,
    pub artist_bio_running: AtomicBool,
    pub artist_recommendation_running: AtomicBool,
    pub top_tracks_running: AtomicBool,
    pub remote_access: upnp::RemoteAccessManager,
    pub restart: Notify,
    pub ffmpeg_available: bool,
    pub ffmpeg_has_fdk_aac: bool,
    pub transcode_semaphore: Arc<tokio::sync::Semaphore>,
    pub hls_generating: tokio::sync::Mutex<std::collections::HashSet<String>>,
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

    // Step 3: Deezer top tracks + artist images (no API key needed)
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
    let state = Arc::new(AppState {
        db: pool,
        config: RwLock::new(config.clone()),
        jwt_secret: config.auth.jwt_secret.clone(),
        enrichment_running: AtomicBool::new(false),
        analysis_running: AtomicBool::new(false),
        summarization_running: AtomicBool::new(false),
        rating_running: AtomicBool::new(false),
        recommendation_running: AtomicBool::new(false),
        artist_bio_running: AtomicBool::new(false),
        artist_recommendation_running: AtomicBool::new(false),
        top_tracks_running: AtomicBool::new(false),
        remote_access: upnp::RemoteAccessManager::new(config.server.https_port),
        restart: Notify::new(),
        ffmpeg_available,
        ffmpeg_has_fdk_aac,
        transcode_semaphore: Arc::new(tokio::sync::Semaphore::new(max_transcodes)),
        hls_generating: tokio::sync::Mutex::new(std::collections::HashSet::new()),
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
        match server.serve(https_app.into_make_service()).await {
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

async fn health(State(_state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
