//! Konfiguracja, zdrowie i śledzone pozycje źródeł danych (etap 8).

use std::ops::RangeInclusive;

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

use crate::area::{AreaEntry, area_text, parse_area};
use crate::error::DbError;
use crate::priority::Priority;

/// Dozwolony interwał odpytywania w sekundach.
pub const INTERVAL_RANGE: RangeInclusive<u64> = 60..=3600;
const DEFAULT_INTERVAL_S: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceConfig {
    pub enabled: bool,
    pub area: Vec<AreaEntry>,
    /// Postać kanoniczna obszaru; tym tekstem porównujemy obszar (R9).
    pub area_text: String,
    pub interval_s: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStatus {
    pub source_id: String,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub consecutive_failures: i64,
    pub last_error: Option<String>,
}

impl SourceStatus {
    /// R13: ostatni sukces dawniej niż `max(3 · interwał, 30 min)`; brak
    /// sukcesu liczy się tylko, gdy były próby.
    pub fn is_stale(&self, interval_s: u64, now: DateTime<Utc>) -> bool {
        let limit_s = interval_s.saturating_mul(3).max(1800);
        let limit = i64::try_from(limit_s)
            .ok()
            .and_then(chrono::Duration::try_seconds)
            .unwrap_or(chrono::Duration::MAX);
        match self.last_success_at {
            Some(at) => now.signed_duration_since(at) > limit,
            None => self.last_run_at.is_some(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackedState {
    Active,
    Cancelled,
    Expired,
    Replaced,
    Dropped,
}

impl TrackedState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Replaced => "replaced",
            Self::Dropped => "dropped",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "cancelled" => Some(Self::Cancelled),
            "expired" => Some(Self::Expired),
            "replaced" => Some(Self::Replaced),
            "dropped" => Some(Self::Dropped),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedItem {
    pub id: i64,
    pub source_id: String,
    pub item_key: String,
    pub topic: String,
    pub fingerprint: String,
    pub area: String,
    pub summary: String,
    pub cancel_text: String,
    pub priority: Priority,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub state: TrackedState,
    pub misses: i64,
    /// Czy komunikat o tej pozycji poszedł na antenę (lepkie).
    pub aired: bool,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub queue_item_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTracked {
    pub source_id: String,
    pub item_key: String,
    pub topic: String,
    pub fingerprint: String,
    pub area: String,
    pub summary: String,
    pub cancel_text: String,
    pub priority: Priority,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub aired: bool,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub queue_item_id: Option<i64>,
}

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse_iso(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value).ok().map(|dt| dt.with_timezone(&Utc))
}

fn row_to_item(row: &sqlx::sqlite::SqliteRow) -> TrackedItem {
    let opt = |name: &str| row.get::<Option<String>, _>(name).as_deref().and_then(parse_iso);
    let req = |name: &str| opt(name).unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    TrackedItem {
        id: row.get("id"),
        source_id: row.get("source_id"),
        item_key: row.get("item_key"),
        topic: row.get("topic"),
        fingerprint: row.get("fingerprint"),
        area: row.get("area"),
        summary: row.get("summary"),
        cancel_text: row.get("cancel_text"),
        priority: Priority::from_i64(row.get("priority")).unwrap_or(Priority::Normal),
        valid_from: opt("valid_from"),
        valid_until: opt("valid_until"),
        state: TrackedState::parse(&row.get::<String, _>("state")).unwrap_or(TrackedState::Dropped),
        misses: row.get("misses"),
        aired: row.get::<i64, _>("aired") != 0,
        first_seen_at: req("first_seen_at"),
        last_seen_at: req("last_seen_at"),
        queue_item_id: row.get("queue_item_id"),
    }
}

const ITEM_COLUMNS: &str = "id, source_id, item_key, topic, fingerprint, area, summary, cancel_text, \
    priority, valid_from, valid_until, state, misses, aired, first_seen_at, last_seen_at, queue_item_id";

#[derive(Clone)]
pub struct SourceStateRepo {
    pool: SqlitePool,
}

impl SourceStateRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn setting(&self, key: &str) -> Result<Option<String>, DbError> {
        Ok(sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// `None` — nieznane źródło (brak wiersza w `source_state`).
    pub async fn config(&self, id: &str) -> Result<Option<SourceConfig>, DbError> {
        let enabled: Option<i64> = sqlx::query_scalar("SELECT enabled FROM source_state WHERE source_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(enabled) = enabled else {
            return Ok(None);
        };

        let raw_area = self.setting(&format!("source:{id}:area")).await?.unwrap_or_default();
        let (area, area_text) = match parse_area(&raw_area) {
            Ok(entries) => {
                let text = area_text(&entries);
                (entries, text)
            }
            Err(error) => {
                tracing::error!(source = id, %error, "zepsuty obszar źródła w bazie; traktuję jak pusty");
                (Vec::new(), String::new())
            }
        };
        let interval_s = self
            .setting(&format!("source:{id}:interval_s"))
            .await?
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| INTERVAL_RANGE.contains(v))
            .unwrap_or(DEFAULT_INTERVAL_S);

        Ok(Some(SourceConfig { enabled: enabled != 0, area, area_text, interval_s }))
    }

    pub async fn save_config(&self, id: &str, area_text: &str, interval_s: u64) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        for (key, value) in [
            (format!("source:{id}:area"), area_text.to_owned()),
            (format!("source:{id}:interval_s"), interval_s.to_string()),
        ] {
            sqlx::query(
                "INSERT INTO settings (key, value) VALUES (?, ?)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Liczba zmienionych wierszy: 0 znaczy nieznane źródło.
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<u64, DbError> {
        let result = sqlx::query("UPDATE source_state SET enabled = ? WHERE source_id = ?")
            .bind(i64::from(enabled))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn statuses(&self) -> Result<Vec<SourceStatus>, DbError> {
        let rows = sqlx::query(
            "SELECT source_id, enabled, last_run_at, last_success_at, consecutive_failures, last_error
               FROM source_state ORDER BY source_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| SourceStatus {
                source_id: row.get("source_id"),
                enabled: row.get::<i64, _>("enabled") != 0,
                last_run_at: row.get::<Option<String>, _>("last_run_at").as_deref().and_then(parse_iso),
                last_success_at: row.get::<Option<String>, _>("last_success_at").as_deref().and_then(parse_iso),
                consecutive_failures: row.get("consecutive_failures"),
                last_error: row.get("last_error"),
            })
            .collect())
    }

    pub async fn record_success(&self, id: &str, now: DateTime<Utc>) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE source_state SET last_run_at = ?1, last_success_at = ?1,
               consecutive_failures = 0, last_error = NULL WHERE source_id = ?2",
        )
        .bind(iso(now))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Zwraca nowy licznik kolejnych porażek.
    pub async fn record_failure(&self, id: &str, now: DateTime<Utc>, error: &str) -> Result<i64, DbError> {
        let count: Option<i64> = sqlx::query_scalar(
            "UPDATE source_state SET last_run_at = ?, consecutive_failures = consecutive_failures + 1,
               last_error = ? WHERE source_id = ? RETURNING consecutive_failures",
        )
        .bind(iso(now))
        .bind(error)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(count.unwrap_or(0))
    }

    pub async fn active_items(&self, id: &str) -> Result<Vec<TrackedItem>, DbError> {
        let rows = sqlx::query(&format!(
            "SELECT {ITEM_COLUMNS} FROM source_items WHERE source_id = ? AND state = 'active' ORDER BY id"
        ))
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_item).collect())
    }

    /// Wstawia pozycję albo — przy tym samym `(source_id, item_key)` —
    /// nadpisuje ją i przywraca do `active` z `misses = 0`.
    pub async fn upsert_active(&self, item: &NewTracked, now: DateTime<Utc>) -> Result<i64, DbError> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO source_items
               (source_id, item_key, topic, fingerprint, area, summary, cancel_text, priority,
                valid_from, valid_until, state, misses, aired, first_seen_at, last_seen_at, queue_item_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', 0, ?, ?, ?, ?)
             ON CONFLICT(source_id, item_key) DO UPDATE SET
               topic = excluded.topic, fingerprint = excluded.fingerprint, area = excluded.area,
               summary = excluded.summary, cancel_text = excluded.cancel_text,
               priority = excluded.priority, valid_from = excluded.valid_from,
               valid_until = excluded.valid_until, state = 'active', misses = 0, aired = excluded.aired,
               last_seen_at = ?, queue_item_id = excluded.queue_item_id
             RETURNING id",
        )
        .bind(&item.source_id)
        .bind(&item.item_key)
        .bind(&item.topic)
        .bind(&item.fingerprint)
        .bind(&item.area)
        .bind(&item.summary)
        .bind(&item.cancel_text)
        .bind(item.priority as i64)
        .bind(item.valid_from.map(iso))
        .bind(item.valid_until.map(iso))
        .bind(i64::from(item.aired))
        .bind(iso(item.first_seen_at))
        .bind(iso(item.last_seen_at))
        .bind(item.queue_item_id)
        .bind(iso(now))
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Pozycja widziana w odpowiedzi: zeruje licznik nieobecności.
    pub async fn touch(&self, row: i64, now: DateTime<Utc>) -> Result<(), DbError> {
        sqlx::query("UPDATE source_items SET last_seen_at = ?, misses = 0 WHERE id = ?")
            .bind(iso(now))
            .bind(row)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_misses(&self, row: i64, misses: i64) -> Result<(), DbError> {
        sqlx::query("UPDATE source_items SET misses = ? WHERE id = ?")
            .bind(misses)
            .bind(row)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_aired(&self, row: i64) -> Result<(), DbError> {
        sqlx::query("UPDATE source_items SET aired = 1 WHERE id = ?")
            .bind(row)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_state(&self, row: i64, state: TrackedState) -> Result<(), DbError> {
        sqlx::query("UPDATE source_items SET state = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(row)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_queue_item(&self, row: i64, queue_item_id: i64) -> Result<(), DbError> {
        sqlx::query("UPDATE source_items SET queue_item_id = ? WHERE id = ?")
            .bind(queue_item_id)
            .bind(row)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    fn tracked(key: &str) -> NewTracked {
        NewTracked {
            source_id: "imgw-meteo".into(),
            item_key: key.into(),
            topic: "Burze".into(),
            fingerprint: "fp1".into(),
            area: "10 województwo łódzkie".into(),
            summary: "Burze, stopień 1".into(),
            cancel_text: "Odwołanie".into(),
            priority: Priority::Normal,
            valid_from: Some(at("2026-07-23T08:00:00Z")),
            valid_until: Some(at("2026-07-23T20:00:00Z")),
            aired: false,
            first_seen_at: at("2026-07-23T08:01:00Z"),
            last_seen_at: at("2026-07-23T08:01:00Z"),
            queue_item_id: None,
        }
    }

    #[tokio::test]
    async fn imgw_meteo_exists_disabled_after_migration() {
        let repo = SourceStateRepo::new(test_pool().await);
        let cfg = repo.config("imgw-meteo").await.unwrap().unwrap();
        assert!(!cfg.enabled);
        assert!(cfg.area.is_empty());
        assert_eq!(cfg.interval_s, 300);
        let statuses = repo.statuses().await.unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].source_id, "imgw-meteo");
    }

    #[tokio::test]
    async fn config_round_trips_area_and_interval() {
        let repo = SourceStateRepo::new(test_pool().await);
        repo.save_config("imgw-meteo", "10\n1061 Łódź", 600).await.unwrap();
        assert_eq!(repo.set_enabled("imgw-meteo", true).await.unwrap(), 1);
        let cfg = repo.config("imgw-meteo").await.unwrap().unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.area.len(), 2);
        assert_eq!(cfg.area_text, "10 województwo łódzkie\n1061 Łódź");
        assert_eq!(cfg.interval_s, 600);

        repo.save_config("imgw-meteo", "zepsute", 5).await.unwrap();
        let cfg = repo.config("imgw-meteo").await.unwrap().unwrap();
        assert!(cfg.area.is_empty());
        assert_eq!(cfg.interval_s, 300);
    }

    #[tokio::test]
    async fn failures_count_up_and_a_success_resets_them() {
        let repo = SourceStateRepo::new(test_pool().await);
        let now = at("2026-07-23T10:00:00Z");
        assert_eq!(repo.record_failure("imgw-meteo", now, "boom").await.unwrap(), 1);
        assert_eq!(repo.record_failure("imgw-meteo", now, "boom2").await.unwrap(), 2);
        let s = &repo.statuses().await.unwrap()[0];
        assert_eq!(s.consecutive_failures, 2);
        assert_eq!(s.last_error.as_deref(), Some("boom2"));
        repo.record_success("imgw-meteo", now).await.unwrap();
        let s = &repo.statuses().await.unwrap()[0];
        assert_eq!(s.consecutive_failures, 0);
        assert_eq!(s.last_error, None);
        assert_eq!(s.last_success_at, Some(now));
    }

    #[test]
    fn staleness_follows_three_intervals_or_half_an_hour() {
        let now = at("2026-07-23T12:00:00Z");
        let status = |minutes_ago: i64| SourceStatus {
            source_id: "x".into(),
            enabled: true,
            last_run_at: Some(now),
            last_success_at: Some(now - chrono::Duration::minutes(minutes_ago)),
            consecutive_failures: 0,
            last_error: None,
        };
        assert!(!status(29).is_stale(300, now));
        assert!(status(31).is_stale(300, now));
        assert!(!status(50).is_stale(1200, now));
        assert!(status(61).is_stale(1200, now));
        let never = SourceStatus { last_success_at: None, ..status(0) };
        assert!(never.is_stale(300, now));
        let untried = SourceStatus { last_run_at: None, ..never };
        assert!(!untried.is_stale(300, now));
    }

    #[tokio::test]
    async fn an_item_seen_again_after_cancellation_becomes_active() {
        let repo = SourceStateRepo::new(test_pool().await);
        let now = at("2026-07-23T09:00:00Z");
        let row = repo.upsert_active(&tracked("a"), now).await.unwrap();
        repo.set_misses(row, 1).await.unwrap();
        repo.set_state(row, TrackedState::Cancelled).await.unwrap();
        assert!(repo.active_items("imgw-meteo").await.unwrap().is_empty());

        let again = repo.upsert_active(&tracked("a"), now).await.unwrap();
        assert_eq!(again, row);
        let active = repo.active_items("imgw-meteo").await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].misses, 0);
        assert_eq!(active[0].state, TrackedState::Active);
        assert_eq!(active[0].valid_until, Some(at("2026-07-23T20:00:00Z")));
    }

    #[tokio::test]
    async fn unknown_source_has_no_config() {
        let repo = SourceStateRepo::new(test_pool().await);
        assert!(repo.config("nie-ma").await.unwrap().is_none());
        assert_eq!(repo.set_enabled("nie-ma", true).await.unwrap(), 0);
    }
}
