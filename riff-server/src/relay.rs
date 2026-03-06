use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::Router;
use futures::stream::StreamExt;
use futures::SinkExt;
use http::Request;
use http_body_util::BodyExt;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use crate::middleware::{ClientIp, IsRemote};
use crate::relay_protocol::{RelayToServer, ServerToRelay, CHUNK_SIZE};
use crate::AppState;

/// Run the relay tunnel client as a background task.
/// Connects outbound to the relay service and processes proxied requests.
/// Derive the HTTP base URL from the WebSocket tunnel URL.
/// e.g. "wss://relay.riff.audio/ws/tunnel" -> "https://relay.riff.audio"
fn relay_base_url(ws_url: &str) -> String {
    let http_url = ws_url
        .replace("wss://", "https://")
        .replace("ws://", "http://");
    // Strip path — find the first '/' after the scheme's "://"
    let after_scheme = if http_url.starts_with("https://") { 8 } else { 7 };
    if let Some(slash_pos) = http_url[after_scheme..].find('/') {
        http_url[..after_scheme + slash_pos].to_string()
    } else {
        http_url
    }
}

/// Register with the relay service to obtain server_id and api_key.
async fn register_with_relay(state: &Arc<AppState>, base_url: &str) -> anyhow::Result<(String, String)> {
    let url = format!("{base_url}/register");
    tracing::info!(url = %url, "registering with relay");

    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await?
        .error_for_status()?;

    let body: serde_json::Value = resp.json().await?;
    let server_id = body["server_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing server_id in registration response"))?
        .to_string();
    let api_key = body["api_key"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing api_key in registration response"))?
        .to_string();

    // Save credentials to config
    let mut config = state.config.write().await;
    config.set_relay_identity(server_id.clone(), api_key.clone());
    config.save()?;
    tracing::info!(server_id = %server_id, "registered with relay");

    Ok((server_id, api_key))
}

pub async fn run_relay_tunnel(state: Arc<AppState>, router: Router) {
    let config = state.config.read().await;
    let url = config.relay.url.clone();
    let has_identity = config.has_relay_identity();
    let existing_server_id = config.relay.server_id.clone();
    let existing_api_key = config.relay.api_key.clone();
    drop(config);

    // Register with relay if we don't have credentials yet
    let (server_id, api_key) = if has_identity {
        (existing_server_id.unwrap(), existing_api_key.unwrap())
    } else {
        let base_url = relay_base_url(&url);
        match register_with_relay(&state, &base_url).await {
            Ok(creds) => creds,
            Err(e) => {
                tracing::error!(error = %e, "failed to register with relay");
                return;
            }
        }
    };

    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        tracing::info!(url = %url, server_id = %server_id, "connecting to relay");

        match connect_and_run(&state, &router, &url, &server_id, &api_key).await {
            Ok(()) => {
                tracing::info!("relay tunnel closed gracefully");
            }
            Err(e) => {
                tracing::warn!(error = %e, "relay tunnel error");
            }
        }

        state.relay_connected.store(false, Ordering::Relaxed);

        // Exponential backoff with ±50% jitter
        let jitter_factor = 0.5 + rand::random::<f64>(); // 0.5..1.5
        let sleep_dur = backoff.mul_f64(jitter_factor);
        tracing::info!(delay_ms = sleep_dur.as_millis(), "reconnecting to relay");
        tokio::time::sleep(sleep_dur).await;

        backoff = (backoff * 2).min(max_backoff);
    }
}

async fn connect_and_run(
    state: &Arc<AppState>,
    router: &Router,
    url: &str,
    server_id: &str,
    api_key: &str,
) -> anyhow::Result<()> {
    let (ws_stream, _response) = tokio_tungstenite::connect_async(url).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // Send Register message
    let register = ServerToRelay::Register {
        server_id: server_id.to_string(),
        api_key: api_key.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let register_bytes = rmp_serde::to_vec(&register)?;
    ws_tx.send(Message::Binary(register_bytes.into())).await?;

    state.relay_connected.store(true, Ordering::Relaxed);
    tracing::info!(server_id = %server_id, "relay tunnel registered");

    // Channel for handler tasks to send responses back to the WS writer
    let (resp_tx, mut resp_rx) = mpsc::channel::<ServerToRelay>(256);

    // Track in-flight request tasks for cancellation
    let mut in_flight: HashMap<u32, tokio::task::JoinHandle<()>> = HashMap::new();

    loop {
        tokio::select! {
            // Outgoing: forward responses from handlers to WS
            Some(msg) = resp_rx.recv() => {
                let data = rmp_serde::to_vec(&msg)?;
                ws_tx.send(Message::Binary(data.into())).await?;
            }

            // Incoming: read messages from relay
            ws_msg = ws_rx.next() => {
                match ws_msg {
                    Some(Ok(Message::Binary(data))) => {
                        let relay_msg: RelayToServer = rmp_serde::from_slice(&data)?;
                        match relay_msg {
                            RelayToServer::Request { stream_id, method, uri, headers, body } => {
                                let handler_router = router.clone();
                                let handler_tx = resp_tx.clone();

                                let handle = tokio::spawn(async move {
                                    if let Err(e) = handle_request(
                                        handler_router, handler_tx, stream_id,
                                        method, uri, headers, body,
                                    ).await {
                                        tracing::warn!(stream_id, error = %e, "relay request handler failed");
                                    }
                                });
                                in_flight.insert(stream_id, handle);
                            }
                            RelayToServer::Cancel { stream_id } => {
                                if let Some(handle) = in_flight.remove(&stream_id) {
                                    handle.abort();
                                }
                            }
                            RelayToServer::Ping => {
                                let pong = rmp_serde::to_vec(&ServerToRelay::Pong)?;
                                ws_tx.send(Message::Binary(pong.into())).await?;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        ws_tx.send(Message::Pong(data)).await?;
                    }
                    Some(Ok(_)) => {} // ignore text, pong
                    Some(Err(e)) => return Err(e.into()),
                }
            }
        }

        // Clean up completed in-flight tasks
        in_flight.retain(|_, handle| !handle.is_finished());
    }

    // Abort all in-flight handlers on disconnect
    for (_, handle) in in_flight {
        handle.abort();
    }

    Ok(())
}

async fn handle_request(
    router: Router,
    tx: mpsc::Sender<ServerToRelay>,
    stream_id: u32,
    method: String,
    uri: String,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
) -> anyhow::Result<()> {
    // Build HTTP request
    let mut builder = Request::builder()
        .method(method.as_str())
        .uri(&uri);

    for (name, value) in &headers {
        if let Ok(hv) = http::HeaderValue::from_bytes(value) {
            builder = builder.header(name.as_str(), hv);
        }
    }

    let mut request = builder.body(Body::from(body))?;

    // Mark as remote so streaming routes transcode
    request.extensions_mut().insert(IsRemote(true));
    // Extract client IP from X-Forwarded-For if present, otherwise "relay"
    let client_ip = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-forwarded-for"))
        .and_then(|(_, v)| String::from_utf8(v.clone()).ok())
        .and_then(|xff| xff.split(',').next().map(|s| s.trim().to_string()))
        .unwrap_or_else(|| "relay".to_string());
    request.extensions_mut().insert(ClientIp(client_ip));

    // Call the router in-process (no TCP loopback)
    let response = router.oneshot(request).await?;
    let (parts, response_body) = response.into_parts();

    let status = parts.status.as_u16();
    let resp_headers: Vec<(String, Vec<u8>)> = parts
        .headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
        .collect();

    // Check content-length to decide streaming vs complete
    let content_length = parts
        .headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let is_streaming = content_length.map_or(true, |len| len > CHUNK_SIZE as u64);

    if !is_streaming {
        // Small response — collect and send as one message
        let body_bytes = response_body
            .collect()
            .await?
            .to_bytes()
            .to_vec();

        tx.send(ServerToRelay::Response {
            stream_id,
            status,
            headers: resp_headers,
            body: body_bytes,
        })
        .await?;
    } else {
        // Large/streaming response — send head + chunks
        tx.send(ServerToRelay::ResponseHead {
            stream_id,
            status,
            headers: resp_headers,
        })
        .await?;

        let mut body_stream = response_body.into_data_stream();
        let mut buffer = Vec::new();

        while let Some(chunk_result) = body_stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    buffer.extend_from_slice(&chunk);
                    // Flush in CHUNK_SIZE increments
                    while buffer.len() >= CHUNK_SIZE {
                        let data: Vec<u8> = buffer.drain(..CHUNK_SIZE).collect();
                        tx.send(ServerToRelay::ResponseChunk {
                            stream_id,
                            data,
                            is_final: false,
                        })
                        .await?;
                    }
                }
                Err(e) => {
                    tracing::warn!(stream_id, error = %e, "body stream error");
                    break;
                }
            }
        }

        // Send remaining buffer as final chunk
        tx.send(ServerToRelay::ResponseChunk {
            stream_id,
            data: buffer,
            is_final: true,
        })
        .await?;
    }

    Ok(())
}
