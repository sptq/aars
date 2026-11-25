//! Odpalenia biuletynu: wiersz powstaje przed składaniem, więc restart ani
//! ponowny tik nie nadaje tej samej pory dwa razy.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

use crate::error::DbError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulletinFire {
    pub fire_at: DateTime<Utc>,
    pub skipped_reason: Option<String>,
    pub queue_item_id: Option<i64>,
}

#[derive(Clone)]
pub struct BulletinFireRepo {
    pool: SqlitePool,
}

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

impl BulletinFireRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// `true` — pora zajęta teraz przez nas; `false` — ktoś już ją miał.
    pub async fn claim(
        &self,
        fire_at: DateTime<Utc>,
        skipped_reason: Option<&str>,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT OR IGNORE INTO bulletin_fires (fire_at, skipped_reason) VALUES (?, ?)",
        )
        .bind(iso(fire_at))
        .bind(skipped_reason)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn set_queued(
        &self,
        fire_at: DateTime<Utc>,
        queue_item_id: i64,
    ) -> Result<(), DbError> {
        sqlx::query("UPDATE bulletin_fires SET queue_item_id = ? WHERE fire_at = ?")
            .bind(queue_item_id)
            .bind(iso(fire_at))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_skipped(&self, fire_at: DateTime<Utc>, reason: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE bulletin_fires SET skipped_reason = ? WHERE fire_at = ?")
            .bind(reason)
            .bind(iso(fire_at))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Pory już zajęte od `from` (włącznie) — runner nie próbuje ich drugi
    /// raz przy każdym tiku.
    pub async fn claimed_since(&self, from: DateTime<Utc>) -> Result<Vec<DateTime<Utc>>, DbError> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT fire_at FROM bulletin_fires WHERE fire_at >= ? ORDER BY fire_at")
            .bind(iso(from))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| chrono::NaiveDateTime::parse_from_str(r, "%Y-%m-%dT%H:%M:%SZ").ok())
            .map(|n| n.and_utc())
            .collect())
    }

    /// Usuwa wpisy starsze niż `before` (tryb cykliczny to 144 wpisy na dobę).
    pub async fn prune_before(&self, before: DateTime<Utc>) -> Result<u64, DbError> {
        let r = sqlx::query("DELETE FROM bulletin_fires WHERE fire_at < ?")
            .bind(iso(before))
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    pub async fn get(&self, fire_at: DateTime<Utc>) -> Result<Option<BulletinFire>, DbError> {
        let row = sqlx::query(
            "SELECT skipped_reason, queue_item_id FROM bulletin_fires WHERE fire_at = ?",
        )
        .bind(iso(fire_at))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| BulletinFire {
            fire_at,
            skipped_reason: row.get("skipped_reason"),
            queue_item_id: row.get("queue_item_id"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    #[tokio::test]
    async fn a_fire_is_claimed_once() {
        let pool = test_pool().await;
        let repo = BulletinFireRepo::new(pool.clone());
        let t = DateTime::parse_from_rfc3339("2026-11-22T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(repo.claim(t, None).await.unwrap());
        assert!(!repo.claim(t, None).await.unwrap());

        let id: i64 = sqlx::query_scalar(
            "INSERT INTO queue_items (rendered_text, priority) VALUES ('x', 2) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        repo.set_queued(t, id).await.unwrap();
        assert_eq!(repo.get(t).await.unwrap().unwrap().queue_item_id, Some(id));

        repo.set_skipped(t, "cisza").await.unwrap();
        assert_eq!(
            repo.get(t)
                .await
                .unwrap()
                .unwrap()
                .skipped_reason
                .as_deref(),
            Some("cisza")
        );
        assert!(
            repo.get(t + chrono::Duration::hours(1))
                .await
                .unwrap()
                .is_none()
        );

        let later = t + chrono::Duration::minutes(10);
        assert!(repo.claim(later, Some("x")).await.unwrap());
        assert_eq!(repo.claimed_since(t).await.unwrap(), [t, later]);
        assert_eq!(repo.claimed_since(later).await.unwrap(), [later]);
        assert_eq!(repo.prune_before(later).await.unwrap(), 1);
        assert!(repo.get(t).await.unwrap().is_none());
        assert!(repo.get(later).await.unwrap().is_some());
    }
}
