-- Stan pozycji śledzonych przez źródła danych (etap 8): co już ogłosiliśmy,
-- żeby powtórne odpytanie niczego nie nadawało drugi raz.
CREATE TABLE source_items (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id     TEXT    NOT NULL,
    item_key      TEXT    NOT NULL,
    topic         TEXT    NOT NULL,
    fingerprint   TEXT    NOT NULL,
    area          TEXT    NOT NULL,           -- tekst obszaru, przy którym śledzono (R9)
    summary       TEXT    NOT NULL,           -- wiersz do panelu
    cancel_text   TEXT    NOT NULL,
    priority      INTEGER NOT NULL CHECK (priority BETWEEN 0 AND 3),
    valid_from    TEXT,
    valid_until   TEXT,
    state         TEXT    NOT NULL CHECK (state IN ('active','cancelled','expired','replaced','dropped')),
    misses        INTEGER NOT NULL DEFAULT 0,
    first_seen_at TEXT    NOT NULL,
    last_seen_at  TEXT    NOT NULL,
    queue_item_id INTEGER REFERENCES queue_items(id) ON DELETE SET NULL,
    UNIQUE (source_id, item_key)
);
CREATE INDEX source_items_active_idx ON source_items(source_id, state);

INSERT INTO source_state (source_id, enabled) VALUES ('imgw-meteo', 0);
INSERT INTO settings (key, value) VALUES
    ('source:imgw-meteo:area', ''),
    ('source:imgw-meteo:interval_s', '300');
