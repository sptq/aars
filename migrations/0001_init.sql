-- Użytkownicy i sesje (spec §5, §8)
CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,
    role          TEXT    NOT NULL CHECK (role IN ('viewer', 'operator', 'admin')),
    disabled      INTEGER NOT NULL DEFAULT 0,
    must_change_password INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

-- Sesje trzymane w bazie, nie tylko w podpisanym cookie: wylogowanie
-- i wyłączenie konta muszą działać natychmiast (spec §5).
CREATE TABLE sessions (
    id         TEXT    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at TEXT    NOT NULL,
    user_agent TEXT,
    created_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
CREATE INDEX sessions_user_idx ON sessions(user_id);

-- Definicja komunikatu: co nadawać. Rozdzielone od queue_items, by edycja
-- definicji nie przepisywała przeszłości (spec §5).
CREATE TABLE messages (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    title            TEXT    NOT NULL,
    kind             TEXT    NOT NULL DEFAULT 'manual' CHECK (kind IN ('manual', 'source')),
    body_template    TEXT    NOT NULL,
    voice            TEXT,
    priority         INTEGER NOT NULL DEFAULT 2 CHECK (priority BETWEEN 0 AND 3),
    interrupting     INTEGER NOT NULL DEFAULT 0,
    repeat_count     INTEGER NOT NULL DEFAULT 1 CHECK (repeat_count >= 1),
    repeat_interval_s INTEGER NOT NULL DEFAULT 0 CHECK (repeat_interval_s >= 0),
    schedule_kind    TEXT    NOT NULL DEFAULT 'immediate' CHECK (schedule_kind IN ('immediate', 'at', 'cron')),
    schedule_at      TEXT,
    cron_expr        TEXT,
    valid_from       TEXT,
    valid_until      TEXT,
    enabled          INTEGER NOT NULL DEFAULT 1,
    source_id        TEXT,
    created_by       INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at       TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at       TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    -- Harmonogram musi mieć czym odpalić.
    CHECK (schedule_kind <> 'at'   OR schedule_at IS NOT NULL),
    CHECK (schedule_kind <> 'cron' OR cron_expr   IS NOT NULL)
);

-- Jedno konkretne nadanie.
CREATE TABLE queue_items (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id    INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    rendered_text TEXT    NOT NULL,
    audio_path    TEXT,
    priority      INTEGER NOT NULL CHECK (priority BETWEEN 0 AND 3),
    interrupting  INTEGER NOT NULL DEFAULT 0,
    state         TEXT    NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending', 'rendering', 'ready', 'transmitting',
                                   'done', 'failed', 'cancelled', 'expired', 'interrupted')),
    not_before    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    expires_at    TEXT,
    repeat_group  TEXT,
    repeat_index  INTEGER NOT NULL DEFAULT 0,
    attempt       INTEGER NOT NULL DEFAULT 0,
    created_by    INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    started_at    TEXT,
    finished_at   TEXT,
    error         TEXT
);
-- Porządek wybierania: priorytet, potem czas (spec §5.1).
CREATE INDEX queue_items_pick_idx ON queue_items(state, priority, not_before, created_at);
CREATE INDEX queue_items_group_idx ON queue_items(repeat_group);

-- Idempotencja harmonogramu po restarcie (spec §6).
CREATE TABLE schedule_fires (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    fire_at    TEXT    NOT NULL,
    queued_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    skipped_reason TEXT,
    UNIQUE (message_id, fire_at)
);

-- Dziennik tego, co faktycznie poszło na pasmo (spec §5).
CREATE TABLE tx_log (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    queue_item_id INTEGER REFERENCES queue_items(id) ON DELETE SET NULL,
    started_at    TEXT    NOT NULL,
    ended_at      TEXT,
    duration_ms   INTEGER,
    ptt_backend   TEXT    NOT NULL,
    result        TEXT    NOT NULL CHECK (result IN ('ok', 'aborted', 'failed', 'timeout')),
    note          TEXT
);
CREATE INDEX tx_log_started_idx ON tx_log(started_at DESC);

-- Cache audio adresowany treścią (spec §4.1).
CREATE TABLE audio_cache (
    hash         TEXT PRIMARY KEY,
    path         TEXT NOT NULL,
    bytes        INTEGER NOT NULL,
    duration_ms  INTEGER NOT NULL,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    last_used_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
CREATE INDEX audio_cache_lru_idx ON audio_cache(last_used_at);

-- Zdrowie źródeł danych (spec §7).
CREATE TABLE source_state (
    source_id            TEXT PRIMARY KEY,
    enabled              INTEGER NOT NULL DEFAULT 1,
    last_run_at          TEXT,
    last_success_at      TEXT,
    last_payload_hash    TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    last_error           TEXT
);

-- Parametry operacyjne zmieniane z panelu (spec §9).
CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

INSERT INTO settings (key, value) VALUES
    ('tx_enabled',         'true'),
    ('callsign',           ''),
    ('id_mode',            'voice'),
    ('id_interval_s',      '600'),
    ('tx_max_duration_s',  '180'),
    ('misfire_grace_s',    '600'),
    ('quiet_hours',        ''),
    ('quiet_min_priority', '0'),
    ('poisoned',           'false');

-- Kto kazał nadać i kiedy (spec §5).
CREATE TABLE audit_log (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    action  TEXT    NOT NULL,
    detail  TEXT
);
CREATE INDEX audit_log_at_idx ON audit_log(at DESC);
