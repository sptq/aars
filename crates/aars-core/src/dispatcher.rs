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
use crate::txlog::{TxLogEntry, TxLogRepo};
use crate::watchdog::TX_TIMEOUT_ERROR;

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
    /// Nazwa backendu PTT zapisywana w dzienniku nadań.
    pub ptt_backend: &'static str,
}

/// Spina kolejkę z sekwencerem: bierze zadanie, syntezuje, nadaje, zapisuje
/// wynik. Jedyne miejsce, które decyduje o przerwaniu trwającego nadania.
pub struct Dispatcher {
    queue: Arc<QueueRepo>,
    tx_log: TxLogRepo,
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
        let tx_log = TxLogRepo::new(queue.pool().clone());
        Self { queue, tx_log, renderer, sequencer, config }
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
        // Dziennik nadań jest dodatkiem: jego awaria jest tylko logowana
        // i nie rusza aktualizacji stanu kolejki.
        let ended_at = Utc::now();
        let played_for =
            chrono::Duration::from_std(played).unwrap_or_else(|_| chrono::Duration::zero());
        let (tx_result, note) = tx_result(&outcome);
        let entry = TxLogEntry {
            queue_item_id: id,
            started_at: ended_at - played_for,
            ended_at,
            ptt_backend: self.config.ptt_backend.to_owned(),
            result: tx_result,
            note,
        };
        if let Err(err) = self.tx_log.record(entry).await {
            tracing::error!(id, %err, "zapis do dziennika nadań zawiódł");
        }

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

        // Bierzemy je przez claim_next: porządek kolejki gwarantuje, że to
        // właśnie ten element, a claim chroni przed podwójnym wzięciem.
        let claimed = match self.queue.claim_next().await {
            // Gdy w międzyczasie pojawiło się coś jeszcze pilniejszego, ono
            // też nadaje się do przerwania — działamy na tym, co dostaliśmy.
            Ok(Some(item)) => item,
            Ok(None) => return,
            Err(err) => {
                tracing::error!(%err, "pobranie zadania przerywającego zawiodło");
                return;
            }
        };
        debug_assert!(claimed.priority.outranks(current) || claimed.id == candidate.id);

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

/// Wynik do `tx_log` i notatka (treść błędu) dla zakończonego nadania.
fn tx_result(outcome: &TxOutcome) -> (&'static str, Option<String>) {
    match outcome {
        TxOutcome::Ok => ("ok", None),
        TxOutcome::Aborted => ("aborted", None),
        TxOutcome::Failed(err) if err == TX_TIMEOUT_ERROR => ("timeout", Some(err.clone())),
        TxOutcome::Failed(err) => ("failed", Some(err.clone())),
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

    fn spawn_sequencer(ptt: Arc<FakePtt>) -> crate::sequencer::SequencerHandle {
        Sequencer::spawn(
            ptt,
            Arc::new(FakeSink::new()),
            Arc::new(InMemoryInterlocks::permissive()),
            SequencerConfig {
                tx_tail: Duration::from_millis(1),
                tx_max_duration: Duration::from_secs(30),
                id_interval: Duration::from_secs(600),
            },
            None,
        )
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
        let shutdown = CancelToken::new();
        let dispatcher = Dispatcher::new(
            repo.clone(),
            Arc::new(FakeRenderer::new(48_000, ms_per_char)),
            spawn_sequencer(ptt.clone()),
            DispatcherConfig { tick: Duration::from_millis(10), ptt_backend: "fake" },
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
            wait_for_state(self.repo.pool(), id, state, within).await
        }
    }

    async fn wait_for_state(
        pool: &sqlx::SqlitePool,
        id: i64,
        state: &str,
        within: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        while tokio::time::Instant::now() < deadline {
            let actual: String = sqlx::query_scalar("SELECT state FROM queue_items WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap();
            if actual == state {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
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

        // Dowód faktycznego przerwania, nie zbiegu okoliczności: pierwsze
        // zadanie ma zapisany powód przerwania, a PTT było podnoszone dwa
        // razy. Bez tego test przeszedłby też wtedy, gdyby pierwsze nadanie
        // zdążyło skończyć się samo.
        let (state, error): (String, Option<String>) =
            sqlx::query_as("SELECT state, error FROM queue_items WHERE id = ?")
                .bind(slow)
                .fetch_one(h.repo.pool())
                .await
                .unwrap();
        assert!(
            matches!(state.as_str(), "pending" | "transmitting" | "done"),
            "przerwany komunikat nie może przepaść, jest w stanie {state}"
        );
        assert_eq!(
            error.as_deref(),
            Some("przerwane pilniejszym komunikatem"),
            "pierwsze zadanie musi mieć zapisany powód przerwania"
        );
        let keys = h.ptt.events().iter().filter(|e| **e == PttEvent::Key).count();
        assert!(keys >= 2, "oczekiwano dwóch nadań, było {keys}");

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
        let repo = Arc::new(QueueRepo::new(pool.clone()));
        let shutdown = CancelToken::new();
        let dispatcher = Dispatcher::new(
            repo.clone(),
            Arc::new(FakeRenderer::new(48_000, 2)),
            spawn_sequencer(Arc::new(FakePtt::new())),
            DispatcherConfig { tick: Duration::from_millis(10), ptt_backend: "fake" },
        );
        let task = tokio::spawn(dispatcher.run(shutdown.clone()));

        assert!(
            wait_for_state(&pool, ids[0], "done", Duration::from_secs(5)).await,
            "zadanie osierocone restartem musi zostać nadane po starcie dyspozytora"
        );

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    async fn tx_log_rows(repo: &QueueRepo) -> Vec<crate::txlog::TxLogRow> {
        crate::txlog::TxLogRepo::new(repo.pool().clone()).recent(10).await.unwrap()
    }

    #[tokio::test]
    async fn a_finished_transmission_is_written_to_the_tx_log() {
        let h = harness(2).await;
        let id = h.repo.enqueue(req("komunikat", Priority::Normal, false)).await.unwrap()[0];
        assert!(h.wait_for_state(id, "done", Duration::from_secs(5)).await);

        let rows = tx_log_rows(&h.repo).await;

        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].queue_item_id, id);
        assert_eq!(rows[0].result, "ok");
        assert_eq!(rows[0].ptt_backend, "fake");
        assert!(rows[0].duration_ms > 0, "{:?}", rows[0]);
        assert_eq!(rows[0].text.as_deref(), Some("komunikat"));

        h.finish().await;
    }

    #[tokio::test]
    async fn an_interrupted_transmission_is_logged_as_aborted() {
        let h = harness(40).await;
        let slow = h
            .repo
            .enqueue(req("dlugi komunikat zwyczajny", Priority::Normal, false))
            .await
            .unwrap()[0];
        assert!(h.wait_for_state(slow, "transmitting", Duration::from_secs(5)).await);
        let urgent = h.repo.enqueue(req("alarm", Priority::Emergency, true)).await.unwrap()[0];
        assert!(h.wait_for_state(urgent, "done", Duration::from_secs(10)).await);

        let rows = tx_log_rows(&h.repo).await;

        assert!(
            rows.iter().any(|r| r.queue_item_id == slow && r.result == "aborted"),
            "{rows:?}"
        );

        h.finish().await;
    }

    #[test]
    fn only_the_watchdog_message_maps_to_a_timeout() {
        use crate::watchdog::TX_TIMEOUT_ERROR;
        assert_eq!(tx_result(&TxOutcome::Ok), ("ok", None));
        assert_eq!(tx_result(&TxOutcome::Aborted), ("aborted", None));
        assert_eq!(
            tx_result(&TxOutcome::Failed(TX_TIMEOUT_ERROR.to_owned())),
            ("timeout", Some(TX_TIMEOUT_ERROR.to_owned()))
        );
        assert_eq!(
            tx_result(&TxOutcome::Failed("inny błąd".to_owned())),
            ("failed", Some("inny błąd".to_owned()))
        );
    }
}
