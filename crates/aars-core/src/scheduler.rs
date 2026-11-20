use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use croner::Cron;

use crate::error::DbError;
use crate::message::{Message, MessageRepo, ScheduleKind};
use crate::priority::Priority;
use crate::queue::{EnqueueRequest, QueueRepo};
use crate::settings::SettingsRepo;
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
    /// Harmonogram wyłączony w panelu: ten tik niczego nie zrobił ani nie zapisał.
    pub disabled: bool,
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
        // R14: wyłączony harmonogram nie wstawia niczego i nie zapisuje
        // `schedule_fires`, więc po włączeniu zaległości ocenia polityka
        // spóźnień (bez lawiny). Brak klucza albo wartość inna niż `false`
        // znaczy: włączony.
        let flag = SettingsRepo::new(self.queue.pool().clone()).kv_get("module:scheduler:enabled").await?;
        if flag.as_deref() == Some("false") {
            return Ok(TickReport { disabled: true, ..TickReport::default() });
        }

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
            if !self.claim_fire(message.id, fire_at, decision).await? {
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

    fn decide(
        &self,
        message: &Message,
        fire_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> FireDecision {
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
            let cron = Cron::from_str(expr).map_err(|e| format!("wzorzec „{expr}”: {e}"))?;
            let previous = cron
                .find_previous_occurrence(&now, true)
                .map_err(|e| format!("wyznaczenie odpalenia dla „{expr}”: {e}"))?;
            Ok(Some(previous))
        }
    }
}

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
            .create(definition("alarm", ScheduleKind::Cron("0 18 * * *".into()), Priority::Emergency))
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

    async fn set_scheduler_enabled(rig: &Rig, enabled: bool) {
        SettingsRepo::new(rig.messages.pool().clone())
            .kv_put("module:scheduler:enabled", if enabled { "true" } else { "false" })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_disabled_scheduler_fires_nothing_and_records_nothing() {
        let rig = rig(Duration::from_secs(300)).await;
        rig.messages
            .create(definition("co minutę", ScheduleKind::Cron("* * * * *".into()), Priority::Normal))
            .await
            .unwrap();
        set_scheduler_enabled(&rig, false).await;

        for now in ["2026-10-13T18:00:10Z", "2026-10-13T18:01:10Z"] {
            let report = rig.scheduler.tick(at(now)).await.unwrap();
            assert!(report.disabled, "{report:?}");
            assert_eq!(report.queued, 0);
        }
        assert!(rig.queue.pending(10).await.unwrap().is_empty());
        let fires: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schedule_fires")
            .fetch_one(rig.messages.pool())
            .await
            .unwrap();
        assert_eq!(fires, 0);
    }

    #[tokio::test]
    async fn re_enabling_does_not_replay_missed_fires() {
        let rig = rig(Duration::from_secs(300)).await;
        rig.messages
            .create(definition("co minutę", ScheduleKind::Cron("* * * * *".into()), Priority::Normal))
            .await
            .unwrap();
        let start = at("2026-10-13T18:00:10Z");
        set_scheduler_enabled(&rig, false).await;
        for minute in 0..20 {
            let r = rig.scheduler.tick(start + chrono::Duration::minutes(minute)).await.unwrap();
            assert!(r.disabled);
        }
        set_scheduler_enabled(&rig, true).await;

        let report = rig.scheduler.tick(start + chrono::Duration::minutes(20)).await.unwrap();
        assert!(!report.disabled);
        assert_eq!(report.queued, 1, "{report:?}");
        let pending = rig.queue.pending(100).await.unwrap();
        assert_eq!(pending.len(), 1, "żadnej lawiny zaległości");
        let fires: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schedule_fires")
            .fetch_one(rig.messages.pool())
            .await
            .unwrap();
        assert_eq!(fires, 1);
    }
}
