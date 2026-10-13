use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::interlock::{Interlocks, RefusalReason};
use crate::priority::Priority;
use crate::ptt::PttBackend;
use crate::sink::{AudioSink, CancelToken};
use crate::synth::Pcm;

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
}

pub enum SeqCmd {
    Transmit(TransmitRequest),
    Shutdown,
}

/// Stan trwającego nadania, widoczny dla `SequencerHandle`, żeby umiał
/// zdecydować o przerwaniu.
type Current = Arc<std::sync::Mutex<Option<(Priority, CancelToken)>>>;

pub struct Sequencer;

pub struct SequencerHandle {
    cmd: mpsc::UnboundedSender<SeqCmd>,
    events: mpsc::UnboundedReceiver<SeqEvent>,
    current: Current,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Sequencer {
    pub fn spawn(
        ptt: Arc<dyn PttBackend>,
        sink: Arc<dyn AudioSink>,
        interlocks: Arc<dyn Interlocks>,
        config: SequencerConfig,
    ) -> SequencerHandle {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SeqCmd>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<SeqEvent>();
        let current: Current = Arc::new(std::sync::Mutex::new(None));

        let worker_current = current.clone();
        let thread = std::thread::Builder::new()
            .name("aars-sequencer".to_owned())
            .spawn(move || {
                while let Some(cmd) = cmd_rx.blocking_recv() {
                    match cmd {
                        SeqCmd::Shutdown => break,
                        SeqCmd::Transmit(request) => {
                            let id = request.id;
                            if let Err(reason) = interlocks.check(request.priority) {
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

        SequencerHandle { cmd: cmd_tx, events: event_rx, current, thread: Some(thread) }
    }
}

/// Strażnik RAII: każde wyjście z nadania — sukces, błąd, panika —
/// opuszcza PTT (spec §4.2).
struct PttGuard<'a> {
    ptt: &'a dyn PttBackend,
}

impl Drop for PttGuard<'_> {
    fn drop(&mut self) {
        if let Err(err) = self.ptt.unkey() {
            tracing::error!(%err, "unkey zawiódł, wymuszam opuszczenie PTT");
            self.ptt.force_unkey();
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
    let _guard = PttGuard { ptt };
    let _ = event_tx.send(SeqEvent::Started { id: request.id });

    let started = Instant::now();
    std::thread::sleep(config.tx_tail);

    let report = match sink.play(&full, cancel) {
        Ok(report) => report,
        Err(err) => {
            tracing::error!(%err, id = request.id, "błąd wyjścia audio");
            return (TxOutcome::Failed(err.to_string()), started.elapsed());
        }
    };

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

    pub async fn next_event(&mut self) -> Option<SeqEvent> {
        self.events.recv().await
    }

    pub fn shutdown(mut self) {
        let _ = self.cmd.send(SeqCmd::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interlock::InMemoryInterlocks;
    use crate::ptt::{FakePtt, PttEvent};
    use crate::sink::FakeSink;
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
        );
        Rig { handle, ptt, interlocks }
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
}
