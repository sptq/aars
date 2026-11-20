use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::ptt::PttBackend;

/// Okres sprawdzania. Krótki na tyle, by przekroczenie limitu kończyło się
/// opuszczeniem PTT z dokładnością do ułamka sekundy.
const TICK: Duration = Duration::from_millis(50);

struct Shared {
    /// Punkt odniesienia dla wszystkich pomiarów — zegar monotoniczny.
    started: Instant,
    /// Milisekundy od `started` do podniesienia PTT, plus jeden. Zero znaczy
    /// „nie nadaje", dlatego przesunięcie: nadanie rozpoczęte w pierwszej
    /// milisekundzie też musi być widoczne.
    keyed_at_ms: AtomicU64,
    poisoned: AtomicBool,
    stop: AtomicBool,
}

/// Treść błędu, którym kończy się nadanie ucięte twardym limitem czasu.
/// Dyspozytor rozpoznaje po niej wynik `timeout` w `tx_log`.
pub const TX_TIMEOUT_ERROR: &str = "przekroczony twardy limit czasu nadawania";

/// Niezależny strażnik czasu nadawania. Osobny wątek OS i monotoniczny
/// zegar, bo musi opuścić PTT nawet gdy runtime tokio stanie (spec D7).
pub struct Watchdog;

pub struct WatchdogHandle {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// Lekki uchwyt do stanu strażnika, bez jego wątku. Sekwencer znaczy nim
/// podniesienie i opuszczenie PTT, binarka i panel czytają i zdejmują nim
/// flagę `poisoned`. Klony współdzielą stan z `WatchdogHandle`.
#[derive(Clone)]
pub struct WatchdogMarker {
    shared: Arc<Shared>,
}

impl Watchdog {
    pub fn spawn(ptt: Arc<dyn PttBackend>, limit: Duration) -> WatchdogHandle {
        let shared = Arc::new(Shared {
            started: Instant::now(),
            keyed_at_ms: AtomicU64::new(0),
            poisoned: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });

        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("aars-watchdog".to_owned())
            .spawn(move || {
                while !worker.stop.load(Ordering::SeqCst) {
                    let keyed_at = worker.keyed_at_ms.load(Ordering::SeqCst);
                    if keyed_at > 0 {
                        let now_ms = worker.started.elapsed().as_millis() as u64;
                        let elapsed = now_ms.saturating_sub(keyed_at - 1);
                        if Duration::from_millis(elapsed) > limit {
                            ptt.force_unkey();
                            worker.keyed_at_ms.store(0, Ordering::SeqCst);
                            worker.poisoned.store(true, Ordering::SeqCst);
                            tracing::error!(
                                limit_ms = limit.as_millis(),
                                elapsed_ms = elapsed,
                                "watchdog opuścił PTT po przekroczeniu limitu czasu nadawania"
                            );
                        }
                    }
                    std::thread::sleep(TICK);
                }
            })
            .expect("uruchomienie wątku watchdoga");

        WatchdogHandle { shared, thread: Some(thread) }
    }
}

impl WatchdogMarker {
    /// Wołane przez sekwencer bezpośrednio po `key()`.
    pub fn mark_keyed(&self) {
        let marker = self.shared.started.elapsed().as_millis() as u64 + 1;
        self.shared.keyed_at_ms.store(marker, Ordering::SeqCst);
    }

    /// Wołane przez sekwencer bezpośrednio po `unkey()`.
    pub fn mark_unkeyed(&self) {
        self.shared.keyed_at_ms.store(0, Ordering::SeqCst);
    }

    /// Czy strażnik uważa, że nadajnik jest na antenie.
    pub fn is_keyed(&self) -> bool {
        self.shared.keyed_at_ms.load(Ordering::SeqCst) > 0
    }

    pub fn is_poisoned(&self) -> bool {
        self.shared.poisoned.load(Ordering::SeqCst)
    }

    /// Zdejmuje lepką flagę. Wywoływane wyłącznie z panelu, przez człowieka.
    pub fn clear_poison(&self) {
        self.shared.poisoned.store(false, Ordering::SeqCst);
        tracing::warn!("flaga poisoned zdjęta ręcznie");
    }
}

impl WatchdogHandle {
    /// Uchwyt dla sekwencera i panelu; wątek strażnika zostaje tutaj.
    pub fn marker(&self) -> WatchdogMarker {
        WatchdogMarker { shared: self.shared.clone() }
    }

    pub fn mark_keyed(&self) {
        self.marker().mark_keyed();
    }

    pub fn mark_unkeyed(&self) {
        self.marker().mark_unkeyed();
    }

    pub fn is_poisoned(&self) -> bool {
        self.marker().is_poisoned()
    }

    pub fn clear_poison(&self) {
        self.marker().clear_poison();
    }

    pub fn stop(mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for WatchdogHandle {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptt::{FakePtt, PttEvent};

    #[test]
    fn an_overrunning_transmission_is_forced_down_and_poisons_the_sequencer() {
        let ptt = Arc::new(FakePtt::new());
        let handle = Watchdog::spawn(ptt.clone(), Duration::from_millis(100));

        ptt.key().unwrap();
        handle.mark_keyed();
        std::thread::sleep(Duration::from_millis(400));

        assert!(ptt.events().contains(&PttEvent::ForceUnkey), "{:?}", ptt.events());
        assert!(!ptt.is_keyed());
        assert!(handle.is_poisoned(), "interwencja watchdoga musi zostawić lepką flagę");

        handle.stop();
    }

    #[test]
    fn a_transmission_within_the_limit_is_left_alone() {
        let ptt = Arc::new(FakePtt::new());
        let handle = Watchdog::spawn(ptt.clone(), Duration::from_millis(500));

        ptt.key().unwrap();
        handle.mark_keyed();
        std::thread::sleep(Duration::from_millis(100));
        handle.mark_unkeyed();
        ptt.unkey().unwrap();
        std::thread::sleep(Duration::from_millis(300));

        assert!(!ptt.events().contains(&PttEvent::ForceUnkey), "{:?}", ptt.events());
        assert!(!handle.is_poisoned());

        handle.stop();
    }

    #[test]
    fn poison_must_be_cleared_by_hand() {
        let ptt = Arc::new(FakePtt::new());
        let handle = Watchdog::spawn(ptt.clone(), Duration::from_millis(50));

        ptt.key().unwrap();
        handle.mark_keyed();
        std::thread::sleep(Duration::from_millis(250));
        assert!(handle.is_poisoned());

        // Samo opuszczenie PTT nie wystarcza — potrzebna jest decyzja człowieka.
        handle.mark_unkeyed();
        std::thread::sleep(Duration::from_millis(100));
        assert!(handle.is_poisoned(), "flaga nie może zniknąć sama");

        handle.clear_poison();
        assert!(!handle.is_poisoned());

        handle.stop();
    }

    #[test]
    fn a_stuck_key_marked_through_the_marker_is_forced_down_and_poisons() {
        let ptt = Arc::new(FakePtt::new());
        let handle = Watchdog::spawn(ptt.clone(), Duration::from_millis(100));
        let marker = handle.marker();

        ptt.key().unwrap();
        marker.mark_keyed();
        assert!(marker.is_keyed());
        std::thread::sleep(Duration::from_millis(400));

        assert!(!ptt.is_keyed(), "{:?}", ptt.events());
        assert!(marker.is_poisoned(), "marker musi widzieć flagę strażnika");
        assert!(handle.is_poisoned());
        assert!(!marker.is_keyed(), "po interwencji strażnik uznaje PTT za opuszczone");

        marker.clear_poison();
        assert!(!handle.is_poisoned(), "zdjęcie flagi markerem dotyczy strażnika");

        handle.stop();
    }
}
