-- Performance indexes for common query patterns

-- Playlist track reordering and lookups
CREATE INDEX IF NOT EXISTS idx_playlist_tracks_playlist_sort
    ON playlist_tracks(playlist_id, sort_order);

-- Play history time-based stats queries
CREATE INDEX IF NOT EXISTS idx_play_history_user_played
    ON play_history(user_id, played_at);

-- Favorites existence checks
CREATE INDEX IF NOT EXISTS idx_favorites_user_entity
    ON favorites(user_id, entity_type, entity_id);

-- Daily mixes by user and date
CREATE INDEX IF NOT EXISTS idx_daily_mixes_user_date
    ON daily_mixes(user_id, mix_date);
