use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum SynthError {
    #[error("silnik syntezy zawiódł: {0}")]
    Engine(String),
    #[error("nie można złożyć audio o różnych częstotliwościach: {left} vs {right}")]
    SampleRateMismatch { left: u32, right: u32 },
    #[error("błąd wejścia/wyjścia: {0}")]
    Io(#[from] std::io::Error),
}

/// Mono PCM 16-bit. Jedyna reprezentacja audio w systemie — sekwencer,
/// cache i wyjście audio mówią tym samym typem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pcm {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}

impl Pcm {
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.samples.len() as f64 / self.sample_rate as f64)
    }

    pub fn silence(sample_rate: u32, duration: Duration) -> Self {
        let count = (duration.as_secs_f64() * sample_rate as f64).round() as usize;
        Self { samples: vec![0i16; count], sample_rate }
    }

    pub fn append(&mut self, other: &Pcm) -> Result<(), SynthError> {
        if self.sample_rate != other.sample_rate {
            return Err(SynthError::SampleRateMismatch {
                left: self.sample_rate,
                right: other.sample_rate,
            });
        }
        self.samples.extend_from_slice(&other.samples);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceParams {
    /// Nazwa głosu; `None` znaczy głos domyślny z konfiguracji.
    pub voice: Option<String>,
    /// Mnożnik tempa mowy; 1.0 to tempo modelu.
    pub rate: f32,
}

impl Default for VoiceParams {
    fn default() -> Self {
        Self { voice: None, rate: 1.0 }
    }
}

pub trait Synthesizer: Send + Sync {
    fn synthesize(&self, text: &str, voice: &VoiceParams) -> Result<Pcm, SynthError>;

    /// Tożsamość syntezatora (model, częstotliwość) wchodzi do klucza cache,
    /// żeby zmiana głosu albo częstotliwości nie oddała starego audio (R19).
    fn identity(&self) -> String {
        String::new()
    }
}

/// Syntezator testowy: deterministyczny ton o długości proporcjonalnej do
/// tekstu. Pozwala testować sekwencer i kolejkę bez karty dźwiękowej.
pub struct FakeSynth {
    sample_rate: u32,
    requests: Mutex<Vec<String>>,
    identity: String,
}

impl FakeSynth {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate, requests: Mutex::new(Vec::new()), identity: String::new() }
    }

    /// Tożsamość do klucza cache (testy zmiany głosu).
    pub fn with_identity(mut self, id: &str) -> Self {
        self.identity = id.to_owned();
        self
    }

    /// Teksty, o których syntezę ktoś poprosił, w kolejności.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("mutex fake'a").clone()
    }
}

impl Synthesizer for FakeSynth {
    fn identity(&self) -> String {
        self.identity.clone()
    }

    fn synthesize(&self, text: &str, _voice: &VoiceParams) -> Result<Pcm, SynthError> {
        self.requests.lock().expect("mutex fake'a").push(text.to_owned());

        // 50 ms na znak — krótko, żeby testy były szybkie, ale mierzalnie.
        let samples_per_char = self.sample_rate as usize / 20;
        let count = text.chars().count() * samples_per_char;
        let samples = (0..count)
            .map(|i| {
                let t = i as f32 / self.sample_rate as f32;
                (f32::sin(t * 2.0 * std::f32::consts::PI * 440.0) * 8000.0) as i16
            })
            .collect();

        Ok(Pcm { samples, sample_rate: self.sample_rate })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn duration_follows_sample_count_and_rate() {
        let pcm = Pcm { samples: vec![0i16; 48_000], sample_rate: 48_000 };
        assert_eq!(pcm.duration(), Duration::from_secs(1));
    }

    #[test]
    fn silence_has_the_requested_length() {
        let pcm = Pcm::silence(48_000, Duration::from_millis(250));
        assert_eq!(pcm.samples.len(), 12_000);
        assert!(pcm.samples.iter().all(|s| *s == 0));
    }

    #[test]
    fn append_concatenates_and_rejects_mismatched_rates() {
        let mut a = Pcm { samples: vec![1, 2], sample_rate: 48_000 };
        a.append(&Pcm { samples: vec![3], sample_rate: 48_000 }).unwrap();
        assert_eq!(a.samples, vec![1, 2, 3]);

        let err = a.append(&Pcm { samples: vec![4], sample_rate: 22_050 }).unwrap_err();
        assert!(matches!(err, SynthError::SampleRateMismatch { .. }));
    }

    #[test]
    fn fake_synth_is_deterministic_and_records_what_it_was_asked_for() {
        let synth = FakeSynth::new(48_000);
        let voice = VoiceParams::default();

        let a = synth.synthesize("ostrzeżenie", &voice).unwrap();
        let b = synth.synthesize("ostrzeżenie", &voice).unwrap();
        assert_eq!(a.samples, b.samples, "ten sam tekst musi dać to samo audio");

        let c = synth.synthesize("ostrzeżenie dłuższe", &voice).unwrap();
        assert!(c.samples.len() > a.samples.len(), "dłuższy tekst to dłuższe audio");

        assert_eq!(synth.requests(), vec!["ostrzeżenie", "ostrzeżenie", "ostrzeżenie dłuższe"]);
    }
}
