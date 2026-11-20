use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

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
///
/// Opcjonalny termin (`expire_at`) działa jak anulowanie, które samo
/// zapada o czasie — tak sekwencer ucina nadanie na twardym limicie z tą
/// samą ziarnistością co przerwanie, bez dotykania wyjść audio.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<TokenState>);

#[derive(Debug, Default)]
struct TokenState {
    cancelled: AtomicBool,
    deadline: OnceLock<Instant>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
    }

    /// Ustala termin, po którym token uznaje się za anulowany. Tylko
    /// pierwsze wywołanie się liczy — termin nie da się odsunąć.
    pub fn expire_at(&self, deadline: Instant) {
        let _ = self.0.deadline.set(deadline);
    }

    /// Czy minął termin z `expire_at`.
    pub fn is_expired(&self) -> bool {
        self.0.deadline.get().is_some_and(|deadline| Instant::now() >= *deadline)
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst) || self.is_expired()
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
    /// Odtwarza w czasie rzeczywistym: sypia tyle, ile trwa bufor. Sink
    /// zwracający natychmiast nie modelowałby nadawania — nadanie kończyłoby
    /// się przed tym, jak cokolwiek zdąży je przerwać, i test przerywania nie
    /// testowałby niczego.
    fn play(&self, pcm: &Pcm, cancel: &CancelToken) -> Result<PlayReport, SinkError> {
        // Bufor 1024 próbek, tak jak realne wyjście — anulowanie ma tę samą
        // ziarnistość w teście i na sprzęcie.
        const CHUNK: usize = 1024;
        let chunk_time = Duration::from_secs_f64(CHUNK as f64 / pcm.sample_rate as f64);
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
            std::thread::sleep(chunk_time);
        }

        Ok(PlayReport {
            played: Duration::from_secs_f64(played as f64 / pcm.sample_rate as f64),
            cancelled: false,
        })
    }
}

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

    #[test]
    fn a_token_past_its_deadline_counts_as_cancelled() {
        let sink = FakeSink::new();
        let pcm = Pcm::silence(48_000, Duration::from_secs(10));
        let cancel = CancelToken::new();
        cancel.expire_at(Instant::now() + Duration::from_millis(200));
        assert!(!cancel.is_cancelled(), "przed terminem token jest ważny");

        let report = sink.play(&pcm, &cancel).unwrap();

        assert!(report.cancelled);
        assert!(cancel.is_expired());
        assert!(report.played < Duration::from_secs(1), "odtworzono {:?}", report.played);
    }
}
