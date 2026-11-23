-- Etap 11b: biuletyn staje się przełączalny (domyślnie wyłączony).
INSERT OR IGNORE INTO settings (key, value) VALUES ('module:bulletin:enabled', 'false');
-- Odpalenia biuletynu: wiersz przed składaniem, więc restart nie dubluje.
CREATE TABLE bulletin_fires (
    fire_at        TEXT PRIMARY KEY,
    skipped_reason TEXT,
    queue_item_id  INTEGER REFERENCES queue_items(id) ON DELETE SET NULL,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
