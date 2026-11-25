//! Dziennik źródeł danych: dlaczego źródło nie działa i kiedy wróciło.
//!
//! Kolejne jednakowe zdarzenia (ten sam rodzaj i treść) to jeden wpis
//! z licznikiem i czasem pierwszego i ostatniego wystąpienia, więc źródło
//! padające co pięć minut przez dobę zostawia jeden wiersz, nie trzysta.

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use crate::error::DbError;

/// Tyle ostatnich wpisów zostaje na każde źródło.
pub const KEEP_PER_SOURCE: i64 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// Odpytanie nie powiodło się (sieć, kod HTTP, nieczytelna odpowiedź).
    Error,
    /// Źródło włączone, ale nie mogło nawet spróbować (brak pozycji, obszaru).
    Skipped,
    /// Pierwsze udane odpytanie po błędzie albo pominięciu.
    Recovered,
}

impl LogKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Skipped => "skipped",
            Self::Recovered => "recovered",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "error" => Some(Self::Error),
            "skipped" => Some(Self::Skipped),
            "recovered" => Some(Self::Recovered),
            _ => None,
        }
    }

    /// Etykieta w panelu.
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "błąd",
            Self::Skipped => "pominięte",
            Self::Recovered => "działa ponownie",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub id: i64,
    pub source_id: String,
    pub kind: LogKind,
    pub message: String,
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
    pub count: i64,
}

/// Komunikat wpisu „działa ponownie”.
pub const RECOVERED_MESSAGE: &str = "odpytanie znów się udało";

#[derive(Clone)]
pub struct SourceLogRepo {
    pool: SqlitePool,
}

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse_iso(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

type Row = (i64, String, String, String, String, String, i64);

fn to_entry((id, source_id, kind, message, first_at, last_at, count): Row) -> LogEntry {
    LogEntry {
        id,
        source_id,
        kind: LogKind::parse(&kind).unwrap_or(LogKind::Error),
        message,
        first_at: parse_iso(&first_at),
        last_at: parse_iso(&last_at),
        count,
    }
}

const COLUMNS: &str = "id, source_id, kind, message, first_at, last_at, count";

impl SourceLogRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Ostatni wpis źródła.
    pub async fn latest(&self, source_id: &str) -> Result<Option<LogEntry>, DbError> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM source_log WHERE source_id = ? ORDER BY id DESC LIMIT 1"
        ))
        .bind(source_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(to_entry))
    }

    /// Zapisuje błąd albo pominięcie. To samo co ostatni wpis tylko podbija
    /// licznik i czas ostatniego wystąpienia.
    pub async fn problem(
        &self,
        source_id: &str,
        kind: LogKind,
        message: &str,
        now: DateTime<Utc>,
    ) -> Result<(), DbError> {
        debug_assert!(kind != LogKind::Recovered);
        if let Some(last) = self.latest(source_id).await?
            && last.kind == kind
            && last.message == message
        {
            sqlx::query("UPDATE source_log SET last_at = ?, count = count + 1 WHERE id = ?")
                .bind(iso(now))
                .bind(last.id)
                .execute(&self.pool)
                .await?;
            return Ok(());
        }
        self.insert(source_id, kind, message, now).await
    }

    /// Udane odpytanie: wpis „działa ponownie” tylko wtedy, gdy ostatnim
    /// wpisem był błąd albo pominięcie (zwykły sukces nic nie zapisuje).
    pub async fn success(&self, source_id: &str, now: DateTime<Utc>) -> Result<(), DbError> {
        match self.latest(source_id).await? {
            Some(last) if last.kind != LogKind::Recovered => {
                self.insert(source_id, LogKind::Recovered, RECOVERED_MESSAGE, now).await
            }
            _ => Ok(()),
        }
    }

    async fn insert(
        &self,
        source_id: &str,
        kind: LogKind,
        message: &str,
        now: DateTime<Utc>,
    ) -> Result<(), DbError> {
        let at = iso(now);
        sqlx::query(
            "INSERT INTO source_log (source_id, kind, message, first_at, last_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(source_id)
        .bind(kind.as_str())
        .bind(message)
        .bind(&at)
        .bind(&at)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "DELETE FROM source_log WHERE source_id = ?1 AND id NOT IN
               (SELECT id FROM source_log WHERE source_id = ?1 ORDER BY id DESC LIMIT ?2)",
        )
        .bind(source_id)
        .bind(KEEP_PER_SOURCE)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Najnowsze wpisy wszystkich źródeł (po czasie ostatniego wystąpienia).
    pub async fn recent(&self, limit: i64) -> Result<Vec<LogEntry>, DbError> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM source_log ORDER BY last_at DESC, id DESC LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(to_entry).collect())
    }

    /// Najnowsze wpisy jednego źródła.
    pub async fn for_source(&self, source_id: &str, limit: i64) -> Result<Vec<LogEntry>, DbError> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM source_log WHERE source_id = ? ORDER BY id DESC LIMIT ?"
        ))
        .bind(source_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(to_entry).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    #[tokio::test]
    async fn the_same_problem_in_a_row_is_one_entry_with_a_count() {
        let log = SourceLogRepo::new(test_pool().await);
        let e = LogKind::Error;
        log.problem("gios", e, "serwer odpowiedział kodem 503", at("2026-11-25T10:00:00Z")).await.unwrap();
        log.problem("gios", e, "serwer odpowiedział kodem 503", at("2026-11-25T10:05:00Z")).await.unwrap();
        log.problem("gios", e, "serwer odpowiedział kodem 503", at("2026-11-25T10:10:00Z")).await.unwrap();
        let rows = log.for_source("gios", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 3);
        assert_eq!(rows[0].first_at, at("2026-11-25T10:00:00Z"));
        assert_eq!(rows[0].last_at, at("2026-11-25T10:10:00Z"));

        // Inna treść to nowy wpis.
        log.problem("gios", e, "nieczytelna odpowiedź: html", at("2026-11-25T10:15:00Z")).await.unwrap();
        assert_eq!(log.for_source("gios", 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn recovery_is_logged_once_and_only_after_a_problem() {
        let log = SourceLogRepo::new(test_pool().await);
        // Źródło zdrowe od początku: nic.
        log.success("swpc", at("2026-11-25T10:00:00Z")).await.unwrap();
        assert!(log.for_source("swpc", 10).await.unwrap().is_empty());

        log.problem("swpc", LogKind::Skipped, "brak pozycji stacji", at("2026-11-25T10:05:00Z")).await.unwrap();
        log.success("swpc", at("2026-11-25T10:10:00Z")).await.unwrap();
        log.success("swpc", at("2026-11-25T10:15:00Z")).await.unwrap();
        let rows = log.for_source("swpc", 10).await.unwrap();
        let kinds: Vec<_> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, [LogKind::Recovered, LogKind::Skipped]);
        assert_eq!(rows[0].message, RECOVERED_MESSAGE);

        // Ten sam błąd po powrocie to nowy wpis, nie podbicie starego.
        log.problem("swpc", LogKind::Skipped, "brak pozycji stacji", at("2026-11-25T10:20:00Z")).await.unwrap();
        assert_eq!(log.for_source("swpc", 10).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn each_source_keeps_only_its_newest_entries() {
        let log = SourceLogRepo::new(test_pool().await);
        let base = at("2026-11-25T00:00:00Z");
        log.problem("emsc", LogKind::Error, "inne źródło", base).await.unwrap();
        for i in 0..(KEEP_PER_SOURCE + 10) {
            let now = base + chrono::Duration::minutes(i + 1);
            log.problem("gios", LogKind::Error, &format!("błąd {i}"), now).await.unwrap();
        }
        let rows = log.for_source("gios", 1000).await.unwrap();
        assert_eq!(rows.len() as i64, KEEP_PER_SOURCE);
        assert_eq!(rows[0].message, format!("błąd {}", KEEP_PER_SOURCE + 9));
        // Przycinanie jednego źródła nie rusza innych.
        assert_eq!(log.for_source("emsc", 10).await.unwrap().len(), 1);
        // Wspólna lista: najnowsze najpierw.
        let recent = log.recent(2).await.unwrap();
        assert_eq!(recent[0].source_id, "gios");
    }
}
