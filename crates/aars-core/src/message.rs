use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

use crate::error::DbError;
use crate::priority::Priority;

/// Kiedy komunikat ma być nadany.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleKind {
    /// Nadawany tylko wtedy, gdy ktoś go wprost zleci.
    Immediate,
    /// Jednorazowo o zadanej chwili.
    At(DateTime<Utc>),
    /// Cyklicznie po wzorcu cron.
    Cron(String),
}

impl ScheduleKind {
    fn kind_str(&self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::At(_) => "at",
            Self::Cron(_) => "cron",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: i64,
    pub title: String,
    pub body_template: String,
    pub priority: Priority,
    pub interrupting: bool,
    pub repeat_count: u32,
    pub repeat_interval: Duration,
    pub schedule: ScheduleKind,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub enabled: bool,
    pub source_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub title: String,
    pub body_template: String,
    pub priority: Priority,
    pub interrupting: bool,
    pub repeat_count: u32,
    pub repeat_interval: Duration,
    pub schedule: ScheduleKind,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub source_id: Option<String>,
    pub created_by: Option<i64>,
}

/// Jedyne miejsce, które zna tabelę `messages`.
pub struct MessageRepo {
    pool: SqlitePool,
}

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse_iso(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value).ok().map(|dt| dt.with_timezone(&Utc))
}

const COLUMNS: &str = "id, title, body_template, priority, interrupting, repeat_count, \
                       repeat_interval_s, schedule_kind, schedule_at, cron_expr, \
                       valid_from, valid_until, enabled, source_id";

fn row_to_message(row: &sqlx::sqlite::SqliteRow) -> Message {
    let kind: String = row.get("schedule_kind");
    let schedule_at: Option<String> = row.get("schedule_at");
    let cron_expr: Option<String> = row.get("cron_expr");

    let schedule = match kind.as_str() {
        "at" => schedule_at
            .as_deref()
            .and_then(parse_iso)
            .map(ScheduleKind::At)
            // Wiersz z `at` bez czasu jest niemożliwy dzięki CHECK w migracji;
            // gdyby się zdarzył, traktujemy go jako niezaplanowany, zamiast
            // zgadywać godzinę nadania.
            .unwrap_or(ScheduleKind::Immediate),
        "cron" => cron_expr.map(ScheduleKind::Cron).unwrap_or(ScheduleKind::Immediate),
        _ => ScheduleKind::Immediate,
    };

    let valid_from: Option<String> = row.get("valid_from");
    let valid_until: Option<String> = row.get("valid_until");

    Message {
        id: row.get("id"),
        title: row.get("title"),
        body_template: row.get("body_template"),
        priority: Priority::from_i64(row.get::<i64, _>("priority")).unwrap_or(Priority::Normal),
        interrupting: row.get::<i64, _>("interrupting") != 0,
        repeat_count: row.get::<i64, _>("repeat_count").max(1) as u32,
        repeat_interval: Duration::from_secs(row.get::<i64, _>("repeat_interval_s").max(0) as u64),
        schedule,
        valid_from: valid_from.as_deref().and_then(parse_iso),
        valid_until: valid_until.as_deref().and_then(parse_iso),
        enabled: row.get::<i64, _>("enabled") != 0,
        source_id: row.get("source_id"),
    }
}

impl MessageRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn create(&self, new: NewMessage) -> Result<i64, DbError> {
        let (schedule_at, cron_expr) = match &new.schedule {
            ScheduleKind::Immediate => (None, None),
            ScheduleKind::At(at) => (Some(iso(*at)), None),
            ScheduleKind::Cron(expr) => (None, Some(expr.clone())),
        };

        let id: i64 = sqlx::query_scalar(
            "INSERT INTO messages
               (title, kind, body_template, priority, interrupting, repeat_count,
                repeat_interval_s, schedule_kind, schedule_at, cron_expr,
                valid_from, valid_until, source_id, created_by)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(&new.title)
        .bind(if new.source_id.is_some() { "source" } else { "manual" })
        .bind(&new.body_template)
        .bind(new.priority as i64)
        .bind(i64::from(new.interrupting))
        .bind(i64::from(new.repeat_count.max(1)))
        .bind(new.repeat_interval.as_secs() as i64)
        .bind(new.schedule.kind_str())
        .bind(schedule_at)
        .bind(cron_expr)
        .bind(new.valid_from.map(iso))
        .bind(new.valid_until.map(iso))
        .bind(&new.source_id)
        .bind(new.created_by)
        .fetch_one(&self.pool)
        .await?;

        Ok(id)
    }

    pub async fn get(&self, id: i64) -> Result<Option<Message>, DbError> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM messages WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_message))
    }

    pub async fn list(&self) -> Result<Vec<Message>, DbError> {
        let rows = sqlx::query(&format!("SELECT {COLUMNS} FROM messages ORDER BY id"))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_message).collect())
    }

    /// Definicje, którymi zajmuje się harmonogram: włączone i mające czym
    /// odpalić.
    pub async fn scheduled(&self) -> Result<Vec<Message>, DbError> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM messages
              WHERE enabled = 1 AND schedule_kind IN ('at', 'cron')
              ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_message).collect())
    }

    pub async fn set_enabled(&self, id: i64, enabled: bool) -> Result<(), DbError> {
        sqlx::query("UPDATE messages SET enabled = ?, updated_at = ? WHERE id = ?")
            .bind(i64::from(enabled))
            .bind(iso(Utc::now()))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Zmienia treść, priorytet, powtórzenia i harmonogram; zadania już w
    /// kolejce zostają, jakie były. `valid_from`, `valid_until`, `source_id`
    /// i `kind` z `new` są ignorowane (zostają, jakie były). Zwraca
    /// liczbę zmienionych wierszy (0, gdy definicji nie ma).
    pub async fn update(&self, id: i64, new: NewMessage) -> Result<u64, DbError> {
        let (schedule_at, cron_expr) = match &new.schedule {
            ScheduleKind::Immediate => (None, None),
            ScheduleKind::At(at) => (Some(iso(*at)), None),
            ScheduleKind::Cron(expr) => (None, Some(expr.clone())),
        };

        let result = sqlx::query(
            "UPDATE messages
                SET title = ?, body_template = ?, priority = ?, interrupting = ?,
                    repeat_count = ?, repeat_interval_s = ?, schedule_kind = ?,
                    schedule_at = ?, cron_expr = ?, updated_at = ?
              WHERE id = ?",
        )
        .bind(&new.title)
        .bind(&new.body_template)
        .bind(new.priority as i64)
        .bind(i64::from(new.interrupting))
        .bind(i64::from(new.repeat_count.max(1)))
        .bind(new.repeat_interval.as_secs() as i64)
        .bind(new.schedule.kind_str())
        .bind(schedule_at)
        .bind(cron_expr)
        .bind(iso(Utc::now()))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn delete(&self, id: i64) -> Result<u64, DbError> {
        let result =
            sqlx::query("DELETE FROM messages WHERE id = ?").bind(id).execute(&self.pool).await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    fn cron_message(title: &str, expr: &str) -> NewMessage {
        NewMessage {
            title: title.to_owned(),
            body_template: "treść".to_owned(),
            priority: Priority::Normal,
            interrupting: false,
            repeat_count: 1,
            repeat_interval: Duration::ZERO,
            schedule: ScheduleKind::Cron(expr.to_owned()),
            valid_from: None,
            valid_until: None,
            source_id: None,
            created_by: None,
        }
    }

    #[tokio::test]
    async fn a_created_message_reads_back_with_everything_it_was_given() {
        let repo = MessageRepo::new(test_pool().await);
        let mut new = cron_message("biuletyn", "0 18 * * *");
        new.priority = Priority::High;
        new.repeat_count = 2;
        new.repeat_interval = Duration::from_secs(300);

        let id = repo.create(new).await.unwrap();
        let message = repo.get(id).await.unwrap().expect("musi istnieć");

        assert_eq!(message.title, "biuletyn");
        assert_eq!(message.priority, Priority::High);
        assert_eq!(message.repeat_count, 2);
        assert_eq!(message.repeat_interval, Duration::from_secs(300));
        assert_eq!(message.schedule, ScheduleKind::Cron("0 18 * * *".to_owned()));
        assert!(message.enabled);
    }

    #[tokio::test]
    async fn a_one_shot_message_keeps_its_timestamp() {
        let repo = MessageRepo::new(test_pool().await);
        let at = Utc::now() + chrono::Duration::hours(2);
        let mut new = cron_message("jednorazowy", "0 18 * * *");
        new.schedule = ScheduleKind::At(at);

        let id = repo.create(new).await.unwrap();
        let message = repo.get(id).await.unwrap().unwrap();

        match message.schedule {
            ScheduleKind::At(stored) => {
                assert_eq!(stored.timestamp(), at.timestamp(), "sekundy muszą się zgadzać");
            }
            other => panic!("oczekiwano At, dostano {other:?}"),
        }
    }

    #[tokio::test]
    async fn scheduled_skips_immediate_and_disabled_definitions() {
        let repo = MessageRepo::new(test_pool().await);

        let mut immediate = cron_message("natychmiast", "0 18 * * *");
        immediate.schedule = ScheduleKind::Immediate;
        repo.create(immediate).await.unwrap();

        let disabled = repo.create(cron_message("wyłączony", "0 6 * * *")).await.unwrap();
        repo.set_enabled(disabled, false).await.unwrap();

        let active = repo.create(cron_message("aktywny", "0 12 * * *")).await.unwrap();

        let scheduled = repo.scheduled().await.unwrap();
        assert_eq!(scheduled.len(), 1, "{scheduled:?}");
        assert_eq!(scheduled[0].id, active);
    }

    #[tokio::test]
    async fn update_changes_the_definition_but_not_queued_items() {
        use crate::queue::{EnqueueRequest, QueueRepo};

        let pool = test_pool().await;
        let repo = MessageRepo::new(pool.clone());
        let queue = QueueRepo::new(pool);
        let until = Utc::now() + chrono::Duration::days(30);
        let mut old = cron_message("stary", "0 18 * * *");
        old.valid_until = Some(until);
        old.source_id = Some("zrodlo-1".to_owned());
        let id = repo.create(old).await.unwrap();
        queue
            .enqueue(EnqueueRequest {
                message_id: Some(id),
                rendered_text: "stara treść".to_owned(),
                priority: Priority::Normal,
                interrupting: false,
                not_before: None,
                expires_at: None,
                repeat_count: 1,
                repeat_interval: Duration::ZERO,
                created_by: None,
            })
            .await
            .unwrap();

        let mut new = cron_message("nowy", "30 6 * * *");
        new.body_template = "nowa treść".to_owned();
        new.priority = Priority::High;
        assert_eq!(repo.update(id, new).await.unwrap(), 1);

        let message = repo.get(id).await.unwrap().unwrap();
        assert_eq!(message.title, "nowy");
        assert_eq!(message.body_template, "nowa treść");
        assert_eq!(message.priority, Priority::High);
        assert_eq!(message.schedule, ScheduleKind::Cron("30 6 * * *".to_owned()));
        // Pola, których formularz nie edytuje, zostają.
        assert_eq!(message.valid_until.map(|t| t.timestamp()), Some(until.timestamp()));
        assert_eq!(message.source_id.as_deref(), Some("zrodlo-1"));

        let pending = queue.pending(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].rendered_text, "stara treść");
    }
}
