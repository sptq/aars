use std::future::Future;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{mpsc, oneshot};

use crate::interlock::RefusalReason;
use crate::priority::Priority;
use crate::queue::{QueueItem, QueueRepo};
use crate::sequencer::{SeqEvent, SequencerHandle, TransmitRequest, TxOutcome};
use crate::sink::CancelToken;
use crate::synth::{Pcm, tone};
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
    /// Częstotliwość wyjścia audio: w niej powstaje cisza testu PTT i ton
    /// testu audio.
    pub sample_rate: u32,
}

/// Identyfikator nadania testowego w sekwencerze. Wiersze `queue_items`
/// mają identyfikatory dodatnie, więc nie ma kolizji.
pub const TEST_TX_ID: i64 = -1;

/// Dozwolona długość testu PTT w sekundach (R10).
pub const TEST_PTT_RANGE: RangeInclusive<u64> = 1..=5;

/// Tekst syntezy w teście audio (R11).
pub const TEST_AUDIO_TEXT: &str = "Test audio stacji.";

/// Ile czekamy na opuszczenie PTT po Ctrl-C, zanim dyspozytor się podda.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Ile po Ctrl-C czekamy na zakończenie wątku sekwencera, zanim proces
/// wyjdzie mimo to (strażnik i tak opuścił PTT).
const SHUTDOWN_JOIN: Duration = Duration::from_secs(5);

/// Jak często w trakcie nadania sprawdzamy zatrzymanie: Ctrl-C ma zdjąć
/// nadajnik z anteny w ~150 ms, niezależnie od `tick`.
const STOP_POLL: Duration = Duration::from_millis(20);

const ABORTED_BY_PREEMPTION: &str = "przerwane pilniejszym komunikatem";
const ABORTED_BY_SHUTDOWN: &str = "przerwane zatrzymaniem stacji";
/// Zadanie wzięte, ale jeszcze nie na antenie, gdy przyszło zatrzymanie.
const HELD_BY_SHUTDOWN: &str = "wstrzymane: zatrzymanie stacji";
const STOPPING: &str = "stacja się zatrzymuje";

const VOX_PTT: &str = "VOX nie steruje nadajnikiem — test PTT nie ma czego sprawdzić; \
                       nadaj krótki komunikat z /compose";
const VOX_AUDIO: &str = "przy VOX dźwięk na wyjściu nadaje na antenę — test audio niedostępny";

/// Test stacji zlecony z panelu (R10, R11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StationTest {
    Ptt { seconds: u64, by: String },
    Audio { by: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum TestOutcome {
    Done { played: Duration, note: Option<String> },
    Refused(RefusalReason),
    /// Stacja nadaje albo trwa inny test.
    Busy,
    Unsupported(&'static str),
    Failed(String),
}

type TestRequest = (StationTest, oneshot::Sender<TestOutcome>);

/// Uchwyt panelu do dyspozytora: zleca testy stacji.
#[derive(Clone)]
pub struct DispatcherControl {
    tests: mpsc::Sender<TestRequest>,
    busy: Arc<AtomicBool>,
    beat: Arc<Heartbeat>,
}

/// Puls pętli dyspozytora (etap 10, R9): ostatni obrót pętli albo jałowego
/// czekania. W trakcie nadania nie tyka, więc stary puls trwa najwyżej tyle,
/// ile limit nadania.
#[derive(Debug)]
struct Heartbeat {
    origin: std::time::Instant,
    last_ms: AtomicU64,
}

impl Heartbeat {
    fn new() -> Self {
        Self { origin: std::time::Instant::now(), last_ms: AtomicU64::new(0) }
    }

    fn beat(&self) {
        let ms = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, Ordering::SeqCst);
    }

    fn since(&self) -> Duration {
        self.origin.elapsed().saturating_sub(Duration::from_millis(self.last_ms.load(Ordering::SeqCst)))
    }
}

impl DispatcherControl {
    /// Ile minęło od ostatniego obrotu pętli dyspozytora.
    pub fn since_beat(&self) -> Duration {
        self.beat.since()
    }

    /// Zleca test i czeka na wynik najwyżej `timeout`. Gdy stacja nadaje
    /// albo inny test czeka, odpowiada `Busy` od razu.
    pub async fn run_test(&self, test: StationTest, timeout: Duration) -> TestOutcome {
        if self.busy.load(Ordering::SeqCst) {
            return TestOutcome::Busy;
        }
        let (reply, receiver) = oneshot::channel();
        match self.tests.try_send((test, reply)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return TestOutcome::Busy,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return TestOutcome::Failed("dyspozytor nie działa".to_owned());
            }
        }
        // Po przekroczeniu czasu odbiornik ginie, a dyspozytor, widząc
        // zamknięty kanał odpowiedzi, porzuca test zamiast go wykonać.
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => TestOutcome::Failed(STOPPING.to_owned()),
            Err(_) => TestOutcome::Failed("brak odpowiedzi stacji".to_owned()),
        }
    }
}

/// Spina kolejkę z sekwencerem: bierze zadanie, syntezuje, nadaje, zapisuje
/// wynik. Jedyne miejsce, które decyduje o przerwaniu trwającego nadania.
pub struct Dispatcher {
    queue: Arc<QueueRepo>,
    tx_log: TxLogRepo,
    renderer: Arc<dyn Renderer>,
    sequencer: SequencerHandle,
    config: DispatcherConfig,
    /// Odstęp ponownej próby po odmowie przez interlock.
    hold_retry: Duration,
    tests_tx: mpsc::Sender<TestRequest>,
    tests_rx: mpsc::Receiver<TestRequest>,
    /// Prawda, gdy trwa nadanie z kolejki albo test.
    busy: Arc<AtomicBool>,
    /// Ile po Ctrl-C czekamy na wątek sekwencera (restart czeka bez limitu).
    join_timeout: Duration,
    beat: Arc<Heartbeat>,
}

/// Odstęp ponownej próby po odmowie: po włączeniu nadawania komunikat idzie
/// szybko, a dyspozytor nie kręci się w pętli.
const HOLD_RETRY: Duration = Duration::from_secs(30);

/// Jak skończyło się czekanie na nadania w locie.
enum Settled {
    /// Wszystkie się zakończyły; dla nadania testowego jego ostatnie zdarzenie
    /// i czas na antenie (od `Started`, czyli z oboma ogonami).
    All(Option<(SeqEvent, Option<Duration>)>),
    /// Zatrzymanie: sekwencer nie oddał nadań w `SHUTDOWN_GRACE` albo nie żyje.
    Gave,
}

/// Ustawia flagę zajętości na czas życia strażnika.
struct BusyGuard<'a>(&'a AtomicBool);

impl<'a> BusyGuard<'a> {
    fn new(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self(flag)
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Dispatcher {
    pub fn new(
        queue: Arc<QueueRepo>,
        renderer: Arc<dyn Renderer>,
        sequencer: SequencerHandle,
        config: DispatcherConfig,
    ) -> Self {
        let tx_log = TxLogRepo::new(queue.pool().clone());
        let (tests_tx, tests_rx) = mpsc::channel(1);
        Self {
            queue,
            tx_log,
            renderer,
            sequencer,
            config,
            hold_retry: HOLD_RETRY,
            tests_tx,
            tests_rx,
            busy: Arc::new(AtomicBool::new(false)),
            join_timeout: SHUTDOWN_JOIN,
            beat: Arc::new(Heartbeat::new()),
        }
    }

    /// Skraca odstęp ponownej próby po odmowie (testy).
    pub fn with_hold_retry(mut self, hold_retry: Duration) -> Self {
        self.hold_retry = hold_retry;
        self
    }

    /// Zmienia limit czekania na wątek sekwencera po Ctrl-C (testy).
    pub fn with_join_timeout(mut self, timeout: Duration) -> Self {
        self.join_timeout = timeout;
        self
    }

    /// Uchwyt do zlecania testów stacji z panelu.
    pub fn control(&mut self) -> DispatcherControl {
        DispatcherControl { tests: self.tests_tx.clone(), busy: self.busy.clone(), beat: self.beat.clone() }
    }

    /// Pętla dyspozytora. `drain` (restart, R4): nie bierze nic nowego,
    /// dokańcza bieżące nadanie i wraca. `shutdown` (Ctrl-C): przerywa
    /// bieżące nadanie i wraca najwyżej po `SHUTDOWN_GRACE`. W obu
    /// przypadkach przed powrotem zamyka sekwencer i czeka na jego wątek,
    /// więc PTT i wyjście audio są zwolnione, zanim powstanie nowy stos.
    pub async fn run(mut self, shutdown: CancelToken, drain: CancelToken) {
        // Zadania osierocone restartem wracają do kolejki, zanim weźmiemy
        // cokolwiek nowego.
        if let Err(err) = self.queue.recover_after_restart().await {
            tracing::error!(%err, "odzyskiwanie kolejki po restarcie zawiodło");
        }

        while !shutdown.is_cancelled() && !drain.is_cancelled() {
            self.beat.beat();
            // Najpierw jeden test z panelu, potem kolejka.
            if let Ok((test, reply)) = self.tests_rx.try_recv() {
                if reply.is_closed() {
                    // Proszący już się poddał: test wykonany później
                    // podniósłby PTT, kiedy nikt się tego nie spodziewa.
                    tracing::warn!(?test, "porzucony test stacji, pomijam");
                    continue;
                }
                let busy = self.busy.clone();
                let _busy = BusyGuard::new(&busy);
                let outcome = self.station_test(test, &shutdown, &drain).await;
                let _ = reply.send(outcome);
                continue;
            }

            if let Err(err) = self.queue.expire_stale().await {
                tracing::error!(%err, "oznaczanie przedawnionych zadań zawiodło");
            }

            let claimed = match self.queue.claim_next().await {
                Ok(item) => item,
                Err(err) => {
                    tracing::error!(%err, "pobranie zadania z kolejki zawiodło");
                    self.idle(&shutdown, &drain).await;
                    continue;
                }
            };

            match claimed {
                None => self.idle(&shutdown, &drain).await,
                Some(item) => {
                    let busy = self.busy.clone();
                    let _busy = BusyGuard::new(&busy);
                    self.handle(item, &shutdown, &drain).await;
                }
            }
        }

        let Dispatcher { sequencer, tests_rx, join_timeout, .. } = self;
        // Czekające prośby o test dostają odpowiedź od razu (kanał zamknięty).
        drop(tests_rx);
        let releaser = sequencer.releaser();
        // Join wątku sekwencera blokuje, więc poza wątkami tokio — i na
        // zwykłym, odłączonym wątku, nie w `spawn_blocking`: runtime tokio
        // przy zamykaniu czeka bez limitu na swoje wątki blokujące, więc
        // zawieszony join zatrzymałby wyjście procesu po Ctrl-C. Odłączony
        // wątek std nie blokuje wyjścia.
        //
        // Restart (drain) czeka bez limitu: nowy stos nie może otworzyć PTT
        // i karty, zanim stary wątek je zwolni. Ctrl-C czeka najwyżej
        // `join_timeout` (także gdy przyszło w trakcie czekania na restart),
        // potem wychodzi mimo to — strażnik już opuścił PTT, a my wymuszamy
        // to raz jeszcze.
        let (joined_tx, mut join) = oneshot::channel::<()>();
        let spawned = std::thread::Builder::new().name("aars-seq-join".to_owned()).spawn(
            move || {
                sequencer.shutdown();
                let _ = joined_tx.send(());
            },
        );
        if let Err(err) = spawned {
            // Bez wątku nie da się zrobić joinu; uchwyt sekwencera zginął
            // razem z domknięciem, więc wątek sekwencera już się zamyka.
            tracing::error!(%err, "nie udało się uruchomić wątku zamykania sekwencera");
            releaser.force_unkey();
        }
        let mut give_up_at: Option<tokio::time::Instant> = None;
        loop {
            let now = tokio::time::Instant::now();
            if shutdown.is_cancelled() {
                let deadline = *give_up_at.get_or_insert(now + join_timeout);
                if now >= deadline {
                    tracing::error!(
                        timeout_ms = join_timeout.as_millis(),
                        "wątek sekwencera nie zakończył się po zatrzymaniu, wychodzę mimo to"
                    );
                    releaser.force_unkey();
                    break;
                }
            }
            match tokio::time::timeout(STOP_POLL, &mut join).await {
                Ok(Ok(())) => break,
                Ok(Err(_)) => {
                    // Nadawca zginął bez wysłania: panika w joinie albo brak wątku.
                    tracing::error!("zamykanie sekwencera nie potwierdziło joinu");
                    releaser.force_unkey();
                    break;
                }
                Err(_) => continue,
            }
        }
        tracing::info!("dyspozytor zatrzymany");
    }

    /// Przerwa między zajrzeniami do kolejki, przerywana zatrzymaniem.
    async fn idle(&self, shutdown: &CancelToken, drain: &CancelToken) {
        let until = tokio::time::Instant::now() + self.config.tick;
        loop {
            let now = tokio::time::Instant::now();
            if now >= until || shutdown.is_cancelled() || drain.is_cancelled() {
                return;
            }
            self.beat.beat();
            tokio::time::sleep((until - now).min(STOP_POLL)).await;
        }
    }

    async fn handle(&mut self, item: QueueItem, shutdown: &CancelToken, drain: &CancelToken) {
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

        // Synteza bywa długa: jeśli w tym czasie przyszło zatrzymanie,
        // zadanie jeszcze nie weszło na antenę — wraca do kolejki bez
        // zużycia próby, a nadajnik nie drgnie.
        if shutdown.is_cancelled() || drain.is_cancelled() {
            if let Err(err) = self.queue.hold(id, HELD_BY_SHUTDOWN, Utc::now()).await {
                tracing::error!(id, %err, "zwrot zadania przy zatrzymaniu zawiódł");
            }
            return;
        }

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

        self.await_outcome(id, item.priority, shutdown, drain, true).await;
    }

    /// Czeka, aż skończą się wszystkie nadania w locie: zlecone `first`
    /// i to, co je przerwało. W tym czasie pilnuje zadań przerywających
    /// o wyższym priorytecie (gdy `preempt` i nie ma zatrzymania), a przy
    /// `shutdown` przerywa nadanie i czeka na nie najwyżej `SHUTDOWN_GRACE`.
    ///
    /// Zdarzenia zadań z kolejki są zapisywane tu; ostatnie zdarzenie
    /// nadania testowego (`TEST_TX_ID`) jest zwracane wołającemu.
    async fn await_outcome(
        &mut self,
        first: i64,
        priority: Priority,
        shutdown: &CancelToken,
        drain: &CancelToken,
        preempt: bool,
    ) -> Settled {
        let mut in_flight = vec![first];
        // Chwila `Started` (PTT już podniesione) każdego nadania w locie.
        let mut keyed_at: Vec<(i64, std::time::Instant)> = Vec::new();
        let mut current = priority;
        let mut test_event = None;
        let mut give_up_at: Option<tokio::time::Instant> = None;
        let mut next_preempt_check = tokio::time::Instant::now() + self.config.tick;

        while !in_flight.is_empty() {
            if shutdown.is_cancelled() {
                let now = tokio::time::Instant::now();
                let deadline = *give_up_at.get_or_insert_with(|| {
                    tracing::warn!(?in_flight, "zatrzymanie: przerywam trwające nadanie");
                    // Nic czekającego w sekwencerze nie podniesie już PTT.
                    self.sequencer.close();
                    now + SHUTDOWN_GRACE
                });
                // Powtarzane co obieg: komenda mogła jeszcze nie dojść do
                // wątku sekwencera, gdy przyszło zatrzymanie.
                self.sequencer.abort_current();
                if now >= deadline {
                    tracing::error!(
                        ?in_flight,
                        "sekwencer nie oddał nadania po zatrzymaniu, zwalniam zadania"
                    );
                    self.release_in_flight(&in_flight, ABORTED_BY_SHUTDOWN).await;
                    return Settled::Gave;
                }
            }

            tokio::select! {
                event = self.sequencer.next_event() => {
                    let Some(event) = event else {
                        tracing::error!(?in_flight, "sekwencer nie żyje");
                        self.release_in_flight(&in_flight, "sekwencer nie żyje").await;
                        return Settled::Gave;
                    };
                    let done = match &event {
                        SeqEvent::Started { id } => {
                            keyed_at.push((*id, std::time::Instant::now()));
                            continue;
                        }
                        SeqEvent::Refused { id, .. }
                        | SeqEvent::Finished { id, .. }
                        | SeqEvent::Skipped { id } => *id,
                    };
                    in_flight.retain(|id| *id != done);
                    let on_air = keyed_at
                        .iter()
                        .position(|(id, _)| *id == done)
                        .map(|at| keyed_at.swap_remove(at).1.elapsed());
                    if done == TEST_TX_ID {
                        test_event = Some((event, on_air));
                    } else {
                        self.record_event(event, on_air, shutdown).await;
                    }
                }
                _ = tokio::time::sleep(STOP_POLL) => {
                    let now = tokio::time::Instant::now();
                    if preempt
                        && !shutdown.is_cancelled()
                        && !drain.is_cancelled()
                        && now >= next_preempt_check
                    {
                        next_preempt_check = now + self.config.tick;
                        if let Some((id, priority)) =
                            self.maybe_preempt(current, shutdown, drain).await
                        {
                            in_flight.push(id);
                            current = priority;
                        }
                    }
                }
            }
        }

        Settled::All(test_event)
    }

    /// Zadania, o których sekwencer już nic nie powie, wracają do kolejki
    /// (albo kończą jako anulowane), żeby nie utknęły w `transmitting`.
    async fn release_in_flight(&self, in_flight: &[i64], reason: &str) {
        for id in in_flight.iter().copied().filter(|id| *id != TEST_TX_ID) {
            if let Err(err) = self.queue.interrupted(id, reason).await {
                tracing::error!(id, %err, "zwolnienie zadania zawiodło");
            }
        }
    }

    async fn record_event(&self, event: SeqEvent, on_air: Option<Duration>, shutdown: &CancelToken) {
        match event {
            SeqEvent::Started { .. } => {}
            SeqEvent::Refused { id, reason } => {
                tracing::warn!(id, ?reason, "nadanie odrzucone, wracam do kolejki");
                // Odmowa nie gubi komunikatu: wraca wstrzymany, z powodem
                // i odstępem, bez zużycia próby.
                let retry_at = Utc::now()
                    + chrono::Duration::from_std(self.hold_retry)
                        .unwrap_or_else(|_| chrono::Duration::seconds(30));
                if let Err(err) = self.queue.hold(id, hold_reason(reason), retry_at).await {
                    tracing::error!(id, %err, "wstrzymanie zadania zawiodło");
                }
            }
            SeqEvent::Skipped { id } => {
                // Nie weszło na antenę: bez wpisu w dzienniku nadań.
                let reason = skipped_reason(shutdown.is_cancelled());
                if let Err(err) = self.queue.hold(id, reason, Utc::now()).await {
                    tracing::error!(id, %err, "zwrot pominiętego zadania zawiódł");
                }
            }
            SeqEvent::Finished { id, outcome, played } => {
                let aborted_because = if shutdown.is_cancelled() {
                    ABORTED_BY_SHUTDOWN
                } else {
                    ABORTED_BY_PREEMPTION
                };
                self.record_outcome(id, outcome, on_air.unwrap_or(played), played, aborted_because)
                    .await;
            }
        }
    }

    fn log_entry(
        &self,
        queue_item_id: Option<i64>,
        on_air: Duration,
        result: &'static str,
        note: Option<String>,
    ) -> TxLogEntry {
        let ended_at = Utc::now();
        let played_for =
            chrono::Duration::from_std(on_air).unwrap_or_else(|_| chrono::Duration::zero());
        TxLogEntry {
            queue_item_id,
            started_at: ended_at - played_for,
            ended_at,
            ptt_backend: self.config.ptt_backend.to_owned(),
            result,
            note,
        }
    }

    /// `on_air` (czas podniesionego PTT, z ogonami) idzie do dziennika
    /// nadań, `played` (samo audio) do logów.
    async fn record_outcome(
        &self,
        id: i64,
        outcome: TxOutcome,
        on_air: Duration,
        played: Duration,
        aborted_because: &str,
    ) {
        // Dziennik nadań jest dodatkiem: jego awaria jest tylko logowana
        // i nie rusza aktualizacji stanu kolejki.
        let (tx_result, note) = tx_result(&outcome);
        let entry = self.log_entry(Some(id), on_air, tx_result, note);
        if let Err(err) = self.tx_log.record(entry).await {
            tracing::error!(id, %err, "zapis do dziennika nadań zawiódł");
        }

        let result = match outcome {
            TxOutcome::Ok => {
                tracing::info!(id, played_ms = played.as_millis(), "nadanie zakończone");
                self.queue.mark_done(id).await
            }
            TxOutcome::Aborted => {
                tracing::warn!(
                    id,
                    reason = aborted_because,
                    "nadanie przerwane, zadanie wraca do kolejki"
                );
                self.queue.interrupted(id, aborted_because).await
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

    async fn station_test(
        &mut self,
        test: StationTest,
        shutdown: &CancelToken,
        drain: &CancelToken,
    ) -> TestOutcome {
        match test {
            StationTest::Ptt { seconds, by } => self.test_ptt(seconds, &by, shutdown, drain).await,
            StationTest::Audio { by } => self.test_audio(&by, shutdown).await,
        }
    }

    /// Test PTT (R10): cisza o niskim priorytecie przez sekwencer, więc
    /// działają wszystkie interlocki, limit TX i strażnik. Bez identyfikacji.
    async fn test_ptt(
        &mut self,
        seconds: u64,
        by: &str,
        shutdown: &CancelToken,
        drain: &CancelToken,
    ) -> TestOutcome {
        if self.config.ptt_backend == "vox" {
            return TestOutcome::Unsupported(VOX_PTT);
        }
        let seconds = seconds.clamp(*TEST_PTT_RANGE.start(), *TEST_PTT_RANGE.end());
        tracing::info!(seconds, by, "test PTT");

        let request = TransmitRequest {
            id: TEST_TX_ID,
            priority: Priority::Low,
            interrupting: false,
            body: Pcm::silence(self.config.sample_rate, Duration::from_secs(seconds)),
            id_audio: None,
        };
        if let Err(err) = self.sequencer.transmit(request).await {
            return TestOutcome::Failed(err);
        }

        let settled =
            self.await_outcome(TEST_TX_ID, Priority::Low, shutdown, drain, false).await;
        let (event, on_air) = match settled {
            Settled::All(Some(settled)) => settled,
            Settled::All(None) | Settled::Gave => return TestOutcome::Failed(STOPPING.to_owned()),
        };

        match event {
            SeqEvent::Refused { reason, .. } => TestOutcome::Refused(reason),
            SeqEvent::Skipped { .. } | SeqEvent::Started { .. } => {
                TestOutcome::Failed(STOPPING.to_owned())
            }
            SeqEvent::Finished { outcome, played, .. } => {
                let (result, error) = tx_result(&outcome);
                let note = match &error {
                    Some(error) => format!("test PTT: {by}; {error}"),
                    None => format!("test PTT: {by}"),
                };
                let entry = self.log_entry(None, on_air.unwrap_or(played), result, Some(note));
                if let Err(err) = self.tx_log.record(entry).await {
                    tracing::error!(%err, "zapis testu PTT do dziennika nadań zawiódł");
                }
                match outcome {
                    TxOutcome::Ok => TestOutcome::Done { played, note: None },
                    TxOutcome::Aborted => {
                        TestOutcome::Failed(format!("test przerwany: {STOPPING}"))
                    }
                    TxOutcome::Failed(err) => TestOutcome::Failed(err),
                }
            }
        }
    }

    /// Test audio (R11): ton, cisza i synteza na skonfigurowanym wyjściu,
    /// bez podnoszenia PTT. Gdy synteza zawiedzie, gra sam ton.
    async fn test_audio(&mut self, by: &str, shutdown: &CancelToken) -> TestOutcome {
        if self.config.ptt_backend == "vox" {
            return TestOutcome::Unsupported(VOX_AUDIO);
        }
        tracing::info!(by, "test audio");

        let rate = self.config.sample_rate;
        let mut pcm = tone(rate, 1000.0, Duration::from_millis(500), -6.0);
        pcm.append(&Pcm::silence(rate, Duration::from_millis(300)))
            .expect("ta sama częstotliwość");
        let note = match self.renderer.render(TEST_AUDIO_TEXT).await {
            Ok(speech) => pcm.append(&speech).err().map(|err| err.to_string()),
            Err(err) => Some(err),
        }
        .map(|err| format!("synteza nie powiodła się: {err}"));

        if shutdown.is_cancelled() {
            return TestOutcome::Failed(STOPPING.to_owned());
        }
        let mut reply = match self.sequencer.play_local(pcm) {
            Ok(reply) => reply,
            Err(err) => return TestOutcome::Failed(err),
        };

        let mut give_up_at: Option<tokio::time::Instant> = None;
        loop {
            if shutdown.is_cancelled() {
                let now = tokio::time::Instant::now();
                let deadline = *give_up_at.get_or_insert(now + SHUTDOWN_GRACE);
                self.sequencer.abort_current();
                if now >= deadline {
                    return TestOutcome::Failed(STOPPING.to_owned());
                }
            }
            tokio::select! {
                result = &mut reply => {
                    return match result {
                        Ok(Ok(played)) => TestOutcome::Done { played, note },
                        Ok(Err(err)) => TestOutcome::Failed(err),
                        Err(_) => TestOutcome::Failed("sekwencer nie odpowiedział".to_owned()),
                    };
                }
                _ = tokio::time::sleep(STOP_POLL) => {}
            }
        }
    }

    /// Jeśli w kolejce czeka zadanie przerywające o wyższym priorytecie,
    /// bierze je i zleca — sekwencer przerwie trwające nadanie. Zwraca
    /// identyfikator i priorytet zleconego zadania.
    async fn maybe_preempt(
        &mut self,
        current: Priority,
        shutdown: &CancelToken,
        drain: &CancelToken,
    ) -> Option<(i64, Priority)> {
        let candidate = match self.queue.peek_interrupting(current).await {
            Ok(Some(item)) => item,
            Ok(None) => return None,
            Err(err) => {
                tracing::error!(%err, "sprawdzenie zadań przerywających zawiodło");
                return None;
            }
        };

        // Bierzemy je przez claim_next: porządek kolejki gwarantuje, że to
        // właśnie ten element, a claim chroni przed podwójnym wzięciem.
        let claimed = match self.queue.claim_next().await {
            // Gdy w międzyczasie pojawiło się coś jeszcze pilniejszego, ono
            // też nadaje się do przerwania — działamy na tym, co dostaliśmy.
            Ok(Some(item)) => item,
            Ok(None) => return None,
            Err(err) => {
                tracing::error!(%err, "pobranie zadania przerywającego zawiodło");
                return None;
            }
        };
        debug_assert!(claimed.priority.outranks(current) || claimed.id == candidate.id);

        let body = match self.renderer.render(&claimed.rendered_text).await {
            Ok(pcm) => pcm,
            Err(err) => {
                tracing::error!(id = claimed.id, %err, "synteza komunikatu przerywającego zawiodła");
                let _ = self.queue.mark_failed(claimed.id, &err).await;
                return None;
            }
        };

        // Synteza mogła trwać sekundy: zatrzymanie albo restart, które
        // przyszły w tym czasie, nie mogą wpuścić nowego nadania na antenę.
        if shutdown.is_cancelled() || drain.is_cancelled() {
            if let Err(err) = self.queue.hold(claimed.id, HELD_BY_SHUTDOWN, Utc::now()).await {
                tracing::error!(id = claimed.id, %err, "zwrot komunikatu przerywającego zawiódł");
            }
            return None;
        }

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
            return None;
        }
        Some((claimed.id, claimed.priority))
    }
}

/// Powód zwrotu zadania, które sekwencer pominął przed `key()`: zatrzymanie
/// stacji albo przerwanie pilniejszym komunikatem (anulowany token).
fn skipped_reason(shutting_down: bool) -> &'static str {
    if shutting_down { HELD_BY_SHUTDOWN } else { ABORTED_BY_PREEMPTION }
}

/// Powód wstrzymania pokazywany w panelu. Prefiks `wstrzymane:` odróżnia go
/// od błędów (UI pokazuje tylko wstrzymania).
pub const HELD_PREFIX: &str = "wstrzymane:";

fn hold_reason(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::TxDisabled => "wstrzymane: nadawanie wyłączone",
        RefusalReason::Poisoned => "wstrzymane: blokada watchdoga",
        RefusalReason::QuietHours => "wstrzymane: okno ciszy",
        RefusalReason::Inhibited => "wstrzymane: blokada sprzętowa",
        RefusalReason::ChannelBusy => "wstrzymane: kanał zajęty",
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

    struct InterlockHarness {
        repo: Arc<QueueRepo>,
        interlocks: Arc<InMemoryInterlocks>,
        shutdown: CancelToken,
        task: tokio::task::JoinHandle<()>,
    }

    async fn harness_with_interlocks() -> InterlockHarness {
        let repo = Arc::new(QueueRepo::new(test_pool().await));
        let interlocks = Arc::new(InMemoryInterlocks::permissive());
        let sequencer = Sequencer::spawn(
            Arc::new(FakePtt::new()),
            Arc::new(FakeSink::new()),
            interlocks.clone(),
            SequencerConfig {
                tx_tail: Duration::from_millis(1),
                tx_max_duration: Duration::from_secs(30),
                id_interval: Duration::from_secs(600),
            },
            None,
        );
        let shutdown = CancelToken::new();
        let dispatcher = Dispatcher::new(
            repo.clone(),
            Arc::new(FakeRenderer::new(48_000, 2)),
            sequencer,
            DispatcherConfig { tick: Duration::from_millis(10), ptt_backend: "fake", sample_rate: 48_000 },
        )
        .with_hold_retry(Duration::from_millis(50));
        let task = tokio::spawn(dispatcher.run(shutdown.clone(), CancelToken::new()));
        InterlockHarness { repo, interlocks, shutdown, task }
    }

    impl InterlockHarness {
        async fn finish(self) {
            self.shutdown.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
        }
    }

    #[tokio::test]
    async fn a_refused_item_waits_instead_of_failing() {
        let h = harness_with_interlocks().await;
        h.interlocks.set_tx_enabled(false);
        let id = h.repo.enqueue(req("czeka", Priority::Normal, false)).await.unwrap()[0];

        // Dużo więcej niż MAX_ATTEMPTS cykli odmowy (co 50 ms). Próbkujemy
        // stan, bo element chwilowo bywa `transmitting` między claimem
        // a zwrotem do kolejki.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
        let mut seen_pending = None;
        while tokio::time::Instant::now() < deadline {
            let row: (String, i64, Option<String>) =
                sqlx::query_as("SELECT state, attempt, error FROM queue_items WHERE id = ?")
                    .bind(id)
                    .fetch_one(h.repo.pool())
                    .await
                    .unwrap();
            assert_ne!(row.0, "failed", "odmowa nie może kończyć się porażką");
            if row.0 == "pending" {
                seen_pending = Some(row);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let (state, attempt, error) = seen_pending.expect("element musi czekać jako pending");
        assert_eq!(state, "pending", "odmowa nie może kończyć się porażką");
        assert_eq!(attempt, 0, "odmowa nie liczy się jako próba");
        assert_eq!(error.as_deref(), Some("wstrzymane: nadawanie wyłączone"));

        h.finish().await;
    }

    #[tokio::test]
    async fn a_held_item_goes_out_after_transmitting_is_enabled_again() {
        let h = harness_with_interlocks().await;
        h.interlocks.set_tx_enabled(false);
        let id = h.repo.enqueue(req("czeka", Priority::Normal, false)).await.unwrap()[0];
        tokio::time::sleep(Duration::from_millis(500)).await;

        h.interlocks.set_tx_enabled(true);

        assert!(
            wait_for_state(h.repo.pool(), id, "done", Duration::from_secs(5)).await,
            "wstrzymany komunikat musi zostać nadany po włączeniu nadawania"
        );
        let error: Option<String> = sqlx::query_scalar("SELECT error FROM queue_items WHERE id = ?")
            .bind(id)
            .fetch_one(h.repo.pool())
            .await
            .unwrap();
        assert_eq!(error, None, "nadany element nie niesie powodu wstrzymania");

        h.finish().await;
    }

    #[tokio::test]
    async fn a_real_failure_still_counts_attempts() {
        let h = harness(2).await;
        let id = h.repo.enqueue(req("FAIL", Priority::Normal, false)).await.unwrap()[0];
        assert!(h.wait_for_state(id, "failed", Duration::from_secs(5)).await);

        let attempt: i64 = sqlx::query_scalar("SELECT attempt FROM queue_items WHERE id = ?")
            .bind(id)
            .fetch_one(h.repo.pool())
            .await
            .unwrap();
        assert_eq!(attempt, 1, "prawdziwa porażka liczy próbę");

        h.finish().await;
    }

    #[tokio::test]
    async fn a_held_item_still_expires() {
        let h = harness_with_interlocks().await;
        h.interlocks.set_tx_enabled(false);
        let mut r = req("przedawni się", Priority::Normal, false);
        r.expires_at = Some(Utc::now() + chrono::Duration::milliseconds(300));
        let id = h.repo.enqueue(r).await.unwrap()[0];

        assert!(
            wait_for_state(h.repo.pool(), id, "expired", Duration::from_secs(5)).await,
            "wstrzymany komunikat wygasa przez expires_at"
        );

        h.finish().await;
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
            DispatcherConfig { tick: Duration::from_millis(10), ptt_backend: "fake", sample_rate: 48_000 },
        );
        let task = tokio::spawn(dispatcher.run(shutdown.clone(), CancelToken::new()));

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
    async fn the_heartbeat_ticks_while_idle_and_stops_with_the_loop() {
        let repo = Arc::new(QueueRepo::new(test_pool().await));
        let ptt = Arc::new(FakePtt::new());
        let shutdown = CancelToken::new();
        let mut dispatcher = Dispatcher::new(
            repo,
            Arc::new(FakeRenderer::new(48_000, 1)),
            spawn_sequencer(ptt),
            DispatcherConfig { tick: Duration::from_millis(10), ptt_backend: "fake", sample_rate: 48_000 },
        );
        let control = dispatcher.control();
        let task = tokio::spawn(dispatcher.run(shutdown.clone(), CancelToken::new()));

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(control.since_beat() < Duration::from_millis(200), "{:?}", control.since_beat());

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(control.since_beat() >= Duration::from_millis(250), "pętla stoi, puls też");
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
            DispatcherConfig { tick: Duration::from_millis(10), ptt_backend: "fake", sample_rate: 48_000 },
        );
        let task = tokio::spawn(dispatcher.run(shutdown.clone(), CancelToken::new()));

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
        assert_eq!(rows[0].queue_item_id, Some(id));
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
            rows.iter().any(|r| r.queue_item_id == Some(slow) && r.result == "aborted"),
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

    // --- Testy stacji (R10–R12) i zatrzymanie dyspozytora (R4) ---

    struct Station {
        repo: Arc<QueueRepo>,
        ptt: Arc<FakePtt>,
        sink: Arc<FakeSink>,
        interlocks: Arc<InMemoryInterlocks>,
        control: DispatcherControl,
        shutdown: CancelToken,
        drain: CancelToken,
        task: tokio::task::JoinHandle<()>,
    }

    struct StationOpts {
        renderer: Arc<dyn Renderer>,
        ptt_backend: &'static str,
        watchdog: Option<crate::watchdog::WatchdogMarker>,
        tick: Duration,
        tx_tail: Duration,
        /// Wyjście zamiast `FakeSink` (np. zawieszone).
        sink: Option<Arc<dyn crate::sink::AudioSink>>,
        join_timeout: Option<Duration>,
    }

    impl Default for StationOpts {
        fn default() -> Self {
            Self {
                renderer: Arc::new(FakeRenderer::new(48_000, 2)),
                ptt_backend: "fake",
                watchdog: None,
                tick: Duration::from_millis(10),
                tx_tail: Duration::from_millis(1),
                sink: None,
                join_timeout: None,
            }
        }
    }

    async fn station(opts: StationOpts) -> Station {
        let repo = Arc::new(QueueRepo::new(test_pool().await));
        let ptt = Arc::new(FakePtt::new());
        let sink = Arc::new(FakeSink::new());
        let interlocks = Arc::new(InMemoryInterlocks::permissive());
        let out: Arc<dyn crate::sink::AudioSink> = match opts.sink {
            Some(custom) => custom,
            None => sink.clone(),
        };
        let sequencer = Sequencer::spawn(
            ptt.clone(),
            out,
            interlocks.clone(),
            SequencerConfig {
                tx_tail: opts.tx_tail,
                tx_max_duration: Duration::from_secs(30),
                id_interval: Duration::from_secs(600),
            },
            opts.watchdog,
        );
        let mut dispatcher = Dispatcher::new(
            repo.clone(),
            opts.renderer,
            sequencer,
            DispatcherConfig { tick: opts.tick, ptt_backend: opts.ptt_backend, sample_rate: 48_000 },
        );
        if let Some(timeout) = opts.join_timeout {
            dispatcher = dispatcher.with_join_timeout(timeout);
        }
        let control = dispatcher.control();
        let shutdown = CancelToken::new();
        let drain = CancelToken::new();
        let task = tokio::spawn(dispatcher.run(shutdown.clone(), drain.clone()));
        Station { repo, ptt, sink, interlocks, control, shutdown, drain, task }
    }

    impl Station {
        async fn test(&self, test: StationTest) -> TestOutcome {
            self.control.run_test(test, Duration::from_secs(20)).await
        }

        async fn finish(self) {
            self.shutdown.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
        }

        async fn queue_count(&self) -> i64 {
            sqlx::query_scalar("SELECT COUNT(*) FROM queue_items")
                .fetch_one(self.repo.pool())
                .await
                .unwrap()
        }
    }

    fn ptt_test(seconds: u64) -> StationTest {
        StationTest::Ptt { seconds, by: "admin".to_owned() }
    }

    fn audio_test() -> StationTest {
        StationTest::Audio { by: "admin".to_owned() }
    }

    fn done(outcome: TestOutcome) -> (Duration, Option<String>) {
        match outcome {
            TestOutcome::Done { played, note } => (played, note),
            other => panic!("oczekiwano Done, dostano {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_test_ptt_keys_for_the_requested_time_and_logs_without_a_queue_item() {
        let s = station(StationOpts::default()).await;

        let (played, _) = done(s.test(ptt_test(1)).await);

        assert!(
            (Duration::from_millis(900)..=Duration::from_millis(1300)).contains(&played),
            "test trwał {played:?}"
        );
        assert_eq!(s.ptt.events(), vec![PttEvent::Key, PttEvent::Unkey]);
        let rows: Vec<(Option<i64>, String, Option<String>)> =
            sqlx::query_as("SELECT queue_item_id, result, note FROM tx_log")
                .fetch_all(s.repo.pool())
                .await
                .unwrap();
        assert_eq!(rows, vec![(None, "ok".to_owned(), Some("test PTT: admin".to_owned()))]);
        assert_eq!(s.queue_count().await, 0, "test PTT nie dotyka kolejki");

        s.finish().await;
    }

    async fn assert_refused_without_touching_ptt(s: &Station, expected: RefusalReason) {
        let outcome = s.test(ptt_test(2)).await;
        assert_eq!(outcome, TestOutcome::Refused(expected));
        assert!(!s.ptt.events().contains(&PttEvent::Key), "{:?}", s.ptt.events());
        let logged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tx_log")
            .fetch_one(s.repo.pool())
            .await
            .unwrap();
        assert_eq!(logged, 0, "odmowa nie trafia do dziennika nadań");
    }

    #[tokio::test]
    async fn a_test_ptt_is_refused_by_the_kill_switch() {
        let s = station(StationOpts::default()).await;
        s.interlocks.set_tx_enabled(false);

        assert_refused_without_touching_ptt(&s, RefusalReason::TxDisabled).await;

        s.finish().await;
    }

    #[tokio::test]
    async fn a_test_ptt_is_refused_after_a_watchdog_intervention() {
        // Strażnik z osobnym PTT, żeby jego interwencja nie zostawiła zdarzeń
        // w PTT stacji: liczy się tylko lepka flaga.
        use crate::ptt::PttBackend;
        let other = Arc::new(FakePtt::new());
        let watchdog = crate::watchdog::Watchdog::spawn(other.clone(), Duration::from_millis(50));
        let marker = watchdog.marker();
        other.key().unwrap();
        marker.mark_keyed();
        std::thread::sleep(Duration::from_millis(300));
        assert!(marker.is_poisoned());

        let s = station(StationOpts { watchdog: Some(marker), ..StationOpts::default() }).await;

        assert_refused_without_touching_ptt(&s, RefusalReason::Poisoned).await;
        assert!(s.ptt.events().is_empty(), "{:?}", s.ptt.events());

        s.finish().await;
        watchdog.stop();
    }

    #[tokio::test]
    async fn a_test_ptt_in_quiet_hours_is_refused() {
        let s = station(StationOpts::default()).await;
        s.interlocks.set_quiet(true, Priority::Emergency);

        assert_refused_without_touching_ptt(&s, RefusalReason::QuietHours).await;

        s.finish().await;
    }

    #[tokio::test]
    async fn the_test_duration_is_clamped_to_five_seconds() {
        let s = station(StationOpts::default()).await;

        let (played, _) = done(s.test(ptt_test(60)).await);

        assert!(played <= Duration::from_millis(5200), "test trwał {played:?}");
        assert!(played >= Duration::from_millis(4800), "test trwał {played:?}");
        assert_eq!(s.ptt.events(), vec![PttEvent::Key, PttEvent::Unkey]);

        s.finish().await;
    }

    #[tokio::test]
    async fn a_zero_second_test_is_raised_to_one_second() {
        let s = station(StationOpts::default()).await;

        let (played, _) = done(s.test(ptt_test(0)).await);

        assert!(played >= Duration::from_millis(900), "test trwał {played:?}");

        s.finish().await;
    }

    #[tokio::test]
    async fn test_audio_never_keys_the_ptt() {
        let s = station(StationOpts::default()).await;

        let (played, note) = done(s.test(audio_test()).await);

        assert!(!s.ptt.events().contains(&PttEvent::Key), "{:?}", s.ptt.events());
        assert!(s.sink.played_samples() > 0);
        assert!(played >= Duration::from_millis(800), "grało {played:?}");
        assert_eq!(note, None);
        let logged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tx_log")
            .fetch_one(s.repo.pool())
            .await
            .unwrap();
        assert_eq!(logged, 0, "test audio nie jest nadaniem");

        s.finish().await;
    }

    struct FailingRenderer;

    impl Renderer for FailingRenderer {
        fn render<'a>(&'a self, _text: &'a str) -> RenderFuture<'a> {
            Box::pin(async { Err("piper nie wstał".to_owned()) })
        }

        fn ident(&self) -> Option<Pcm> {
            None
        }
    }

    #[tokio::test]
    async fn test_audio_with_a_failing_synth_plays_the_tone_and_says_so() {
        let s =
            station(StationOpts { renderer: Arc::new(FailingRenderer), ..StationOpts::default() })
                .await;

        let (played, note) = done(s.test(audio_test()).await);

        assert!(
            (Duration::from_millis(750)..=Duration::from_millis(900)).contains(&played),
            "grało {played:?}"
        );
        let note = note.expect("wynik mówi o błędzie syntezy");
        assert!(note.contains("synteza nie powiodła się"), "{note}");
        assert!(note.contains("piper nie wstał"), "{note}");
        assert!(!s.ptt.events().contains(&PttEvent::Key));

        s.finish().await;
    }

    #[tokio::test]
    async fn vox_refuses_both_tests() {
        let s = station(StationOpts { ptt_backend: "vox", ..StationOpts::default() }).await;

        let ptt = s.test(ptt_test(2)).await;
        let audio = s.test(audio_test()).await;

        assert!(matches!(&ptt, TestOutcome::Unsupported(why) if why.contains("/compose")), "{ptt:?}");
        assert!(matches!(&audio, TestOutcome::Unsupported(why) if why.contains("VOX")), "{audio:?}");
        assert!(s.ptt.events().is_empty(), "{:?}", s.ptt.events());
        assert_eq!(s.sink.played_samples(), 0, "przy VOX nic nie gra");

        s.finish().await;
    }

    #[tokio::test]
    async fn a_test_while_transmitting_is_busy() {
        let s = station(StationOpts {
            renderer: Arc::new(FakeRenderer::new(48_000, 100)),
            ..StationOpts::default()
        })
        .await;
        let id = s.repo.enqueue(req("dwudziestoznakowy ko", Priority::Normal, false)).await.unwrap()[0];
        assert!(wait_for_state(s.repo.pool(), id, "transmitting", Duration::from_secs(5)).await);
        tokio::time::sleep(Duration::from_millis(100)).await;

        let began = tokio::time::Instant::now();
        let outcome = s.test(ptt_test(2)).await;

        assert_eq!(outcome, TestOutcome::Busy);
        assert!(began.elapsed() < Duration::from_millis(100), "{:?}", began.elapsed());

        s.finish().await;
    }

    #[tokio::test]
    async fn an_abandoned_test_request_never_keys() {
        // Długi tik: dyspozytor śpi, a prośba czeka w kanale, aż proszący
        // się podda. Po przebudzeniu nie wolno jej wykonać.
        let s = station(StationOpts { tick: Duration::from_millis(400), ..StationOpts::default() })
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let outcome = s.control.run_test(ptt_test(1), Duration::from_millis(10)).await;
        assert_eq!(outcome, TestOutcome::Failed("brak odpowiedzi stacji".to_owned()));

        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert!(s.ptt.events().is_empty(), "porzucony test nie może nadać: {:?}", s.ptt.events());

        s.finish().await;
    }

    #[tokio::test]
    async fn drain_finishes_the_current_transmission_and_takes_nothing_new() {
        let s = station(StationOpts {
            renderer: Arc::new(FakeRenderer::new(48_000, 100)),
            ..StationOpts::default()
        })
        .await;
        let first = s.repo.enqueue(req("pierwszy komuni", Priority::Normal, false)).await.unwrap()[0];
        let second = s.repo.enqueue(req("drugi", Priority::Normal, false)).await.unwrap()[0];
        assert!(wait_for_state(s.repo.pool(), first, "transmitting", Duration::from_secs(5)).await);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let began = tokio::time::Instant::now();
        s.drain.cancel();
        tokio::time::timeout(Duration::from_millis(2500), s.task)
            .await
            .expect("dyspozytor kończy po bieżącym nadaniu")
            .unwrap();

        assert!(began.elapsed() >= Duration::from_millis(800), "drain nie może uciąć nadania");
        assert_eq!(s.repo.state_of(first).await.unwrap(), Some(crate::queue::QueueState::Done));
        assert_eq!(s.repo.state_of(second).await.unwrap(), Some(crate::queue::QueueState::Pending));
        let events = s.ptt.events();
        assert_eq!(events, vec![PttEvent::Key, PttEvent::Unkey, PttEvent::ForceUnkey]);
        assert!(!s.ptt.is_keyed());
        assert_eq!(
            Arc::strong_count(&s.ptt),
            1,
            "wątek sekwencera musi być zakończony (join), zanim run wróci"
        );
    }

    #[tokio::test]
    async fn shutdown_aborts_the_current_transmission_quickly() {
        let s = station(StationOpts {
            renderer: Arc::new(FakeRenderer::new(48_000, 100)),
            ..StationOpts::default()
        })
        .await;
        let id = s
            .repo
            .enqueue(req("komunikat na piec sekund, bardzo dlugi i nudny..", Priority::Normal, false))
            .await
            .unwrap()[0];
        assert!(wait_for_state(s.repo.pool(), id, "transmitting", Duration::from_secs(5)).await);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(s.ptt.is_keyed());

        let began = tokio::time::Instant::now();
        s.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), s.task)
            .await
            .expect("Ctrl-C przerywa nadanie szybko")
            .unwrap();

        assert!(began.elapsed() <= Duration::from_secs(1), "{:?}", began.elapsed());
        assert!(!s.ptt.is_keyed(), "PTT opuszczone");
        let (state, error): (String, Option<String>) =
            sqlx::query_as("SELECT state, error FROM queue_items WHERE id = ?")
                .bind(id)
                .fetch_one(s.repo.pool())
                .await
                .unwrap();
        assert_eq!(state, "pending", "przerwane zadanie wraca do kolejki");
        assert_eq!(error.as_deref(), Some("przerwane zatrzymaniem stacji"));
        let result: String = sqlx::query_scalar("SELECT result FROM tx_log WHERE queue_item_id = ?")
            .bind(id)
            .fetch_one(s.repo.pool())
            .await
            .unwrap();
        assert_eq!(result, "aborted");
        assert_eq!(Arc::strong_count(&s.ptt), 1, "wątek sekwencera zakończony");
    }

    #[tokio::test]
    async fn shutdown_aborts_a_running_test_ptt() {
        let s = station(StationOpts::default()).await;
        let control = s.control.clone();
        let test = tokio::spawn(async move { control.run_test(ptt_test(5), Duration::from_secs(10)).await });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(s.ptt.is_keyed());

        s.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), s.task).await.expect("szybko").unwrap();

        assert!(!s.ptt.is_keyed());
        let outcome = test.await.unwrap();
        assert!(matches!(outcome, TestOutcome::Failed(_)), "{outcome:?}");
        let result: String = sqlx::query_scalar("SELECT result FROM tx_log")
            .fetch_one(s.repo.pool())
            .await
            .unwrap();
        assert_eq!(result, "aborted");
    }

    #[tokio::test]
    async fn a_test_ptt_log_row_includes_the_tail() {
        let s = station(StationOpts { tx_tail: Duration::from_millis(300), ..StationOpts::default() })
            .await;

        done(s.test(ptt_test(1)).await);

        let duration_ms: i64 = sqlx::query_scalar("SELECT duration_ms FROM tx_log")
            .fetch_one(s.repo.pool())
            .await
            .unwrap();
        assert!(duration_ms >= 1500, "czas na antenie z dwoma ogonami, było {duration_ms} ms");
        assert!(duration_ms <= 2500, "{duration_ms} ms");

        s.finish().await;
    }

    /// Wyjście, które się zawiesza i ignoruje anulowanie.
    struct HungSink(Duration);

    impl crate::sink::AudioSink for HungSink {
        fn play(
            &self,
            _pcm: &Pcm,
            _cancel: &CancelToken,
        ) -> Result<crate::sink::PlayReport, crate::sink::SinkError> {
            std::thread::sleep(self.0);
            Ok(crate::sink::PlayReport { played: self.0, cancelled: false })
        }
    }

    #[tokio::test]
    async fn shutdown_gives_up_on_a_hung_sequencer_and_forces_the_ptt_down() {
        let s = station(StationOpts {
            sink: Some(Arc::new(HungSink(Duration::from_secs(8)))),
            join_timeout: Some(Duration::from_millis(300)),
            ..StationOpts::default()
        })
        .await;
        let id = s.repo.enqueue(req("zawieszone", Priority::Normal, false)).await.unwrap()[0];
        assert!(wait_for_state(s.repo.pool(), id, "transmitting", Duration::from_secs(5)).await);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(s.ptt.is_keyed());

        let began = tokio::time::Instant::now();
        s.shutdown.cancel();
        tokio::time::timeout(Duration::from_millis(4000), s.task)
            .await
            .expect("Ctrl-C nie może wisieć na zawieszonym sekwencerze")
            .unwrap();

        assert!(began.elapsed() < Duration::from_millis(3500), "{:?}", began.elapsed());
        assert!(!s.ptt.is_keyed(), "PTT wymuszone w dół: {:?}", s.ptt.events());
        assert!(s.ptt.events().contains(&PttEvent::ForceUnkey));
        assert_eq!(s.repo.state_of(id).await.unwrap(), Some(crate::queue::QueueState::Pending));
    }

    /// Renderer, który syntezuje komunikaty z „alarm” powoli (jak Piper).
    struct SlowAlarmRenderer(FakeRenderer);

    impl Renderer for SlowAlarmRenderer {
        fn render<'a>(&'a self, text: &'a str) -> RenderFuture<'a> {
            Box::pin(async move {
                if text.contains("alarm") {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                }
                self.0.render(text).await
            })
        }

        fn ident(&self) -> Option<Pcm> {
            None
        }
    }

    #[tokio::test]
    async fn a_preemption_rendered_during_drain_is_not_aired() {
        let s = station(StationOpts {
            renderer: Arc::new(SlowAlarmRenderer(FakeRenderer::new(48_000, 100))),
            ..StationOpts::default()
        })
        .await;
        let slow = s.repo.enqueue(req("pierwszy komuni", Priority::Normal, false)).await.unwrap()[0];
        assert!(wait_for_state(s.repo.pool(), slow, "transmitting", Duration::from_secs(5)).await);
        let urgent = s.repo.enqueue(req("alarm", Priority::Emergency, true)).await.unwrap()[0];
        // Wzięte do przerwania i w trakcie (powolnej) syntezy.
        assert!(wait_for_state(s.repo.pool(), urgent, "transmitting", Duration::from_secs(2)).await);

        s.drain.cancel();
        tokio::time::timeout(Duration::from_secs(4), s.task).await.expect("drain kończy").unwrap();

        assert_eq!(s.repo.state_of(slow).await.unwrap(), Some(crate::queue::QueueState::Done));
        assert_eq!(s.repo.state_of(urgent).await.unwrap(), Some(crate::queue::QueueState::Pending));
        let keys = s.ptt.events().iter().filter(|e| **e == PttEvent::Key).count();
        assert_eq!(keys, 1, "podczas drain nic nowego nie wchodzi na antenę: {:?}", s.ptt.events());
    }

    /// Cały proces, nie tylko `run`: po Ctrl-C z zawieszonym wyjściem audio
    /// także zamknięcie runtime'u tokio (jak koniec `main`) musi się skończyć.
    /// Wątek blokujący tokio, który wisiałby w joinie, zatrzymałby wyjście.
    #[test]
    fn ctrl_c_with_a_hung_sequencer_does_not_hang_runtime_teardown() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let began = std::time::Instant::now();
            let runtime =
                tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            let ptt = runtime.block_on(async {
                let s = station(StationOpts {
                    sink: Some(Arc::new(HungSink(Duration::from_secs(20)))),
                    join_timeout: Some(Duration::from_millis(300)),
                    ..StationOpts::default()
                })
                .await;
                let id = s.repo.enqueue(req("zawieszone", Priority::Normal, false)).await.unwrap()[0];
                assert!(
                    wait_for_state(s.repo.pool(), id, "transmitting", Duration::from_secs(5)).await
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
                s.shutdown.cancel();
                s.task.await.unwrap();
                s.ptt.clone()
            });
            drop(runtime);
            let _ = done_tx.send((began.elapsed(), ptt.is_keyed()));
        });

        let (took, keyed) = done_rx
            .recv_timeout(Duration::from_secs(8))
            .expect("zamknięcie runtime'u wisi na wątku sekwencera");
        assert!(took < Duration::from_secs(6), "zamknięcie trwało {took:?}");
        assert!(!keyed, "PTT opuszczone");
    }

    #[test]
    fn a_skipped_item_is_held_with_the_reason_of_what_stopped_it() {
        assert_eq!(skipped_reason(true), HELD_BY_SHUTDOWN);
        assert_eq!(skipped_reason(false), ABORTED_BY_PREEMPTION);
    }
}
