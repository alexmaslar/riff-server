use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::Response,
    Json,
};
use futures::TryStreamExt;
use riff_core::plugin::capabilities::StreamingQuality;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: String,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub quality: Option<String>,
}

/// GET /streaming/search?q=&limit=
pub async fn search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Value>, AppError> {
    let registry = state.plugin_registry.read().await;
    if registry.streaming_providers().is_empty() {
        return Ok(Json(json!({ "providers": [] })));
    }

    let limit = params.limit.unwrap_or(20);
    let results = registry.search_all_streaming(&params.q, limit).await;

    let providers: Vec<Value> = results
        .into_iter()
        .map(|(name, result)| match result {
            Ok(data) => json!({
                "provider": name,
                "albums": data.albums,
                "artists": data.artists,
                "tracks": data.tracks,
            }),
            Err(e) => json!({
                "provider": name,
                "error": e.to_string(),
                "albums": [],
                "artists": [],
                "tracks": [],
            }),
        })
        .collect();

    Ok(Json(json!({ "providers": providers })))
}

/// GET /streaming/albums/{provider}/{id}
pub async fn get_album(
    State(state): State<Arc<AppState>>,
    Path((provider, id)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let registry = state.plugin_registry.read().await;
    let streaming = registry
        .streaming_providers()
        .iter()
        .find(|p| p.provider_name() == provider)
        .ok_or_else(|| AppError::NotFound(format!("streaming provider '{provider}' not found")))?
        .clone();

    let detail = streaming
        .get_album(&id)
        .await
        .map_err(|e| AppError::Internal(format!("streaming album fetch failed: {e}")))?;

    Ok(Json(json!({
        "provider": provider,
        "album": detail.album,
        "tracks": detail.tracks,
    })))
}

/// GET /streaming/artists/{provider}/{id}/albums
pub async fn get_artist_albums(
    State(state): State<Arc<AppState>>,
    Path((provider, id)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let registry = state.plugin_registry.read().await;
    let streaming = registry
        .streaming_providers()
        .iter()
        .find(|p| p.provider_name() == provider)
        .ok_or_else(|| AppError::NotFound(format!("streaming provider '{provider}' not found")))?
        .clone();

    let albums = streaming
        .get_artist_albums(&id)
        .await
        .map_err(|e| AppError::Internal(format!("streaming artist albums failed: {e}")))?;

    Ok(Json(json!({
        "provider": provider,
        "albums": albums,
    })))
}

/// GET /streaming/tracks/{provider}/{id}/stream?quality=
/// Proxies audio from the streaming provider's CDN.
pub async fn stream_track(
    State(state): State<Arc<AppState>>,
    Path((provider, id)): Path<(String, String)>,
    Query(params): Query<StreamParams>,
    request: axum::extract::Request,
) -> Result<Response, AppError> {
    let registry = state.plugin_registry.read().await;
    let streaming = registry
        .streaming_providers()
        .iter()
        .find(|p| p.provider_name() == provider)
        .ok_or_else(|| AppError::NotFound(format!("streaming provider '{provider}' not found")))?
        .clone();

    let quality = parse_quality(params.quality.as_deref());

    let stream_url = streaming
        .get_stream_url(&id, quality)
        .await
        .map_err(|e| AppError::Internal(format!("stream URL resolution failed: {e}")))?;

    // Proxy the request to the CDN
    let http = reqwest::Client::new();
    let mut cdn_req = http.get(&stream_url.url);

    // Forward Range header for seeking
    if let Some(range) = request.headers().get(header::RANGE) {
        cdn_req = cdn_req.header("Range", range.to_str().unwrap_or(""));
    }

    let cdn_resp = cdn_req
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("CDN request failed: {e}")))?;

    let status = if cdn_resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    let mut builder = Response::builder().status(status);

    // Forward content headers
    if let Some(ct) = cdn_resp.headers().get("content-type") {
        builder = builder.header(header::CONTENT_TYPE, ct);
    } else {
        builder = builder.header(header::CONTENT_TYPE, &stream_url.mime_type);
    }
    if let Some(cl) = cdn_resp.headers().get("content-length") {
        builder = builder.header(header::CONTENT_LENGTH, cl);
    }
    if let Some(cr) = cdn_resp.headers().get("content-range") {
        builder = builder.header("Content-Range", cr);
    }
    builder = builder.header("Accept-Ranges", "bytes");
    builder = builder.header("X-Riff-Provider", &provider);
    builder = builder.header(
        "X-Riff-Quality",
        format!("{:?}", stream_url.quality).to_lowercase(),
    );

    let stream = cdn_resp
        .bytes_stream()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
    let body = Body::from_stream(stream);

    builder
        .body(body)
        .map_err(|e| AppError::Internal(format!("response build failed: {e}")))
}

fn parse_quality(s: Option<&str>) -> StreamingQuality {
    match s {
        Some("hires") | Some("hi_res") => StreamingQuality::HiRes,
        Some("lossless") => StreamingQuality::Lossless,
        Some("high") => StreamingQuality::High,
        Some("low") => StreamingQuality::Low,
        _ => StreamingQuality::Lossless,
    }
}
