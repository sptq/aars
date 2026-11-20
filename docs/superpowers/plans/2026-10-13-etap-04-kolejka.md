# Etap 4: Trwała kolejka — priorytety, przerywanie, powtórzenia

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Kolejka, która ocala restart: zadania nadania żyją w SQLite, są wybierane po priorytecie i czasie, przerywane przez komunikat pilniejszy, rozwijane w powtórzenia i możliwe do anulowania grupą.

**Architecture:** `QueueRepo` to jedyne miejsce, które zna tabelę `queue_items`; operuje typami domenowymi, nie wierszami. `Dispatcher` spina kolejkę z sekwencerem przez trait `Renderer`, więc `aars-core` nadal nie wie nic o Piperze ani o karcie dźwiękowej. Nadanie przerwane wraca do kolejki, a nie przepada — w stacji ostrzegawczej zgubiony komunikat jest gorszy niż nadany dwa razy.

**Tech Stack:** sqlx 0.8 (SQLite, `UPDATE ... RETURNING`), tokio (zadanie dyspozytora), chrono.

**Spec:** `docs/superpowers/specs/2026-10-13-aars-design.md`

## Global Constraints

- SQLite jest źródłem prawdy dla kolejki (spec D3). Po restarcie kolejka i harmonogram muszą ocaleć.
- Porządek wybierania: `(priority, not_before, created_at)`; niższa liczba priorytetu znaczy wyższy priorytet (spec §5.1).
- Element z `interrupting = true` i priorytetem ściśle wyższym niż trwające nadanie przerywa je; przerwany wraca do kolejki (spec §5.1).
- Powtórzenia rozwijane z góry w osobne zadania ze wspólnym `repeat_group`, żeby kolejka w panelu pokazywała prawdę o tym, co się stanie (spec §5.2).
- Odmowa interlocka nie gubi komunikatu — wraca do kolejki z powodem (spec §4.4).
- `aars-core` nie wie o HTTP ani o syntezie (spec D6, D8).
- Żaden test nie dotyka karty dźwiękowej, GPIO ani sieci (spec §11).
- TDD: test przed implementacją, commit po każdym zielonym cyklu.

---

### Task 1: `QueueRepo`

**Files:**
- Create: `crates/aars-core/src/queue.rs`
- Modify: `crates/aars-core/src/lib.rs`

**Interfaces:**
- Consumes: `aars_core::priority::Priority`, `aars_core::error::DbError`, pula `sqlx::SqlitePool`
- Produces: `aars_core::queue::{QueueRepo, QueueItem, QueueState, EnqueueRequest, MAX_ATTEMPTS}`; `QueueRepo::new(pool)`; metody `enqueue`, `claim_next`, `peek_interrupting`, `mark_done`, `mark_failed`, `requeue`, `cancel`, `cancel_group`, `expire_stale`, `recover_after_restart`, `pending`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-core/src/queue.rs`:

```rust
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

        assert!(repo.claim_next().await.unwrap().is_none(), "drugi claim nie może wziąć tego samego");
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

        // Tylko pierwsze wystąpienie jest gotowe teraz.
        let claimed = repo.claim_next().await.unwrap().unwrap();
        assert_eq!(claimed.repeat_index, 0);
        assert!(repo.claim_next().await.unwrap().is_none(), "kolejne czekają na swój czas");
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
        let failed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM queue_items WHERE state = 'failed'",
        )
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
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-core queue`
Expected: FAIL — `cannot find type QueueRepo`

- [ ] **Step 3: Zaimplementuj `QueueRepo`**

`crates/aars-core/src/queue.rs` — nad blokiem testów:

```rust
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

    pub fn from_str(value: &str) -> Option<Self> {
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

/// Jedyne miejsce, które zna tabelę `queue_items`.
pub struct QueueRepo {
    pool: SqlitePool,
}

/// Licznik do identyfikatorów grup powtórzeń. Znacznik czasu sam nie
/// wystarcza: dwa zlecenia w tej samej milisekundzie dostałyby jedną grupę.
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
        state: QueueState::from_str(&state_raw).unwrap_or(QueueState::Pending),
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

        let mut ids = Vec::with_capacity(count as usize);
        for index in 0..count {
            let offset = request.repeat_interval * index;
            let not_before = base
                + chrono::Duration::from_std(offset).unwrap_or(chrono::Duration::zero());

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
            .bind(i64::from(request.interrupting))
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

    /// Szuka zadania przerywającego, które bije trwające nadanie. Nie zabiera
    /// go — decyzję o przerwaniu podejmuje dyspozytor.
    pub async fn peek_interrupting(
        &self,
        current: Priority,
    ) -> Result<Option<QueueItem>, DbError> {
        let now = iso(Utc::now());
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM queue_items
              WHERE state = 'pending'
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

    async fn finish(
        &self,
        id: i64,
        state: QueueState,
        error: Option<&str>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE queue_items
                SET state = ?, finished_at = ?, error = COALESCE(?, error)
              WHERE id = ?",
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
                SET state = 'pending', not_before = ?, started_at = NULL, error = ?
              WHERE id = ?",
        )
        .bind(iso(not_before))
        .bind(reason)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn cancel(&self, id: i64) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE queue_items SET state = 'cancelled', finished_at = ?
              WHERE id = ? AND state = 'pending'",
        )
        .bind(iso(Utc::now()))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Anuluje pozostałe wystąpienia powtórzenia.
    pub async fn cancel_group(&self, group: &str) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE queue_items SET state = 'cancelled', finished_at = ?
              WHERE repeat_group = ? AND state = 'pending'",
        )
        .bind(iso(Utc::now()))
        .bind(group)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
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
                SET state = 'pending', started_at = NULL,
                    error = 'przerwane restartem procesu'
              WHERE state = 'transmitting'",
        )
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
```

`crates/aars-core/src/lib.rs` — dopisz `pub mod queue;`

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core queue`
Expected: PASS — dziesięć testów kolejki

- [ ] **Step 5: Commit**

```bash
git add crates/aars-core
git commit -m "feat(core): trwała kolejka nadań z priorytetami i powtórzeniami"
```

---

### Task 2: `Dispatcher`

**Files:**
- Create: `crates/aars-core/src/dispatcher.rs`
- Modify: `crates/aars-core/src/lib.rs`

**Interfaces:**
- Consumes: `aars_core::queue::{QueueRepo, QueueItem}`, `aars_core::sequencer::{SequencerHandle, SeqEvent, TransmitRequest, TxOutcome}`, `aars_core::synth::Pcm`
- Produces: `aars_core::dispatcher::{Dispatcher, DispatcherConfig, Renderer, RenderFuture, FakeRenderer}`; `Dispatcher::run(self, shutdown: CancelToken)`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-core/src/dispatcher.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;
    use crate::interlock::InMemoryInterlocks;
    use crate::ptt::{FakePtt, PttEvent};
    use crate::queue::EnqueueRequest;
    use crate::sequencer::{Sequencer, SequencerConfig};
    use crate::sink::FakeSink;

    fn req(text: &str, priority: Priority, interrupting: bool) -> EnqueueRequest {
        EnqueueRequest {
            message_id: None,
            rendered_text: text.to_owned(),
            priority,
            interrupting,
            not_before: None,
            expires_at: None,
            repeat_count: 1,
            repeat_interval: Duration::ZERO,
            created_by: None,
        }
    }

    struct Harness {
        repo: Arc<QueueRepo>,
        ptt: Arc<FakePtt>,
        shutdown: CancelToken,
        task: tokio::task::JoinHandle<()>,
    }

    /// Dyspozytor z fake'ami: baza w pamięci, PTT i wyjście audio udawane,
    /// renderer dający audio o długości proporcjonalnej do tekstu.
    async fn harness(ms_per_char: u64) -> Harness {
        let repo = Arc::new(QueueRepo::new(test_pool().await));
        let ptt = Arc::new(FakePtt::new());
        let sequencer = Sequencer::spawn(
            ptt.clone(),
            Arc::new(FakeSink::new()),
            Arc::new(InMemoryInterlocks::permissive()),
            SequencerConfig {
                tx_tail: Duration::from_millis(1),
                tx_max_duration: Duration::from_secs(30),
                id_interval: Duration::from_secs(600),
            },
        );
        let shutdown = CancelToken::new();
        let dispatcher = Dispatcher::new(
            repo.clone(),
            Arc::new(FakeRenderer::new(48_000, ms_per_char)),
            sequencer,
            DispatcherConfig { tick: Duration::from_millis(10) },
        );
        let task = tokio::spawn(dispatcher.run(shutdown.clone()));

        Harness { repo, ptt, shutdown, task }
    }

    impl Harness {
        async fn finish(self) {
            self.shutdown.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
        }

        async fn wait_for_state(&self, id: i64, state: &str, within: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + within;
            while tokio::time::Instant::now() < deadline {
                let actual: String =
                    sqlx::query_scalar("SELECT state FROM queue_items WHERE id = ?")
                        .bind(id)
                        .fetch_one(self.repo.pool())
                        .await
                        .unwrap();
                if actual == state {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            false
        }
    }

    #[tokio::test]
    async fn a_queued_item_gets_transmitted_and_marked_done() {
        let h = harness(2).await;
        let ids = h.repo.enqueue(req("komunikat", Priority::Normal, false)).await.unwrap();

        assert!(
            h.wait_for_state(ids[0], "done", Duration::from_secs(5)).await,
            "zadanie z kolejki musi zostać nadane"
        );
        assert!(h.ptt.events().contains(&PttEvent::Key));
        assert!(!h.ptt.is_keyed(), "po nadaniu PTT opuszczone");

        h.finish().await;
    }

    #[tokio::test]
    async fn an_emergency_item_preempts_a_normal_one_and_the_interrupted_returns_to_the_queue() {
        // 40 ms na znak, żeby pierwsze nadanie trwało na tyle długo, by dało
        // się je przerwać.
        let h = harness(40).await;
        let slow = h
            .repo
            .enqueue(req("dlugi komunikat zwyczajny", Priority::Normal, false))
            .await
            .unwrap()[0];

        assert!(
            h.wait_for_state(slow, "transmitting", Duration::from_secs(5)).await,
            "pierwsze zadanie musi wejść na pasmo"
        );

        let urgent = h.repo.enqueue(req("alarm", Priority::Emergency, true)).await.unwrap()[0];

        assert!(
            h.wait_for_state(urgent, "done", Duration::from_secs(10)).await,
            "komunikat pilny musi zostać nadany"
        );

        // Przerwany wraca do kolejki albo zostaje dokończony później — ale
        // nie przepada.
        let state: String = sqlx::query_scalar("SELECT state FROM queue_items WHERE id = ?")
            .bind(slow)
            .fetch_one(h.repo.pool())
            .await
            .unwrap();
        assert!(
            matches!(state.as_str(), "pending" | "transmitting" | "done"),
            "przerwany komunikat nie może przepaść, jest w stanie {state}"
        );

        // Nigdy dwa nadania naraz: po każdym Key jest Unkey przed następnym Key.
        let events = h.ptt.events();
        let mut keyed = false;
        for event in &events {
            match event {
                PttEvent::Key => {
                    assert!(!keyed, "drugie Key bez Unkey: {events:?}");
                    keyed = true;
                }
                PttEvent::Unkey | PttEvent::ForceUnkey => keyed = false,
            }
        }

        h.finish().await;
    }

    #[tokio::test]
    async fn a_render_failure_fails_the_item_without_touching_the_transmitter() {
        let h = harness(2).await;
        let ids = h.repo.enqueue(req("FAIL", Priority::Normal, false)).await.unwrap();

        assert!(
            h.wait_for_state(ids[0], "failed", Duration::from_secs(5)).await,
            "błąd syntezy musi oznaczyć zadanie jako nieudane"
        );
        assert!(h.ptt.events().is_empty(), "nieudana synteza nie rusza PTT");

        h.finish().await;
    }

    #[tokio::test]
    async fn the_dispatcher_recovers_items_left_transmitting_by_a_crash() {
        let pool = test_pool().await;
        let repo = QueueRepo::new(pool.clone());
        let ids = repo.enqueue(req("po restarcie", Priority::Normal, false)).await.unwrap();
        repo.claim_next().await.unwrap();

        // Nowy dyspozytor na tej samej bazie, jak po restarcie procesu.
        let repo = Arc::new(QueueRepo::new(pool));
        let ptt = Arc::new(FakePtt::new());
        let sequencer = Sequencer::spawn(
            ptt,
            Arc::new(FakeSink::new()),
            Arc::new(InMemoryInterlocks::permissive()),
            SequencerConfig {
                tx_tail: Duration::from_millis(1),
                tx_max_duration: Duration::from_secs(30),
                id_interval: Duration::from_secs(600),
            },
        );
        let shutdown = CancelToken::new();
        let dispatcher = Dispatcher::new(
            repo.clone(),
            Arc::new(FakeRenderer::new(48_000, 2)),
            sequencer,
            DispatcherConfig { tick: Duration::from_millis(10) },
        );
        let task = tokio::spawn(dispatcher.run(shutdown.clone()));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut done = false;
        while tokio::time::Instant::now() < deadline {
            let state: String = sqlx::query_scalar("SELECT state FROM queue_items WHERE id = ?")
                .bind(ids[0])
                .fetch_one(repo.pool())
                .await
                .unwrap();
            if state == "done" {
                done = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(done, "zadanie osierocone restartem musi zostać nadane po starcie dyspozytora");

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-core dispatcher`
Expected: FAIL — `cannot find type Dispatcher`

- [ ] **Step 3: Zaimplementuj `Dispatcher`**

`crates/aars-core/src/dispatcher.rs` — nad blokiem testów:

```rust
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::priority::Priority;
use crate::queue::{QueueItem, QueueRepo};
use crate::sequencer::{SeqEvent, SequencerHandle, TransmitRequest, TxOutcome};
use crate::sink::CancelToken;
use crate::synth::Pcm;

pub type RenderFuture<'a> = Pin<Box<dyn Future<Output = Result<Pcm, String>> + Send + 'a>>;

/// Zamiana tekstu zadania w audio. Trait, bo `aars-core` nie może wiedzieć
/// o Piperze ani o cache (spec D6, D8).
pub trait Renderer: Send + Sync {
    fn render<'a>(&'a self, text: &'a str) -> RenderFuture<'a>;

    /// Audio znaku wywoławczego; `None` wyłącza identyfikację.
    fn ident(&self) -> Option<Pcm>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DispatcherConfig {
    /// Jak często zaglądać do kolejki, gdy nic nie leci.
    pub tick: Duration,
}

/// Spina kolejkę z sekwencerem: bierze zadanie, syntezuje, nadaje, zapisuje
/// wynik. Jedyne miejsce, które decyduje o przerwaniu trwającego nadania.
pub struct Dispatcher {
    queue: Arc<QueueRepo>,
    renderer: Arc<dyn Renderer>,
    sequencer: SequencerHandle,
    config: DispatcherConfig,
}

impl Dispatcher {
    pub fn new(
        queue: Arc<QueueRepo>,
        renderer: Arc<dyn Renderer>,
        sequencer: SequencerHandle,
        config: DispatcherConfig,
    ) -> Self {
        Self { queue, renderer, sequencer, config }
    }

    pub async fn run(mut self, shutdown: CancelToken) {
        // Zadania osierocone restartem wracają do kolejki, zanim weźmiemy
        // cokolwiek nowego.
        if let Err(err) = self.queue.recover_after_restart().await {
            tracing::error!(%err, "odzyskiwanie kolejki po restarcie zawiodło");
        }

        while !shutdown.is_cancelled() {
            if let Err(err) = self.queue.expire_stale().await {
                tracing::error!(%err, "oznaczanie przedawnionych zadań zawiodło");
            }

            let claimed = match self.queue.claim_next().await {
                Ok(item) => item,
                Err(err) => {
                    tracing::error!(%err, "pobranie zadania z kolejki zawiodło");
                    tokio::time::sleep(self.config.tick).await;
                    continue;
                }
            };

            match claimed {
                None => tokio::time::sleep(self.config.tick).await,
                Some(item) => self.handle(item, &shutdown).await,
            }
        }

        tracing::info!("dyspozytor zatrzymany");
    }

    async fn handle(&mut self, item: QueueItem, shutdown: &CancelToken) {
        let id = item.id;

        let body = match self.renderer.render(&item.rendered_text).await {
            Ok(pcm) => pcm,
            Err(err) => {
                tracing::error!(id, %err, "synteza zadania zawiodła");
                if let Err(err) = self.queue.mark_failed(id, &err).await {
                    tracing::error!(id, %err, "zapis błędu syntezy zawiódł");
                }
                return;
            }
        };

        let request = TransmitRequest {
            id,
            priority: item.priority,
            interrupting: item.interrupting,
            body,
            id_audio: self.renderer.ident(),
        };

        if let Err(err) = self.sequencer.transmit(request).await {
            tracing::error!(id, %err, "sekwencer nie przyjął zadania");
            let _ = self.queue.mark_failed(id, &err).await;
            return;
        }

        self.await_outcome(id, item.priority, shutdown).await;
    }

    /// Czeka na koniec nadania, w tym czasie pilnując, czy nie pojawiło się
    /// zadanie przerywające o wyższym priorytecie.
    async fn await_outcome(&mut self, id: i64, priority: Priority, shutdown: &CancelToken) {
        loop {
            tokio::select! {
                event = self.sequencer.next_event() => {
                    match event {
                        None => return,
                        Some(SeqEvent::Started { .. }) => continue,
                        Some(SeqEvent::Refused { id: refused, reason }) => {
                            tracing::warn!(id = refused, ?reason, "nadanie odrzucone, wracam do kolejki");
                            // Odmowa nie gubi komunikatu: wraca z powodem i
                            // odstępem, żeby nie kręcić się w kółko.
                            let retry_at = Utc::now() + chrono::Duration::seconds(30);
                            if let Err(err) =
                                self.queue.requeue(refused, &format!("{reason:?}"), retry_at).await
                            {
                                tracing::error!(id = refused, %err, "zwrot zadania do kolejki zawiódł");
                            }
                            if refused == id {
                                return;
                            }
                        }
                        Some(SeqEvent::Finished { id: finished, outcome, played }) => {
                            self.record_outcome(finished, outcome, played).await;
                            if finished == id {
                                return;
                            }
                        }
                    }
                }
                _ = tokio::time::sleep(self.config.tick) => {
                    if shutdown.is_cancelled() {
                        return;
                    }
                    self.maybe_preempt(priority).await;
                }
            }
        }
    }

    async fn record_outcome(&self, id: i64, outcome: TxOutcome, played: Duration) {
        let result = match outcome {
            TxOutcome::Ok => {
                tracing::info!(id, played_ms = played.as_millis(), "nadanie zakończone");
                self.queue.mark_done(id).await
            }
            TxOutcome::Aborted => {
                tracing::warn!(id, "nadanie przerwane, zadanie wraca do kolejki");
                self.queue.requeue(id, "przerwane pilniejszym komunikatem", Utc::now()).await
            }
            TxOutcome::Failed(err) => {
                tracing::error!(id, %err, "nadanie nieudane");
                self.queue.mark_failed(id, &err).await
            }
        };

        if let Err(err) = result {
            tracing::error!(id, %err, "zapis wyniku nadania zawiódł");
        }
    }

    /// Jeśli w kolejce czeka zadanie przerywające o wyższym priorytecie,
    /// bierze je i zleca — sekwencer przerwie trwające nadanie.
    async fn maybe_preempt(&mut self, current: Priority) {
        let candidate = match self.queue.peek_interrupting(current).await {
            Ok(Some(item)) => item,
            Ok(None) => return,
            Err(err) => {
                tracing::error!(%err, "sprawdzenie zadań przerywających zawiodło");
                return;
            }
        };

        // Bierzemy je przez claim_next: kolejność gwarantuje, że to właśnie
        // ten element, a claim chroni przed podwójnym wzięciem.
        let claimed = match self.queue.claim_next().await {
            Ok(Some(item)) if item.id == candidate.id => item,
            Ok(Some(other)) => {
                // Pojawiło się coś jeszcze pilniejszego — też nadaje się do
                // przerwania, więc działamy na nim.
                other
            }
            Ok(None) => return,
            Err(err) => {
                tracing::error!(%err, "pobranie zadania przerywającego zawiodło");
                return;
            }
        };

        let body = match self.renderer.render(&claimed.rendered_text).await {
            Ok(pcm) => pcm,
            Err(err) => {
                tracing::error!(id = claimed.id, %err, "synteza komunikatu przerywającego zawiodła");
                let _ = self.queue.mark_failed(claimed.id, &err).await;
                return;
            }
        };

        let request = TransmitRequest {
            id: claimed.id,
            priority: claimed.priority,
            interrupting: true,
            body,
            id_audio: self.renderer.ident(),
        };

        if let Err(err) = self.sequencer.transmit(request).await {
            tracing::error!(id = claimed.id, %err, "sekwencer nie przyjął komunikatu przerywającego");
            let _ = self.queue.mark_failed(claimed.id, &err).await;
        }
    }
}

/// Renderer testowy: audio proporcjonalne do długości tekstu, a tekst
/// „FAIL" wywołuje błąd syntezy.
pub struct FakeRenderer {
    sample_rate: u32,
    ms_per_char: u64,
}

impl FakeRenderer {
    pub fn new(sample_rate: u32, ms_per_char: u64) -> Self {
        Self { sample_rate, ms_per_char }
    }
}

impl Renderer for FakeRenderer {
    fn render<'a>(&'a self, text: &'a str) -> RenderFuture<'a> {
        Box::pin(async move {
            if text.contains("FAIL") {
                return Err("wymuszony błąd syntezy".to_owned());
            }
            let ms = self.ms_per_char * text.chars().count() as u64;
            Ok(Pcm::silence(self.sample_rate, Duration::from_millis(ms)))
        })
    }

    fn ident(&self) -> Option<Pcm> {
        None
    }
}
```

`crates/aars-core/src/lib.rs` — dopisz `pub mod dispatcher;`

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core dispatcher`
Expected: PASS — cztery testy dyspozytora

- [ ] **Step 5: Commit**

```bash
git add crates/aars-core
git commit -m "feat(core): dyspozytor spinający kolejkę z sekwencerem"
```

---

### Task 3: CLI kolejki i `aars run`

**Files:**
- Create: `crates/aars/src/render.rs`
- Modify: `crates/aars/src/cli.rs`
- Modify: `crates/aars/src/main.rs`
- Modify: `crates/aars/tests/cli.rs`

**Interfaces:**
- Consumes: `aars_core::dispatcher::{Dispatcher, DispatcherConfig, Renderer}`, `aars_core::queue::{QueueRepo, EnqueueRequest}`, `aars_audio::{cache::AudioCache, piper::PiperSynth}`
- Produces: `aars::render::PiperRenderer`; podkomendy `aars queue add|list|cancel` i `aars run`

- [ ] **Step 1: Napisz test, który ma nie przejść**

Dopisz do `crates/aars/tests/cli.rs`:

```rust
#[test]
fn queue_add_then_list_shows_the_item_in_priority_order() {
    let dir = std::env::temp_dir().join(format!("aars-queue-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("queue.db");

    let migrate = aars().env("AARS_STORAGE__DB_PATH", &db).arg("migrate").output().unwrap();
    assert!(migrate.status.success(), "{}", String::from_utf8_lossy(&migrate.stderr));

    for (text, priority) in [("zwykly", "normal"), ("alarm", "emergency")] {
        let out = aars()
            .env("AARS_STORAGE__DB_PATH", &db)
            .args(["queue", "add", "--priority", priority, text])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args(["queue", "list"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let stdout = String::from_utf8_lossy(&out.stdout);
    let alarm = stdout.find("alarm").expect("alarm w wyniku");
    let zwykly = stdout.find("zwykly").expect("zwykly w wyniku");
    assert!(alarm < zwykly, "pilniejsze musi być wyżej:\n{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}
```

- [ ] **Step 2: Uruchom test i potwierdź, że nie przechodzi**

Run: `cargo test -p aars queue_add_then_list`
Expected: FAIL — `unrecognized subcommand 'queue'`

- [ ] **Step 3: Zaimplementuj renderer produkcyjny i podkomendy**

`crates/aars/src/render.rs`:

```rust
use std::sync::Arc;

use aars_core::dispatcher::{RenderFuture, Renderer};
use aars_core::ident::{IdMode, MorseParams, morse_pcm};
use aars_core::synth::{Pcm, Synthesizer, VoiceParams};
use aars_audio::cache::AudioCache;

/// Renderer produkcyjny: synteza przez cache, znak wywoławczy złożony raz
/// przy starcie, bo nie zmienia się między nadaniami.
pub struct PiperRenderer {
    cache: AudioCache,
    synth: Arc<dyn Synthesizer>,
    ident: Option<Pcm>,
}

impl PiperRenderer {
    pub fn new(
        cache: AudioCache,
        synth: Arc<dyn Synthesizer>,
        callsign: &str,
        id_mode: IdMode,
        sample_rate: u32,
    ) -> Self {
        let ident = if callsign.is_empty() {
            tracing::warn!("znak wywoławczy nieustawiony — nadania bez identyfikacji");
            None
        } else {
            match id_mode {
                IdMode::Cw => Some(morse_pcm(callsign, &MorseParams::default(), sample_rate)),
                IdMode::Voice => match synth.synthesize(callsign, &VoiceParams::default()) {
                    Ok(pcm) => Some(pcm),
                    Err(err) => {
                        tracing::error!(%err, "synteza znaku wywoławczego zawiodła, przechodzę na CW");
                        Some(morse_pcm(callsign, &MorseParams::default(), sample_rate))
                    }
                },
            }
        };

        Self { cache, synth, ident }
    }
}

impl Renderer for PiperRenderer {
    fn render<'a>(&'a self, text: &'a str) -> RenderFuture<'a> {
        Box::pin(async move {
            self.cache
                .render(self.synth.as_ref(), text, &VoiceParams::default())
                .await
                .map(|(_, pcm)| pcm)
                .map_err(|e| e.to_string())
        })
    }

    fn ident(&self) -> Option<Pcm> {
        self.ident.clone()
    }
}
```

`crates/aars/src/cli.rs` — dopisz do `enum Command` oraz nowy podenum:

```rust
    /// Operacje na kolejce nadań.
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
    /// Uruchamia stację: dyspozytor przetwarza kolejkę aż do przerwania.
    Run,
```

```rust
#[derive(Debug, Subcommand)]
pub enum QueueAction {
    /// Dodaje komunikat do kolejki.
    Add {
        text: String,
        #[arg(long, default_value = "normal")]
        priority: String,
        /// Przerywa trwające nadanie o niższym priorytecie.
        #[arg(long)]
        interrupting: bool,
        /// Ile razy nadać.
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        /// Odstęp między powtórzeniami w sekundach.
        #[arg(long, default_value_t = 600)]
        every: u64,
    },
    /// Wypisuje zadania czekające w kolejce.
    List,
    /// Anuluje zadanie albo całą grupę powtórzeń.
    Cancel {
        #[arg(long, group = "target")]
        id: Option<i64>,
        #[arg(long, group = "target")]
        group: Option<String>,
    },
}
```

`crates/aars/src/main.rs` — dopisz `mod render;`, pomocniczą funkcję parsowania priorytetu i gałęzie:

```rust
fn parse_priority(value: &str) -> Result<aars_core::priority::Priority> {
    use aars_core::priority::Priority;
    match value {
        "emergency" => Ok(Priority::Emergency),
        "high" => Ok(Priority::High),
        "normal" => Ok(Priority::Normal),
        "low" => Ok(Priority::Low),
        other => anyhow::bail!("nieznany priorytet: {other}"),
    }
}
```

```rust
        cli::Command::Queue { action } => {
            use aars_core::queue::{EnqueueRequest, QueueRepo};

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;
            let repo = QueueRepo::new(pool);

            match action {
                cli::QueueAction::Add { text, priority, interrupting, repeat, every } => {
                    let ids = repo
                        .enqueue(EnqueueRequest {
                            message_id: None,
                            rendered_text: text,
                            priority: parse_priority(&priority)?,
                            interrupting,
                            not_before: None,
                            expires_at: None,
                            repeat_count: repeat,
                            repeat_interval: std::time::Duration::from_secs(every),
                            created_by: None,
                        })
                        .await
                        .context("dodanie do kolejki")?;
                    println!("dodano zadania: {ids:?}");
                }
                cli::QueueAction::List => {
                    let items = repo.pending(50).await.context("odczyt kolejki")?;
                    if items.is_empty() {
                        println!("kolejka pusta");
                    }
                    for item in items {
                        println!(
                            "{:>5}  {:?}  {}  próba {}  {}{}",
                            item.id,
                            item.priority,
                            item.not_before.format("%Y-%m-%d %H:%M:%S"),
                            item.attempt,
                            if item.interrupting { "[przerywa] " } else { "" },
                            item.rendered_text
                        );
                    }
                }
                cli::QueueAction::Cancel { id, group } => match (id, group) {
                    (Some(id), _) => {
                        let count = repo.cancel(id).await.context("anulowanie zadania")?;
                        println!("anulowano {count}");
                    }
                    (_, Some(group)) => {
                        let count =
                            repo.cancel_group(&group).await.context("anulowanie grupy")?;
                        println!("anulowano {count}");
                    }
                    (None, None) => anyhow::bail!("podaj --id albo --group"),
                },
            }
        }
        cli::Command::Run => {
            use aars_core::dispatcher::{Dispatcher, DispatcherConfig};
            use aars_core::ident::IdMode;
            use aars_core::queue::QueueRepo;
            use aars_core::sink::CancelToken;

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;

            let stack = radio::build_stack(&config, &pool).await?;

            let callsign: String =
                sqlx::query_scalar("SELECT value FROM settings WHERE key = 'callsign'")
                    .fetch_one(&pool)
                    .await
                    .context("odczyt znaku wywoławczego")?;
            let id_mode: String =
                sqlx::query_scalar("SELECT value FROM settings WHERE key = 'id_mode'")
                    .fetch_one(&pool)
                    .await
                    .context("odczyt trybu identyfikacji")?;
            let id_mode = if id_mode == "cw" { IdMode::Cw } else { IdMode::Voice };

            let synth: std::sync::Arc<dyn aars_core::synth::Synthesizer> =
                std::sync::Arc::new(aars_audio::piper::PiperSynth::new(
                    &config.tts.piper_bin,
                    &config.tts.model,
                    config.audio.sample_rate,
                ));
            let cache = aars_audio::cache::AudioCache::new(
                pool.clone(),
                config.storage.cache_dir.clone(),
                config.storage.cache_max_bytes,
            );
            let renderer = std::sync::Arc::new(render::PiperRenderer::new(
                cache,
                synth,
                &callsign,
                id_mode,
                config.audio.sample_rate,
            ));

            let shutdown = CancelToken::new();
            let dispatcher = Dispatcher::new(
                std::sync::Arc::new(QueueRepo::new(pool)),
                renderer,
                stack.sequencer,
                DispatcherConfig { tick: std::time::Duration::from_millis(500) },
            );

            let task = tokio::spawn(dispatcher.run(shutdown.clone()));
            tracing::info!(bind = %config.server.bind, "stacja uruchomiona, Ctrl-C kończy");

            tokio::signal::ctrl_c().await.context("oczekiwanie na sygnał")?;
            tracing::info!("zatrzymuję stację");
            shutdown.cancel();
            let _ = task.await;
            stack.watchdog.stop();
        }
```

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 5: Sprawdź ręcznie cykl kolejki**

```bash
export AARS_STORAGE__DB_PATH=/tmp/aars-q/a.db
cargo run -q -p aars -- migrate
cargo run -q -p aars -- queue add --priority high --repeat 3 --every 60 "ostrzeżenie testowe"
cargo run -q -p aars -- queue list
```
Expected: trzy zadania z narastającym `not_before`, wszystkie w jednej grupie

- [ ] **Step 6: Commit**

```bash
git add crates/aars
git commit -m "feat(bin): CLI kolejki i podkomenda run"
```

---

## Kryterium wyjścia z etapu

- `cargo test --workspace` zielone; `cargo clippy --workspace --all-targets -- -D warnings` bez zastrzeżeń.
- Komunikat `Emergency` z `interrupting` przerywa trwające nadanie, a przerwany wraca do kolejki — dowiedzione testem dyspozytora, w tym asercją, że po każdym `Key` jest `Unkey` przed następnym `Key`.
- Zadanie zostawione w stanie `transmitting` przez zanik zasilania wraca do kolejki po starcie dyspozytora.
- Powtórzenia rozwijają się w osobne zadania ze wspólną grupą, a anulowanie grupy usuwa pozostałe wystąpienia.
- Odmowa interlocka zwraca zadanie do kolejki z powodem; po `MAX_ATTEMPTS` zadanie jest oznaczane jako nieudane, nie krąży w nieskończoność.
