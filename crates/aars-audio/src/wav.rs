use std::path::Path;

use aars_core::synth::{Pcm, SynthError};

pub fn write_wav_mono(path: &Path, pcm: &Pcm) -> Result<(), SynthError> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: pcm.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| SynthError::Engine(format!("zapis WAV {}: {e}", path.display())))?;
    for sample in &pcm.samples {
        writer
            .write_sample(*sample)
            .map_err(|e| SynthError::Engine(format!("zapis próbki: {e}")))?;
    }
    writer.finalize().map_err(|e| SynthError::Engine(format!("domknięcie WAV: {e}")))?;
    Ok(())
}

pub fn read_wav_mono(path: &Path) -> Result<Pcm, SynthError> {
    let mut reader = hound::WavReader::open(path)
        .map_err(|e| SynthError::Engine(format!("odczyt WAV {}: {e}", path.display())))?;
    let spec = reader.spec();

    let samples: Result<Vec<i16>, _> = reader.samples::<i16>().collect();
    let samples =
        samples.map_err(|e| SynthError::Engine(format!("odczyt próbek {}: {e}", path.display())))?;

    // Gdy plik jest wielokanałowy, bierzemy pierwszy kanał: nadajemy mono.
    let samples = if spec.channels > 1 {
        samples.iter().step_by(spec.channels as usize).copied().collect()
    } else {
        samples
    };

    Ok(Pcm { samples, sample_rate: spec.sample_rate })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::synth::Pcm;

    #[test]
    fn a_wav_round_trips_through_write_and_read() {
        let dir = std::env::temp_dir().join(format!("aars-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.wav");

        let pcm = Pcm { samples: vec![0, 1000, -1000, 32767, -32768], sample_rate: 22_050 };
        write_wav_mono(&path, &pcm).unwrap();
        let read = read_wav_mono(&path).unwrap();

        assert_eq!(read, pcm);
        std::fs::remove_dir_all(&dir).ok();
    }
}
