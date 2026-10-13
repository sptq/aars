# Etap 1: Szkielet — konfiguracja, baza, CLI

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Workspace Cargo z crate'ami z §3 speca, wczytywanie konfiguracji z pliku i zmiennych środowiskowych, schemat SQLite z migracjami oraz binarka z CLI i logami.

**Architecture:** `aars-core` dostaje dwa moduły bez żadnych zależności od warstw wyższych: `config` (czysta deserializacja, zero I/O poza czytaniem pliku) i `db` (pula SQLite + migracje + helper testowy). Binarka `aars` jest cienka: parsuje argumenty, inicjuje `tracing`, woła core.

**Tech Stack:** Rust 1.93, edition 2024, figment 0.10 (toml + env + test), sqlx 0.9 (sqlite, runtime-tokio), clap 4, tracing + tracing-subscriber, thiserror, anyhow, tokio 1.

**Spec:** `docs/superpowers/specs/2026-10-13-aars-design.md`

## Global Constraints

- Kierunek zależności jednostronny: `aars` → {`aars-web`, `aars-sources`, `aars-audio`, `aars-ptt`} → `aars-core`. `aars-core` nie zna HTTP (spec D6).
- SQLite jest źródłem prawdy dla kolejki i harmonogramu (spec D3).
- Konfiguracja na dwóch poziomach: `aars.toml` (sprzęt, rozruch, wymaga restartu) i tabela `settings` (parametry operacyjne, zmieniane z panelu) — spec §9.
- `thiserror` w bibliotekach, `anyhow` w binarce, `tracing` wszędzie (spec §10).
- Nadpisywanie konfiguracji zmiennymi środowiskowymi z prefiksem `AARS_` i separatorem `__` dla zagnieżdżenia (spec §9).
- Żaden test nie dotyka karty dźwiękowej, GPIO ani sieci (spec §11).
- Priorytety: `Emergency = 0, High = 1, Normal = 2, Low = 3` — niższa liczba znaczy wyższy priorytet (spec §5.1).
- TDD: test przed implementacją, commit po każdym zielonym cyklu.

---

### Task 1: Workspace i konfiguracja

**Files:**
- Create: `Cargo.toml` (workspace)
- Create: `crates/aars-core/Cargo.toml`
- Create: `crates/aars-core/src/lib.rs`
- Create: `crates/aars-core/src/config.rs`
- Create: `crates/aars-core/src/error.rs`
- Create: `aars.example.toml`
- Create: `.gitignore`

**Interfaces:**
- Consumes: nic (pierwsze zadanie)
- Produces: `aars_core::config::{Config, ServerConfig, AudioConfig, PttConfig, PttBackendKind, GpioConfig, SerialConfig, TtsConfig, StorageConfig, ChannelBusyKind}`; `Config::load(path: Option<&Path>) -> Result<Config, ConfigError>`; `aars_core::error::ConfigError`

- [ ] **Step 1: Utwórz szkielet workspace**

`Cargo.toml`:

```toml
[workspace]
resolver = "3"
members = ["crates/aars-core", "crates/aars"]

[workspace.package]
edition = "2024"
rust-version = "1.93"
license = "MIT"

[workspace.dependencies]
anyhow = "1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "signal"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
figment = { version = "0.10", features = ["toml", "env", "test"] }
sqlx = { version = "0.9", default-features = false, features = ["sqlite", "runtime-tokio", "macros", "migrate", "chrono"] }
chrono = { version = "0.4", features = ["serde"] }
clap = { version = "4", features = ["derive"] }
```

`.gitignore`:

```
/target
/aars.toml
/var
*.wav
```

`crates/aars-core/Cargo.toml`:

```toml
[package]
name = "aars-core"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
thiserror.workspace = true
figment.workspace = true
sqlx.workspace = true
chrono.workspace = true
tracing.workspace = true
tokio.workspace = true

[features]
testing = []
```

`crates/aars-core/src/lib.rs`:

```rust
pub mod config;
pub mod error;
```

- [ ] **Step 2: Napisz test, który ma nie przejść**

`crates/aars-core/src/config.rs` — na razie tylko testy:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_usable_without_any_file() {
        let cfg = Config::load(None).expect("domyślna konfiguracja musi się wczytać");
        assert_eq!(cfg.server.bind, "0.0.0.0:8080");
        assert_eq!(cfg.audio.sample_rate, 48_000);
        assert_eq!(cfg.ptt.backend, PttBackendKind::Vox);
        assert_eq!(cfg.ptt.channel_busy, ChannelBusyKind::None);
    }

    #[test]
    fn toml_overrides_defaults_and_env_overrides_toml() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "aars.toml",
                r#"
                [server]
                bind = "127.0.0.1:9000"

                [ptt]
                backend = "gpio"

                [ptt.gpio]
                line = 17
                "#,
            )?;
            jail.set_env("AARS_AUDIO__SAMPLE_RATE", "44100");

            let cfg = Config::load(Some(std::path::Path::new("aars.toml"))).unwrap();
            assert_eq!(cfg.server.bind, "127.0.0.1:9000");
            assert_eq!(cfg.ptt.backend, PttBackendKind::Gpio);
            assert_eq!(cfg.ptt.gpio.line, 17);
            assert_eq!(cfg.audio.sample_rate, 44_100, "env musi wygrywać z plikiem");
            assert_eq!(cfg.ptt.gpio.chip, "/dev/gpiochip0", "nietknięte pola zachowują domyślne");
            Ok(())
        });
    }

    #[test]
    fn missing_explicit_config_file_is_an_error() {
        let err = Config::load(Some(std::path::Path::new("/nie/ma/takiego.toml"))).unwrap_err();
        assert!(matches!(err, crate::error::ConfigError::NotFound { .. }));
    }
}
```

- [ ] **Step 3: Uruchom test i potwierdź, że nie przechodzi**

Run: `cargo test -p aars-core config`
Expected: FAIL — `cannot find type Config in this scope`

- [ ] **Step 4: Zaimplementuj konfigurację**

`crates/aars-core/src/error.rs`:

```rust
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("plik konfiguracyjny nie istnieje: {path}")]
    NotFound { path: PathBuf },
    #[error("niepoprawna konfiguracja: {0}")]
    Invalid(#[from] figment::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("błąd bazy danych: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("błąd migracji: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}
```

`crates/aars-core/src/config.rs` — nad blokiem testów:

```rust
use std::path::{Path, PathBuf};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub audio: AudioConfig,
    pub ptt: PttConfig,
    pub tts: TtsConfig,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    /// Nazwa urządzenia wyjściowego; "default" znaczy domyślne urządzenie systemu.
    pub device: String,
    pub sample_rate: u32,
    /// Docelowe wzmocnienie po normalizacji, w dB względem pełnej skali.
    pub gain_db: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PttBackendKind {
    Gpio,
    Serial,
    Vox,
}

/// Źródło informacji o zajętości kanału. W MVP zawsze `None` — spec §4.4
/// zostawia to pole, by dołożenie COS/RMS nie wymagało ruszania sekwencera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelBusyKind {
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PttConfig {
    pub backend: PttBackendKind,
    pub channel_busy: ChannelBusyKind,
    /// Czas, jaki radio potrzebuje na przełączenie się na nadawanie, w ms.
    pub tx_tail_ms: u64,
    pub gpio: GpioConfig,
    pub serial: SerialConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpioConfig {
    pub chip: String,
    pub line: u32,
    pub active_low: bool,
    /// Opcjonalne wejście blokujące nadawanie (spec §4.4 krok 3).
    pub inhibit_line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerialPttLine {
    Dtr,
    Rts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SerialConfig {
    pub port: String,
    pub line: SerialPttLine,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TtsConfig {
    /// Ścieżka do binarki `piper`.
    pub piper_bin: String,
    /// Ścieżka do modelu głosu .onnx (obok musi leżeć .onnx.json).
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub db_path: PathBuf,
    pub cache_dir: PathBuf,
    pub cache_max_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig { bind: "0.0.0.0:8080".to_owned() },
            audio: AudioConfig {
                device: "default".to_owned(),
                sample_rate: 48_000,
                gain_db: -3.0,
            },
            ptt: PttConfig {
                backend: PttBackendKind::Vox,
                channel_busy: ChannelBusyKind::None,
                tx_tail_ms: 250,
                gpio: GpioConfig {
                    chip: "/dev/gpiochip0".to_owned(),
                    line: 17,
                    active_low: false,
                    inhibit_line: None,
                },
                serial: SerialConfig {
                    port: "/dev/ttyUSB0".to_owned(),
                    line: SerialPttLine::Dtr,
                },
            },
            tts: TtsConfig {
                piper_bin: "piper".to_owned(),
                model: "/var/lib/aars/voices/pl_PL-darkman-medium.onnx".to_owned(),
            },
            storage: StorageConfig {
                db_path: PathBuf::from("var/aars.db"),
                cache_dir: PathBuf::from("var/cache"),
                cache_max_bytes: 512 * 1024 * 1024,
            },
        }
    }
}

impl Config {
    /// Wczytuje konfigurację: domyślne wartości, potem plik, potem `AARS_*`.
    ///
    /// Gdy `path` jest `Some`, plik musi istnieć — cicha praca na domyślnych
    /// wartościach po literówce w ścieżce byłaby groźna dla stacji nadawczej.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut figment = Figment::from(Serialized::defaults(Config::default()));

        if let Some(path) = path {
            if !path.exists() {
                return Err(ConfigError::NotFound { path: path.to_path_buf() });
            }
            figment = figment.merge(Toml::file(path));
        }

        figment
            .merge(Env::prefixed("AARS_").split("__"))
            .extract()
            .map_err(ConfigError::Invalid)
    }
}
```

`crates/aars-core/src/lib.rs` zostaje bez zmian (oba moduły już zadeklarowane).

- [ ] **Step 5: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core`
Expected: PASS — trzy testy konfiguracji

- [ ] **Step 6: Zapisz przykładową konfigurację**

`aars.example.toml`:

```toml
# Skopiuj do aars.toml i dostosuj. Każde pole można nadpisać zmienną
# środowiskową, np. AARS_SERVER__BIND=127.0.0.1:8080

[server]
bind = "0.0.0.0:8080"

[audio]
device = "default"
sample_rate = 48000
gain_db = -3.0

[ptt]
backend = "vox"         # gpio | serial | vox
channel_busy = "none"
tx_tail_ms = 250

[ptt.gpio]
chip = "/dev/gpiochip0"
line = 17
active_low = false
# inhibit_line = 27

[ptt.serial]
port = "/dev/ttyUSB0"
line = "dtr"            # dtr | rts

[tts]
piper_bin = "piper"
model = "/var/lib/aars/voices/pl_PL-darkman-medium.onnx"

[storage]
db_path = "var/aars.db"
cache_dir = "var/cache"
cache_max_bytes = 536870912
```

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml .gitignore aars.example.toml crates/aars-core
git commit -m "feat(core): workspace i wczytywanie konfiguracji"
```

---

### Task 2: Schemat bazy i migracje

**Files:**
- Create: `migrations/0001_init.sql`
- Create: `crates/aars-core/src/db.rs`
- Modify: `crates/aars-core/src/lib.rs`
- Modify: `crates/aars-core/Cargo.toml` (dev-dependency `tokio` z `rt`)

**Interfaces:**
- Consumes: `aars_core::config::StorageConfig`, `aars_core::error::DbError`
- Produces: `aars_core::db::{connect, migrate, test_pool}`; `connect(path: &Path) -> Result<SqlitePool, DbError>`; `migrate(pool: &SqlitePool) -> Result<(), DbError>`; `test_pool() -> SqlitePool` (dostępne przy `cfg(test)` albo feature `testing`)

- [ ] **Step 1: Napisz testy, które mają nie przejść**

Na końcu `crates/aars-core/src/db.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_create_every_table_from_the_spec() {
        let pool = test_pool().await;
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlx%' ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        for expected in [
            "audio_cache",
            "audit_log",
            "messages",
            "queue_items",
            "schedule_fires",
            "sessions",
            "settings",
            "source_state",
            "tx_log",
            "users",
        ] {
            assert!(tables.iter().any(|t| t == expected), "brak tabeli {expected}: {tables:?}");
        }
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        let pool = test_pool().await;
        let err = sqlx::query("INSERT INTO sessions (id, user_id, expires_at) VALUES ('s1', 999, '2030-01-01T00:00:00Z')")
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("foreign key"), "{err}");
    }

    #[tokio::test]
    async fn schedule_fires_rejects_duplicate_fire_times() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO messages (id, title, kind, body_template, priority) VALUES (1, 'test', 'manual', 'tekst', 2)")
            .execute(&pool)
            .await
            .unwrap();
        let insert = "INSERT INTO schedule_fires (message_id, fire_at) VALUES (1, '2026-10-13T18:00:00Z')";
        sqlx::query(insert).execute(&pool).await.unwrap();
        let err = sqlx::query(insert).execute(&pool).await.unwrap_err();
        assert!(err.to_string().to_lowercase().contains("unique"), "{err}");
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let pool = test_pool().await;
        migrate(&pool).await.expect("powtórna migracja nie może się wywalić");
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-core db`
Expected: FAIL — `cannot find function test_pool`

- [ ] **Step 3: Napisz migrację**

`migrations/0001_init.sql`:

```sql
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
```

- [ ] **Step 4: Zaimplementuj moduł `db`**

`crates/aars-core/src/db.rs` — nad blokiem testów:

```rust
use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

use crate::error::DbError;

/// Migracje wkompilowane w binarkę — wdrożenie to jeden plik (spec §12).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Otwiera pulę do pliku bazy, tworząc plik i katalog, jeśli ich nie ma.
pub async fn connect(path: &Path) -> Result<SqlitePool, DbError> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| {
                DbError::Sqlx(sqlx::Error::Io(e))
            })?;
        }
    }

    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        // WAL: panel czyta, gdy sekwencer pisze.
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new().max_connections(8).connect_with(options).await?;
    Ok(pool)
}

pub async fn migrate(pool: &SqlitePool) -> Result<(), DbError> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

/// Pula w pamięci z nałożonymi migracjami. Testy nie dotykają dysku.
#[cfg(any(test, feature = "testing"))]
pub async fn test_pool() -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .in_memory(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        // Baza w pamięci żyje tak długo, jak połączenie — jedno połączenie
        // gwarantuje, że wszystkie zapytania widzą ten sam schemat.
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("pula testowa");
    migrate(&pool).await.expect("migracje testowe");
    pool
}
```

`crates/aars-core/src/lib.rs`:

```rust
pub mod config;
pub mod db;
pub mod error;
```

`crates/aars-core/Cargo.toml` — dopisz:

```toml
[dev-dependencies]
tokio = { workspace = true, features = ["rt", "macros"] }
```

- [ ] **Step 5: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core`
Expected: PASS — siedem testów (trzy konfiguracji, cztery bazy)

- [ ] **Step 6: Commit**

```bash
git add migrations crates/aars-core
git commit -m "feat(core): schemat SQLite i migracje"
```

---

### Task 3: Binarka, CLI i logi

**Files:**
- Create: `crates/aars/Cargo.toml`
- Create: `crates/aars/src/main.rs`
- Create: `crates/aars/src/cli.rs`
- Create: `crates/aars/tests/cli.rs`

**Interfaces:**
- Consumes: `aars_core::config::Config`, `aars_core::db::{connect, migrate}`
- Produces: binarka `aars` z podkomendami `migrate` i `info`; `aars::cli::{Cli, Command}`

- [ ] **Step 1: Napisz test, który ma nie przejść**

`crates/aars/tests/cli.rs`:

```rust
use std::process::Command;

fn aars() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aars"))
}

#[test]
fn migrate_creates_the_database_file() {
    let dir = std::env::temp_dir().join(format!("aars-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("test.db");
    let _ = std::fs::remove_file(&db);

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .arg("migrate")
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(db.exists(), "migrate musi utworzyć plik bazy");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn info_prints_the_effective_configuration() {
    let out = aars().env("AARS_SERVER__BIND", "127.0.0.1:1234").arg("info").output().unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("127.0.0.1:1234"), "info musi pokazać efektywną konfigurację: {stdout}");
}

#[test]
fn a_missing_explicit_config_file_fails_loudly() {
    let out = aars().args(["--config", "/nie/ma/takiego.toml", "info"]).output().unwrap();

    assert!(!out.status.success(), "literówka w ścieżce nie może być milcząco zignorowana");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nie istnieje"), "stderr: {stderr}");
}
```

- [ ] **Step 2: Uruchom test i potwierdź, że nie przechodzi**

Run: `cargo test -p aars`
Expected: FAIL — brak pakietu `aars`

- [ ] **Step 3: Zaimplementuj binarkę**

`crates/aars/Cargo.toml`:

```toml
[package]
name = "aars"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[[bin]]
name = "aars"
path = "src/main.rs"

[dependencies]
aars-core = { path = "../aars-core" }
anyhow.workspace = true
clap.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
```

`crates/aars/src/cli.rs`:

```rust
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "aars", about = "Automatic Alert Radio Station", version)]
pub struct Cli {
    /// Ścieżka do aars.toml. Pominięta — używane są domyślne wartości i AARS_*.
    #[arg(long, short, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Nakłada migracje na bazę.
    Migrate,
    /// Wypisuje efektywną konfigurację.
    Info,
}
```

`crates/aars/src/main.rs`:

```rust
mod cli;

use anyhow::{Context, Result};
use clap::Parser;

use aars_core::config::Config;
use aars_core::db;

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_env("AARS_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = cli::Cli::parse();
    let config = Config::load(cli.config.as_deref()).context("wczytywanie konfiguracji")?;

    match cli.command {
        cli::Command::Migrate => {
            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;
            tracing::info!(path = %config.storage.db_path.display(), "migracje nałożone");
        }
        cli::Command::Info => {
            println!("bind:        {}", config.server.bind);
            println!("baza:        {}", config.storage.db_path.display());
            println!("cache:       {}", config.storage.cache_dir.display());
            println!("audio:       {} @ {} Hz", config.audio.device, config.audio.sample_rate);
            println!("PTT:         {:?}", config.ptt.backend);
            println!("model głosu: {}", config.tts.model);
        }
    }

    Ok(())
}
```

Dopisz `"crates/aars"` do `members` w `Cargo.toml` workspace, jeśli jeszcze nie ma.

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test --workspace`
Expected: PASS — dziesięć testów

- [ ] **Step 5: Sprawdź, że błąd konfiguracji jest czytelny**

Run: `cargo run -p aars -- info`
Expected: wypisana konfiguracja domyślna, kod wyjścia 0

Run: `cargo run -p aars -- --config /brak.toml info`
Expected: `Error: wczytywanie konfiguracji` z przyczyną `plik konfiguracyjny nie istnieje: /brak.toml`, kod wyjścia niezerowy

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/aars
git commit -m "feat(bin): CLI z migrate i info oraz inicjalizacja logów"
```

---

## Kryterium wyjścia z etapu

- `cargo test --workspace` zielone.
- `cargo clippy --workspace --all-targets -- -D warnings` bez zastrzeżeń.
- `aars migrate` tworzy schemat z §5 speca; `aars info` pokazuje efektywną konfigurację.
- Nieistniejący plik konfiguracyjny podany jawnie kończy się błędem, nie cichym użyciem domyślnych wartości.
