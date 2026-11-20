//! Dziennik nadań: co faktycznie poszło na pasmo (spec §5).

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

use crate::error::DbError;

#[derive(Debug, Clone, PartialEq)]
pub struct TxLogEntry {
    /// `None` dla nadania spoza kolejki (test PTT, R10).
    pub queue_item_id: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub ptt_backend: String,
    /// Jedna z wartości `CHECK` w tabeli: ok, aborted, failed, timeout.
    pub result: &'static str,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TxLogRow {
    /// `None` dla nadania spoza kolejki albo gdy zadanie usunięto.
    pub queue_item_id: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub ptt_backend: String,
    pub result: String,
    pub note: Option<String>,
    /// Treść z `queue_items`; `None`, gdy zadanie już usunięto.
    pub text: Option<String>,
}

pub struct TxLogRepo {
    pool: SqlitePool,
}

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn parse_iso(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

impl TxLogRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn record(&self, entry: TxLogEntry) -> Result<(), DbError> {
        let duration_ms = (entry.ended_at - entry.started_at)
            .num_milliseconds()
            .max(0);
        sqlx::query(
            "INSERT INTO tx_log \
             (queue_item_id, started_at, ended_at, duration_ms, ptt_backend, result, note) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(entry.queue_item_id)
        .bind(iso(entry.started_at))
        .bind(iso(entry.ended_at))
        .bind(duration_ms)
        .bind(&entry.ptt_backend)
        .bind(entry.result)
        .bind(&entry.note)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Najnowsze nadania pierwsze.
    pub async fn recent(&self, limit: i64) -> Result<Vec<TxLogRow>, DbError> {
        let rows = sqlx::query(
            "SELECT t.queue_item_id, t.started_at, t.ended_at, t.duration_ms, t.ptt_backend, \
                    t.result, t.note, q.rendered_text \
             FROM tx_log t LEFT JOIN queue_items q ON q.id = t.queue_item_id \
             ORDER BY t.started_at DESC, t.id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|row| {
                let started: String = row.get("started_at");
                let ended: Option<String> = row.get("ended_at");
                let duration_ms: Option<i64> = row.get("duration_ms");
                TxLogRow {
                    queue_item_id: row.get("queue_item_id"),
                    started_at: parse_iso(&started),
                    ended_at: ended
                        .as_deref()
                        .map(parse_iso)
                        .unwrap_or_else(|| parse_iso(&started)),
                    duration_ms: duration_ms.unwrap_or(0),
                    ptt_backend: row.get("ptt_backend"),
                    result: row.get("result"),
                    note: row.get("note"),
                    text: row.get("rendered_text"),
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;
    use crate::priority::Priority;
    use crate::queue::{EnqueueRequest, QueueRepo};
    use chrono::TimeZone;
    use std::time::Duration;

    async fn item(pool: &SqlitePool, text: &str) -> i64 {
        QueueRepo::new(pool.clone())
            .enqueue(EnqueueRequest {
                message_id: None,
                rendered_text: text.to_owned(),
                priority: Priority::Normal,
                interrupting: false,
                not_before: None,
                expires_at: None,
                repeat_count: 1,
                repeat_interval: Duration::ZERO,
                created_by: None,
            })
            .await
            .unwrap()[0]
    }

    fn entry(id: i64, minute: u32, result: &'static str) -> TxLogEntry {
        let started = Utc.with_ymd_and_hms(2026, 11, 20, 12, minute, 0).unwrap();
        TxLogEntry {
            queue_item_id: Some(id),
            started_at: started,
            ended_at: started + chrono::Duration::seconds(5),
            ptt_backend: "gpio".to_owned(),
            result,
            note: None,
        }
    }

    #[tokio::test]
    async fn recent_returns_newest_first_with_the_message_text() {
        let pool = test_pool().await;
        let repo = TxLogRepo::new(pool.clone());
        let first = item(&pool, "pierwszy").await;
        let second = item(&pool, "drugi").await;
        repo.record(entry(first, 1, "ok")).await.unwrap();
        repo.record(entry(second, 2, "failed")).await.unwrap();

        let rows = repo.recent(10).await.unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].queue_item_id, Some(second));
        assert_eq!(rows[0].text.as_deref(), Some("drugi"));
        assert_eq!(rows[0].result, "failed");
        assert_eq!(rows[1].text.as_deref(), Some("pierwszy"));
        assert_eq!(rows[1].duration_ms, 5000);
        assert_eq!(rows[1].ptt_backend, "gpio");

        assert_eq!(repo.recent(1).await.unwrap().len(), 1, "limit obowiązuje");
    }

    #[tokio::test]
    async fn an_entry_without_a_queue_item_is_listed_with_its_note() {
        let pool = test_pool().await;
        let repo = TxLogRepo::new(pool.clone());
        let mut test = entry(0, 3, "ok");
        test.queue_item_id = None;
        test.note = Some("test PTT: admin".to_owned());
        repo.record(test).await.unwrap();

        let rows = repo.recent(10).await.unwrap();

        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].queue_item_id, None);
        assert_eq!(rows[0].text, None);
        assert_eq!(rows[0].note.as_deref(), Some("test PTT: admin"));
        assert_eq!(rows[0].result, "ok");
        let raw: Option<i64> = sqlx::query_scalar("SELECT queue_item_id FROM tx_log")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(raw, None, "test bez zadania zapisuje NULL, nie 0");
    }
}
