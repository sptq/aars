use aars_core::synth::{Pcm, SynthError};
use audioadapter::Adapter;
use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

/// Przelicza PCM na docelową częstotliwość. Piper daje 22,05 kHz, karta
/// zwykle chce 48 kHz.
///
/// Resampler FFT z antyaliasingiem: przy schodzeniu w dół częstotliwości
/// obcina pasmo, więc konwersja w obie strony jest bezpieczna.
pub fn resample(input: &Pcm, target_rate: u32) -> Result<Pcm, SynthError> {
    if input.sample_rate == target_rate || input.samples.is_empty() {
        return Ok(Pcm { samples: input.samples.clone(), sample_rate: target_rate });
    }

    let floats: Vec<f32> = input.samples.iter().map(|s| *s as f32 / 32_768.0).collect();
    let frames = floats.len();

    let mut resampler = Fft::<f32>::new(
        input.sample_rate as usize,
        target_rate as usize,
        1024,
        1,
        FixedSync::Both,
    )
    .map_err(|e| SynthError::Engine(format!("inicjalizacja resamplera: {e}")))?;

    let adapter = InterleavedSlice::new(floats.as_slice(), 1, frames)
        .map_err(|e| SynthError::Engine(format!("adapter wejścia resamplera: {e}")))?;

    // `process_all` przepuszcza cały materiał jednym wywołaniem i sam obcina
    // opóźnienie rozruchowe filtra, więc nie musimy pilnować bloków.
    let output = resampler
        .process_all(&adapter, frames, None)
        .map_err(|e| SynthError::Engine(format!("resampling: {e}")))?;

    // Bufor wyjściowy jest wymiarowany na najgorszy przypadek, więc bierzemy
    // tylko tyle ramek, ile wynika z proporcji częstotliwości.
    let expected =
        (frames as f64 * target_rate as f64 / input.sample_rate as f64).round() as usize;
    let available = output.frames();
    let count = expected.min(available);

    let mut samples = Vec::with_capacity(count);
    for frame in 0..count {
        let value = output.read_sample(0, frame).unwrap_or(0.0);
        samples.push((value.clamp(-1.0, 1.0) * 32_767.0) as i16);
    }

    Ok(Pcm { samples, sample_rate: target_rate })
}

/// Normalizuje szczyt do zadanego poziomu w dBFS. Nie wzmacnia ciszy i
/// nigdy nie przepełnia — nadajnik dostaje przewidywalny poziom.
pub fn apply_gain_db(input: &Pcm, target_dbfs: f32) -> Pcm {
    let peak = input.samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    if peak == 0 {
        return input.clone();
    }

    let target_peak = 32_767.0 * 10f32.powf(target_dbfs / 20.0);
    let factor = target_peak / peak as f32;

    let samples = input
        .samples
        .iter()
        .map(|s| ((*s as f32 * factor).clamp(-32_767.0, 32_767.0)) as i16)
        .collect();

    Pcm { samples, sample_rate: input.sample_rate }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::synth::Pcm;

    fn tone(sample_rate: u32, secs: f32) -> Pcm {
        let count = (sample_rate as f32 * secs) as usize;
        let samples = (0..count)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (f32::sin(t * 2.0 * std::f32::consts::PI * 440.0) * 10_000.0) as i16
            })
            .collect();
        Pcm { samples, sample_rate }
    }

    #[test]
    fn resampling_preserves_duration_within_a_millisecond() {
        let input = tone(22_050, 1.0);
        let output = resample(&input, 48_000).unwrap();

        assert_eq!(output.sample_rate, 48_000);
        let delta = output.duration().as_secs_f64() - input.duration().as_secs_f64();
        assert!(delta.abs() < 0.001, "różnica długości {delta}s");
    }

    #[test]
    fn resampling_to_the_same_rate_is_a_no_op() {
        let input = tone(48_000, 0.1);
        let output = resample(&input, 48_000).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn gain_scales_towards_the_target_peak_and_never_clips() {
        let quiet = Pcm { samples: vec![100, -100, 50], sample_rate: 48_000 };
        let loud = apply_gain_db(&quiet, -3.0);

        let peak = loud.samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        let expected = (32_767.0 * 10f32.powf(-3.0 / 20.0)) as u32;
        assert!(
            (peak as i64 - expected as i64).abs() < 64,
            "szczyt {peak}, oczekiwany ~{expected}"
        );
        assert!(loud.samples.iter().all(|s| *s != i16::MIN), "brak przepełnienia");
    }

    #[test]
    fn gain_leaves_silence_alone() {
        let silence = Pcm { samples: vec![0; 100], sample_rate: 48_000 };
        let out = apply_gain_db(&silence, -3.0);
        assert!(out.samples.iter().all(|s| *s == 0), "cisza nie ma czego wzmacniać");
    }
}
