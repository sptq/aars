use std::sync::Arc;

use aars_audio::cache::AudioCache;
use aars_core::dispatcher::{RenderFuture, Renderer};
use aars_core::ident::{IdMode, MorseParams, morse_pcm};
use aars_core::synth::{Pcm, Synthesizer, VoiceParams};
use aars_web::preview::{BUSY, PreviewFuture, PreviewRenderer};
use tokio::sync::Semaphore;

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
    /// Jeden podgląd naraz: synteza to osobny proces i CPU, a panel nie może
    /// przez nią zagłodzić nadawania.
    permit: Arc<Semaphore>,
}

impl PiperPreview {
    pub fn new(cache: AudioCache, synth: Arc<dyn Synthesizer>) -> Self {
        Self {
            cache,
            synth,
            permit: Arc::new(Semaphore::new(1)),
        }
    }
}

impl PreviewRenderer for PiperPreview {
    fn render_wav<'a>(&'a self, text: &'a str) -> PreviewFuture<'a> {
        Box::pin(async move {
            let _permit = self.permit.try_acquire().map_err(|_| BUSY.to_owned())?;

            // `synthesize` jest synchroniczne (uruchamia proces i czeka), więc
            // idzie na wątek blokujący, nie na wątek roboczy tokio.
            let cache = self.cache.clone();
            let synth = self.synth.clone();
            let owned = text.to_owned();
            let handle = tokio::runtime::Handle::current();
            let rendered = tokio::task::spawn_blocking(move || {
                handle.block_on(cache.render(synth.as_ref(), &owned, &VoiceParams::default()))
            })
            .await
            .map_err(|e| format!("wątek syntezy: {e}"))?
            .map_err(|e| e.to_string());

            if let Err(err) = self.cache.prune().await {
                tracing::error!(%err, "czyszczenie cache po podglądzie zawiodło");
            }

            let (_, pcm) = rendered?;
            if pcm.samples.is_empty() {
                return Err("brak treści do odczytania po normalizacji".to_owned());
            }
            aars_audio::wav::wav_bytes_mono(&pcm).map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::synth::{FakeSynth, SynthError};

    struct EmptySynth;
    impl Synthesizer for EmptySynth {
        fn synthesize(&self, _: &str, _: &VoiceParams) -> Result<Pcm, SynthError> {
            Ok(Pcm {
                samples: Vec::new(),
                sample_rate: 22_050,
            })
        }
    }

    async fn preview(synth: Arc<dyn Synthesizer>) -> (PiperPreview, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "aars-prev-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = AudioCache::new(aars_core::db::test_pool().await, dir.clone(), 1 << 20);
        (PiperPreview::new(cache, synth), dir)
    }

    /// Unikalny w procesie: na macOS dwa testy potrafią dostać te same
    /// nanosekundy i wspólny katalog (os error 17), stąd licznik.
    fn rand_suffix() -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_preview_renders_wav_bytes() {
        let (p, dir) = preview(Arc::new(FakeSynth::new(22_050))).await;
        let bytes = p.render_wav("Uwaga").await.unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn text_that_normalises_to_nothing_is_an_error() {
        let (p, dir) = preview(Arc::new(EmptySynth)).await;
        let err = p.render_wav("...").await.unwrap_err();
        assert!(err.contains("brak treści do odczytania"), "{err}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_preview_while_one_runs_is_busy() {
        let (p, dir) = preview(Arc::new(FakeSynth::new(22_050))).await;
        let _held = p.permit.clone().try_acquire_owned().unwrap();
        let err = p.render_wav("Uwaga").await.unwrap_err();
        assert_eq!(err, BUSY);
        std::fs::remove_dir_all(dir).ok();
    }
}
