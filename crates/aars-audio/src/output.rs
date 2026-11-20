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

/// Nazwy wyjść, takie same jak te, które porównuje [`CpalSink::open`].
/// Tylko listuje; żadne urządzenie nie jest otwierane.
pub fn output_device_names() -> Result<Vec<String>, SinkError> {
    let devices = cpal::default_host()
        .output_devices()
        .map_err(|e| SinkError::Device(format!("lista urządzeń: {e}")))?;
    let mut names: Vec<String> = devices
        .filter_map(|d| d.description().ok().map(|desc| desc.name().to_owned()))
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

impl CpalSink {
    pub fn open(device_name: &str, sample_rate: u32) -> Result<Self, SinkError> {
        let host = cpal::default_host();

        let device = if device_name == "default" {
            host.default_output_device().ok_or_else(|| {
                SinkError::Device("brak domyślnego urządzenia wyjściowego".to_owned())
            })?
        } else {
            host.output_devices()
                .map_err(|e| SinkError::Device(format!("lista urządzeń: {e}")))?
                .find(|d| {
                    d.description().map(|desc| desc.name() == device_name).unwrap_or(false)
                })
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

        // W cpal 0.18 `SampleRate` jest aliasem na u32, a konfiguracja idzie
        // do buildera przez wartość.
        let config = cpal::StreamConfig {
            channels: 1,
            sample_rate: self.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let samples = Arc::new(pcm.samples.clone());
        let position = Arc::new(AtomicUsize::new(0));
        let feeder_samples = samples.clone();
        let feeder_position = position.clone();

        let stream = self
            .device
            .build_output_stream(
                config,
                move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let start = feeder_position.load(Ordering::SeqCst);
                    for (i, slot) in out.iter_mut().enumerate() {
                        *slot = feeder_samples.get(start + i).copied().unwrap_or(0);
                    }
                    feeder_position
                        .store((start + out.len()).min(feeder_samples.len()), Ordering::SeqCst);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_device_fails_with_its_name_in_the_message() {
        let Err(err) = CpalSink::open("nie-ma-takiego-urzadzenia", 48_000) else {
            panic!("nieistniejące urządzenie nie może się otworzyć");
        };
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
