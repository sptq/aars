use std::path::Path;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use crate::error::DbError;

/// Migracje wkompilowane w binarkę — wdrożenie to jeden plik (spec §12).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Otwiera pulę do pliku bazy, tworząc plik i katalog, jeśli ich nie ma.
pub async fn connect(path: &Path) -> Result<SqlitePool, DbError> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
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
    let options = SqliteConnectOptions::new().in_memory(true).foreign_keys(true);
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
            "bulletin_fires",
            "messages",
            "queue_items",
            "schedule_fires",
            "sessions",
            "settings",
            "source_items",
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

    #[tokio::test]
    async fn migration_0005_applies_on_a_version_4_database_with_rows() {
        let options = SqliteConnectOptions::new().in_memory(true).foreign_keys(true);
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(options).await.unwrap();
        let mut old = sqlx::migrate::Migrator::DEFAULT;
        old.migrations = MIGRATOR.migrations.iter().filter(|m| m.version <= 4).cloned().collect::<Vec<_>>().into();
        old.run(&pool).await.unwrap();
        sqlx::query("UPDATE settings SET value = '600' WHERE key = 'source:imgw-meteo:interval_s'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages (id, title, kind, body_template, priority) VALUES (1, 'stary', 'manual', 'tekst', 2)")
            .execute(&pool)
            .await
            .unwrap();

        migrate(&pool).await.expect("0005 na bazie w wersji 4");

        let kept: String =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'source:imgw-meteo:interval_s'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kept, "600");
        let sched: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'module:scheduler:enabled'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sched, "true");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn migration_0007_gives_old_tracked_items_their_summary_as_text() {
        let options = SqliteConnectOptions::new().in_memory(true).foreign_keys(true);
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(options).await.unwrap();
        let mut old = sqlx::migrate::Migrator::DEFAULT;
        old.migrations = MIGRATOR.migrations.iter().filter(|m| m.version <= 6).cloned().collect::<Vec<_>>().into();
        old.run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO source_items (source_id, item_key, topic, fingerprint, area, summary, cancel_text, priority, state, aired,
                                       first_seen_at, last_seen_at)
             VALUES ('swpc', 'G', 'G', 'G3', '', 'G3 — silna burza', '', 2, 'active', 1,
                     '2026-11-22T18:00:00Z', '2026-11-22T18:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        migrate(&pool).await.expect("0007 na bazie w wersji 6");

        let text: String = sqlx::query_scalar("SELECT text FROM source_items WHERE item_key = 'G'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(text, "G3 — silna burza");
    }

    #[tokio::test]
    async fn migration_0006_applies_on_a_version_5_database() {
        let options = SqliteConnectOptions::new().in_memory(true).foreign_keys(true);
        let build = || async {
            let pool = SqlitePoolOptions::new().max_connections(1).connect_with(options.clone()).await.unwrap();
            let mut old = sqlx::migrate::Migrator::DEFAULT;
            old.migrations = MIGRATOR.migrations.iter().filter(|m| m.version <= 5).cloned().collect::<Vec<_>>().into();
            old.run(&pool).await.unwrap();
            pool
        };
        let value = |pool: SqlitePool| async move {
            sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = 'module:bulletin:enabled'")
                .fetch_optional(&pool)
                .await
                .unwrap()
        };

        let with_row = build().await;
        sqlx::query("INSERT INTO settings (key, value) VALUES ('module:bulletin:enabled', 'true')")
            .execute(&with_row)
            .await
            .unwrap();
        migrate(&with_row).await.expect("0006 na bazie w wersji 5");
        assert_eq!(value(with_row.clone()).await.as_deref(), Some("true"));
        sqlx::query("SELECT fire_at FROM bulletin_fires").fetch_all(&with_row).await.unwrap();

        let without_row = build().await;
        migrate(&without_row).await.expect("0006 na bazie w wersji 5");
        assert_eq!(value(without_row).await.as_deref(), Some("false"));
    }
}
