use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use crate::interlock::{Interlocks, RefusalReason};
use crate::priority::Priority;
use crate::ptt::PttBackend;
use crate::sink::{AudioSink, CancelToken};
use crate::synth::Pcm;
use crate::watchdog::{TX_TIMEOUT_ERROR, WatchdogMarker};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SequencerConfig {
    /// Czas, jaki radio potrzebuje na przełączenie się na nadawanie.
    pub tx_tail: Duration,
    pub tx_max_duration: Duration,
    /// Po jakim czasie nadawania wtrącić identyfikację w środku.
    pub id_interval: Duration,
}

/// Żądanie nadania. Zawiera gotowe PCM — synteza jest poza sekwencerem
/// (spec D8), więc sekwencer nie wie, czy pod spodem był Piper, czy fake.
#[derive(Debug, Clone)]
pub struct TransmitRequest {
    pub id: i64,
    pub priority: Priority,
    pub interrupting: bool,
    pub body: Pcm,
    /// Znak wywoławczy jako audio; `None` wyłącza identyfikację.
    pub id_audio: Option<Pcm>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TxOutcome {
    Ok,
    Aborted,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SeqEvent {
    Started { id: i64 },
    Finished { id: i64, outcome: TxOutcome, played: Duration },
    Refused { id: i64, reason: RefusalReason },
    /// Pominięte, bo sekwencer się zamyka: PTT nie drgnęło.
    Skipped { id: i64 },
}

pub enum SeqCmd {
    Transmit(TransmitRequest),
    /// Odtworzenie na wyjściu audio bez podnoszenia PTT (test audio, R11).
    /// Odpowiedź: ile zagrano albo błąd.
    PlayLocal { pcm: Pcm, reply: oneshot::Sender<Result<Duration, String>> },
    Shutdown,
}

/// Najdłuższe odtwarzanie lokalne (R11): test audio nie może grać bez końca.
pub const LOCAL_PLAY_LIMIT: Duration = Duration::from_secs(15);

/// Stan trwającego nadania, widoczny dla `SequencerHandle`, żeby umiał
/// zdecydować o przerwaniu.
type Current = Arc<std::sync::Mutex<Option<(Priority, CancelToken)>>>;

/// Token trwającego odtwarzania lokalnego (`PlayLocal`).
type Local = Arc<std::sync::Mutex<Option<CancelToken>>>;

pub struct Sequencer;

pub struct SequencerHandle {
    cmd: mpsc::UnboundedSender<SeqCmd>,
    events: mpsc::UnboundedReceiver<SeqEvent>,
    current: Current,
    local: Local,
    /// Ustawiane przy zamykaniu: komendy czekające za `Shutdown` nie mogą
    /// już podnieść PTT.
    closing: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Sequencer {
    pub fn spawn(
        ptt: Arc<dyn PttBackend>,
        sink: Arc<dyn AudioSink>,
        interlocks: Arc<dyn Interlocks>,
        config: SequencerConfig,
        watchdog: Option<WatchdogMarker>,
    ) -> SequencerHandle {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SeqCmd>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<SeqEvent>();
        let current: Current = Arc::new(std::sync::Mutex::new(None));
        let local: Local = Arc::new(std::sync::Mutex::new(None));
        let closing = Arc::new(AtomicBool::new(false));

        let worker_current = current.clone();
        let worker_local = local.clone();
        let worker_closing = closing.clone();
        let thread = std::thread::Builder::new()
            .name("aars-sequencer".to_owned())
            .spawn(move || {
                while let Some(cmd) = cmd_rx.blocking_recv() {
                    match cmd {
                        SeqCmd::Shutdown => break,
                        SeqCmd::PlayLocal { pcm, reply } => {
                            let result = if worker_closing.load(Ordering::SeqCst) {
                                Err("sekwencer się zamyka".to_owned())
                            } else if watchdog.as_ref().is_some_and(|w| w.is_poisoned()) {
                                // Po interwencji strażnika nie wiemy, czy PTT
                                // naprawdę opadło — dźwięk mógłby iść na antenę.
                                Err("blokada watchdoga: odtwarzanie niedostępne".to_owned())
                            } else {
                                let cancel = CancelToken::new();
                                *worker_local.lock().expect("mutex sekwencera") =
                                    Some(cancel.clone());
                                let result = play_local(ptt.as_ref(), sink.as_ref(), &pcm, &cancel);
                                *worker_local.lock().expect("mutex sekwencera") = None;
                                result
                            };
                            let _ = reply.send(result);
                        }
                        SeqCmd::Transmit(request) => {
                            let id = request.id;
                            if worker_closing.load(Ordering::SeqCst) {
                                // Zamykanie: nic nowego nie wchodzi na antenę.
                                tracing::warn!(id, "nadanie pominięte, sekwencer się zamyka");
                                let _ = event_tx.send(SeqEvent::Skipped { id });
                                continue;
                            }
                            // Strażnik pyta się pierwszy: binarka przenosi
                            // jego flagę do interlocków z opóźnieniem, a po
                            // interwencji nie wolno nadać ani jednej sekundy.
                            let poisoned = watchdog.as_ref().is_some_and(|w| w.is_poisoned());
                            let verdict = if poisoned {
                                Err(RefusalReason::Poisoned)
                            } else {
                                interlocks.check(request.priority)
                            };
                            if let Err(reason) = verdict {
                                tracing::warn!(id, ?reason, "nadanie odrzucone przez interlock");
                                let _ = event_tx.send(SeqEvent::Refused { id, reason });
                                continue;
                            }

                            let cancel = CancelToken::new();
                            *worker_current.lock().expect("mutex sekwencera") =
                                Some((request.priority, cancel.clone()));

                            let (outcome, played) = transmit_one(
                                ptt.as_ref(),
                                sink.as_ref(),
                                &request,
                                &config,
                                &cancel,
                                watchdog.as_ref(),
                                &event_tx,
                            );

                            *worker_current.lock().expect("mutex sekwencera") = None;
                            let _ = event_tx.send(SeqEvent::Finished { id, outcome, played });
                        }
                    }
                }

                // Wyjście z pętli nie może zostawić nadajnika na antenie.
                ptt.force_unkey();
            })
            .expect("uruchomienie wątku sekwencera");

        SequencerHandle {
            cmd: cmd_tx,
            events: event_rx,
            current,
            local,
            closing,
            thread: Some(thread),
        }
    }
}

/// Test audio (R11): najpierw opuszcza PTT, a gdy to się nie uda, nic nie
/// gra. Nigdy nie podnosi PTT i nie oznacza nadawania u strażnika.
/// Odtwarzanie jest ucięte po `LOCAL_PLAY_LIMIT`.
fn play_local(
    ptt: &dyn PttBackend,
    sink: &dyn AudioSink,
    pcm: &Pcm,
    cancel: &CancelToken,
) -> Result<Duration, String> {
    if let Err(err) = ptt.unkey() {
        tracing::error!(%err, "test audio: nie udało się opuścić PTT, nie gram");
        ptt.force_unkey();
        return Err(format!("nie udało się opuścić PTT: {err}"));
    }
    cancel.expire_at(Instant::now() + LOCAL_PLAY_LIMIT);
    sink.play(pcm, cancel).map(|report| report.played).map_err(|err| err.to_string())
}

/// Strażnik RAII: każde wyjście z nadania — sukces, błąd, panika —
/// opuszcza PTT (spec §4.2) i dopiero potem zdejmuje znacznik watchdoga.
struct PttGuard<'a> {
    ptt: &'a dyn PttBackend,
    watchdog: Option<&'a WatchdogMarker>,
}

impl Drop for PttGuard<'_> {
    fn drop(&mut self) {
        if let Err(err) = self.ptt.unkey() {
            tracing::error!(%err, "unkey zawiódł, wymuszam opuszczenie PTT");
            self.ptt.force_unkey();
        }
        if let Some(watchdog) = self.watchdog {
            watchdog.mark_unkeyed();
        }
    }
}

/// `event_tx` dostaje `Started` dopiero po udanym podniesieniu PTT:
/// zdarzenie „zaczęliśmy nadawać" dla nadania, które nigdy nie weszło na
/// antenę, wprowadzałoby w błąd panel i dziennik.
fn transmit_one(
    ptt: &dyn PttBackend,
    sink: &dyn AudioSink,
    request: &TransmitRequest,
    config: &SequencerConfig,
    cancel: &CancelToken,
    watchdog: Option<&WatchdogMarker>,
    event_tx: &mpsc::UnboundedSender<SeqEvent>,
) -> (TxOutcome, Duration) {
    let full = match assemble(request, config) {
        Ok(pcm) => pcm,
        Err(err) => return (TxOutcome::Failed(err), Duration::ZERO),
    };

    if let Err(err) = ptt.key() {
        tracing::error!(%err, id = request.id, "nie udało się podnieść PTT");
        // Guard nie powstał, więc wymuszamy stan bezpieczny wprost.
        ptt.force_unkey();
        return (TxOutcome::Failed(err.to_string()), Duration::ZERO);
    }
    // Guard powstaje przed znacznikiem: gdyby cokolwiek dalej spanikowało,
    // PTT i tak opadnie, a znacznik zostanie zdjęty.
    let _guard = PttGuard { ptt, watchdog };
    let started = Instant::now();
    if let Some(watchdog) = watchdog {
        watchdog.mark_keyed();
    }
    // Twardy limit liczony od podniesienia PTT, sprawdzany przez wyjście
    // audio co bufor — jak przerwanie (spec §4.3). Watchdog jest drugą linią.
    cancel.expire_at(started + config.tx_max_duration);
    let _ = event_tx.send(SeqEvent::Started { id: request.id });

    std::thread::sleep(config.tx_tail);

    let report = match sink.play(&full, cancel) {
        Ok(report) => report,
        Err(err) => {
            tracing::error!(%err, id = request.id, "błąd wyjścia audio");
            return (TxOutcome::Failed(err.to_string()), started.elapsed());
        }
    };

    if cancel.is_expired() {
        // Powrót z wyjścia po terminie to przekroczenie limitu, nawet gdy
        // wyjście nie zgłosiło anulowania (zawieszony sink). Limit ma też
        // pierwszeństwo przed przerwaniem: nadanie, które go przekracza,
        // przekroczy go i po ponownym wstawieniu do kolejki.
        tracing::error!(
            id = request.id,
            limit_s = config.tx_max_duration.as_secs(),
            "nadanie ucięte na twardym limicie czasu"
        );
        return (TxOutcome::Failed(TX_TIMEOUT_ERROR.to_owned()), report.played);
    }

    std::thread::sleep(config.tx_tail);

    let outcome = if report.cancelled { TxOutcome::Aborted } else { TxOutcome::Ok };
    (outcome, report.played)
}

/// Składa nadanie: znak, cisza, treść, cisza, znak (spec §4.1, §4.5).
/// Przy nadaniu dłuższym niż `id_interval` znak wchodzi też w środku.
fn assemble(request: &TransmitRequest, config: &SequencerConfig) -> Result<Pcm, String> {
    let rate = request.body.sample_rate;
    let gap = Pcm::silence(rate, Duration::from_millis(300));
    let mut out = Pcm { samples: Vec::new(), sample_rate: rate };

    fn append(out: &mut Pcm, part: &Pcm) -> Result<(), String> {
        out.append(part).map_err(|e| e.to_string())
    }

    if let Some(id_audio) = &request.id_audio {
        append(&mut out, id_audio)?;
        append(&mut out, &gap)?;
    }

    if request.body.duration() > config.id_interval
        && let Some(id_audio) = &request.id_audio
    {
        // Treść dłuższa niż interwał identyfikacji: przełam ją znakiem.
        let half = request.body.samples.len() / 2;
        let first = Pcm { samples: request.body.samples[..half].to_vec(), sample_rate: rate };
        let second = Pcm { samples: request.body.samples[half..].to_vec(), sample_rate: rate };
        append(&mut out, &first)?;
        append(&mut out, &gap)?;
        append(&mut out, id_audio)?;
        append(&mut out, &gap)?;
        append(&mut out, &second)?;
    } else {
        append(&mut out, &request.body)?;
    }

    if let Some(id_audio) = &request.id_audio {
        append(&mut out, &gap)?;
        append(&mut out, id_audio)?;
    }

    Ok(out)
}

impl SequencerHandle {
    /// Zleca nadanie. Jeśli żądanie jest przerywające i ma wyższy priorytet
    /// niż trwające nadanie, przerywa je — ale nigdy nie nadaje równolegle:
    /// anulowanie idzie przed komendą, a sekwencer bierze komendy po kolei.
    pub async fn transmit(&self, request: TransmitRequest) -> Result<(), String> {
        if request.interrupting {
            let snapshot = self.current.lock().expect("mutex sekwencera").clone();
            if let Some((current_priority, cancel)) = snapshot
                && request.priority.outranks(current_priority)
            {
                tracing::info!(
                    id = request.id,
                    "przerywam trwające nadanie komunikatem o wyższym priorytecie"
                );
                cancel.cancel();
            }
        }

        self.cmd.send(SeqCmd::Transmit(request)).map_err(|_| "sekwencer nie żyje".to_owned())
    }

    /// Odtwarza PCM na wyjściu audio bez podnoszenia PTT (R11). Odpowiedź
    /// przychodzi po zakończeniu odtwarzania.
    pub fn play_local(
        &self,
        pcm: Pcm,
    ) -> Result<oneshot::Receiver<Result<Duration, String>>, String> {
        let (reply, receiver) = oneshot::channel();
        self.cmd
            .send(SeqCmd::PlayLocal { pcm, reply })
            .map_err(|_| "sekwencer nie żyje".to_owned())?;
        Ok(receiver)
    }

    /// Przerywa trwające nadanie albo odtwarzanie lokalne (Ctrl-C). PTT
    /// opada przez strażnika RAII w wątku sekwencera.
    pub fn abort_current(&self) {
        if let Some((_, cancel)) = self.current.lock().expect("mutex sekwencera").as_ref() {
            cancel.cancel();
        }
        if let Some(cancel) = self.local.lock().expect("mutex sekwencera").as_ref() {
            cancel.cancel();
        }
    }

    /// Od tej chwili sekwencer nie podnosi już PTT dla czekających komend
    /// (dostają `Skipped`). Trwające nadanie nie jest tym przerywane.
    pub fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    pub async fn next_event(&mut self) -> Option<SeqEvent> {
        self.events.recv().await
    }

    /// Zamyka sekwencer i czeka na koniec jego wątku (join). Trwające
    /// nadanie dochodzi do końca (albo do limitu); komendy czekające za nim
    /// już nie podnoszą PTT.
    pub fn shutdown(mut self) {
        self.closing.store(true, Ordering::SeqCst);
        let _ = self.cmd.send(SeqCmd::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for SequencerHandle {
    fn drop(&mut self) {
        // Porzucony uchwyt: wątek dokończy bieżące nadanie i wyjdzie, ale nie
        // weźmie już niczego z bufora komend.
        self.closing.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interlock::InMemoryInterlocks;
    use crate::ptt::{FakePtt, PttEvent};
    use crate::sink::{FakeSink, PlayReport, SinkError};
    use crate::watchdog::{Watchdog, WatchdogHandle};
    use std::sync::Mutex;
    use std::time::Duration;

    struct Rig {
        handle: SequencerHandle,
        ptt: Arc<FakePtt>,
        interlocks: Arc<InMemoryInterlocks>,
    }

    fn rig() -> Rig {
        let ptt = Arc::new(FakePtt::new());
        let sink = Arc::new(FakeSink::new());
        let interlocks = Arc::new(InMemoryInterlocks::permissive());
        let handle = Sequencer::spawn(
            ptt.clone(),
            sink,
            interlocks.clone(),
            SequencerConfig {
                tx_tail: Duration::from_millis(1),
                tx_max_duration: Duration::from_secs(10),
                id_interval: Duration::from_secs(600),
            },
            None,
        );
        Rig { handle, ptt, interlocks }
    }

    /// Wyjście, które zapamiętuje, co strażnik widział w chwili grania,
    /// i na życzenie zawodzi zamiast grać.
    struct ProbeSink {
        inner: FakeSink,
        marker: WatchdogMarker,
        fail: bool,
        keyed_while_playing: Mutex<Vec<bool>>,
    }

    impl AudioSink for ProbeSink {
        fn play(&self, pcm: &Pcm, cancel: &CancelToken) -> Result<PlayReport, SinkError> {
            self.keyed_while_playing.lock().unwrap().push(self.marker.is_keyed());
            if self.fail {
                return Err(SinkError::Device("urządzenie zniknęło".to_owned()));
            }
            self.inner.play(pcm, cancel)
        }
    }

    struct GuardedRig {
        handle: SequencerHandle,
        ptt: Arc<FakePtt>,
        sink: Arc<ProbeSink>,
        marker: WatchdogMarker,
        watchdog: WatchdogHandle,
    }

    /// Sekwencer ze strażnikiem. Strażnik ma limit z dużym zapasem, żeby w
    /// tych testach nigdy nie interweniował — sprawdzamy tylko znakowanie.
    fn guarded_rig(tx_max: Duration, fail: bool) -> GuardedRig {
        let ptt = Arc::new(FakePtt::new());
        let watchdog = Watchdog::spawn(ptt.clone(), tx_max + Duration::from_secs(30));
        let marker = watchdog.marker();
        let sink = Arc::new(ProbeSink {
            inner: FakeSink::new(),
            marker: marker.clone(),
            fail,
            keyed_while_playing: Mutex::new(Vec::new()),
        });
        let handle = Sequencer::spawn(
            ptt.clone(),
            sink.clone(),
            Arc::new(InMemoryInterlocks::permissive()),
            SequencerConfig {
                tx_tail: Duration::from_millis(1),
                tx_max_duration: tx_max,
                id_interval: Duration::from_secs(600),
            },
            Some(marker.clone()),
        );
        GuardedRig { handle, ptt, sink, marker, watchdog }
    }

    async fn finished(handle: &mut SequencerHandle) -> SeqEvent {
        loop {
            let event = handle.next_event().await.expect("sekwencer żyje");
            if !matches!(event, SeqEvent::Started { .. }) {
                return event;
            }
        }
    }

    fn request(id: i64, priority: Priority) -> TransmitRequest {
        TransmitRequest {
            id,
            priority,
            interrupting: false,
            body: Pcm::silence(48_000, Duration::from_millis(100)),
            id_audio: Some(Pcm::silence(48_000, Duration::from_millis(20))),
        }
    }

    #[tokio::test]
    async fn a_transmission_keys_plays_and_unkeys_in_that_order() {
        let mut rig = rig();
        rig.handle.transmit(request(1, Priority::Normal)).await.unwrap();

        let event = rig.handle.next_event().await.unwrap();
        assert!(matches!(event, SeqEvent::Started { id: 1 }));
        let event = rig.handle.next_event().await.unwrap();
        assert!(
            matches!(event, SeqEvent::Finished { id: 1, outcome: TxOutcome::Ok, .. }),
            "{event:?}"
        );

        assert_eq!(rig.ptt.events(), vec![PttEvent::Key, PttEvent::Unkey]);
        assert!(!rig.ptt.is_keyed());
    }

    #[tokio::test]
    async fn the_callsign_is_transmitted_before_and_after_the_body() {
        let mut rig = rig();
        let mut req = request(1, Priority::Normal);
        req.body = Pcm::silence(48_000, Duration::from_millis(100));
        req.id_audio = Some(Pcm::silence(48_000, Duration::from_millis(50)));

        rig.handle.transmit(req).await.unwrap();
        let _started = rig.handle.next_event().await.unwrap();
        let finished = rig.handle.next_event().await.unwrap();

        let SeqEvent::Finished { played, .. } = finished else {
            panic!("oczekiwano Finished, dostano {finished:?}");
        };
        // Treść 100 ms plus dwa razy znak 50 ms plus przerwy.
        assert!(
            played >= Duration::from_millis(200),
            "nadano {played:?}, za krótko na treść z identyfikacją"
        );
    }

    #[tokio::test]
    async fn a_failed_key_never_leaves_the_transmitter_on_air() {
        let mut rig = rig();
        rig.ptt.fail_next_key();

        rig.handle.transmit(request(1, Priority::Normal)).await.unwrap();
        let event = rig.handle.next_event().await.unwrap();

        assert!(
            matches!(event, SeqEvent::Finished { outcome: TxOutcome::Failed(_), .. }),
            "{event:?}"
        );
        assert!(!rig.ptt.is_keyed(), "nieudane key nie może zostawić nadajnika na antenie");
    }

    #[tokio::test]
    async fn the_kill_switch_refuses_without_losing_the_message() {
        let mut rig = rig();
        rig.interlocks.set_tx_enabled(false);

        rig.handle.transmit(request(7, Priority::Normal)).await.unwrap();
        let event = rig.handle.next_event().await.unwrap();

        assert!(
            matches!(event, SeqEvent::Refused { id: 7, reason: RefusalReason::TxDisabled }),
            "{event:?}"
        );
        assert!(rig.ptt.events().is_empty(), "PTT nie może drgnąć przy odmowie");
    }

    #[tokio::test]
    async fn poisoned_state_refuses_everything() {
        let mut rig = rig();
        rig.interlocks.set_poisoned(true);

        rig.handle.transmit(request(1, Priority::Emergency)).await.unwrap();
        let event = rig.handle.next_event().await.unwrap();

        assert!(
            matches!(event, SeqEvent::Refused { reason: RefusalReason::Poisoned, .. }),
            "nawet Emergency nie przechodzi przez poisoned: {event:?}"
        );
    }

    #[tokio::test]
    async fn quiet_hours_let_high_priority_through_and_stop_the_rest() {
        let mut rig = rig();
        rig.interlocks.set_quiet(true, Priority::High);

        rig.handle.transmit(request(1, Priority::Normal)).await.unwrap();
        let event = rig.handle.next_event().await.unwrap();
        assert!(
            matches!(event, SeqEvent::Refused { reason: RefusalReason::QuietHours, .. }),
            "{event:?}"
        );

        rig.handle.transmit(request(2, Priority::Emergency)).await.unwrap();
        let event = rig.handle.next_event().await.unwrap();
        assert!(matches!(event, SeqEvent::Started { id: 2 }), "{event:?}");
    }

    #[tokio::test]
    async fn a_higher_priority_interrupting_message_aborts_the_current_one() {
        let mut rig = rig();

        let mut long = request(1, Priority::Normal);
        long.body = Pcm::silence(48_000, Duration::from_secs(5));
        rig.handle.transmit(long).await.unwrap();
        let started = rig.handle.next_event().await.unwrap();
        assert!(matches!(started, SeqEvent::Started { id: 1 }));

        let mut urgent = request(2, Priority::Emergency);
        urgent.interrupting = true;
        rig.handle.transmit(urgent).await.unwrap();

        let aborted = rig.handle.next_event().await.unwrap();
        assert!(
            matches!(aborted, SeqEvent::Finished { id: 1, outcome: TxOutcome::Aborted, .. }),
            "{aborted:?}"
        );
        let started = rig.handle.next_event().await.unwrap();
        assert!(matches!(started, SeqEvent::Started { id: 2 }), "{started:?}");

        // Ani chwili z dwoma nadaniami: PTT opada przed ponownym podniesieniem.
        let events = rig.ptt.events();
        assert_eq!(
            events,
            vec![PttEvent::Key, PttEvent::Unkey, PttEvent::Key],
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_lower_priority_message_does_not_interrupt() {
        let mut rig = rig();

        let mut long = request(1, Priority::High);
        long.body = Pcm::silence(48_000, Duration::from_millis(400));
        rig.handle.transmit(long).await.unwrap();
        assert!(matches!(rig.handle.next_event().await.unwrap(), SeqEvent::Started { id: 1 }));

        let mut later = request(2, Priority::Low);
        later.interrupting = true;
        rig.handle.transmit(later).await.unwrap();

        let finished = rig.handle.next_event().await.unwrap();
        assert!(
            matches!(finished, SeqEvent::Finished { id: 1, outcome: TxOutcome::Ok, .. }),
            "nadanie o wyższym priorytecie musi dojść do końca: {finished:?}"
        );
    }

    #[tokio::test]
    async fn a_transmission_longer_than_the_limit_is_cut_and_reported_as_timeout() {
        let mut rig = guarded_rig(Duration::from_secs(1), false);
        let mut long = request(1, Priority::Normal);
        long.body = Pcm::silence(48_000, Duration::from_secs(3));
        long.id_audio = None;

        let began = Instant::now();
        rig.handle.transmit(long).await.unwrap();
        let event = finished(&mut rig.handle).await;
        let took = began.elapsed();

        let SeqEvent::Finished { id: 1, outcome, played } = event else {
            panic!("oczekiwano Finished, dostano {event:?}");
        };
        assert_eq!(outcome, TxOutcome::Failed(TX_TIMEOUT_ERROR.to_owned()));
        assert!(played < Duration::from_millis(1500), "grało {played:?} mimo limitu 1 s");
        assert!(took < Duration::from_millis(1500), "nadanie trwało {took:?} mimo limitu 1 s");
        assert!(!rig.ptt.is_keyed(), "po ucięciu PTT musi opaść");
        assert_eq!(rig.ptt.events(), vec![PttEvent::Key, PttEvent::Unkey]);
        assert!(!rig.marker.is_keyed());
        assert!(
            !rig.marker.is_poisoned(),
            "sekwencer uciął sam — strażnik nie miał powodu interweniować"
        );
        rig.watchdog.stop();
    }

    #[tokio::test]
    async fn the_watchdog_marker_sees_key_and_unkey() {
        let mut rig = guarded_rig(Duration::from_secs(10), false);

        rig.handle.transmit(request(1, Priority::Normal)).await.unwrap();
        let event = finished(&mut rig.handle).await;

        assert!(matches!(event, SeqEvent::Finished { outcome: TxOutcome::Ok, .. }), "{event:?}");
        assert_eq!(
            *rig.sink.keyed_while_playing.lock().unwrap(),
            vec![true],
            "w trakcie grania strażnik musi wiedzieć, że nadajemy"
        );
        assert!(!rig.marker.is_keyed(), "po nadaniu strażnik musi widzieć „nie nadaje”");
        rig.watchdog.stop();
    }

    #[tokio::test]
    async fn ptt_is_released_and_the_marker_cleared_when_playback_fails() {
        let mut rig = guarded_rig(Duration::from_secs(10), true);

        rig.handle.transmit(request(1, Priority::Normal)).await.unwrap();
        let event = finished(&mut rig.handle).await;

        assert!(
            matches!(&event, SeqEvent::Finished { outcome: TxOutcome::Failed(err), .. }
                if err != TX_TIMEOUT_ERROR),
            "{event:?}"
        );
        assert_eq!(*rig.sink.keyed_while_playing.lock().unwrap(), vec![true]);
        assert!(!rig.ptt.is_keyed());
        assert!(!rig.marker.is_keyed(), "błąd audio nie może zostawić znacznika podniesienia");
        rig.watchdog.stop();
    }

    #[tokio::test]
    async fn a_poisoned_watchdog_refuses_the_next_transmission_without_touching_ptt() {
        let ptt = Arc::new(FakePtt::new());
        let watchdog = Watchdog::spawn(ptt.clone(), Duration::from_millis(50));
        let marker = watchdog.marker();
        // Zawieszone nadanie spoza sekwencera: strażnik musi interweniować.
        ptt.key().unwrap();
        marker.mark_keyed();
        std::thread::sleep(Duration::from_millis(300));
        assert!(marker.is_poisoned());
        let before = ptt.events();

        // Interlocki jeszcze o niczym nie wiedzą (binarka przenosi flagę co
        // sekundę) — sekwencer i tak nie może nadać.
        let mut handle = Sequencer::spawn(
            ptt.clone(),
            Arc::new(FakeSink::new()),
            Arc::new(InMemoryInterlocks::permissive()),
            SequencerConfig {
                tx_tail: Duration::from_millis(1),
                tx_max_duration: Duration::from_secs(10),
                id_interval: Duration::from_secs(600),
            },
            Some(marker),
        );
        handle.transmit(request(1, Priority::Emergency)).await.unwrap();
        let event = handle.next_event().await.unwrap();

        assert!(
            matches!(event, SeqEvent::Refused { id: 1, reason: RefusalReason::Poisoned }),
            "{event:?}"
        );
        assert_eq!(ptt.events(), before, "PTT nie może drgnąć przy odmowie");
        watchdog.stop();
    }

    /// Wyjście, które się zawiesza: ignoruje anulowanie i termin, śpi długo
    /// po limicie — model zawieszonego sekwencera. Zapamiętuje, czy PTT było
    /// jeszcze podniesione, gdy w końcu wróciło.
    struct HangingSink {
        hang: Duration,
        ptt: Arc<FakePtt>,
        keyed_on_return: Mutex<Option<bool>>,
    }

    impl AudioSink for HangingSink {
        fn play(&self, _pcm: &Pcm, _cancel: &CancelToken) -> Result<PlayReport, SinkError> {
            std::thread::sleep(self.hang);
            *self.keyed_on_return.lock().unwrap() = Some(self.ptt.is_keyed());
            Ok(PlayReport { played: self.hang, cancelled: false })
        }
    }

    #[tokio::test]
    async fn a_hung_sequencer_is_forced_down_by_the_watchdog_and_the_next_transmission_refused() {
        let ptt = Arc::new(FakePtt::new());
        // Limit sekwencera 200 ms, watchdog 300 ms; sink wisi 1,5 s.
        let watchdog = Watchdog::spawn(ptt.clone(), Duration::from_millis(300));
        let marker = watchdog.marker();
        let sink = Arc::new(HangingSink {
            hang: Duration::from_millis(1500),
            ptt: ptt.clone(),
            keyed_on_return: Mutex::new(None),
        });
        let mut handle = Sequencer::spawn(
            ptt.clone(),
            sink.clone(),
            Arc::new(InMemoryInterlocks::permissive()),
            SequencerConfig {
                tx_tail: Duration::from_millis(1),
                tx_max_duration: Duration::from_millis(200),
                id_interval: Duration::from_secs(600),
            },
            Some(marker.clone()),
        );

        handle.transmit(request(1, Priority::Normal)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // Sekwencer wciąż wisi w sinku, a nadajnik już jest zdjęty z anteny.
        assert!(!ptt.is_keyed(), "watchdog musi opuścić PTT zawieszonego sekwencera");
        assert!(ptt.events().contains(&PttEvent::ForceUnkey), "{:?}", ptt.events());
        assert!(marker.is_poisoned());

        let event = finished(&mut handle).await;
        assert_eq!(*sink.keyed_on_return.lock().unwrap(), Some(false));
        assert!(
            matches!(&event, SeqEvent::Finished { id: 1, outcome: TxOutcome::Failed(err), .. }
                if err == TX_TIMEOUT_ERROR),
            "nadanie po terminie to timeout, nawet gdy sink udaje sukces: {event:?}"
        );

        let keys_before = ptt.events().iter().filter(|e| **e == PttEvent::Key).count();
        handle.transmit(request(2, Priority::Emergency)).await.unwrap();
        let event = handle.next_event().await.unwrap();
        assert!(
            matches!(event, SeqEvent::Refused { id: 2, reason: RefusalReason::Poisoned }),
            "{event:?}"
        );
        let keys_after = ptt.events().iter().filter(|e| **e == PttEvent::Key).count();
        assert_eq!(keys_before, keys_after, "po interwencji PTT nie może się podnieść");
        watchdog.stop();
    }

    #[tokio::test]
    async fn play_local_never_keys_and_is_bounded() {
        let rig = guarded_rig(Duration::from_secs(10), false);

        let began = Instant::now();
        let reply = rig.handle.play_local(Pcm::silence(48_000, Duration::from_secs(20))).unwrap();
        let played = reply.await.expect("sekwencer odpowiada").expect("odtworzenie udane");
        let took = began.elapsed();

        assert!(played <= Duration::from_millis(15_200), "grało {played:?} mimo limitu 15 s");
        // Dolna granica na zegarze ściennym: FakeSink śpi dłużej niż bufor,
        // więc liczba zagranych próbek bywa mniejsza niż upływ czasu.
        assert!(took >= Duration::from_secs(14), "trwało tylko {took:?}");
        assert!(took <= Duration::from_millis(15_500), "trwało {took:?}");
        assert!(!rig.ptt.events().contains(&PttEvent::Key), "{:?}", rig.ptt.events());
        assert_eq!(rig.ptt.events(), vec![PttEvent::Unkey], "test audio najpierw opuszcza PTT");
        assert_eq!(
            *rig.sink.keyed_while_playing.lock().unwrap(),
            vec![false],
            "test audio nie oznacza nadawania u strażnika"
        );
        assert!(!rig.marker.is_keyed());
        rig.watchdog.stop();
    }

    #[tokio::test]
    async fn abort_current_cuts_local_playback() {
        let rig = guarded_rig(Duration::from_secs(10), false);

        let reply = rig.handle.play_local(Pcm::silence(48_000, Duration::from_secs(10))).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        rig.handle.abort_current();
        let played = tokio::time::timeout(Duration::from_secs(1), reply)
            .await
            .expect("przerwane odtwarzanie wraca szybko")
            .unwrap()
            .unwrap();

        assert!(played < Duration::from_secs(1), "grało {played:?}");
        rig.watchdog.stop();
    }

    #[tokio::test]
    async fn a_transmission_queued_behind_shutdown_never_keys() {
        let mut rig = rig();
        let mut long = request(1, Priority::Normal);
        long.body = Pcm::silence(48_000, Duration::from_millis(300));
        rig.handle.transmit(long).await.unwrap();
        assert!(matches!(rig.handle.next_event().await.unwrap(), SeqEvent::Started { id: 1 }));
        rig.handle.transmit(request(2, Priority::Normal)).await.unwrap();

        rig.handle.shutdown();

        let keys = rig.ptt.events().iter().filter(|e| **e == PttEvent::Key).count();
        assert_eq!(keys, 1, "po zamknięciu sekwencer nie bierze kolejnych nadań: {:?}", rig.ptt.events());
        assert!(!rig.ptt.is_keyed());
    }
}
