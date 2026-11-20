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
}
