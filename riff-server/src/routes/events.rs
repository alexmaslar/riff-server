use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures::stream::Stream;
use riff_core::plugin::events::ServerEvent;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use crate::AppState;

pub async fn event_stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_bus.subscribe();

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ServerEvent::EnrichmentCompleted { album_ids, artist_ids }) => {
                    let data = serde_json::json!({
                        "type": "enrichment_completed",
                        "albumIds": album_ids,
                        "artistIds": artist_ids,
                    });
                    return Some((Ok(Event::default().data(data.to_string())), rx));
                }
                Ok(ServerEvent::ScanCompleted { .. }) => {
                    return Some((Ok(Event::default().data(r#"{"type":"scan_completed"}"#)), rx));
                }
                Ok(_) => continue, // ignore other events for SSE
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None, // channel closed
            }
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("ping"),
    )
}
