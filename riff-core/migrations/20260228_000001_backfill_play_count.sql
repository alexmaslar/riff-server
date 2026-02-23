-- Backfill album play counts from completed play history
UPDATE albums SET play_count = COALESCE(
    (SELECT COUNT(*) FROM play_history ph
     JOIN tracks t ON ph.track_id = t.id
     WHERE t.album_id = albums.id AND ph.completed = 1),
    0
);
