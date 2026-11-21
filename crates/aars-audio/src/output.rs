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
    /// Liczba kanałów strumienia; próbka mono idzie na każdy z nich.
    channels: u16,
}

/// Jedna obsługiwana konfiguracja wyjścia: liczba kanałów i przedział
/// częstotliwości próbkowania (z `supported_output_configs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigRange {
    pub channels: u16,
    pub min_rate: u32,
    pub max_rate: u32,
    /// Konfiguracja z próbkami `i16`, czyli w formacie, w którym
    /// [`CpalSink`] buduje strumień (przegląd 1–4, M-5).
    pub is_i16: bool,
}

/// Ile kanałów otworzyć przy danej częstotliwości (R16): mono, gdy karta je
/// ma, potem stereo, potem najmniej kanałów. Karty USB (CM108) i `hw:X,Y`
/// bez `plug` często mają tylko stereo. Gdy żadna konfiguracja nie ma tej
/// częstotliwości, błąd wymienia, co karta obsługuje — wychodzi przy budowie
/// stosu, więc taka konfiguracja nie zostaje „ostatnią działającą”.
///
/// Strumień jest zawsze `i16`, a ALSA podaje osobne zakresy dla każdego
/// formatu próbek. Dlatego najpierw liczą się zakresy `i16`, a dopiero gdy
/// żaden nie pasuje (CoreAudio podaje zwykle tylko F32 i sam konwertuje),
/// wszystkie (przegląd 1–4, M-5).
pub fn choose_channels(ranges: &[ConfigRange], rate: u32) -> Result<u16, String> {
    // Mono, potem stereo, potem najmniej kanałów — to po prostu minimum.
    let fits = |r: &&ConfigRange| r.channels >= 1 && r.min_rate <= rate && rate <= r.max_rate;
    ranges
        .iter()
        .filter(|r| r.is_i16)
        .filter(fits)
        .map(|r| r.channels)
        .min()
        .or_else(|| ranges.iter().filter(fits).map(|r| r.channels).min())
        .ok_or_else(|| {
            let offered: Vec<String> = ranges
                .iter()
                .map(|r| format!("{} kan. {}–{} Hz", r.channels, r.min_rate, r.max_rate))
                .collect();
            if offered.is_empty() {
                format!("nie obsługuje {rate} Hz (brak konfiguracji wyjścia)")
            } else {
                format!(
                    "nie obsługuje {rate} Hz (obsługuje: {})",
                    offered.join(", ")
                )
            }
        })
}

/// Rozkłada próbki mono na `channels` kanałów z przeplotem: ta sama próbka
/// na każdy kanał ramki. Ramki bez próbki mono dostają ciszę.
pub fn upmix_into<T: Copy + Default>(mono: &[T], channels: u16, out: &mut [T]) {
    let channels = usize::from(channels.max(1));
    for (i, frame) in out.chunks_mut(channels).enumerate() {
        frame.fill(mono.get(i).copied().unwrap_or_default());
    }
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

        let channels = match device.supported_output_configs() {
            Ok(configs) => {
                let ranges: Vec<ConfigRange> = configs
                    .map(|c| ConfigRange {
                        channels: c.channels(),
                        min_rate: c.min_sample_rate(),
                        max_rate: c.max_sample_rate(),
                        is_i16: c.sample_format() == cpal::SampleFormat::I16,
                    })
                    .collect();
                choose_channels(&ranges, sample_rate)
                    .map_err(|e| SinkError::Device(format!("urządzenie {device_name} {e}")))?
            }
            Err(err) => {
                // Jak przed etapem 10: mono, a ewentualny błąd wyjdzie przy
                // budowie strumienia.
                tracing::warn!(
                    %err,
                    device = device_name,
                    "lista konfiguracji wyjścia niedostępna — próbuję mono"
                );
                1
            }
        };

        Ok(Self {
            device,
            sample_rate,
            channels,
        })
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
            channels: self.channels,
            sample_rate: self.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let samples = Arc::new(pcm.samples.clone());
        let position = Arc::new(AtomicUsize::new(0));
        let feeder_samples = samples.clone();
        let feeder_position = position.clone();
        let channels = self.channels;
        let frame = usize::from(channels.max(1));

        let stream = self
            .device
            .build_output_stream(
                config,
                move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    // Pozycja liczy próbki mono, czyli ramki strumienia.
                    let start = feeder_position
                        .load(Ordering::SeqCst)
                        .min(feeder_samples.len());
                    let frames = out.len() / frame;
                    let end = (start + frames).min(feeder_samples.len());
                    upmix_into(&feeder_samples[start..end], channels, out);
                    feeder_position.store(end, Ordering::SeqCst);
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

    fn range(channels: u16, min_rate: u32, max_rate: u32) -> ConfigRange {
        ConfigRange {
            channels,
            min_rate,
            max_rate,
            is_i16: true,
        }
    }

    fn range_f32(channels: u16, min_rate: u32, max_rate: u32) -> ConfigRange {
        ConfigRange {
            is_i16: false,
            ..range(channels, min_rate, max_rate)
        }
    }

    #[test]
    fn i16_ranges_win_because_the_stream_is_i16() {
        // `hw:` z mono tylko w S32 i stereo w S16: strumień `i16` dostaje 2.
        let ranges = [range_f32(1, 8_000, 48_000), range(2, 8_000, 48_000)];
        assert_eq!(choose_channels(&ranges, 48_000), Ok(2));
        // Bez `i16` przy tej częstotliwości liczą się wszystkie zakresy
        // (CoreAudio podaje tylko F32).
        let ranges = [range_f32(2, 44_100, 48_000), range(1, 8_000, 16_000)];
        assert_eq!(choose_channels(&ranges, 48_000), Ok(2));
        let ranges = [range_f32(1, 8_000, 192_000)];
        assert_eq!(choose_channels(&ranges, 48_000), Ok(1));
    }

    #[test]
    fn mono_is_preferred_when_offered() {
        let ranges = [range(2, 8_000, 192_000), range(1, 8_000, 48_000)];
        assert_eq!(choose_channels(&ranges, 48_000), Ok(1));
    }

    #[test]
    fn stereo_only_cards_get_two_channels() {
        // CM108 na `hw:X,Y`: tylko stereo, 44,1 i 48 kHz.
        let ranges = [range(2, 44_100, 48_000)];
        assert_eq!(choose_channels(&ranges, 48_000), Ok(2));
        // Mono w innej częstotliwości nie wygrywa ze stereo w tej właściwej.
        let ranges = [range(1, 8_000, 16_000), range(2, 44_100, 48_000)];
        assert_eq!(choose_channels(&ranges, 48_000), Ok(2));
    }

    #[test]
    fn a_rate_outside_every_range_is_an_error_listing_the_ranges() {
        let err = choose_channels(&[range(2, 44_100, 48_000)], 22_050).unwrap_err();
        assert!(err.contains("22050 Hz"), "{err}");
        assert!(err.contains("44100–48000"), "{err}");
        assert!(err.contains("2 kan."), "{err}");

        let err = choose_channels(&[], 48_000).unwrap_err();
        assert!(err.contains("48000 Hz"), "{err}");
    }

    #[test]
    fn more_channels_only_when_nothing_smaller_fits() {
        assert_eq!(choose_channels(&[range(8, 48_000, 48_000)], 48_000), Ok(8));
        let ranges = [range(8, 48_000, 48_000), range(4, 44_100, 96_000)];
        assert_eq!(choose_channels(&ranges, 48_000), Ok(4));
        // Zero kanałów to nie wyjście.
        assert!(choose_channels(&[range(0, 48_000, 48_000)], 48_000).is_err());
    }

    #[test]
    fn upmix_copies_each_sample_to_every_channel() {
        let mut out = [0.0f32; 4];
        upmix_into(&[0.5, -0.5], 2, &mut out);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);

        // Mono przechodzi bez zmian, a brak próbek to cisza.
        let mut out = [9i16; 3];
        upmix_into(&[1, 2], 1, &mut out);
        assert_eq!(out, [1, 2, 0]);
        let mut out = [9i16; 6];
        upmix_into(&[7], 3, &mut out);
        assert_eq!(out, [7, 7, 7, 0, 0, 0]);
    }

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
