-- Dziennik źródeł danych: dlaczego źródło nie działa i kiedy wróciło.
-- Kolejne jednakowe zdarzenia to jeden wiersz (first_at, last_at, count).
CREATE TABLE source_log (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id TEXT    NOT NULL,
    kind      TEXT    NOT NULL CHECK (kind IN ('error', 'skipped', 'recovered')),
    message   TEXT    NOT NULL,
    first_at  TEXT    NOT NULL,
    last_at   TEXT    NOT NULL,
    count     INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX source_log_by_source ON source_log (source_id, id);
