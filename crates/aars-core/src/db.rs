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
