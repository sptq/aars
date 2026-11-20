use std::sync::Arc;

use aars_audio::cache::AudioCache;
use aars_core::dispatcher::{RenderFuture, Renderer};
use aars_core::ident::{IdMode, MorseParams, morse_pcm};
use aars_core::synth::{Pcm, Synthesizer, VoiceParams};
use aars_web::preview::{PreviewFuture, PreviewRenderer};

/// Renderer produkcyjny: synteza przez cache, znak wywoławczy złożony raz
/// przy starcie, bo nie zmienia się między nadaniami.
pub struct PiperRenderer {
    cache: AudioCache,
    synth: Arc<dyn Synthesizer>,
    ident: Option<Pcm>,
}

impl PiperRenderer {
    pub fn new(
        cache: AudioCache,
        synth: Arc<dyn Synthesizer>,
        callsign: &str,
        id_mode: IdMode,
        sample_rate: u32,
    ) -> Self {
        let ident = if callsign.is_empty() {
            tracing::warn!("znak wywoławczy nieustawiony — nadania bez identyfikacji");
            None
        } else {
            match id_mode {
                IdMode::Cw => Some(morse_pcm(callsign, &MorseParams::default(), sample_rate)),
                IdMode::Voice => match synth.synthesize(callsign, &VoiceParams::default()) {
                    Ok(pcm) => Some(pcm),
                    Err(err) => {
                        tracing::error!(
                            %err,
                            "synteza znaku wywoławczego zawiodła, przechodzę na CW"
                        );
                        Some(morse_pcm(callsign, &MorseParams::default(), sample_rate))
                    }
                },
            }
        };

        Self {
            cache,
            synth,
            ident,
        }
    }
}

impl Renderer for PiperRenderer {
    fn render<'a>(&'a self, text: &'a str) -> RenderFuture<'a> {
        Box::pin(async move {
            self.cache
                .render(self.synth.as_ref(), text, &VoiceParams::default())
                .await
                .map(|(_, pcm)| pcm)
                .map_err(|e| e.to_string())
        })
    }

    fn ident(&self) -> Option<Pcm> {
        self.ident.clone()
    }
}

/// Odsłuch w panelu: ta sama synteza i ten sam cache co nadanie, więc
/// operator słyszy dokładnie to, co pójdzie na antenę. Bez identyfikacji
/// stacji — podgląd to treść, nie nadanie.
pub struct PiperPreview {
    cache: AudioCache,
    synth: Arc<dyn Synthesizer>,
}

impl PiperPreview {
    pub fn new(cache: AudioCache, synth: Arc<dyn Synthesizer>) -> Self {
        Self { cache, synth }
    }
}

impl PreviewRenderer for PiperPreview {
    fn render_wav<'a>(&'a self, text: &'a str) -> PreviewFuture<'a> {
        Box::pin(async move {
            let (_, pcm) = self
                .cache
                .render(self.synth.as_ref(), text, &VoiceParams::default())
                .await
                .map_err(|e| e.to_string())?;
            aars_audio::wav::wav_bytes_mono(&pcm).map_err(|e| e.to_string())
        })
    }
}
