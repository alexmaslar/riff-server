use serde::{Deserialize, Serialize};

/// Messages sent from the relay service to the riff-server through the WS tunnel.
#[derive(Debug, Serialize, Deserialize)]
pub enum RelayToServer {
    /// Proxied HTTP request from an iOS client.
    Request {
        stream_id: u32,
        method: String,
        uri: String,
        headers: Vec<(String, Vec<u8>)>,
        body: Vec<u8>,
    },
    /// Client disconnected — abort the in-flight handler.
    Cancel { stream_id: u32 },
    /// Keep-alive ping.
    Ping,
}

/// Messages sent from the riff-server back to the relay service.
#[derive(Debug, Serialize, Deserialize)]
pub enum ServerToRelay {
    /// First message after WS connect — authenticates the server.
    Register {
        server_id: String,
        api_key: String,
        version: String,
    },
    /// Complete (non-streaming) HTTP response.
    Response {
        stream_id: u32,
        status: u16,
        headers: Vec<(String, Vec<u8>)>,
        body: Vec<u8>,
    },
    /// Start of a streaming HTTP response (status + headers only).
    ResponseHead {
        stream_id: u32,
        status: u16,
        headers: Vec<(String, Vec<u8>)>,
    },
    /// Body chunk of a streaming response.
    ResponseChunk {
        stream_id: u32,
        data: Vec<u8>,
        is_final: bool,
    },
    /// Keep-alive pong.
    Pong,
}

/// Maximum size of a single response body chunk.
pub const CHUNK_SIZE: usize = 64 * 1024;
