# Etap 3: Nadawanie — PTT, sekwencer, watchdog, identyfikacja

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Faktyczne wejście na pasmo: sterowanie PTT trzema backendami, jeden wyłączny sekwencer, watchdog twardego limitu czasu, automatyczna identyfikacja stacji i interlocki bezpieczeństwa.

**Architecture:** Sekwencer jest jedynym właścicielem `AudioSink` i `PttBackend` (spec D2) i dostaje **gotowe PCM**, nie tekst — synteza została w etapie 2 i sekwencer o niej nie wie. Każde wyjście z nadania przechodzi przez strażnik RAII opuszczający PTT, więc nie istnieje ścieżka zostawiająca nadajnik na antenie. Watchdog żyje w osobnym wątku OS z monotonicznym zegarem (spec D7) i ma własny uchwyt do PTT.

**Tech Stack:** gpiocdev 0.8 (Linux), serialport 4.10, cpal 0.18, tokio sync.

**Spec:** `docs/superpowers/specs/2026-10-13-aars-design.md`

## Global Constraints

- Wyłączny aktor-sekwencer posiada audio i PTT; komunikacja tylko przez `mpsc` (spec D2, §4.2).
- `force_unkey` jest synchroniczne, infallible i bezpieczne z obcego wątku; sprowadza się do jednego zapisu do deskryptora (spec §3.1).
- Kolejność nadania: `key()` → `tx_tail_ms` → audio → `tx_tail_ms` → `unkey()` → wpis do `tx_log` (spec §4.2).
- Interlocki w kolejności: `poisoned` → kill switch → wejście blokujące → okna ciszy → `channel_busy` (spec §4.4). Odmowa nie gubi komunikatu.
- Flaga `poisoned` po interwencji watchdoga jest lepka i wymaga ręcznego zdjęcia (spec §4.3).
- Domyślny limit czasu TX: 180 s, konfigurowalny (spec §4.3).
- Znak wywoławczy na początku i końcu każdego nadania, a przy nadaniu dłuższym niż `id_interval` także w środku (spec §4.5).
- Priorytety: `Emergency = 0, High = 1, Normal = 2, Low = 3`; niższa liczba znaczy wyższy priorytet (spec §5.1).
- Backendy zależne od Linuksa za `#[cfg(target_os = "linux")]`; testy chodzą na fake'ach na macOS i Linuksie (spec §11).
- TDD: test przed implementacją, commit po każdym zielonym cyklu.

---

### Task 1: Trait `PttBackend`, `AudioSink` i fake'i

**Files:**
- Create: `crates/aars-core/src/ptt.rs`
- Create: `crates/aars-core/src/sink.rs`
- Create: `crates/aars-core/src/priority.rs`
- Modify: `crates/aars-core/src/lib.rs`

**Interfaces:**
- Consumes: `aars_core::synth::Pcm`
- Produces: `aars_core::priority::Priority`; `aars_core::ptt::{PttBackend, PttError, FakePtt, PttEvent}`; `aars_core::sink::{AudioSink, SinkError, PlayReport, CancelToken, FakeSink}`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-core/src/priority.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_number_means_higher_priority() {
        assert!(Priority::Emergency.outranks(Priority::High));
        assert!(Priority::High.outranks(Priority::Normal));
        assert!(!Priority::Normal.outranks(Priority::Normal));
        assert!(!Priority::Low.outranks(Priority::Emergency));
    }

    #[test]
    fn priority_round_trips_through_its_database_representation() {
        for p in [Priority::Emergency, Priority::High, Priority::Normal, Priority::Low] {
            assert_eq!(Priority::from_i64(p as i64), Some(p));
        }
        assert_eq!(Priority::from_i64(7), None);
    }
}
```

`crates/aars-core/src/ptt.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_ptt_records_every_transition() {
        let ptt = FakePtt::new();
        ptt.key().unwrap();
        ptt.unkey().unwrap();
        ptt.key().unwrap();
        ptt.force_unkey();

        assert_eq!(
            ptt.events(),
            vec![PttEvent::Key, PttEvent::Unkey, PttEvent::Key, PttEvent::ForceUnkey]
        );
        assert!(!ptt.is_keyed(), "po force_unkey PTT musi być opuszczone");
    }

    #[test]
    fn fake_ptt_can_be_told_to_fail() {
        let ptt = FakePtt::new();
        ptt.fail_next_key();
        assert!(matches!(ptt.key(), Err(PttError::Hardware(_))));
        assert!(!ptt.is_keyed(), "nieudane podniesienie nie może zostawić stanu keyed");
    }

    #[test]
    fn channel_busy_is_unknown_by_default() {
        assert_eq!(FakePtt::new().channel_busy(), None);
    }
}
```

`crates/aars-core/src/sink.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::Pcm;
    use std::time::Duration;

    #[test]
    fn fake_sink_reports_what_it_played() {
        let sink = FakeSink::new();
        let pcm = Pcm::silence(48_000, Duration::from_millis(500));

        let report = sink.play(&pcm, &CancelToken::new()).unwrap();

        assert!(!report.cancelled);
        assert_eq!(report.played, Duration::from_millis(500));
        assert_eq!(sink.played_samples(), 24_000);
    }

    #[test]
    fn a_cancelled_token_stops_playback_early() {
        let sink = FakeSink::new();
        let pcm = Pcm::silence(48_000, Duration::from_secs(10));
        let cancel = CancelToken::new();
        cancel.cancel();

        let report = sink.play(&pcm, &cancel).unwrap();

        assert!(report.cancelled);
        assert!(report.played < Duration::from_secs(1), "odtworzono {:?}", report.played);
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-core`
Expected: FAIL — `cannot find type Priority`, `cannot find type FakePtt`, `cannot find type FakeSink`

- [ ] **Step 3: Zaimplementuj priorytety**

`crates/aars-core/src/priority.rs` — nad blokiem testów:

```rust
use serde::{Deserialize, Serialize};

/// Niższa liczba znaczy wyższy priorytet — tak samo w bazie i w kolejce,
/// żeby `ORDER BY priority` i porównania w kodzie zgadzały się bez konwersji.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(i64)]
pub enum Priority {
    Emergency = 0,
    High = 1,
    Normal = 2,
    Low = 3,
}

impl Priority {
    /// Czy `self` jest ściśle wyższym priorytetem niż `other`.
    pub fn outranks(self, other: Priority) -> bool {
        (self as i64) < (other as i64)
    }

    pub fn from_i64(value: i64) -> Option<Self> {
        match value {
            0 => Some(Self::Emergency),
            1 => Some(Self::High),
            2 => Some(Self::Normal),
            3 => Some(Self::Low),
            _ => None,
        }
    }
}
```

- [ ] **Step 4: Zaimplementuj `PttBackend` i `FakePtt`**

`crates/aars-core/src/ptt.rs` — nad blokiem testów:

```rust
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, thiserror::Error)]
pub enum PttError {
    #[error("błąd sprzętu PTT: {0}")]
    Hardware(String),
    #[error("backend PTT niedostępny na tej platformie: {0}")]
    Unsupported(String),
}

pub trait PttBackend: Send + Sync {
    /// Podnosi PTT — radio przechodzi na nadawanie.
    fn key(&self) -> Result<(), PttError>;

    /// Opuszcza PTT.
    fn unkey(&self) -> Result<(), PttError>;

    /// Awaryjne opuszczenie PTT. Synchroniczne, infallible i bezpieczne z
    /// obcego wątku — watchdog nie może czekać, logować ani alokować.
    fn force_unkey(&self);

    /// Czy kanał jest zajęty. `None` znaczy „nie wiem" i w MVP zwracają to
    /// wszystkie backendy (spec §4.4).
    fn channel_busy(&self) -> Option<bool> {
        None
    }

    /// Nazwa backendu do dziennika nadań.
    fn name(&self) -> &'static str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttEvent {
    Key,
    Unkey,
    ForceUnkey,
}

/// PTT testowe: zapisuje przejścia, umie udawać awarię sprzętu.
pub struct FakePtt {
    keyed: AtomicBool,
    fail_next_key: AtomicBool,
    events: Mutex<Vec<PttEvent>>,
}

impl FakePtt {
    pub fn new() -> Self {
        Self {
            keyed: AtomicBool::new(false),
            fail_next_key: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<PttEvent> {
        self.events.lock().expect("mutex fake'a").clone()
    }

    pub fn is_keyed(&self) -> bool {
        self.keyed.load(Ordering::SeqCst)
    }

    pub fn fail_next_key(&self) {
        self.fail_next_key.store(true, Ordering::SeqCst);
    }
}

impl Default for FakePtt {
    fn default() -> Self {
        Self::new()
    }
}

impl PttBackend for FakePtt {
    fn key(&self) -> Result<(), PttError> {
        if self.fail_next_key.swap(false, Ordering::SeqCst) {
            return Err(PttError::Hardware("awaria wymuszona w teście".to_owned()));
        }
        self.keyed.store(true, Ordering::SeqCst);
        self.events.lock().expect("mutex fake'a").push(PttEvent::Key);
        Ok(())
    }

    fn unkey(&self) -> Result<(), PttError> {
        self.keyed.store(false, Ordering::SeqCst);
        self.events.lock().expect("mutex fake'a").push(PttEvent::Unkey);
        Ok(())
    }

    fn force_unkey(&self) {
        self.keyed.store(false, Ordering::SeqCst);
        if let Ok(mut events) = self.events.lock() {
            events.push(PttEvent::ForceUnkey);
        }
    }

    fn name(&self) -> &'static str {
        "fake"
    }
}
```

- [ ] **Step 5: Zaimplementuj `AudioSink` i `FakeSink`**

`crates/aars-core/src/sink.rs` — nad blokiem testów:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use crate::synth::Pcm;

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("błąd urządzenia audio: {0}")]
    Device(String),
    #[error("urządzenie oczekuje {expected} Hz, dostało {got} Hz")]
    SampleRateMismatch { expected: u32, got: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayReport {
    pub played: Duration,
    pub cancelled: bool,
}

/// Token anulowania sprawdzany co bufor podczas odtwarzania. Przerwanie
/// nadania musi być natychmiastowe, a nie „po skończeniu pliku".
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub trait AudioSink: Send + Sync {
    fn play(&self, pcm: &Pcm, cancel: &CancelToken) -> Result<PlayReport, SinkError>;
}

/// Wyjście testowe: liczy próbki, honoruje anulowanie, nie dotyka sprzętu.
pub struct FakeSink {
    played: AtomicUsize,
}

impl FakeSink {
    pub fn new() -> Self {
        Self { played: AtomicUsize::new(0) }
    }

    pub fn played_samples(&self) -> usize {
        self.played.load(Ordering::SeqCst)
    }
}

impl Default for FakeSink {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSink for FakeSink {
    fn play(&self, pcm: &Pcm, cancel: &CancelToken) -> Result<PlayReport, SinkError> {
        // Bufor 1024 próbek, tak jak realne wyjście — anulowanie ma tę samą
        // ziarnistość w teście i na sprzęcie.
        const CHUNK: usize = 1024;
        let mut played = 0usize;

        for _ in pcm.samples.chunks(CHUNK) {
            if cancel.is_cancelled() {
                return Ok(PlayReport {
                    played: Duration::from_secs_f64(played as f64 / pcm.sample_rate as f64),
                    cancelled: true,
                });
            }
            let step = CHUNK.min(pcm.samples.len() - played);
            played += step;
            self.played.fetch_add(step, Ordering::SeqCst);
        }

        Ok(PlayReport {
            played: Duration::from_secs_f64(played as f64 / pcm.sample_rate as f64),
            cancelled: false,
        })
    }
}
```

`crates/aars-core/src/lib.rs`:

```rust
pub mod config;
pub mod db;
pub mod error;
pub mod priority;
pub mod ptt;
pub mod sink;
pub mod synth;
```

- [ ] **Step 6: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core`
Expected: PASS — osiemnaście testów

- [ ] **Step 7: Commit**

```bash
git add crates/aars-core
git commit -m "feat(core): traity PTT i AudioSink, priorytety, implementacje testowe"
```

---

### Task 2: Watchdog twardego limitu

**Files:**
- Create: `crates/aars-core/src/watchdog.rs`
- Modify: `crates/aars-core/src/lib.rs`

**Interfaces:**
- Consumes: `aars_core::ptt::PttBackend`
- Produces: `aars_core::watchdog::{Watchdog, WatchdogHandle}`; `Watchdog::spawn(ptt: Arc<dyn PttBackend>, limit: Duration) -> WatchdogHandle`; `WatchdogHandle::{mark_keyed, mark_unkeyed, is_poisoned, clear_poison, stop}`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-core/src/watchdog.rs`:

```rust
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
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-core watchdog`
Expected: FAIL — `cannot find type Watchdog`

- [ ] **Step 3: Zaimplementuj watchdog**

`crates/aars-core/src/watchdog.rs` — nad blokiem testów:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::ptt::PttBackend;

/// Okres sprawdzania. Krótki na tyle, by przekroczenie limitu kończyło się
/// opuszczeniem PTT z dokładnością do ułamka sekundy.
const TICK: Duration = Duration::from_millis(50);

struct Shared {
    /// Milisekundy od startu watchdoga do podniesienia PTT; 0 znaczy „nie nadaje".
    keyed_at_ms: AtomicU64,
    poisoned: AtomicBool,
    stop: AtomicBool,
}

/// Niezależny strażnik czasu nadawania. Osobny wątek OS i monotoniczny
/// zegar, bo musi opuścić PTT nawet gdy runtime tokio stanie (spec D7).
pub struct Watchdog;

pub struct WatchdogHandle {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    pub fn spawn(ptt: Arc<dyn PttBackend>, limit: Duration) -> WatchdogHandle {
        let shared = Arc::new(Shared {
            keyed_at_ms: AtomicU64::new(0),
            poisoned: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });

        let worker = shared.clone();
        let started = Instant::now();
        let thread = std::thread::Builder::new()
            .name("aars-watchdog".to_owned())
            .spawn(move || {
                while !worker.stop.load(Ordering::SeqCst) {
                    let keyed_at = worker.keyed_at_ms.load(Ordering::SeqCst);
                    if keyed_at > 0 {
                        let elapsed =
                            started.elapsed().as_millis() as u64.saturating_sub(keyed_at);
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

impl WatchdogHandle {
    /// Wołane przez sekwencer bezpośrednio po `key()`.
    pub fn mark_keyed(&self) {
        // Wartość musi być niezerowa, bo 0 znaczy „nie nadaje".
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(1);
        let _ = now;
        self.shared.keyed_at_ms.store(elapsed_marker(), Ordering::SeqCst);
    }

    /// Wołane przez sekwencer bezpośrednio po `unkey()`.
    pub fn mark_unkeyed(&self) {
        self.shared.keyed_at_ms.store(0, Ordering::SeqCst);
    }

    pub fn is_poisoned(&self) -> bool {
        self.shared.poisoned.load(Ordering::SeqCst)
    }

    /// Zdejmuje lepką flagę. Wywoływane wyłącznie z panelu, przez człowieka.
    pub fn clear_poison(&self) {
        self.shared.poisoned.store(false, Ordering::SeqCst);
        tracing::warn!("flaga poisoned zdjęta ręcznie");
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
```

**Uwaga dla wykonawcy:** powyższy `mark_keyed` i pętla wątku muszą mierzyć
czas tym samym `Instant`. Zamiast funkcji `elapsed_marker()` przechowaj
`Arc<Instant>` w `Shared` i w `mark_keyed` zapisz
`started.elapsed().as_millis() as u64 + 1` (plus jeden, żeby wartość nigdy
nie była zerowa w pierwszej milisekundzie). W pętli policz
`started.elapsed().as_millis() as u64 - (keyed_at - 1)`. Napisz to tak, żeby
kompilowało się bez `as u64.saturating_sub` — ten zapis w szkicu jest
niepoprawny składniowo i trzeba go rozpisać na dwie linie.

`crates/aars-core/src/lib.rs` — dopisz `pub mod watchdog;`

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core watchdog`
Expected: PASS — trzy testy

- [ ] **Step 5: Commit**

```bash
git add crates/aars-core
git commit -m "feat(core): watchdog twardego limitu czasu nadawania"
```

---

### Task 3: Identyfikacja stacji — głos i CW

**Files:**
- Create: `crates/aars-core/src/ident.rs`
- Modify: `crates/aars-core/src/lib.rs`

**Interfaces:**
- Consumes: `aars_core::synth::Pcm`
- Produces: `aars_core::ident::{IdMode, morse_pcm, MorseParams}`; `morse_pcm(callsign: &str, params: &MorseParams, sample_rate: u32) -> Pcm`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-core/src/ident.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn morse_produces_audio_whose_length_matches_the_callsign() {
        let params = MorseParams::default();
        let short = morse_pcm("E", &params, 48_000);   // jedna kropka
        let long = morse_pcm("SQ2IPS", &params, 48_000);

        assert!(short.samples.len() > 0);
        assert!(long.samples.len() > short.samples.len() * 5, "dłuższy znak to dłuższe audio");
        assert_eq!(long.sample_rate, 48_000);
    }

    #[test]
    fn morse_timing_follows_wpm() {
        let fast = morse_pcm("SQ2IPS", &MorseParams { wpm: 30, ..Default::default() }, 48_000);
        let slow = morse_pcm("SQ2IPS", &MorseParams { wpm: 15, ..Default::default() }, 48_000);

        let ratio = slow.samples.len() as f64 / fast.samples.len() as f64;
        assert!((ratio - 2.0).abs() < 0.1, "dwukrotnie niższe WPM to dwa razy dłuższe audio, ratio {ratio}");
    }

    #[test]
    fn morse_contains_both_tone_and_silence() {
        let pcm = morse_pcm("A", &MorseParams::default(), 48_000);
        assert!(pcm.samples.iter().any(|s| s.unsigned_abs() > 1000), "brak tonu");
        assert!(pcm.samples.iter().any(|s| *s == 0), "brak przerw między elementami");
    }

    #[test]
    fn unknown_characters_are_skipped_rather_than_panicking() {
        let pcm = morse_pcm("SQ2IPS!@#", &MorseParams::default(), 48_000);
        let plain = morse_pcm("SQ2IPS", &MorseParams::default(), 48_000);
        assert_eq!(pcm.samples.len(), plain.samples.len());
    }

    #[test]
    fn an_empty_callsign_produces_no_audio() {
        assert!(morse_pcm("", &MorseParams::default(), 48_000).samples.is_empty());
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-core ident`
Expected: FAIL — `cannot find function morse_pcm`

- [ ] **Step 3: Zaimplementuj identyfikację**

`crates/aars-core/src/ident.rs` — nad blokiem testów:

```rust
use serde::{Deserialize, Serialize};

use crate::synth::Pcm;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdMode {
    /// Znak wywoławczy wypowiedziany przez TTS.
    Voice,
    /// Znak wywoławczy nadany telegrafią.
    Cw,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MorseParams {
    pub wpm: u32,
    pub tone_hz: f32,
    pub amplitude: f32,
}

impl Default for MorseParams {
    fn default() -> Self {
        Self { wpm: 20, tone_hz: 800.0, amplitude: 0.5 }
    }
}

const MORSE: &[(char, &str)] = &[
    ('A', ".-"), ('B', "-..."), ('C', "-.-."), ('D', "-.."), ('E', "."),
    ('F', "..-."), ('G', "--."), ('H', "...."), ('I', ".."), ('J', ".---"),
    ('K', "-.-"), ('L', ".-.."), ('M', "--"), ('N', "-."), ('O', "---"),
    ('P', ".--."), ('Q', "--.-"), ('R', ".-."), ('S', "..."), ('T', "-"),
    ('U', "..-"), ('V', "...-"), ('W', ".--"), ('X', "-..-"), ('Y', "-.--"),
    ('Z', "--.."),
    ('0', "-----"), ('1', ".----"), ('2', "..---"), ('3', "...--"),
    ('4', "....-"), ('5', "....."), ('6', "-...."), ('7', "--..."),
    ('8', "---.."), ('9', "----."),
    ('/', "-..-."),
];

/// Generuje znak wywoławczy telegrafią. Długość kropki to standardowe
/// 1200/WPM milisekund, odstęp między elementami kropka, między literami
/// trzy kropki.
pub fn morse_pcm(callsign: &str, params: &MorseParams, sample_rate: u32) -> Pcm {
    let dot_ms = 1200.0 / params.wpm as f64;
    let dot_samples = (dot_ms / 1000.0 * sample_rate as f64).round() as usize;

    let mut samples: Vec<i16> = Vec::new();
    let mut first_letter = true;

    for ch in callsign.to_uppercase().chars() {
        let Some((_, pattern)) = MORSE.iter().find(|(c, _)| *c == ch) else {
            continue;
        };

        if !first_letter {
            // Odstęp międzyliterowy: trzy kropki, z czego jedna już jest po
            // ostatnim elemencie poprzedniej litery.
            samples.extend(std::iter::repeat_n(0i16, dot_samples * 2));
        }
        first_letter = false;

        for (i, element) in pattern.chars().enumerate() {
            if i > 0 {
                samples.extend(std::iter::repeat_n(0i16, dot_samples));
            }
            let length = match element {
                '-' => dot_samples * 3,
                _ => dot_samples,
            };
            append_tone(&mut samples, length, params, sample_rate);
        }
        // Przerwa po literze — dopełnia odstęp międzyliterowy.
        samples.extend(std::iter::repeat_n(0i16, dot_samples));
    }

    Pcm { samples, sample_rate }
}

/// Dokleja ton z łagodnymi zboczami — trzask na włączeniu nośnej brzmi
/// źle i rozmazuje widmo.
fn append_tone(out: &mut Vec<i16>, length: usize, params: &MorseParams, sample_rate: u32) {
    let ramp = (sample_rate as usize / 500).min(length / 2).max(1);
    let peak = params.amplitude.clamp(0.0, 1.0) * 32_767.0;

    for i in 0..length {
        let t = i as f32 / sample_rate as f32;
        let envelope = if i < ramp {
            i as f32 / ramp as f32
        } else if i >= length - ramp {
            (length - i) as f32 / ramp as f32
        } else {
            1.0
        };
        let value = f32::sin(t * 2.0 * std::f32::consts::PI * params.tone_hz) * peak * envelope;
        out.push(value as i16);
    }
}
```

`crates/aars-core/src/lib.rs` — dopisz `pub mod ident;`

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core ident`
Expected: PASS — pięć testów

- [ ] **Step 5: Commit**

```bash
git add crates/aars-core
git commit -m "feat(core): identyfikacja stacji telegrafią"
```

---

### Task 4: Sekwencer i interlocki

**Files:**
- Create: `crates/aars-core/src/sequencer.rs`
- Create: `crates/aars-core/src/interlock.rs`
- Modify: `crates/aars-core/src/lib.rs`

**Interfaces:**
- Consumes: `aars_core::ptt::PttBackend`, `aars_core::sink::{AudioSink, CancelToken}`, `aars_core::synth::Pcm`, `aars_core::priority::Priority`, `aars_core::watchdog::WatchdogHandle`
- Produces: `aars_core::interlock::{Interlocks, InterlockState, RefusalReason, InMemoryInterlocks}`; `aars_core::sequencer::{Sequencer, SequencerHandle, SeqCmd, SeqEvent, TransmitRequest, TxOutcome, SequencerConfig}`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-core/src/sequencer.rs`:

```rust
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
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-core sequencer`
Expected: FAIL — `cannot find type Sequencer`

- [ ] **Step 3: Zaimplementuj interlocki**

`crates/aars-core/src/interlock.rs`:

```rust
use std::sync::Mutex;

use crate::priority::Priority;

/// Dlaczego sekwencer odmówił nadania. Odmowa nie gubi komunikatu — wraca
/// do kolejki z tym powodem widocznym w panelu (spec §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// Watchdog musiał interweniować; wymagana decyzja człowieka.
    Poisoned,
    /// Globalny kill switch.
    TxDisabled,
    /// Fizyczne wejście blokujące.
    Inhibited,
    /// Okno ciszy, a priorytet za niski.
    QuietHours,
    /// Kanał zajęty (w MVP nieosiągalne — backendy zwracają `None`).
    ChannelBusy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterlockState {
    pub poisoned: bool,
    pub tx_enabled: bool,
    pub inhibited: bool,
    pub quiet_now: bool,
    pub quiet_min_priority: Priority,
}

pub trait Interlocks: Send + Sync {
    fn snapshot(&self) -> InterlockState;

    /// Sprawdza wszystkie blokady w kolejności z §4.4 speca.
    fn check(&self, priority: Priority) -> Result<(), RefusalReason> {
        let state = self.snapshot();
        if state.poisoned {
            return Err(RefusalReason::Poisoned);
        }
        if !state.tx_enabled {
            return Err(RefusalReason::TxDisabled);
        }
        if state.inhibited {
            return Err(RefusalReason::Inhibited);
        }
        if state.quiet_now && state.quiet_min_priority.outranks(priority) {
            return Err(RefusalReason::QuietHours);
        }
        Ok(())
    }
}

/// Interlocki trzymane w pamięci: do testów i jako cache stanu z bazy.
pub struct InMemoryInterlocks {
    state: Mutex<InterlockState>,
}

impl InMemoryInterlocks {
    /// Stan, w którym wszystko wolno — punkt wyjścia dla testów.
    pub fn permissive() -> Self {
        Self {
            state: Mutex::new(InterlockState {
                poisoned: false,
                tx_enabled: true,
                inhibited: false,
                quiet_now: false,
                quiet_min_priority: Priority::Emergency,
            }),
        }
    }

    pub fn set_tx_enabled(&self, value: bool) {
        self.state.lock().expect("mutex interlocków").tx_enabled = value;
    }

    pub fn set_poisoned(&self, value: bool) {
        self.state.lock().expect("mutex interlocków").poisoned = value;
    }

    pub fn set_inhibited(&self, value: bool) {
        self.state.lock().expect("mutex interlocków").inhibited = value;
    }

    pub fn set_quiet(&self, quiet_now: bool, min_priority: Priority) {
        let mut state = self.state.lock().expect("mutex interlocków");
        state.quiet_now = quiet_now;
        state.quiet_min_priority = min_priority;
    }
}

impl Interlocks for InMemoryInterlocks {
    fn snapshot(&self) -> InterlockState {
        *self.state.lock().expect("mutex interlocków")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interlocks_are_checked_in_the_order_from_the_spec() {
        let locks = InMemoryInterlocks::permissive();
        assert!(locks.check(Priority::Low).is_ok());

        // Poisoned bije wszystko, także Emergency.
        locks.set_poisoned(true);
        locks.set_tx_enabled(false);
        assert_eq!(locks.check(Priority::Emergency), Err(RefusalReason::Poisoned));

        locks.set_poisoned(false);
        assert_eq!(locks.check(Priority::Emergency), Err(RefusalReason::TxDisabled));

        locks.set_tx_enabled(true);
        locks.set_inhibited(true);
        assert_eq!(locks.check(Priority::Emergency), Err(RefusalReason::Inhibited));
    }

    #[test]
    fn quiet_hours_compare_priorities_by_rank_not_by_number() {
        let locks = InMemoryInterlocks::permissive();
        locks.set_quiet(true, Priority::High);

        assert_eq!(locks.check(Priority::Normal), Err(RefusalReason::QuietHours));
        assert_eq!(locks.check(Priority::Low), Err(RefusalReason::QuietHours));
        assert!(locks.check(Priority::High).is_ok(), "próg jest włączny");
        assert!(locks.check(Priority::Emergency).is_ok());
    }
}
```

- [ ] **Step 4: Zaimplementuj sekwencer**

`crates/aars-core/src/sequencer.rs` — nad blokiem testów:

```rust
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

pub struct Sequencer;

pub struct SequencerHandle {
    cmd: mpsc::UnboundedSender<SeqCmd>,
    events: mpsc::UnboundedReceiver<SeqEvent>,
    /// Anuluje trwające nadanie; wspólny z wątkiem sekwencera.
    cancel: Arc<std::sync::Mutex<Option<(Priority, CancelToken)>>>,
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
        let current: Arc<std::sync::Mutex<Option<(Priority, CancelToken)>>> =
            Arc::new(std::sync::Mutex::new(None));

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

                            let _ = event_tx.send(SeqEvent::Started { id });
                            let (outcome, played) = transmit_one(
                                ptt.as_ref(),
                                sink.as_ref(),
                                &request,
                                &config,
                                &cancel,
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

        SequencerHandle { cmd: cmd_tx, events: event_rx, cancel: current, thread: Some(thread) }
    }
}

/// Strażnik RAII: każde wyjście z nadania — sukces, błąd, panika —
/// opuszcza PTT (spec §4.2).
struct PttGuard<'a> {
    ptt: &'a dyn PttBackend,
    keyed: bool,
}

impl Drop for PttGuard<'_> {
    fn drop(&mut self) {
        if self.keyed {
            if let Err(err) = self.ptt.unkey() {
                tracing::error!(%err, "unkey zawiódł, wymuszam opuszczenie PTT");
                self.ptt.force_unkey();
            }
        }
    }
}

fn transmit_one(
    ptt: &dyn PttBackend,
    sink: &dyn AudioSink,
    request: &TransmitRequest,
    config: &SequencerConfig,
    cancel: &CancelToken,
) -> (TxOutcome, Duration) {
    let full = match assemble(request, config) {
        Ok(pcm) => pcm,
        Err(err) => return (TxOutcome::Failed(err), Duration::ZERO),
    };

    if let Err(err) = ptt.key() {
        tracing::error!(%err, id = request.id, "nie udało się podnieść PTT");
        // Guard nie powstał, ale na wszelki wypadek wymuszamy stan bezpieczny.
        ptt.force_unkey();
        return (TxOutcome::Failed(err.to_string()), Duration::ZERO);
    }
    let _guard = PttGuard { ptt, keyed: true };

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

    let append = |out: &mut Pcm, part: &Pcm| -> Result<(), String> {
        out.append(part).map_err(|e| e.to_string())
    };

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
    /// niż trwające nadanie, przerywa je — ale nigdy nie nadaje równolegle.
    pub async fn transmit(&self, request: TransmitRequest) -> Result<(), String> {
        if request.interrupting
            && let Some((current_priority, cancel)) =
                self.cancel.lock().expect("mutex sekwencera").as_ref()
            && request.priority.outranks(*current_priority)
        {
            tracing::info!(
                id = request.id,
                "przerywam trwające nadanie komunikatem o wyższym priorytecie"
            );
            cancel.cancel();
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
```

**Uwaga dla wykonawcy:** test `a_higher_priority_interrupting_message_aborts_the_current_one`
wymaga, by `transmit` anulował *przed* wysłaniem komendy, a sekwencer
przetwarzał komendy po kolei. Jeśli test okaże się zależny od wyścigu,
dodaj w `SequencerHandle::transmit` oczekiwanie na zwolnienie `cancel`
(pętla z `tokio::task::yield_now` i limitem czasu), zamiast luzować asercję
na kolejność zdarzeń PTT — to ta kolejność jest gwarancją bezpieczeństwa.

`crates/aars-core/src/lib.rs` — dopisz `pub mod interlock;` i `pub mod sequencer;`

- [ ] **Step 5: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core`
Expected: PASS — wszystkie testy sekwencera i interlocków

- [ ] **Step 6: Commit**

```bash
git add crates/aars-core
git commit -m "feat(core): sekwencer nadawania z interlockami i przerywaniem"
```

---

### Task 5: Backendy PTT: VOX, serial, GPIO

**Files:**
- Create: `crates/aars-ptt/Cargo.toml`
- Create: `crates/aars-ptt/src/lib.rs`
- Create: `crates/aars-ptt/src/vox.rs`
- Create: `crates/aars-ptt/src/serial.rs`
- Create: `crates/aars-ptt/src/gpio.rs`
- Modify: `Cargo.toml` (dopisz `crates/aars-ptt` do `members`)

**Interfaces:**
- Consumes: `aars_core::ptt::{PttBackend, PttError}`, `aars_core::config::{PttConfig, PttBackendKind, SerialPttLine}`
- Produces: `aars_ptt::build(config: &PttConfig) -> Result<Arc<dyn PttBackend>, PttError>`; `aars_ptt::vox::VoxPtt`; `aars_ptt::serial::SerialPtt`; `aars_ptt::gpio::GpioPtt` (tylko Linux)

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-ptt/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::config::{Config, PttBackendKind};

    #[test]
    fn vox_backend_is_always_available_and_never_touches_hardware() {
        let mut config = Config::default().ptt;
        config.backend = PttBackendKind::Vox;

        let ptt = build(&config).unwrap();
        assert_eq!(ptt.name(), "vox");
        ptt.key().unwrap();
        ptt.unkey().unwrap();
        ptt.force_unkey();
        assert_eq!(ptt.channel_busy(), None);
    }

    #[test]
    fn a_serial_backend_on_a_missing_port_fails_with_a_clear_message() {
        let mut config = Config::default().ptt;
        config.backend = PttBackendKind::Serial;
        config.serial.port = "/dev/nie-ma-takiego-portu".to_owned();

        let err = build(&config).unwrap_err();
        assert!(err.to_string().contains("nie-ma-takiego-portu"), "{err}");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn the_gpio_backend_reports_itself_unsupported_off_linux() {
        let mut config = Config::default().ptt;
        config.backend = PttBackendKind::Gpio;

        let err = build(&config).unwrap_err();
        assert!(matches!(err, aars_core::ptt::PttError::Unsupported(_)), "{err:?}");
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-ptt`
Expected: FAIL — brak pakietu `aars-ptt`

- [ ] **Step 3: Zaimplementuj backendy**

`crates/aars-ptt/Cargo.toml`:

```toml
[package]
name = "aars-ptt"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
aars-core = { path = "../aars-core" }
tracing.workspace = true
serialport = "4.10"

[target.'cfg(target_os = "linux")'.dependencies]
gpiocdev = "0.8"
```

`crates/aars-ptt/src/vox.rs`:

```rust
use aars_core::ptt::{PttBackend, PttError};

/// Brak sterowania: radio nadaje na VOX, wyzwalane samym sygnałem audio.
/// Przerwy przed i po treści (`tx_tail_ms`) dają VOX-owi czas na reakcję.
pub struct VoxPtt;

impl PttBackend for VoxPtt {
    fn key(&self) -> Result<(), PttError> {
        tracing::debug!("PTT vox: nic do zrobienia, nadawanie wyzwoli audio");
        Ok(())
    }

    fn unkey(&self) -> Result<(), PttError> {
        Ok(())
    }

    fn force_unkey(&self) {}

    fn name(&self) -> &'static str {
        "vox"
    }
}
```

`crates/aars-ptt/src/serial.rs`:

```rust
use std::sync::Mutex;

use aars_core::config::SerialPttLine;
use aars_core::ptt::{PttBackend, PttError};

/// PTT przez podniesienie DTR lub RTS na porcie szeregowym — klasyka
/// interfejsów typu SignaLink i kabli CAT.
pub struct SerialPtt {
    port: Mutex<Box<dyn serialport::SerialPort>>,
    line: SerialPttLine,
}

impl SerialPtt {
    pub fn open(path: &str, line: SerialPttLine) -> Result<Self, PttError> {
        let port = serialport::new(path, 9600)
            .timeout(std::time::Duration::from_millis(100))
            .open()
            .map_err(|e| PttError::Hardware(format!("otwarcie portu {path}: {e}")))?;

        let backend = Self { port: Mutex::new(port), line };
        // Stan początkowy musi być „nie nadaje" — nie zakładamy, w jakim
        // stanie zostawił port poprzedni proces.
        backend.set(false)?;
        Ok(backend)
    }

    fn set(&self, keyed: bool) -> Result<(), PttError> {
        let mut port = self
            .port
            .lock()
            .map_err(|_| PttError::Hardware("mutex portu zatruty".to_owned()))?;
        let result = match self.line {
            SerialPttLine::Dtr => port.write_data_terminal_ready(keyed),
            SerialPttLine::Rts => port.write_request_to_send(keyed),
        };
        result.map_err(|e| PttError::Hardware(format!("ustawienie linii PTT: {e}")))
    }
}

impl PttBackend for SerialPtt {
    fn key(&self) -> Result<(), PttError> {
        self.set(true)
    }

    fn unkey(&self) -> Result<(), PttError> {
        self.set(false)
    }

    fn force_unkey(&self) {
        // Awaryjnie: `try_lock`, by nigdy nie zablokować watchdoga. Gdy
        // mutex jest zajęty, i tak próbujemy — stan bezpieczny jest ważniejszy.
        if let Ok(mut port) = self.port.try_lock() {
            let _ = match self.line {
                SerialPttLine::Dtr => port.write_data_terminal_ready(false),
                SerialPttLine::Rts => port.write_request_to_send(false),
            };
        }
    }

    fn name(&self) -> &'static str {
        "serial"
    }
}
```

`crates/aars-ptt/src/gpio.rs`:

```rust
#[cfg(target_os = "linux")]
mod linux {
    use aars_core::config::GpioConfig;
    use aars_core::ptt::{PttBackend, PttError};
    use gpiocdev::line::{Bias, Value};
    use gpiocdev::request::Request;

    /// PTT przez pin GPIO. Linia wyjściowa inicjowana w stanie „nie nadaje",
    /// opcjonalna linia wejściowa blokuje nadawanie (spec §4.4 krok 3).
    pub struct GpioPtt {
        request: Request,
        line: u32,
        active_low: bool,
        inhibit: Option<(Request, u32)>,
    }

    impl GpioPtt {
        pub fn open(config: &GpioConfig) -> Result<Self, PttError> {
            let inactive = if config.active_low { Value::Active } else { Value::Inactive };

            let request = Request::builder()
                .on_chip(&config.chip)
                .with_line(config.line)
                .as_output(inactive)
                .request()
                .map_err(|e| {
                    PttError::Hardware(format!(
                        "otwarcie linii {} na {}: {e}",
                        config.line, config.chip
                    ))
                })?;

            let inhibit = match config.inhibit_line {
                None => None,
                Some(line) => {
                    let req = Request::builder()
                        .on_chip(&config.chip)
                        .with_line(line)
                        .as_input()
                        .with_bias(Bias::PullUp)
                        .request()
                        .map_err(|e| {
                            PttError::Hardware(format!("otwarcie linii blokującej {line}: {e}"))
                        })?;
                    Some((req, line))
                }
            };

            Ok(Self { request, line: config.line, active_low: config.active_low, inhibit })
        }

        fn value(&self, keyed: bool) -> Value {
            if keyed != self.active_low { Value::Active } else { Value::Inactive }
        }

        /// Czy fizyczne wejście blokuje nadawanie.
        pub fn inhibited(&self) -> bool {
            match &self.inhibit {
                None => false,
                Some((req, line)) => {
                    // Zwarcie do masy (Inactive przy pull-upie) znaczy blokadę.
                    matches!(req.value(*line), Ok(Value::Inactive))
                }
            }
        }
    }

    impl PttBackend for GpioPtt {
        fn key(&self) -> Result<(), PttError> {
            self.request
                .set_value(self.line, self.value(true))
                .map_err(|e| PttError::Hardware(format!("podniesienie PTT: {e}")))
        }

        fn unkey(&self) -> Result<(), PttError> {
            self.request
                .set_value(self.line, self.value(false))
                .map_err(|e| PttError::Hardware(format!("opuszczenie PTT: {e}")))
        }

        fn force_unkey(&self) {
            // Jeden zapis do deskryptora, bez alokacji i bez logowania.
            let _ = self.request.set_value(self.line, self.value(false));
        }

        fn name(&self) -> &'static str {
            "gpio"
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::GpioPtt;
```

`crates/aars-ptt/src/lib.rs` — nad blokiem testów:

```rust
use std::sync::Arc;

use aars_core::config::{PttBackendKind, PttConfig};
use aars_core::ptt::{PttBackend, PttError};

pub mod gpio;
pub mod serial;
pub mod vox;

/// Buduje backend PTT z konfiguracji. Błąd tutaj zatrzymuje start stacji —
/// nadawanie bez pewności, co steruje nadajnikiem, jest nieodpowiedzialne.
pub fn build(config: &PttConfig) -> Result<Arc<dyn PttBackend>, PttError> {
    match config.backend {
        PttBackendKind::Vox => Ok(Arc::new(vox::VoxPtt)),
        PttBackendKind::Serial => Ok(Arc::new(serial::SerialPtt::open(
            &config.serial.port,
            config.serial.line,
        )?)),
        PttBackendKind::Gpio => {
            #[cfg(target_os = "linux")]
            {
                Ok(Arc::new(gpio::GpioPtt::open(&config.gpio)?))
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(PttError::Unsupported(
                    "backend GPIO wymaga Linuksa z /dev/gpiochip*".to_owned(),
                ))
            }
        }
    }
}
```

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-ptt`
Expected: PASS — trzy testy na macOS (w tym test „unsupported"), dwa na Linuksie

- [ ] **Step 5: Sprawdź kompilację na docelową platformę**

Run: `cargo check -p aars-ptt --target aarch64-unknown-linux-gnu`
Expected: kompilacja przechodzi (wymaga `rustup target add aarch64-unknown-linux-gnu`; jeśli brak linkera, wystarczy `cargo check`)

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/aars-ptt
git commit -m "feat(ptt): backendy vox, serial i gpio"
```

---

### Task 6: Wyjście audio na sprzęt

**Files:**
- Create: `crates/aars-audio/src/output.rs`
- Modify: `crates/aars-audio/src/lib.rs`
- Modify: `crates/aars-audio/Cargo.toml`

**Interfaces:**
- Consumes: `aars_core::sink::{AudioSink, CancelToken, PlayReport, SinkError}`, `aars_core::synth::Pcm`
- Produces: `aars_audio::output::CpalSink::open(device: &str, sample_rate: u32) -> Result<CpalSink, SinkError>`

- [ ] **Step 1: Napisz test, który ma nie przejść**

`crates/aars-audio/src/output.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_device_fails_with_its_name_in_the_message() {
        let err = CpalSink::open("nie-ma-takiego-urzadzenia", 48_000).unwrap_err();
        assert!(err.to_string().contains("nie-ma-takiego-urzadzenia"), "{err}");
    }

    /// Wymaga karty dźwiękowej, więc domyślnie pominięty (spec §11).
    /// `cargo test -p aars-audio -- --ignored output`
    #[test]
    #[ignore = "wymaga karty dźwiękowej"]
    fn playing_a_short_tone_on_the_default_device_reports_its_length() {
        use aars_core::synth::Pcm;
        use std::time::Duration;

        let sink = CpalSink::open("default", 48_000).unwrap();
        let pcm = Pcm::silence(48_000, Duration::from_millis(200));
        let report = sink.play(&pcm, &CancelToken::new()).unwrap();

        assert!(!report.cancelled);
        assert!(report.played >= Duration::from_millis(150), "{:?}", report.played);
    }
}
```

- [ ] **Step 2: Uruchom test i potwierdź, że nie przechodzi**

Run: `cargo test -p aars-audio output`
Expected: FAIL — `cannot find type CpalSink`

- [ ] **Step 3: Zaimplementuj wyjście**

`crates/aars-audio/Cargo.toml` — dopisz do `[dependencies]`:

```toml
cpal = "0.18"
```

`crates/aars-audio/src/output.rs` — nad blokiem testów:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aars_core::sink::{AudioSink, CancelToken, PlayReport, SinkError};
use aars_core::synth::Pcm;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Wyjście audio przez cpal: CoreAudio na macOS, ALSA na Linuksie. Jedno
/// źródło kodu dla maszyny rozwojowej i Raspberry Pi.
pub struct CpalSink {
    device: cpal::Device,
    sample_rate: u32,
}

impl CpalSink {
    pub fn open(device_name: &str, sample_rate: u32) -> Result<Self, SinkError> {
        let host = cpal::default_host();

        let device = if device_name == "default" {
            host.default_output_device()
                .ok_or_else(|| SinkError::Device("brak domyślnego urządzenia wyjściowego".into()))?
        } else {
            host.output_devices()
                .map_err(|e| SinkError::Device(format!("lista urządzeń: {e}")))?
                .find(|d| d.name().map(|n| n == device_name).unwrap_or(false))
                .ok_or_else(|| {
                    SinkError::Device(format!("nie znaleziono urządzenia {device_name}"))
                })?
        };

        Ok(Self { device, sample_rate })
    }
}

impl AudioSink for CpalSink {
    fn play(&self, pcm: &Pcm, cancel: &CancelToken) -> Result<PlayReport, SinkError> {
        if pcm.sample_rate != self.sample_rate {
            return Err(SinkError::SampleRateMismatch {
                expected: self.sample_rate,
                got: pcm.sample_rate,
            });
        }
        if pcm.samples.is_empty() {
            return Ok(PlayReport { played: Duration::ZERO, cancelled: false });
        }

        let config = cpal::StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(self.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let samples = Arc::new(pcm.samples.clone());
        let position = Arc::new(AtomicUsize::new(0));
        let feeder_samples = samples.clone();
        let feeder_position = position.clone();

        let stream = self
            .device
            .build_output_stream(
                &config,
                move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let start = feeder_position.load(Ordering::SeqCst);
                    for (i, slot) in out.iter_mut().enumerate() {
                        *slot = feeder_samples.get(start + i).copied().unwrap_or(0);
                    }
                    feeder_position.store((start + out.len()).min(feeder_samples.len()), Ordering::SeqCst);
                },
                |err| tracing::error!(%err, "błąd strumienia audio"),
                None,
            )
            .map_err(|e| SinkError::Device(format!("budowa strumienia: {e}")))?;

        stream.play().map_err(|e| SinkError::Device(format!("start strumienia: {e}")))?;

        // Pętla oczekiwania sprawdza anulowanie z tą samą ziarnistością co
        // FakeSink, żeby test na fake'u mówił prawdę o sprzęcie.
        let total = samples.len();
        let mut cancelled = false;
        while position.load(Ordering::SeqCst) < total {
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(stream);

        let played_samples = position.load(Ordering::SeqCst).min(total);
        Ok(PlayReport {
            played: Duration::from_secs_f64(played_samples as f64 / self.sample_rate as f64),
            cancelled,
        })
    }
}
```

`crates/aars-audio/src/lib.rs` — dopisz `pub mod output;`

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-audio`
Expected: PASS, dwa testy `ignored`

- [ ] **Step 5: Commit**

```bash
git add crates/aars-audio
git commit -m "feat(audio): wyjście audio przez cpal"
```

---

### Task 7: Podkomenda `aars tx`

**Files:**
- Modify: `crates/aars/src/cli.rs`
- Modify: `crates/aars/src/main.rs`
- Modify: `crates/aars/Cargo.toml`
- Create: `crates/aars/src/radio.rs`

**Interfaces:**
- Consumes: `aars_ptt::build`, `aars_audio::{output::CpalSink, cache::AudioCache, piper::PiperSynth}`, `aars_core::{sequencer::*, interlock::InMemoryInterlocks, watchdog::Watchdog, ident::morse_pcm}`
- Produces: `aars tx <tekst> [--priority ...] [--no-id]`; `aars::radio::build_stack(config) -> Result<RadioStack>`

- [ ] **Step 1: Napisz test, który ma nie przejść**

Dopisz do `crates/aars/tests/cli.rs`:

```rust
#[test]
fn tx_refuses_when_the_kill_switch_is_off() {
    let dir = std::env::temp_dir().join(format!("aars-tx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("tx.db");

    // Stacja z wyłączonym nadawaniem musi odmówić, nie nadać.
    let migrate = aars().env("AARS_STORAGE__DB_PATH", &db).arg("migrate").output().unwrap();
    assert!(migrate.status.success());

    let set = std::process::Command::new("sqlite3")
        .arg(&db)
        .arg("UPDATE settings SET value = 'false' WHERE key = 'tx_enabled';")
        .status()
        .unwrap();
    assert!(set.success());

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args(["tx", "--dry-run", "test"])
        .output()
        .unwrap();

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("TxDisabled") || combined.contains("nadawanie wyłączone"), "{combined}");

    std::fs::remove_dir_all(&dir).ok();
}
```

- [ ] **Step 2: Uruchom test i potwierdź, że nie przechodzi**

Run: `cargo test -p aars tx_refuses`
Expected: FAIL — `unrecognized subcommand 'tx'`

- [ ] **Step 3: Zaimplementuj `aars tx`**

`crates/aars/Cargo.toml` — dopisz do `[dependencies]`:

```toml
aars-ptt = { path = "../aars-ptt" }
```

`crates/aars/src/radio.rs`:

```rust
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use aars_core::config::Config;
use aars_core::interlock::{InMemoryInterlocks, Interlocks};
use aars_core::priority::Priority;
use aars_core::sequencer::{Sequencer, SequencerConfig, SequencerHandle};
use aars_core::watchdog::{Watchdog, WatchdogHandle};

/// Złożony stos nadawczy: PTT, wyjście audio, sekwencer i watchdog.
pub struct RadioStack {
    pub sequencer: SequencerHandle,
    pub watchdog: WatchdogHandle,
    pub interlocks: Arc<InMemoryInterlocks>,
}

/// Wczytuje parametry operacyjne z tabeli `settings` (spec §9).
async fn read_setting(pool: &SqlitePool, key: &str) -> Result<String> {
    let value: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_one(pool)
        .await
        .with_context(|| format!("odczyt ustawienia {key}"))?;
    Ok(value)
}

pub async fn build_stack(config: &Config, pool: &SqlitePool) -> Result<RadioStack> {
    let ptt = aars_ptt::build(&config.ptt).context("budowa backendu PTT")?;
    let sink: Arc<dyn aars_core::sink::AudioSink> = Arc::new(
        aars_audio::output::CpalSink::open(&config.audio.device, config.audio.sample_rate)
            .context("otwarcie urządzenia audio")?,
    );

    let tx_max_s: u64 = read_setting(pool, "tx_max_duration_s").await?.parse().unwrap_or(180);
    let id_interval_s: u64 = read_setting(pool, "id_interval_s").await?.parse().unwrap_or(600);
    let tx_enabled = read_setting(pool, "tx_enabled").await? == "true";
    let poisoned = read_setting(pool, "poisoned").await? == "true";
    let quiet_min: i64 = read_setting(pool, "quiet_min_priority").await?.parse().unwrap_or(0);

    let interlocks = Arc::new(InMemoryInterlocks::permissive());
    interlocks.set_tx_enabled(tx_enabled);
    interlocks.set_poisoned(poisoned);
    interlocks.set_quiet(false, Priority::from_i64(quiet_min).unwrap_or(Priority::Emergency));

    let watchdog = Watchdog::spawn(ptt.clone(), Duration::from_secs(tx_max_s));

    let sequencer = Sequencer::spawn(
        ptt,
        sink,
        interlocks.clone() as Arc<dyn Interlocks>,
        SequencerConfig {
            tx_tail: Duration::from_millis(config.ptt.tx_tail_ms),
            tx_max_duration: Duration::from_secs(tx_max_s),
            id_interval: Duration::from_secs(id_interval_s),
        },
    );

    Ok(RadioStack { sequencer, watchdog, interlocks })
}
```

`crates/aars/src/cli.rs` — dopisz do `enum Command`:

```rust
    /// Nadaje komunikat natychmiast.
    Tx {
        /// Treść komunikatu.
        text: String,
        /// Priorytet: emergency | high | normal | low.
        #[arg(long, default_value = "normal")]
        priority: String,
        /// Pomiń identyfikację stacji (tylko do testów warsztatowych).
        #[arg(long)]
        no_id: bool,
        /// Sprawdź interlocki i złóż audio, ale nie wchodź na pasmo.
        #[arg(long)]
        dry_run: bool,
    },
```

`crates/aars/src/main.rs` — dopisz `mod radio;` oraz gałąź:

```rust
        cli::Command::Tx { text, priority, no_id, dry_run } => {
            use aars_core::interlock::Interlocks;
            use aars_core::priority::Priority;

            let priority = match priority.as_str() {
                "emergency" => Priority::Emergency,
                "high" => Priority::High,
                "normal" => Priority::Normal,
                "low" => Priority::Low,
                other => anyhow::bail!("nieznany priorytet: {other}"),
            };

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;

            let mut stack = radio::build_stack(&config, &pool).await?;

            if let Err(reason) = stack.interlocks.check(priority) {
                println!("odmowa nadania: {reason:?}");
                return Ok(());
            }

            let synth = aars_audio::piper::PiperSynth::new(
                &config.tts.piper_bin,
                &config.tts.model,
                config.audio.sample_rate,
            );
            let cache = aars_audio::cache::AudioCache::new(
                pool.clone(),
                config.storage.cache_dir.clone(),
                config.storage.cache_max_bytes,
            );
            let voice = aars_core::synth::VoiceParams::default();
            let (_, body) = cache.render(&synth, &text, &voice).await.context("synteza treści")?;

            let callsign: String =
                sqlx::query_scalar("SELECT value FROM settings WHERE key = 'callsign'")
                    .fetch_one(&pool)
                    .await
                    .context("odczyt znaku wywoławczego")?;

            let id_audio = if no_id || callsign.is_empty() {
                if !no_id {
                    tracing::warn!("znak wywoławczy nieustawiony — nadanie bez identyfikacji");
                }
                None
            } else {
                Some(aars_core::ident::morse_pcm(
                    &callsign,
                    &aars_core::ident::MorseParams::default(),
                    config.audio.sample_rate,
                ))
            };

            if dry_run {
                println!("gotowe do nadania: {:?} treści, priorytet {priority:?}", body.duration());
                return Ok(());
            }

            stack
                .sequencer
                .transmit(aars_core::sequencer::TransmitRequest {
                    id: 0,
                    priority,
                    interrupting: false,
                    body,
                    id_audio,
                })
                .await
                .map_err(anyhow::Error::msg)?;

            while let Some(event) = stack.sequencer.next_event().await {
                println!("{event:?}");
                if matches!(
                    event,
                    aars_core::sequencer::SeqEvent::Finished { .. }
                        | aars_core::sequencer::SeqEvent::Refused { .. }
                ) {
                    break;
                }
            }

            stack.sequencer.shutdown();
            stack.watchdog.stop();
        }
```

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 5: Sprawdź odmowę i nadanie ręcznie**

Run: `cargo run -p aars -- tx --dry-run "komunikat testowy"`
Expected: wypisana długość audio i priorytet, PTT nietknięte

- [ ] **Step 6: Commit**

```bash
git add crates/aars
git commit -m "feat(bin): podkomenda tx nadająca komunikat przez sekwencer"
```

---

## Kryterium wyjścia z etapu

- `cargo test --workspace` zielone; `cargo clippy --workspace --all-targets -- -D warnings` bez zastrzeżeń.
- Testy dowodzą, że żadna ścieżka — nieudane `key`, błąd audio, anulowanie, wyjście z pętli — nie zostawia PTT podniesionego.
- Watchdog opuszcza PTT po przekroczeniu limitu i zostawia lepką flagę `poisoned`, której nie da się zdjąć inaczej niż ręcznie.
- Komunikat `Emergency` z `interrupting` przerywa trwające nadanie, a zdarzenia PTT dowodzą, że nie ma momentu z dwoma nadaniami.
- Każdy interlock z §4.4 ma test odmowy, a odmowa nie rusza PTT.
- `cargo check -p aars-ptt --target aarch64-unknown-linux-gnu` przechodzi.
