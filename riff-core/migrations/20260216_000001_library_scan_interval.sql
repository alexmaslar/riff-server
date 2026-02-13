-- Per-library scan interval override (NULL = use global default)
ALTER TABLE libraries ADD COLUMN scan_interval INTEGER;
