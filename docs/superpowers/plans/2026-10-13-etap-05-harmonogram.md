# Etap 5: Harmonogram — cron, czas jednorazowy, polityka spóźnień

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Definicje komunikatów odpalają się same: cyklicznie po wzorcu cron albo jednorazowo o zadanej godzinie — raz na okres, także po restarcie, i bez nadawania zaległej lawiny po dłuższym postoju.

**Architecture:** `MessageRepo` zna tabelę `messages`, `Scheduler` wyznacza czas odpalenia i wstawia wiersz do `schedule_fires`. Idempotencja nie opiera się na pamięci procesu, tylko na `UNIQUE (message_id, fire_at)` — po restarcie i po dwóch tikach w tej samej sekundzie wynik jest ten sam. Dopiero po wygranym wstawieniu scheduler decyduje, czy odpalenie jest aktualne, czy spóźnione.

**Tech Stack:** croner 4 (`Cron::from_str`, `find_previous_occurrence`), chrono, sqlx 0.8, tokio.

**Spec:** `docs/superpowers/specs/2026-10-13-aars-design.md`

## Global Constraints

- Idempotencja po `UNIQUE (message_id, fire_at)` w `schedule_fires` — restart ani dwa tiki w tej samej sekundzie nie dublują nadań (spec §6).
- Polityka spóźnień: odpalenie starsze niż `misfire_grace_s` (domyślnie 600 s) jest pomijane z adnotacją — z wyjątkiem priorytetu `Emergency`, który odpala i tak. Biuletyn pogodowy z zeszłej nocy nie ma wartości; ostrzeżenie ma (spec §6).
- Definicja z `schedule_kind = 'at'` musi mieć `schedule_at`, z `'cron'` musi mieć `cron_expr` — wymuszone przez `CHECK` w migracji.
- `valid_from` i `valid_until` ograniczają okres, w którym definicja odpala.
- Harmonogram tylko wstawia do kolejki; nadawaniem zajmuje się dyspozytor z etapu 4.
- `aars-core` nie wie o HTTP ani o syntezie (spec D6, D8).
- Żaden test nie dotyka karty dźwiękowej, GPIO ani sieci (spec §11).
- TDD: test przed implementacją, commit po każdym zielonym cyklu.

---

### Task 1: `MessageRepo`

**Files:**
- Create: `crates/aars-core/src/message.rs`
- Modify: `crates/aars-core/src/lib.rs`

**Interfaces:**
- Consumes: `aars_core::priority::Priority`, `aars_core::error::DbError`
- Produces: `aars_core::message::{MessageRepo, Message, ScheduleKind, NewMessage}`; metody `create`, `get`, `list`, `scheduled`, `set_enabled`, `delete`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-core/src/message.rs`:

```rust
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
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-core message`
Expected: FAIL — `cannot find type MessageRepo`

- [ ] **Step 3: Zaimplementuj `MessageRepo`**

`crates/aars-core/src/message.rs` — nad blokiem testów:

```rust
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
        let rows = sqlx::query(&format!("SELECT {COLUMNS} FROM messages ORDER BY title"))
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

    pub async fn delete(&self, id: i64) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM messages WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}
```

`crates/aars-core/src/lib.rs` — dopisz `pub mod message;`

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core message`
Expected: PASS — trzy testy

- [ ] **Step 5: Commit**

```bash
git add crates/aars-core
git commit -m "feat(core): repozytorium definicji komunikatów"
```

---

### Task 2: `Scheduler`

**Files:**
- Create: `crates/aars-core/src/scheduler.rs`
- Modify: `crates/aars-core/src/lib.rs`
- Modify: `crates/aars-core/Cargo.toml` (dodaj `croner`)
- Modify: `Cargo.toml` (dodaj `croner` do `workspace.dependencies`)

**Interfaces:**
- Consumes: `aars_core::message::{MessageRepo, Message, ScheduleKind}`, `aars_core::queue::{QueueRepo, EnqueueRequest}`, `aars_core::priority::Priority`
- Produces: `aars_core::scheduler::{Scheduler, SchedulerConfig, TickReport, FireDecision}`; `Scheduler::tick(&self, now) -> Result<TickReport, DbError>`; `Scheduler::run(self, shutdown)`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-core/src/scheduler.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;
    use crate::message::NewMessage;

    fn at(iso: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&Utc)
    }

    fn definition(title: &str, schedule: ScheduleKind, priority: Priority) -> NewMessage {
        NewMessage {
            title: title.to_owned(),
            body_template: format!("treść {title}"),
            priority,
            interrupting: false,
            repeat_count: 1,
            repeat_interval: Duration::ZERO,
            schedule,
            valid_from: None,
            valid_until: None,
            source_id: None,
            created_by: None,
        }
    }

    struct Rig {
        scheduler: Scheduler,
        messages: MessageRepo,
        queue: Arc<QueueRepo>,
    }

    async fn rig(misfire_grace: Duration) -> Rig {
        let pool = test_pool().await;
        let messages = MessageRepo::new(pool.clone());
        let queue = Arc::new(QueueRepo::new(pool.clone()));
        let scheduler = Scheduler::new(
            MessageRepo::new(pool),
            queue.clone(),
            SchedulerConfig { misfire_grace, tick: Duration::from_millis(50) },
        );
        Rig { scheduler, messages, queue }
    }

    #[tokio::test]
    async fn a_cron_definition_fires_once_per_occurrence() {
        let rig = rig(Duration::from_secs(600)).await;
        rig.messages
            .create(definition("biuletyn", ScheduleKind::Cron("0 18 * * *".into()), Priority::Normal))
            .await
            .unwrap();

        // 18:00:30 — minutę po trafieniu we wzorzec.
        let now = at("2026-10-13T18:00:30Z");
        let report = rig.scheduler.tick(now).await.unwrap();
        assert_eq!(report.queued, 1, "{report:?}");

        // Kolejne tiki w tym samym okresie nie mogą dodać nic więcej.
        for offset in [60, 600, 3599] {
            let later = now + chrono::Duration::seconds(offset);
            let report = rig.scheduler.tick(later).await.unwrap();
            assert_eq!(report.queued, 0, "tik +{offset}s zdublował nadanie: {report:?}");
        }

        assert_eq!(rig.queue.pending(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_same_occurrence_is_not_queued_twice_after_a_restart() {
        let pool = test_pool().await;
        let messages = MessageRepo::new(pool.clone());
        let id = messages
            .create(definition("biuletyn", ScheduleKind::Cron("0 18 * * *".into()), Priority::Normal))
            .await
            .unwrap();
        let queue = Arc::new(QueueRepo::new(pool.clone()));
        let config = SchedulerConfig {
            misfire_grace: Duration::from_secs(600),
            tick: Duration::from_millis(50),
        };
        let now = at("2026-10-13T18:00:30Z");

        let first = Scheduler::new(MessageRepo::new(pool.clone()), queue.clone(), config);
        assert_eq!(first.tick(now).await.unwrap().queued, 1);

        // Nowy scheduler na tej samej bazie — jak po restarcie procesu.
        // Idempotencja nie może opierać się na pamięci procesu.
        let second = Scheduler::new(MessageRepo::new(pool.clone()), queue.clone(), config);
        assert_eq!(second.tick(now).await.unwrap().queued, 0);

        let fires: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM schedule_fires WHERE message_id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(fires, 1);
        assert_eq!(queue.pending(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_stale_occurrence_is_skipped_rather_than_transmitted_late() {
        let rig = rig(Duration::from_secs(600)).await;
        rig.messages
            .create(definition("biuletyn", ScheduleKind::Cron("0 18 * * *".into()), Priority::Normal))
            .await
            .unwrap();

        // Stacja wstała o 23:00. Biuletyn z 18:00 nie ma już wartości.
        let report = rig.scheduler.tick(at("2026-10-13T23:00:00Z")).await.unwrap();

        assert_eq!(report.queued, 0);
        assert_eq!(report.skipped, 1, "{report:?}");
        assert!(rig.queue.pending(10).await.unwrap().is_empty());

        let reason: Option<String> =
            sqlx::query_scalar("SELECT skipped_reason FROM schedule_fires LIMIT 1")
                .fetch_one(rig.messages.pool())
                .await
                .unwrap();
        assert!(reason.is_some(), "pominięcie musi być zapisane");
    }

    #[tokio::test]
    async fn a_stale_emergency_occurrence_fires_anyway() {
        let rig = rig(Duration::from_secs(600)).await;
        rig.messages
            .create(definition(
                "alarm",
                ScheduleKind::Cron("0 18 * * *".into()),
                Priority::Emergency,
            ))
            .await
            .unwrap();

        let report = rig.scheduler.tick(at("2026-10-13T23:00:00Z")).await.unwrap();

        assert_eq!(report.queued, 1, "ostrzeżenie spóźnione nadal ma wartość: {report:?}");
        assert_eq!(report.skipped, 0);
    }

    #[tokio::test]
    async fn a_one_shot_definition_fires_once_and_then_disables_itself() {
        let rig = rig(Duration::from_secs(600)).await;
        let when = at("2026-10-13T18:00:00Z");
        let id = rig
            .messages
            .create(definition("jednorazowy", ScheduleKind::At(when), Priority::Normal))
            .await
            .unwrap();

        assert_eq!(rig.scheduler.tick(when).await.unwrap().queued, 1);
        assert_eq!(
            rig.scheduler.tick(when + chrono::Duration::seconds(30)).await.unwrap().queued,
            0,
            "jednorazowy nie może odpalić drugi raz"
        );

        let message = rig.messages.get(id).await.unwrap().unwrap();
        assert!(!message.enabled, "po odpaleniu definicja jednorazowa wyłącza się sama");
    }

    #[tokio::test]
    async fn a_one_shot_definition_does_not_fire_before_its_time() {
        let rig = rig(Duration::from_secs(600)).await;
        let when = at("2026-10-13T18:00:00Z");
        rig.messages
            .create(definition("jednorazowy", ScheduleKind::At(when), Priority::Normal))
            .await
            .unwrap();

        let report = rig.scheduler.tick(when - chrono::Duration::minutes(1)).await.unwrap();
        assert_eq!(report.queued, 0);
        assert_eq!(report.skipped, 0);
    }

    #[tokio::test]
    async fn a_definition_outside_its_validity_window_does_not_fire() {
        let rig = rig(Duration::from_secs(600)).await;
        let mut expired =
            definition("sezonowy", ScheduleKind::Cron("0 18 * * *".into()), Priority::Normal);
        expired.valid_until = Some(at("2026-10-01T00:00:00Z"));
        rig.messages.create(expired).await.unwrap();

        let mut future =
            definition("przyszły", ScheduleKind::Cron("0 18 * * *".into()), Priority::Normal);
        future.valid_from = Some(at("2026-12-01T00:00:00Z"));
        rig.messages.create(future).await.unwrap();

        let report = rig.scheduler.tick(at("2026-10-13T18:00:10Z")).await.unwrap();

        assert_eq!(report.queued, 0, "{report:?}");
        assert!(rig.queue.pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_broken_cron_expression_is_reported_without_stopping_the_others() {
        let rig = rig(Duration::from_secs(600)).await;
        rig.messages
            .create(definition("zepsuty", ScheduleKind::Cron("to nie cron".into()), Priority::Normal))
            .await
            .unwrap();
        rig.messages
            .create(definition("dobry", ScheduleKind::Cron("0 18 * * *".into()), Priority::Normal))
            .await
            .unwrap();

        let report = rig.scheduler.tick(at("2026-10-13T18:00:10Z")).await.unwrap();

        assert_eq!(report.errors, 1, "{report:?}");
        assert_eq!(report.queued, 1, "zepsuta definicja nie może blokować pozostałych: {report:?}");
    }

    #[tokio::test]
    async fn a_definition_with_repeats_expands_them_in_the_queue() {
        let rig = rig(Duration::from_secs(600)).await;
        let mut repeated =
            definition("ostrzeżenie", ScheduleKind::Cron("0 18 * * *".into()), Priority::High);
        repeated.repeat_count = 3;
        repeated.repeat_interval = Duration::from_secs(600);
        rig.messages.create(repeated).await.unwrap();

        rig.scheduler.tick(at("2026-10-13T18:00:10Z")).await.unwrap();

        let pending = rig.queue.pending(10).await.unwrap();
        assert_eq!(pending.len(), 3, "powtórzenia rozwijane przez kolejkę");
        assert!(pending.iter().all(|i| i.repeat_group.is_some()));
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-core scheduler`
Expected: FAIL — `cannot find type Scheduler`

- [ ] **Step 3: Zaimplementuj `Scheduler`**

`Cargo.toml` workspace — dopisz do `[workspace.dependencies]`:

```toml
croner = "4"
```

`crates/aars-core/Cargo.toml` — dopisz do `[dependencies]`:

```toml
croner.workspace = true
```

`crates/aars-core/src/scheduler.rs` — nad blokiem testów:

```rust
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use croner::Cron;

use crate::error::DbError;
use crate::message::{Message, MessageRepo, ScheduleKind};
use crate::priority::Priority;
use crate::queue::{EnqueueRequest, QueueRepo};
use crate::sink::CancelToken;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SchedulerConfig {
    /// Jak dawno mogło minąć odpalenie, by nadal było warte nadania.
    pub misfire_grace: Duration,
    /// Jak często sprawdzać harmonogram.
    pub tick: Duration,
}

/// Co zrobił jeden przebieg harmonogramu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickReport {
    /// Odpalenia wstawione do kolejki.
    pub queued: u32,
    /// Odpalenia pominięte jako spóźnione.
    pub skipped: u32,
    /// Definicje, których nie dało się policzyć (np. zepsuty wzorzec cron).
    pub errors: u32,
}

/// Dlaczego scheduler zdecydował tak, a nie inaczej.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireDecision {
    /// Odpalenie aktualne — do kolejki.
    Fire,
    /// Odpalenie spóźnione ponad `misfire_grace` i nie dość pilne.
    Skip,
}

/// Zamienia definicje komunikatów w zadania kolejki. Nie nadaje — tym
/// zajmuje się dyspozytor.
pub struct Scheduler {
    messages: MessageRepo,
    queue: Arc<QueueRepo>,
    config: SchedulerConfig,
}

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

impl Scheduler {
    pub fn new(messages: MessageRepo, queue: Arc<QueueRepo>, config: SchedulerConfig) -> Self {
        Self { messages, queue, config }
    }

    /// Jeden przebieg. Wydzielony z `run` i przyjmujący `now` jawnie, żeby
    /// polityka spóźnień dała się przetestować bez czekania na zegar.
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<TickReport, DbError> {
        let mut report = TickReport::default();

        for message in self.messages.scheduled().await? {
            if !in_validity_window(&message, now) {
                continue;
            }

            let fire_at = match due_occurrence(&message, now) {
                Ok(None) => continue,
                Ok(Some(at)) => at,
                Err(err) => {
                    tracing::error!(
                        id = message.id,
                        title = %message.title,
                        %err,
                        "nie można policzyć harmonogramu definicji"
                    );
                    report.errors += 1;
                    continue;
                }
            };

            let decision = self.decide(&message, fire_at, now);

            // Wstawienie do `schedule_fires` jest naszym zamkiem: UNIQUE
            // (message_id, fire_at) sprawia, że tylko jeden przebieg — w tym
            // procesie czy po restarcie — wygra to odpalenie.
            let claimed = self.claim_fire(message.id, fire_at, decision).await?;
            if !claimed {
                continue;
            }

            match decision {
                FireDecision::Skip => {
                    tracing::warn!(
                        id = message.id,
                        title = %message.title,
                        fire_at = %iso(fire_at),
                        "odpalenie pominięte jako spóźnione"
                    );
                    report.skipped += 1;
                }
                FireDecision::Fire => {
                    self.enqueue(&message, now).await?;
                    if matches!(message.schedule, ScheduleKind::At(_)) {
                        // Jednorazowy ma odpalić raz; wyłączamy go, żeby nie
                        // wisiał w harmonogramie na zawsze.
                        self.messages.set_enabled(message.id, false).await?;
                    }
                    report.queued += 1;
                }
            }
        }

        Ok(report)
    }

    fn decide(&self, message: &Message, fire_at: DateTime<Utc>, now: DateTime<Utc>) -> FireDecision {
        let lateness = now.signed_duration_since(fire_at);
        let grace = chrono::Duration::from_std(self.config.misfire_grace)
            .unwrap_or(chrono::Duration::zero());

        // Ostrzeżenie spóźnione nadal ma wartość; biuletyn z zeszłej nocy nie.
        if lateness <= grace || message.priority == Priority::Emergency {
            FireDecision::Fire
        } else {
            FireDecision::Skip
        }
    }

    /// Zgłasza odpalenie. `false` znaczy, że ktoś już je zgłosił.
    async fn claim_fire(
        &self,
        message_id: i64,
        fire_at: DateTime<Utc>,
        decision: FireDecision,
    ) -> Result<bool, DbError> {
        let reason = match decision {
            FireDecision::Fire => None,
            FireDecision::Skip => Some("spóźnione ponad misfire_grace_s"),
        };

        let result = sqlx::query(
            "INSERT OR IGNORE INTO schedule_fires (message_id, fire_at, skipped_reason)
             VALUES (?, ?, ?)",
        )
        .bind(message_id)
        .bind(iso(fire_at))
        .bind(reason)
        .execute(self.queue.pool())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    async fn enqueue(&self, message: &Message, now: DateTime<Utc>) -> Result<(), DbError> {
        self.queue
            .enqueue(EnqueueRequest {
                message_id: Some(message.id),
                // Szablon trafia do kolejki jako tekst. Wstawianie danych
                // źródeł dochodzi w etapie 8 — dziś komunikaty są stałe.
                rendered_text: message.body_template.clone(),
                priority: message.priority,
                interrupting: message.interrupting,
                not_before: Some(now),
                expires_at: message.valid_until,
                repeat_count: message.repeat_count,
                repeat_interval: message.repeat_interval,
                created_by: None,
            })
            .await?;
        Ok(())
    }

    pub async fn run(self, shutdown: CancelToken) {
        while !shutdown.is_cancelled() {
            match self.tick(Utc::now()).await {
                Ok(report) if report != TickReport::default() => {
                    tracing::info!(
                        queued = report.queued,
                        skipped = report.skipped,
                        errors = report.errors,
                        "harmonogram odpalił"
                    );
                }
                Ok(_) => {}
                Err(err) => tracing::error!(%err, "przebieg harmonogramu zawiódł"),
            }
            tokio::time::sleep(self.config.tick).await;
        }

        tracing::info!("harmonogram zatrzymany");
    }
}

fn in_validity_window(message: &Message, now: DateTime<Utc>) -> bool {
    if let Some(from) = message.valid_from
        && now < from
    {
        return false;
    }
    if let Some(until) = message.valid_until
        && now > until
    {
        return false;
    }
    true
}

/// Ostatnie odpalenie, które powinno już nastąpić, albo `None`, gdy żadne
/// jeszcze nie minęło.
fn due_occurrence(message: &Message, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>, String> {
    match &message.schedule {
        ScheduleKind::Immediate => Ok(None),
        ScheduleKind::At(at) => Ok(if *at <= now { Some(*at) } else { None }),
        ScheduleKind::Cron(expr) => {
            let cron = Cron::from_str(expr).map_err(|e| format!("wzorzec „{expr}": {e}"))?;
            let previous = cron
                .find_previous_occurrence(&now, true)
                .map_err(|e| format!("wyznaczenie odpalenia dla „{expr}": {e}"))?;
            Ok(Some(previous))
        }
    }
}
```

`crates/aars-core/src/lib.rs` — dopisz `pub mod scheduler;`

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core scheduler`
Expected: PASS — dziewięć testów harmonogramu

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/aars-core
git commit -m "feat(core): harmonogram z cronem, czasem jednorazowym i polityką spóźnień"
```

---

### Task 3: CLI komunikatów i harmonogram w `aars run`

**Files:**
- Modify: `crates/aars/src/cli.rs`
- Modify: `crates/aars/src/main.rs`
- Modify: `crates/aars/tests/cli.rs`

**Interfaces:**
- Consumes: `aars_core::message::{MessageRepo, NewMessage, ScheduleKind}`, `aars_core::scheduler::{Scheduler, SchedulerConfig}`
- Produces: podkomendy `aars message add|list|enable|disable|remove`; `aars run` prowadzi harmonogram obok dyspozytora

- [ ] **Step 1: Napisz test, który ma nie przejść**

Dopisz do `crates/aars/tests/cli.rs`:

```rust
#[test]
fn message_add_then_list_shows_the_schedule() {
    let dir = std::env::temp_dir().join(format!("aars-msg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("msg.db");

    assert!(
        aars().env("AARS_STORAGE__DB_PATH", &db).arg("migrate").output().unwrap().status.success()
    );

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args([
            "message", "add", "--title", "biuletyn", "--cron", "0 18 * * *", "--priority", "high",
            "prognoza na dziś",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args(["message", "list"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("biuletyn"), "{stdout}");
    assert!(stdout.contains("0 18 * * *"), "{stdout}");
    assert!(stdout.contains("High"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn message_add_rejects_a_broken_cron_expression_up_front() {
    let dir = std::env::temp_dir().join(format!("aars-badcron-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("bad.db");

    assert!(
        aars().env("AARS_STORAGE__DB_PATH", &db).arg("migrate").output().unwrap().status.success()
    );

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args(["message", "add", "--title", "zly", "--cron", "to nie cron", "treść"])
        .output()
        .unwrap();

    assert!(!out.status.success(), "zepsuty wzorzec nie może wejść do bazy");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cron"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&dir).ok();
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars message_add`
Expected: FAIL — `unrecognized subcommand 'message'`

- [ ] **Step 3: Zaimplementuj podkomendy i wplecenie harmonogramu**

`crates/aars/src/cli.rs` — dopisz do `enum Command`:

```rust
    /// Definicje komunikatów i ich harmonogram.
    Message {
        #[command(subcommand)]
        action: MessageAction,
    },
```

oraz nowy podenum:

```rust
#[derive(Debug, Subcommand)]
pub enum MessageAction {
    /// Dodaje definicję komunikatu.
    Add {
        /// Treść komunikatu.
        text: String,
        #[arg(long)]
        title: String,
        /// Wzorzec cron, np. "0 18 * * *".
        #[arg(long, group = "schedule")]
        cron: Option<String>,
        /// Jednorazowo o zadanej chwili (RFC 3339).
        #[arg(long, group = "schedule")]
        at: Option<String>,
        #[arg(long, default_value = "normal")]
        priority: String,
        #[arg(long)]
        interrupting: bool,
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        #[arg(long, default_value_t = 600)]
        every: u64,
    },
    /// Wypisuje definicje komunikatów.
    List,
    /// Włącza definicję.
    Enable { id: i64 },
    /// Wyłącza definicję.
    Disable { id: i64 },
    /// Usuwa definicję.
    Remove { id: i64 },
}
```

`crates/aars/src/main.rs` — dopisz gałąź `Message` oraz uruchomienie harmonogramu w `Run`:

```rust
        cli::Command::Message { action } => {
            use aars_core::message::{MessageRepo, NewMessage, ScheduleKind};

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;
            let repo = MessageRepo::new(pool);

            match action {
                cli::MessageAction::Add {
                    text,
                    title,
                    cron,
                    at,
                    priority,
                    interrupting,
                    repeat,
                    every,
                } => {
                    let schedule = match (cron, at) {
                        (Some(expr), _) => {
                            // Zepsuty wzorzec odrzucamy tutaj, nie w nocy przy
                            // pierwszym przebiegu harmonogramu.
                            <croner::Cron as std::str::FromStr>::from_str(&expr)
                                .with_context(|| format!("niepoprawny wzorzec cron: {expr}"))?;
                            ScheduleKind::Cron(expr)
                        }
                        (_, Some(when)) => {
                            let parsed = chrono::DateTime::parse_from_rfc3339(&when)
                                .with_context(|| format!("niepoprawna data: {when}"))?;
                            ScheduleKind::At(parsed.with_timezone(&chrono::Utc))
                        }
                        (None, None) => ScheduleKind::Immediate,
                    };

                    let id = repo
                        .create(NewMessage {
                            title,
                            body_template: text,
                            priority: parse_priority(&priority)?,
                            interrupting,
                            repeat_count: repeat,
                            repeat_interval: std::time::Duration::from_secs(every),
                            schedule,
                            valid_from: None,
                            valid_until: None,
                            source_id: None,
                            created_by: None,
                        })
                        .await
                        .context("zapis definicji komunikatu")?;
                    println!("dodano definicję {id}");
                }
                cli::MessageAction::List => {
                    let messages = repo.list().await.context("odczyt definicji")?;
                    if messages.is_empty() {
                        println!("brak definicji");
                    }
                    for message in messages {
                        let schedule = match &message.schedule {
                            ScheduleKind::Immediate => "na żądanie".to_owned(),
                            ScheduleKind::At(at) => at.format("%Y-%m-%d %H:%M:%S").to_string(),
                            ScheduleKind::Cron(expr) => expr.clone(),
                        };
                        println!(
                            "{:>4}  {:<9} {:<20} {:<16} {}{}",
                            message.id,
                            format!("{:?}", message.priority),
                            schedule,
                            message.title,
                            if message.enabled { "" } else { "[wyłączony] " },
                            message.body_template
                        );
                    }
                }
                cli::MessageAction::Enable { id } => {
                    repo.set_enabled(id, true).await.context("włączanie definicji")?;
                    println!("włączono {id}");
                }
                cli::MessageAction::Disable { id } => {
                    repo.set_enabled(id, false).await.context("wyłączanie definicji")?;
                    println!("wyłączono {id}");
                }
                cli::MessageAction::Remove { id } => {
                    let count = repo.delete(id).await.context("usuwanie definicji")?;
                    println!("usunięto {count}");
                }
            }
        }
```

W gałęzi `cli::Command::Run`, po utworzeniu dyspozytora, dodaj harmonogram czytający `misfire_grace_s` z `settings`:

```rust
            let misfire_grace_s: u64 =
                sqlx::query_scalar::<_, String>(
                    "SELECT value FROM settings WHERE key = 'misfire_grace_s'",
                )
                .fetch_one(&pool)
                .await
                .context("odczyt misfire_grace_s")?
                .parse()
                .unwrap_or(600);

            let scheduler = aars_core::scheduler::Scheduler::new(
                aars_core::message::MessageRepo::new(pool.clone()),
                queue.clone(),
                aars_core::scheduler::SchedulerConfig {
                    misfire_grace: std::time::Duration::from_secs(misfire_grace_s),
                    tick: std::time::Duration::from_secs(1),
                },
            );
            let scheduler_task = tokio::spawn(scheduler.run(shutdown.clone()));
```

Przy zatrzymaniu dołóż `let _ = scheduler_task.await;` obok `let _ = task.await;`.

**Uwaga:** `queue` musi być `Arc<QueueRepo>` utworzonym raz i współdzielonym
przez dyspozytora i harmonogram — zmień konstrukcję dyspozytora tak, by
dostał klon tego samego `Arc`, zamiast tworzyć własne repozytorium.

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 5: Sprawdź ręcznie, że harmonogram odpala**

```bash
export AARS_STORAGE__DB_PATH=/tmp/aars-s/a.db
cargo run -q -p aars -- migrate
# Wzorzec „co minutę", żeby nie czekać do 18:00.
cargo run -q -p aars -- message add --title minutnik --cron "* * * * *" "test harmonogramu"
cargo run -q -p aars -- run    # przerwij po ~70 s
cargo run -q -p aars -- queue list
sqlite3 /tmp/aars-s/a.db "SELECT message_id, fire_at, skipped_reason FROM schedule_fires;"
```
Expected: jeden wiersz w `schedule_fires` na minutę, bez duplikatów

- [ ] **Step 6: Commit**

```bash
git add crates/aars
git commit -m "feat(bin): CLI definicji komunikatów i harmonogram w run"
```

---

## Kryterium wyjścia z etapu

- `cargo test --workspace` zielone; `cargo clippy --workspace --all-targets -- -D warnings` bez zastrzeżeń.
- Definicja cron odpala raz na okres, także gdy scheduler zostanie utworzony od nowa na tej samej bazie — idempotencja nie opiera się na pamięci procesu.
- Odpalenie starsze niż `misfire_grace_s` jest pomijane z zapisanym powodem, a odpalenie o priorytecie `Emergency` nadaje się mimo spóźnienia.
- Definicja jednorazowa odpala dokładnie raz i wyłącza się sama.
- Zepsuty wzorzec cron jest odrzucany przy dodawaniu, a gdyby trafił do bazy inną drogą — nie blokuje pozostałych definicji.
- Definicja poza okresem ważności nie odpala.
