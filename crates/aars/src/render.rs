use std::sync::Arc;

use aars_audio::cache::AudioCache;
use aars_audio::resample::apply_gain_db;
use aars_core::dispatcher::{RenderFuture, Renderer};
use aars_core::ident::{IdMode, MorseParams, morse_pcm};
use aars_core::settings::SettingsRepo;
use aars_core::synth::{Pcm, Synthesizer, VoiceParams};
use aars_web::preview::{BUSY, PreviewFuture, PreviewRenderer};
use tokio::sync::Semaphore;

/// Renderer produkcyjny: synteza przez cache, znak wywoławczy złożony raz
/// przy starcie, bo nie zmienia się między nadaniami.
pub struct PiperRenderer {
    cache: AudioCache,
    synth: Arc<dyn Synthesizer>,
    ident: Option<Pcm>,
    /// Poziom szczytowy w dBFS; nakładany po cache (w cache leży PCM bez niego).
    gain_db: f32,
    /// Tempo mowy czytane przy każdym renderze: zmiana w panelu działa od
    /// następnego nadania, bez restartu.
    settings: SettingsRepo,
}

impl PiperRenderer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cache: AudioCache,
        synth: Arc<dyn Synthesizer>,
        callsign: &str,
        id_mode: IdMode,
        sample_rate: u32,
        gain_db: f32,
        settings: SettingsRepo,
        speech_rate: f32,
    ) -> Self {
        let voice = VoiceParams { rate: speech_rate, ..VoiceParams::default() };
        let ident = if callsign.is_empty() {
            tracing::warn!("znak wywoławczy nieustawiony — nadania bez identyfikacji");
            None
        } else {
            match id_mode {
                IdMode::Cw => Some(morse_pcm(callsign, &MorseParams::default(), sample_rate)),
                IdMode::Voice => match synth.synthesize(callsign, &voice) {
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

        let ident = ident.map(|pcm| apply_gain_db(&pcm, gain_db));
        Self {
            cache,
            synth,
            ident,
            gain_db,
            settings,
        }
    }
}

impl Renderer for PiperRenderer {
    fn render<'a>(&'a self, text: &'a str) -> RenderFuture<'a> {
        Box::pin(async move {
            let voice = VoiceParams { rate: self.settings.speech_rate().await, ..VoiceParams::default() };
            self.cache
                .render(self.synth.as_ref(), text, &voice)
                .await
                .map(|(_, pcm)| apply_gain_db(&pcm, self.gain_db))
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
    gain_db: f32,
    settings: SettingsRepo,
}

impl PiperPreview {
    pub fn new(
        cache: AudioCache,
        synth: Arc<dyn Synthesizer>,
        gain_db: f32,
        settings: SettingsRepo,
    ) -> Self {
        Self {
            settings,
            cache,
            synth,
            permit: Arc::new(Semaphore::new(1)),
            gain_db,
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
            let voice = VoiceParams { rate: self.settings.speech_rate().await, ..VoiceParams::default() };
            let handle = tokio::runtime::Handle::current();
            let rendered = tokio::task::spawn_blocking(move || {
                handle.block_on(cache.render(synth.as_ref(), &owned, &voice))
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
            aars_audio::wav::wav_bytes_mono(&apply_gain_db(&pcm, self.gain_db)).map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::synth::{FakeSynth, SynthError};

    struct EmptySynth;
    impl Synthesizer for EmptySynth {
        fn sample_rate(&self) -> u32 {
            22_050
        }

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
        let pool = aars_core::db::test_pool().await;
        let cache = AudioCache::new(pool.clone(), dir.clone(), 1 << 20);
        (PiperPreview::new(cache, synth, -3.0, SettingsRepo::new(pool)), dir)
    }

    /// Unikalny w procesie: na macOS dwa testy potrafią dostać te same
    /// nanosekundy i wspólny katalog (os error 17), stąd licznik.
    fn rand_suffix() -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn peak(pcm: &Pcm) -> f32 {
        f32::from(pcm.samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0))
    }

    async fn renderer(gain_db: f32, mode: IdMode) -> (PiperRenderer, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("aars-rend-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = aars_core::db::test_pool().await;
        let cache = AudioCache::new(pool.clone(), dir.clone(), 1 << 20);
        let r = PiperRenderer::new(
            cache,
            Arc::new(FakeSynth::new(22_050)),
            "SP7TQ",
            mode,
            22_050,
            gain_db,
            SettingsRepo::new(pool),
            0.87,
        );
        (r, dir)
    }

    #[tokio::test]
    async fn the_output_level_follows_gain_db() {
        let (quiet, d1) = renderer(-6.0, IdMode::Voice).await;
        let (loud, d2) = renderer(-3.0, IdMode::Voice).await;
        let q = peak(&quiet.render("Uwaga").await.unwrap());
        let l = peak(&loud.render("Uwaga").await.unwrap());
        let want = 32_767.0 * 10f32.powf(-6.0 / 20.0);
        assert!((q - want).abs() <= want * 0.01, "{q} vs {want}");
        assert!(l > q);
        // identyfikacja głosowa też idzie przez wzmocnienie
        let ident = peak(&quiet.ident().unwrap());
        assert!((ident - want).abs() <= want * 0.01, "{ident}");
        std::fs::remove_dir_all(d1).ok();
        std::fs::remove_dir_all(d2).ok();
    }

    #[tokio::test]
    async fn the_cw_ident_follows_gain_db_too() {
        let (r, dir) = renderer(-6.0, IdMode::Cw).await;
        let want = 32_767.0 * 10f32.powf(-6.0 / 20.0);
        let p = peak(&r.ident().unwrap());
        assert!((p - want).abs() <= want * 0.01, "{p} vs {want}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn the_preview_follows_gain_db() {
        let dir = std::env::temp_dir().join(format!("aars-prevg-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = aars_core::db::test_pool().await;
        let cache = AudioCache::new(pool.clone(), dir.clone(), 1 << 20);
        let p = PiperPreview::new(cache, Arc::new(FakeSynth::new(22_050)), -6.0, SettingsRepo::new(pool));
        let bytes = p.render_wav("Uwaga").await.unwrap();
        let peak = bytes[44..].as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c).unsigned_abs()).max().unwrap();
        let want = 32_767.0 * 10f32.powf(-6.0 / 20.0);
        assert!((f32::from(peak) - want).abs() <= want * 0.01, "{peak}");
        std::fs::remove_dir_all(dir).ok();
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

    /// Zapisuje tempo każdej prośby o syntezę.
    struct RateSynth(std::sync::Mutex<Vec<f32>>);
    impl Synthesizer for RateSynth {
        fn sample_rate(&self) -> u32 {
            22_050
        }
        fn synthesize(&self, _: &str, voice: &VoiceParams) -> Result<Pcm, SynthError> {
            self.0.lock().unwrap().push(voice.rate);
            Ok(Pcm { samples: vec![1000; 100], sample_rate: 22_050 })
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_station_speech_rate_reaches_transmissions_and_previews_live() {
        let dir = std::env::temp_dir().join(format!("aars-rate-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = aars_core::db::test_pool().await;
        let settings = SettingsRepo::new(pool.clone());
        let cache = AudioCache::new(pool.clone(), dir.clone(), 1 << 20);
        let synth = Arc::new(RateSynth(Default::default()));
        let renderer = PiperRenderer::new(
            cache.clone(),
            synth.clone(),
            "SP7TQ",
            IdMode::Voice,
            22_050,
            -3.0,
            SettingsRepo::new(pool.clone()),
            0.93,
        );
        let preview = PiperPreview::new(cache, synth.clone(), -3.0, SettingsRepo::new(pool.clone()));

        // Domyślne 0,93 z migracji; identyfikacja głosowa dostała tempo ze startu.
        renderer.render("Pierwszy").await.unwrap();
        preview.render_wav("Drugi").await.unwrap();
        // Zmiana w panelu działa od następnego renderu, bez restartu.
        settings.kv_put("speech_rate", "1.10").await.unwrap();
        renderer.render("Trzeci").await.unwrap();
        preview.render_wav("Czwarty").await.unwrap();
        let got = synth.0.lock().unwrap().clone();
        // identyfikacja głosowa, nadanie, podgląd, nadanie, podgląd
        assert_eq!(got, [0.93, 0.93, 0.93, 1.10, 1.10]);
        std::fs::remove_dir_all(dir).ok();
    }
}
