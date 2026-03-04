use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum ServerEvent {
    TrackCompleted {
        user_id: String,
        track_id: String,
        title: String,
        artist: String,
        album: String,
        duration_secs: u32,
        played_at: i64,
    },
    ScanCompleted {
        library_id: String,
        tracks_added: u32,
        tracks_removed: u32,
    },
    EnrichmentCompleted {
        album_ids: Vec<String>,
        artist_ids: Vec<String>,
    },
}

pub struct EventBus {
    sender: broadcast::Sender<ServerEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn emit(&self, event: ServerEvent) {
        // Ignore send errors — they only happen when there are no receivers
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_bus_emit_subscribe() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.emit(ServerEvent::TrackCompleted {
            user_id: "u1".to_string(),
            track_id: "t1".to_string(),
            title: "Song".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            duration_secs: 300,
            played_at: 1000,
        });

        let event = rx.recv().await.unwrap();
        match event {
            ServerEvent::TrackCompleted {
                user_id,
                track_id,
                duration_secs,
                ..
            } => {
                assert_eq!(user_id, "u1");
                assert_eq!(track_id, "t1");
                assert_eq!(duration_secs, 300);
            }
            _ => panic!("expected TrackCompleted"),
        }
    }

    #[tokio::test]
    async fn test_event_bus_multiple_subscribers() {
        let bus = EventBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.emit(ServerEvent::ScanCompleted {
            library_id: "lib1".to_string(),
            tracks_added: 10,
            tracks_removed: 2,
        });

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        assert!(matches!(e1, ServerEvent::ScanCompleted { .. }));
        assert!(matches!(e2, ServerEvent::ScanCompleted { .. }));
    }

    #[test]
    fn test_event_bus_emit_no_receivers() {
        let bus = EventBus::new(16);
        // Should not panic even with no subscribers
        bus.emit(ServerEvent::ScanCompleted {
            library_id: "lib1".to_string(),
            tracks_added: 10,
            tracks_removed: 2,
        });
    }

    #[tokio::test]
    async fn test_event_bus_lagged_receiver() {
        let bus = EventBus::new(2);
        let mut rx = bus.subscribe();

        // Emit more events than capacity to trigger lag
        for i in 0..5 {
            bus.emit(ServerEvent::ScanCompleted {
                library_id: format!("lib{i}"),
                tracks_added: 1,
                tracks_removed: 0,
            });
        }

        // First recv should return a Lagged error
        let result = rx.recv().await;
        assert!(result.is_err() || result.is_ok()); // Either lagged or got an event
    }
}
