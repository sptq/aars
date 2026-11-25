use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

use crate::error::DbError;
use crate::priority::Priority;

/// Ile razy próbujemy nadać jedno zadanie, zanim uznamy je za nieudane.
/// Bez limitu element odrzucany przez interlock krążyłby w nieskończoność.
pub const MAX_ATTEMPTS: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueState {
    Pending,
    Transmitting,
    Done,
    Failed,
    Cancelled,
    Expired,
    Interrupted,
}

impl QueueState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Transmitting => "transmitting",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "transmitting" => Some(Self::Transmitting),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "expired" => Some(Self::Expired),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueItem {
    pub id: i64,
    pub message_id: Option<i64>,
    pub rendered_text: String,
    pub priority: Priority,
    pub interrupting: bool,
    pub state: QueueState,
    pub not_before: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub repeat_group: Option<String>,
    pub repeat_index: i64,
    pub attempt: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EnqueueRequest {
    pub message_id: Option<i64>,
    pub rendered_text: String,
    pub priority: Priority,
    pub interrupting: bool,
    /// `None` znaczy „teraz".
    pub not_before: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Ile razy nadać; 1 to jedno nadanie.
    pub repeat_count: u32,
    pub repeat_interval: Duration,
    pub created_by: Option<i64>,
}

#[derive(Clone, Copy)]
enum CancelTarget<'a> {
    Id(i64),
    Group(&'a str),
}

/// Jedyne miejsce, które zna tabelę `queue_items`.
pub struct QueueRepo {
    pool: SqlitePool,
}

/// Licznik do identyfikatorów grup powtórzeń. Znacznik czasu sam nie
/// wystarcza: dwa zlecenia w tej samej chwili dostałyby jedną grupę.
static GROUP_SEQ: AtomicU64 = AtomicU64::new(0);

fn new_group_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seq = GROUP_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{seq:x}")
}

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse_iso(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

const COLUMNS: &str = "id, message_id, rendered_text, priority, interrupting, state, \
                       not_before, expires_at, repeat_group, repeat_index, attempt, error";

fn row_to_item(row: &sqlx::sqlite::SqliteRow) -> QueueItem {
    let priority_raw: i64 = row.get("priority");
    let state_raw: String = row.get("state");
    let not_before: String = row.get("not_before");
    let expires_at: Option<String> = row.get("expires_at");

    QueueItem {
        id: row.get("id"),
        message_id: row.get("message_id"),
        rendered_text: row.get("rendered_text"),
        priority: Priority::from_i64(priority_raw).unwrap_or(Priority::Normal),
        interrupting: row.get::<i64, _>("interrupting") != 0,
        state: QueueState::parse(&state_raw).unwrap_or(QueueState::Pending),
        not_before: parse_iso(&not_before),
        expires_at: expires_at.as_deref().map(parse_iso),
        repeat_group: row.get("repeat_group"),
        repeat_index: row.get("repeat_index"),
        attempt: row.get("attempt"),
        error: row.get("error"),
    }
}

impl QueueRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Wstawia zadanie, rozwijając powtórzenia w osobne wiersze ze wspólną
    /// grupą i narastającym `not_before`.
    pub async fn enqueue(&self, request: EnqueueRequest) -> Result<Vec<i64>, DbError> {
        let count = request.repeat_count.max(1);
        let base = request.not_before.unwrap_or_else(Utc::now);
        let group = if count > 1 { Some(new_group_id()) } else { None };

        // Wszystkie terminy liczymy przed pierwszym INSERT-em: przepełnienie
        // to odmowa całego zlecenia, nie panika ani połowa grupy w kolejce.
        let times = (0..count)
            .map(|index| {
                request
                    .repeat_interval
                    .checked_mul(index)
                    .and_then(|offset| chrono::Duration::from_std(offset).ok())
                    .and_then(|offset| base.checked_add_signed(offset))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                DbError::Invalid("odstęp powtórzeń wychodzi poza zakres dat".to_owned())
            })?;

        let mut ids = Vec::with_capacity(times.len());
        for (index, not_before) in (0..count).zip(times) {

            let id: i64 = sqlx::query_scalar(
                "INSERT INTO queue_items
                   (message_id, rendered_text, priority, interrupting, state,
                    not_before, expires_at, repeat_group, repeat_index, created_by)
                 VALUES (?, ?, ?, ?, 'pending', ?, ?, ?, ?, ?)
                 RETURNING id",
            )
            .bind(request.message_id)
            .bind(&request.rendered_text)
            .bind(request.priority as i64)
            // Przerywać może tylko pierwsze nadanie serii; powtórki czekają
            // na swoją kolej, żeby nie ucinać audycji co odstęp.
            .bind(i64::from(request.interrupting && index == 0))
            .bind(iso(not_before))
            .bind(request.expires_at.map(iso))
            .bind(group.as_deref())
            .bind(i64::from(index))
            .bind(request.created_by)
            .fetch_one(&self.pool)
            .await?;

            ids.push(id);
        }

        Ok(ids)
    }

    /// Bierze najpilniejsze gotowe zadanie i od razu znaczy je jako nadawane,
    /// żeby nikt nie wziął go drugi raz.
    pub async fn claim_next(&self) -> Result<Option<QueueItem>, DbError> {
        let now = iso(Utc::now());
        let row = sqlx::query(&format!(
            "UPDATE queue_items
                SET state = 'transmitting',
                    started_at = ?1,
                    attempt = attempt + 1
              WHERE id = (
                    SELECT id FROM queue_items
                     WHERE state = 'pending'
                       AND cancel_requested = 0
                       AND not_before <= ?1
                       AND (expires_at IS NULL OR expires_at > ?1)
                       AND attempt < {MAX_ATTEMPTS}
                     ORDER BY priority ASC, not_before ASC, created_at ASC
                     LIMIT 1)
            RETURNING {COLUMNS}"
        ))
        .bind(&now)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.as_ref().map(row_to_item))
    }

    /// Czy czeka pilne zadanie (`Emergency` albo `High`), które wejdzie na
    /// antenę w ciągu `horizon`. Aktualizacja go nie wyprzedza (R10c).
    pub async fn urgent_due(&self, horizon: Duration) -> Result<bool, DbError> {
        let now = Utc::now();
        // Horyzont to sekundy albo minuty; absurdalnie długi przycinamy do roku.
        let horizon = chrono::Duration::from_std(horizon)
            .unwrap_or(chrono::Duration::days(365))
            .min(chrono::Duration::days(365));
        let limit = iso(now + horizon);
        let due: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS(
                SELECT 1 FROM queue_items
                 WHERE state = 'pending'
                   AND cancel_requested = 0
                   AND priority <= ?1
                   AND (not_before IS NULL OR not_before <= ?2)
                   AND (expires_at IS NULL OR expires_at > ?3)
                   AND attempt < {MAX_ATTEMPTS})"
        ))
        .bind(Priority::High as i64)
        .bind(&limit)
        .bind(iso(now))
        .fetch_one(&self.pool)
        .await?;
        Ok(due)
    }

    /// Szuka zadania przerywającego, które bije trwające nadanie. Nie zabiera
    /// go — decyzję o przerwaniu podejmuje dyspozytor.
    pub async fn peek_interrupting(&self, current: Priority) -> Result<Option<QueueItem>, DbError> {
        let now = iso(Utc::now());
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM queue_items
              WHERE state = 'pending'
                AND cancel_requested = 0
                AND interrupting = 1
                AND priority < ?1
                AND not_before <= ?2
                AND (expires_at IS NULL OR expires_at > ?2)
                AND attempt < {MAX_ATTEMPTS}
              ORDER BY priority ASC, not_before ASC, created_at ASC
              LIMIT 1"
        ))
        .bind(current as i64)
        .bind(&now)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.as_ref().map(row_to_item))
    }

    pub async fn mark_done(&self, id: i64) -> Result<(), DbError> {
        self.finish(id, QueueState::Done, None).await
    }

    pub async fn mark_failed(&self, id: i64, error: &str) -> Result<(), DbError> {
        self.finish(id, QueueState::Failed, Some(error)).await
    }

    async fn finish(&self, id: i64, state: QueueState, error: Option<&str>) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE queue_items
                SET state = ?1, finished_at = ?2,
                    error = CASE WHEN ?1 = 'done' THEN NULL ELSE COALESCE(?3, error) END
              WHERE id = ?4",
        )
        .bind(state.as_str())
        .bind(iso(Utc::now()))
        .bind(error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Zwraca zadanie do kolejki z powodem. Po wyczerpaniu prób zamiast
    /// kolejnego obiegu zostaje oznaczone jako nieudane.
    pub async fn requeue(
        &self,
        id: i64,
        reason: &str,
        not_before: DateTime<Utc>,
    ) -> Result<(), DbError> {
        let attempt: i64 = sqlx::query_scalar("SELECT attempt FROM queue_items WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;

        if attempt >= MAX_ATTEMPTS {
            tracing::warn!(id, attempt, reason, "wyczerpane próby nadania, oznaczam jako nieudane");
            return self.mark_failed(id, reason).await;
        }

        sqlx::query(
            "UPDATE queue_items
                SET state = CASE WHEN cancel_requested = 1 THEN 'cancelled' ELSE 'pending' END,
                    finished_at = CASE WHEN cancel_requested = 1 THEN ?1 ELSE finished_at END,
                    not_before = ?2, started_at = NULL, error = ?3
              WHERE id = ?4",
        )
        .bind(iso(Utc::now()))
        .bind(iso(not_before))
        .bind(reason)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Wstrzymuje zadanie odrzucone przez interlock: wraca jako `pending`
    /// z powodem i bez zużytej próby (próby liczą tylko prawdziwe porażki).
    /// Element opuści kolejkę tylko przez nadanie, anulowanie lub `expires_at`.
    pub async fn hold(
        &self,
        id: i64,
        reason: &str,
        not_before: DateTime<Utc>,
    ) -> Result<(), DbError> {
        self.release_claim(id, reason, not_before).await
    }

    /// Nadanie przerwane pilniejszym komunikatem: wraca od razu, bez zużycia
    /// próby (spec §5.1), więc wielokrotne przerwania nie kończą się porażką.
    pub async fn interrupted(&self, id: i64, reason: &str) -> Result<(), DbError> {
        self.release_claim(id, reason, Utc::now()).await
    }

    /// Element z prośbą o anulowanie (`cancel` w trakcie claimu) nie wraca
    /// do kolejki, tylko kończy jako `cancelled`.
    async fn release_claim(
        &self,
        id: i64,
        reason: &str,
        not_before: DateTime<Utc>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE queue_items
                SET state = CASE WHEN cancel_requested = 1 THEN 'cancelled' ELSE 'pending' END,
                    finished_at = CASE WHEN cancel_requested = 1 THEN ?1 ELSE finished_at END,
                    not_before = ?2, started_at = NULL, error = ?3,
                    attempt = MAX(attempt - 1, 0)
              WHERE id = ?4 AND state = 'transmitting'",
        )
        .bind(iso(Utc::now()))
        .bind(iso(not_before))
        .bind(reason)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stan jednego elementu kolejki; `None`, gdy go nie ma.
    pub async fn state_of(&self, id: i64) -> Result<Option<QueueState>, DbError> {
        let state: Option<String> = sqlx::query_scalar("SELECT state FROM queue_items WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(state.as_deref().and_then(QueueState::parse))
    }

    /// Anuluje element. Czekający kończy od razu jako `cancelled`. Wzięty
    /// (`transmitting`) dostaje prośbę o anulowanie: nadanie, które już
    /// trwa, kończy się normalnie, ale element odrzucony przez interlock
    /// albo przerwany nie wraca do kolejki (`release_claim`). Zwraca liczbę
    /// elementów anulowanych albo oznaczonych.
    pub async fn cancel(&self, id: i64) -> Result<u64, DbError> {
        self.cancel_where(CancelTarget::Id(id)).await
    }

    /// Stany wszystkich elementów serii, do której należy `id` (sam `id`,
    /// gdy nie jest w serii). Pusta lista, gdy elementu nie ma.
    pub async fn series_states(&self, id: i64) -> Result<Vec<QueueState>, DbError> {
        let states: Vec<String> = sqlx::query_scalar(
            "SELECT state FROM queue_items
              WHERE id = ?1
                 OR (repeat_group IS NOT NULL
                     AND repeat_group = (SELECT repeat_group FROM queue_items WHERE id = ?1))",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(states.iter().filter_map(|s| QueueState::parse(s)).collect())
    }

    /// Grupa powtórzeń elementu (`None` dla pojedynczego albo nieistniejącego).
    pub async fn group_of(&self, id: i64) -> Result<Option<String>, DbError> {
        let group: Option<Option<String>> =
            sqlx::query_scalar("SELECT repeat_group FROM queue_items WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(group.flatten())
    }

    /// Anuluje pozostałe wystąpienia powtórzenia (także wzięte, jak `cancel`).
    pub async fn cancel_group(&self, group: &str) -> Result<u64, DbError> {
        self.cancel_where(CancelTarget::Group(group)).await
    }

    async fn cancel_where(&self, target: CancelTarget<'_>) -> Result<u64, DbError> {
        let filter = match target {
            CancelTarget::Id(_) => "id = ?",
            CancelTarget::Group(_) => "repeat_group = ?",
        };
        let cancel = format!(
            "UPDATE queue_items SET state = 'cancelled', finished_at = ? WHERE {filter} AND state = 'pending'"
        );
        let request = format!(
            "UPDATE queue_items SET cancel_requested = 1
              WHERE {filter} AND state = 'transmitting' AND cancel_requested = 0"
        );
        let cancel = sqlx::query(&cancel).bind(iso(Utc::now()));
        let request = sqlx::query(&request);
        let (cancel, request) = match target {
            CancelTarget::Id(id) => (cancel.bind(id), request.bind(id)),
            CancelTarget::Group(group) => (cancel.bind(group), request.bind(group)),
        };

        let mut tx = self.pool.begin().await?;
        let cancelled = cancel.execute(&mut *tx).await?.rows_affected();
        let requested = request.execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(cancelled + requested)
    }

    /// Oznacza zadania, których termin ważności minął. Nieaktualne
    /// ostrzeżenie nadane jako aktualne jest gorsze niż nienadane.
    pub async fn expire_stale(&self) -> Result<u64, DbError> {
        let now = iso(Utc::now());
        let result = sqlx::query(
            "UPDATE queue_items SET state = 'expired', finished_at = ?1
              WHERE state = 'pending' AND expires_at IS NOT NULL AND expires_at <= ?1",
        )
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Po starcie procesu zadania zostawione w stanie `transmitting` wracają
    /// do kolejki: zanik zasilania w trakcie nadania nie może zgubić
    /// komunikatu ani zostawić wiersza zablokowanego na zawsze.
    pub async fn recover_after_restart(&self) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE queue_items
                SET state = CASE WHEN cancel_requested = 1 THEN 'cancelled' ELSE 'pending' END,
                    finished_at = CASE WHEN cancel_requested = 1 THEN ?1 ELSE finished_at END,
                    started_at = NULL,
                    error = 'przerwane restartem procesu'
              WHERE state = 'transmitting'",
        )
        .bind(iso(Utc::now()))
        .execute(&self.pool)
        .await?;

        if result.rows_affected() > 0 {
            tracing::warn!(
                count = result.rows_affected(),
                "zadania przerwane restartem wróciły do kolejki"
            );
        }
        Ok(result.rows_affected())
    }

    /// Liczba wszystkich wystąpień (także zakończonych) każdej serii, która
    /// ma jeszcze coś w kolejce; mianownik „n/N” na stronie kolejki.
    pub async fn group_sizes(&self) -> Result<HashMap<String, i64>, DbError> {
        let rows = sqlx::query(
            "SELECT repeat_group, COUNT(*) AS size FROM queue_items
              WHERE repeat_group IN (
                    SELECT repeat_group FROM queue_items
                     WHERE state = 'pending' AND repeat_group IS NOT NULL)
              GROUP BY repeat_group",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get("repeat_group"), r.get("size"))).collect())
    }

    pub async fn pending(&self, limit: i64) -> Result<Vec<QueueItem>, DbError> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM queue_items
              WHERE state = 'pending'
              ORDER BY priority ASC, not_before ASC, created_at ASC
              LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_item).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    fn req(text: &str, priority: Priority) -> EnqueueRequest {
        EnqueueRequest {
            message_id: None,
            rendered_text: text.to_owned(),
            priority,
            interrupting: false,
            not_before: None,
            expires_at: None,
            repeat_count: 1,
            repeat_interval: Duration::ZERO,
            created_by: None,
        }
    }

    #[tokio::test]
    async fn urgent_due_sees_high_and_emergency_only_within_the_horizon() {
        let horizon = Duration::from_secs(60);

        let repo = QueueRepo::new(test_pool().await);
        repo.enqueue(req("zwykły teraz", Priority::Normal))
            .await
            .unwrap();
        repo.enqueue(req("niski teraz", Priority::Low))
            .await
            .unwrap();
        assert!(!repo.urgent_due(horizon).await.unwrap());

        let mut later = req("pilny za 10 min", Priority::Emergency);
        later.not_before = Some(Utc::now() + chrono::Duration::minutes(10));
        repo.enqueue(later).await.unwrap();
        assert!(!repo.urgent_due(horizon).await.unwrap());

        let mut soon = req("wysoki za 30 s", Priority::High);
        soon.not_before = Some(Utc::now() + chrono::Duration::seconds(30));
        repo.enqueue(soon).await.unwrap();
        assert!(repo.urgent_due(horizon).await.unwrap());

        // Teraz, ale już nie czeka: nadawane, anulowane i przeterminowane się nie liczą.
        let repo = QueueRepo::new(test_pool().await);
        repo.enqueue(req("nadawany", Priority::Emergency))
            .await
            .unwrap();
        repo.claim_next().await.unwrap().unwrap();
        let ids = repo
            .enqueue(req("anulowany", Priority::High))
            .await
            .unwrap();
        repo.cancel(ids[0]).await.unwrap();
        let mut stale = req("przeterminowany", Priority::Emergency);
        stale.not_before = Some(Utc::now() - chrono::Duration::minutes(10));
        stale.expires_at = Some(Utc::now() - chrono::Duration::minutes(5));
        repo.enqueue(stale).await.unwrap();
        assert!(!repo.urgent_due(horizon).await.unwrap());

        repo.enqueue(req("pilny teraz", Priority::Emergency))
            .await
            .unwrap();
        assert!(repo.urgent_due(horizon).await.unwrap());
    }

    #[tokio::test]
    async fn items_are_picked_by_priority_then_by_time() {
        let repo = QueueRepo::new(test_pool().await);
        repo.enqueue(req("niski", Priority::Low)).await.unwrap();
        repo.enqueue(req("pilny", Priority::Emergency)).await.unwrap();
        repo.enqueue(req("zwykly", Priority::Normal)).await.unwrap();

        let first = repo.claim_next().await.unwrap().expect("coś musi czekać");
        assert_eq!(first.rendered_text, "pilny");
        repo.mark_done(first.id).await.unwrap();

        let second = repo.claim_next().await.unwrap().unwrap();
        assert_eq!(second.rendered_text, "zwykly");
        repo.mark_done(second.id).await.unwrap();

        let third = repo.claim_next().await.unwrap().unwrap();
        assert_eq!(third.rendered_text, "niski");
    }

    #[tokio::test]
    async fn claiming_marks_the_item_transmitting_so_nobody_takes_it_twice() {
        let repo = QueueRepo::new(test_pool().await);
        repo.enqueue(req("jeden", Priority::Normal)).await.unwrap();

        let claimed = repo.claim_next().await.unwrap().expect("pierwszy claim");
        assert_eq!(claimed.state, QueueState::Transmitting);
        assert_eq!(claimed.attempt, 1, "claim liczy próbę");

        assert!(
            repo.claim_next().await.unwrap().is_none(),
            "drugi claim nie może wziąć tego samego"
        );
    }

    #[tokio::test]
    async fn an_item_scheduled_for_later_is_not_claimed_yet() {
        let repo = QueueRepo::new(test_pool().await);
        let mut later = req("za godzinę", Priority::Emergency);
        later.not_before = Some(Utc::now() + chrono::Duration::hours(1));
        repo.enqueue(later).await.unwrap();

        assert!(repo.claim_next().await.unwrap().is_none(), "not_before musi być respektowane");
    }

    #[tokio::test]
    async fn repeats_expand_into_separate_items_sharing_a_group() {
        let repo = QueueRepo::new(test_pool().await);
        let mut repeated = req("ostrzeżenie", Priority::High);
        repeated.repeat_count = 3;
        repeated.repeat_interval = Duration::from_secs(600);

        let ids = repo.enqueue(repeated).await.unwrap();
        assert_eq!(ids.len(), 3, "powtórzenia rozwijamy z góry");

        let pending = repo.pending(10).await.unwrap();
        assert_eq!(pending.len(), 3);
        let group = pending[0].repeat_group.clone().expect("grupa");
        assert!(pending.iter().all(|i| i.repeat_group.as_deref() == Some(group.as_str())));
        assert_eq!(pending.iter().map(|i| i.repeat_index).collect::<Vec<_>>(), vec![0, 1, 2]);

        let claimed = repo.claim_next().await.unwrap().unwrap();
        assert_eq!(claimed.repeat_index, 0);
        assert!(repo.claim_next().await.unwrap().is_none(), "kolejne czekają na swój czas");
    }

    #[tokio::test]
    async fn a_huge_repeat_interval_is_refused_instead_of_panicking() {
        let repo = QueueRepo::new(test_pool().await);
        // Duration * u32 przepełnia się przy trzecim powtórzeniu.
        let mut huge = req("ogromny odstęp", Priority::Normal);
        huge.repeat_count = 3;
        huge.repeat_interval = Duration::from_secs(u64::MAX);
        assert!(repo.enqueue(huge).await.is_err(), "przepełnienie to błąd, nie panika");

        // Mieści się w TimeDelta, ale nie w dacie.
        let mut far = req("daleko", Priority::Normal);
        far.repeat_count = 2;
        far.repeat_interval = Duration::from_secs(1_000_000_000_000_000);
        assert!(repo.enqueue(far).await.is_err(), "data poza zakresem to błąd");

        // Za duże dla TimeDelta — wcześniej po cichu zero.
        let mut silent = req("po cichu", Priority::Normal);
        silent.repeat_count = 2;
        silent.repeat_interval = Duration::from_secs(u64::MAX / 4);
        assert!(repo.enqueue(silent).await.is_err());
        assert!(repo.pending(10).await.unwrap().is_empty(), "nic nie trafia do kolejki");
    }

    #[tokio::test]
    async fn only_the_first_occurrence_of_a_series_interrupts() {
        let repo = QueueRepo::new(test_pool().await);
        let mut series = req("alert", Priority::Emergency);
        series.interrupting = true;
        series.repeat_count = 3;
        series.repeat_interval = Duration::from_secs(60);
        repo.enqueue(series).await.unwrap();
        let mut pending = repo.pending(10).await.unwrap();
        pending.sort_by_key(|i| i.repeat_index);
        assert_eq!(pending.iter().map(|i| i.interrupting).collect::<Vec<_>>(), vec![true, false, false]);
    }

    #[tokio::test]
    async fn cancelling_a_group_drops_every_remaining_occurrence() {
        let repo = QueueRepo::new(test_pool().await);
        let mut repeated = req("ostrzeżenie", Priority::High);
        repeated.repeat_count = 3;
        repeated.repeat_interval = Duration::from_secs(60);
        repo.enqueue(repeated).await.unwrap();

        let group = repo.pending(10).await.unwrap()[0].repeat_group.clone().unwrap();
        let cancelled = repo.cancel_group(&group).await.unwrap();

        assert_eq!(cancelled, 3);
        assert!(repo.pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn peek_interrupting_finds_only_what_outranks_the_current_transmission() {
        let repo = QueueRepo::new(test_pool().await);

        let mut low = req("niski przerywający", Priority::Low);
        low.interrupting = true;
        repo.enqueue(low).await.unwrap();
        assert!(
            repo.peek_interrupting(Priority::Normal).await.unwrap().is_none(),
            "niższy priorytet nie przerywa"
        );

        let mut urgent = req("pilny przerywający", Priority::Emergency);
        urgent.interrupting = true;
        repo.enqueue(urgent).await.unwrap();
        let found = repo.peek_interrupting(Priority::Normal).await.unwrap().expect("musi znaleźć");
        assert_eq!(found.rendered_text, "pilny przerywający");
        assert_eq!(found.state, QueueState::Pending, "peek nie zabiera elementu");
    }

    #[tokio::test]
    async fn a_requeued_item_comes_back_with_its_reason_recorded() {
        let repo = QueueRepo::new(test_pool().await);
        repo.enqueue(req("odrzucony", Priority::Normal)).await.unwrap();
        let claimed = repo.claim_next().await.unwrap().unwrap();

        repo.requeue(claimed.id, "TxDisabled", Utc::now()).await.unwrap();

        let back = repo.pending(10).await.unwrap();
        assert_eq!(back.len(), 1, "odmowa nie może gubić komunikatu");
        assert_eq!(back[0].error.as_deref(), Some("TxDisabled"));
        assert_eq!(back[0].attempt, 1, "liczba prób zostaje");
    }

    #[tokio::test]
    async fn a_held_item_keeps_its_attempts_and_never_fails() {
        let repo = QueueRepo::new(test_pool().await);
        repo.enqueue(req("wstrzymany", Priority::Normal)).await.unwrap();

        for _ in 0..(MAX_ATTEMPTS + 2) {
            let claimed = repo.claim_next().await.unwrap().expect("wraca do kolejki");
            repo.hold(claimed.id, "wstrzymane: x", Utc::now()).await.unwrap();
        }

        let back = repo.pending(10).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].attempt, 0);
        assert_eq!(back[0].error.as_deref(), Some("wstrzymane: x"));
    }

    #[tokio::test]
    async fn an_item_interrupted_many_times_stays_pending_and_finally_goes_out() {
        let repo = QueueRepo::new(test_pool().await);
        let id = repo.enqueue(req("przerywany", Priority::Normal)).await.unwrap()[0];

        for _ in 0..(MAX_ATTEMPTS + 2) {
            let claimed = repo.claim_next().await.unwrap().expect("wraca do kolejki");
            repo.interrupted(claimed.id, "przerwane pilniejszym komunikatem").await.unwrap();
        }
        let back = repo.pending(10).await.unwrap();
        assert_eq!(back[0].attempt, 0);
        assert_eq!(back[0].error.as_deref(), Some("przerwane pilniejszym komunikatem"));

        repo.claim_next().await.unwrap().unwrap();
        repo.mark_done(id).await.unwrap();
        let (state, error): (String, Option<String>) =
            sqlx::query_as("SELECT state, error FROM queue_items WHERE id = ?")
                .bind(id)
                .fetch_one(repo.pool())
                .await
                .unwrap();
        assert_eq!(state, "done");
        assert_eq!(error, None, "nadany element nie niesie starego powodu");
    }

    #[tokio::test]
    async fn an_item_that_exhausted_its_attempts_fails_instead_of_looping() {
        let repo = QueueRepo::new(test_pool().await);
        repo.enqueue(req("uparty", Priority::Normal)).await.unwrap();

        for _ in 0..MAX_ATTEMPTS {
            let claimed = repo.claim_next().await.unwrap().expect("kolejna próba");
            repo.requeue(claimed.id, "awaria", Utc::now()).await.unwrap();
        }

        assert!(
            repo.claim_next().await.unwrap().is_none(),
            "po wyczerpaniu prób element nie może krążyć w nieskończoność"
        );
        let failed: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM queue_items WHERE state = 'failed'")
                .fetch_one(repo.pool())
                .await
                .unwrap();
        assert_eq!(failed, 1);
    }

    #[tokio::test]
    async fn expired_items_are_not_transmitted() {
        let repo = QueueRepo::new(test_pool().await);
        let mut stale = req("nieaktualne", Priority::Emergency);
        stale.expires_at = Some(Utc::now() - chrono::Duration::minutes(5));
        repo.enqueue(stale).await.unwrap();

        let expired = repo.expire_stale().await.unwrap();
        assert_eq!(expired, 1);
        assert!(repo.claim_next().await.unwrap().is_none());
    }

    async fn state(repo: &QueueRepo, id: i64) -> QueueState {
        repo.state_of(id).await.unwrap().expect("element istnieje")
    }

    #[tokio::test]
    async fn a_claimed_item_cancelled_and_then_held_ends_cancelled_and_never_airs() {
        let repo = QueueRepo::new(test_pool().await);
        let id = repo.enqueue(req("wycofany", Priority::Normal)).await.unwrap()[0];
        repo.claim_next().await.unwrap().unwrap();

        assert_eq!(repo.cancel(id).await.unwrap(), 1, "prośba o anulowanie zapisana");
        assert_eq!(state(&repo, id).await, QueueState::Transmitting, "trwające nadanie nie jest cięte");

        repo.hold(id, "wstrzymane: wyłącznik", Utc::now()).await.unwrap();
        assert_eq!(state(&repo, id).await, QueueState::Cancelled);
        assert!(repo.claim_next().await.unwrap().is_none(), "wycofany element nie wraca na antenę");
        assert!(repo.pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_claimed_item_cancelled_and_then_interrupted_or_restarted_ends_cancelled() {
        let pool = test_pool().await;
        let repo = QueueRepo::new(pool.clone());
        let a = repo.enqueue(req("przerwany", Priority::Normal)).await.unwrap()[0];
        repo.claim_next().await.unwrap().unwrap();
        repo.cancel(a).await.unwrap();
        repo.interrupted(a, "przerwane pilniejszym komunikatem").await.unwrap();
        assert_eq!(state(&repo, a).await, QueueState::Cancelled);

        let b = repo.enqueue(req("restart", Priority::Normal)).await.unwrap()[0];
        repo.claim_next().await.unwrap().unwrap();
        repo.cancel(b).await.unwrap();
        QueueRepo::new(pool).recover_after_restart().await.unwrap();
        assert_eq!(state(&repo, b).await, QueueState::Cancelled);
        assert!(repo.claim_next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_claimed_item_cancelled_while_really_on_air_finishes_as_done() {
        let repo = QueueRepo::new(test_pool().await);
        let id = repo.enqueue(req("na antenie", Priority::Normal)).await.unwrap()[0];
        repo.claim_next().await.unwrap().unwrap();
        repo.cancel(id).await.unwrap();
        repo.mark_done(id).await.unwrap();
        assert_eq!(state(&repo, id).await, QueueState::Done, "nadany do końca zostaje nadanym");
    }

    #[tokio::test]
    async fn cancelling_a_finished_item_changes_nothing() {
        let repo = QueueRepo::new(test_pool().await);
        let id = repo.enqueue(req("nadany", Priority::Normal)).await.unwrap()[0];
        repo.claim_next().await.unwrap().unwrap();
        repo.mark_done(id).await.unwrap();
        assert_eq!(repo.cancel(id).await.unwrap(), 0);
        assert_eq!(state(&repo, id).await, QueueState::Done);
    }

    #[tokio::test]
    async fn a_pending_item_with_a_cancel_request_is_never_claimed() {
        let repo = QueueRepo::new(test_pool().await);
        let mut urgent = req("oznaczony", Priority::Emergency);
        urgent.interrupting = true;
        let id = repo.enqueue(urgent).await.unwrap()[0];
        sqlx::query("UPDATE queue_items SET cancel_requested = 1 WHERE id = ?")
            .bind(id)
            .execute(repo.pool())
            .await
            .unwrap();
        assert!(repo.peek_interrupting(Priority::Normal).await.unwrap().is_none());
        assert!(repo.claim_next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelling_a_group_also_withdraws_its_claimed_occurrence() {
        let repo = QueueRepo::new(test_pool().await);
        let mut repeated = req("seria", Priority::High);
        repeated.repeat_count = 2;
        repeated.repeat_interval = Duration::from_secs(60);
        let ids = repo.enqueue(repeated).await.unwrap();
        repo.claim_next().await.unwrap().unwrap();
        let group = repo.pending(10).await.unwrap()[0].repeat_group.clone().unwrap();

        assert_eq!(repo.cancel_group(&group).await.unwrap(), 2);
        repo.hold(ids[0], "wstrzymane: x", Utc::now()).await.unwrap();
        assert_eq!(state(&repo, ids[0]).await, QueueState::Cancelled);
        assert_eq!(state(&repo, ids[1]).await, QueueState::Cancelled);
    }

    #[tokio::test]
    async fn an_item_interrupted_by_power_loss_returns_to_the_queue_after_restart() {
        let pool = test_pool().await;
        let repo = QueueRepo::new(pool.clone());
        repo.enqueue(req("w trakcie", Priority::High)).await.unwrap();
        let claimed = repo.claim_next().await.unwrap().unwrap();
        assert_eq!(claimed.state, QueueState::Transmitting);

        // Zanik zasilania: proces ginie, wiersz zostaje w stanie transmitting.
        let recovered = QueueRepo::new(pool).recover_after_restart().await.unwrap();

        assert_eq!(recovered, 1);
        let pending = repo.pending(10).await.unwrap();
        assert_eq!(pending.len(), 1, "nadanie przerwane zanikiem zasilania wraca do kolejki");
        assert_eq!(pending[0].rendered_text, "w trakcie");
    }
}
