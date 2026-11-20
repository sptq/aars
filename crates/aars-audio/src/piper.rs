use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use aars_core::synth::{Pcm, SynthError, Synthesizer, VoiceParams};

use crate::resample::resample;
use crate::text::normalize;
use crate::wav::read_wav_mono;

/// Synteza przez binarkę `piper`. Podproces, a nie wiązanie natywne:
/// `piper-rs` 0.2 ciągnie natywne espeak-ng i ort, a jego dokumentacja nie
/// buduje się na docs.rs — nie jest fundamentem dla stacji ostrzegawczej.
/// Trait `Synthesizer` pozwala podmienić to później bez ruszania wywołujących.
pub struct PiperSynth {
    piper_bin: String,
    model: String,
    target_sample_rate: u32,
}

impl PiperSynth {
    pub fn new(
        piper_bin: impl Into<String>,
        model: impl Into<String>,
        target_sample_rate: u32,
    ) -> Self {
        Self { piper_bin: piper_bin.into(), model: model.into(), target_sample_rate }
    }

    fn output_path(&self) -> PathBuf {
        std::env::temp_dir().join(format!(
            "aars-piper-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ))
    }
}

impl Synthesizer for PiperSynth {
    fn synthesize(&self, text: &str, voice: &VoiceParams) -> Result<Pcm, SynthError> {
        let spoken = normalize(text);
        if spoken.is_empty() {
            return Ok(Pcm { samples: Vec::new(), sample_rate: self.target_sample_rate });
        }

        let out_path = self.output_path();
        let mut cmd = Command::new(&self.piper_bin);
        cmd.arg("--model")
            .arg(&self.model)
            .arg("--output_file")
            .arg(&out_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        if (voice.rate - 1.0).abs() > f32::EPSILON {
            // Piper wyraża tempo odwrotnie: większa długość to wolniejsza mowa.
            cmd.arg("--length_scale").arg(format!("{:.3}", 1.0 / voice.rate));
        }

        let mut child = cmd.spawn().map_err(|e| {
            SynthError::Engine(format!("nie udało się uruchomić piper ({}): {e}", self.piper_bin))
        })?;

        child
            .stdin
            .as_mut()
            .ok_or_else(|| SynthError::Engine("piper nie przyjął wejścia".to_owned()))?
            .write_all(spoken.as_bytes())?;
        drop(child.stdin.take());

        let output = child
            .wait_with_output()
            .map_err(|e| SynthError::Engine(format!("piper nie zakończył się: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_file(&out_path);
            return Err(SynthError::Engine(format!(
                "piper zakończył się błędem {:?}: {}",
                output.status.code(),
                stderr.trim()
            )));
        }

        let raw = read_wav_mono(&out_path)?;
        let _ = std::fs::remove_file(&out_path);

        resample(&raw, self.target_sample_rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::synth::{Synthesizer, VoiceParams};

    /// Wymaga binarki `piper` i modelu wskazanych przez AARS_TEST_PIPER_BIN
    /// i AARS_TEST_PIPER_MODEL. Uruchamiane jawnie:
    /// `cargo test -p aars-audio -- --ignored`
    #[test]
    #[ignore = "wymaga lokalnej instalacji Pipera"]
    fn piper_synthesizes_audible_audio_at_the_target_rate() {
        let bin = std::env::var("AARS_TEST_PIPER_BIN").expect("AARS_TEST_PIPER_BIN");
        let model = std::env::var("AARS_TEST_PIPER_MODEL").expect("AARS_TEST_PIPER_MODEL");

        let synth = PiperSynth::new(bin, model, 48_000);
        let pcm = synth.synthesize("Uwaga, ostrzeżenie testowe.", &VoiceParams::default()).unwrap();

        assert_eq!(pcm.sample_rate, 48_000);
        assert!(pcm.duration().as_millis() > 500, "zbyt krótkie: {:?}", pcm.duration());
        let peak = pcm.samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 1000, "audio praktycznie ciche, szczyt {peak}");
    }

    #[test]
    fn a_missing_piper_binary_is_a_clear_engine_error() {
        let synth = PiperSynth::new("/nie/ma/takiego/pipera", "/nie/ma/modelu.onnx", 48_000);
        let err = synth.synthesize("test", &VoiceParams::default()).unwrap_err();

        assert!(matches!(err, SynthError::Engine(_)), "{err:?}");
        assert!(err.to_string().contains("piper"), "komunikat musi wskazać winowajcę: {err}");
    }
}
