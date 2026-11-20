use std::path::PathBuf;

use aars_core::synth::{Pcm, SynthError, Synthesizer, VoiceParams};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::wav::{read_wav_mono, write_wav_mono};

/// Cache audio adresowany treścią: ten sam tekst i te same parametry głosu
/// dają ten sam plik, więc podgląd w panelu i nadanie na pasmo korzystają z
/// jednego WAV-a (spec §4.1).
pub struct AudioCache {
    pool: SqlitePool,
    dir: PathBuf,
    max_bytes: u64,
}

impl AudioCache {
    pub fn new(pool: SqlitePool, dir: PathBuf, max_bytes: u64) -> Self {
        Self { pool, dir, max_bytes }
    }

    /// Klucz zawiera tekst i parametry głosu — zmiana któregokolwiek z nich
    /// daje inne audio, więc musi dać inny wpis.
    pub fn cache_key(text: &str, voice: &VoiceParams) -> String {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        hasher.update([0u8]);
        hasher.update(voice.voice.as_deref().unwrap_or("").as_bytes());
        hasher.update([0u8]);
        hasher.update(format!("{:.3}", voice.rate).as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Zwraca ścieżkę i PCM dla tekstu, syntezując tylko wtedy, gdy trzeba.
    pub async fn render(
        &self,
        synth: &dyn Synthesizer,
        text: &str,
        voice: &VoiceParams,
    ) -> Result<(PathBuf, Pcm), SynthError> {
        let key = Self::cache_key(text, voice);

        let existing: Option<String> =
            sqlx::query_scalar("SELECT path FROM audio_cache WHERE hash = ?")
                .bind(&key)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| SynthError::Engine(format!("odczyt cache: {e}")))?;

        if let Some(path) = existing {
            let path = PathBuf::from(path);
            if path.exists() {
                let pcm = read_wav_mono(&path)?;
                sqlx::query(
                    "UPDATE audio_cache SET last_used_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE hash = ?",
                )
                .bind(&key)
                .execute(&self.pool)
                .await
                .map_err(|e| SynthError::Engine(format!("aktualizacja cache: {e}")))?;
                return Ok((path, pcm));
            }
            // Wpis bez pliku: ktoś wyczyścił katalog pod nami. Syntezujemy ponownie.
            tracing::warn!(%key, path = %path.display(), "wpis cache bez pliku, ponowna synteza");
            sqlx::query("DELETE FROM audio_cache WHERE hash = ?")
                .bind(&key)
                .execute(&self.pool)
                .await
                .map_err(|e| SynthError::Engine(format!("czyszczenie wpisu cache: {e}")))?;
        }

        let pcm = synth.synthesize(text, voice)?;
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{key}.wav"));
        write_wav_mono(&path, &pcm)?;

        let bytes = std::fs::metadata(&path)?.len() as i64;
        sqlx::query("INSERT INTO audio_cache (hash, path, bytes, duration_ms) VALUES (?, ?, ?, ?)")
            .bind(&key)
            .bind(path.to_string_lossy().as_ref())
            .bind(bytes)
            .bind(pcm.duration().as_millis() as i64)
            .execute(&self.pool)
            .await
            .map_err(|e| SynthError::Engine(format!("zapis wpisu cache: {e}")))?;

        Ok((path, pcm))
    }

    /// Usuwa najdawniej używane wpisy, aż suma rozmiarów zmieści się w limicie.
    /// Zwraca liczbę zwolnionych bajtów.
    pub async fn prune(&self) -> Result<u64, SynthError> {
        let rows: Vec<(String, String, i64)> =
            sqlx::query_as("SELECT hash, path, bytes FROM audio_cache ORDER BY last_used_at DESC")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| SynthError::Engine(format!("odczyt cache do czyszczenia: {e}")))?;

        let mut kept: u64 = 0;
        let mut freed: u64 = 0;

        for (index, (hash, path, bytes)) in rows.into_iter().enumerate() {
            let bytes = bytes.max(0) as u64;
            // Najświeższy wpis zostaje bezwarunkowo. Wyrzucenie jedynego
            // wpisu, bo sam przekracza limit, zamieniłoby cache w maszynę do
            // ponownej syntezy tego samego tekstu.
            if index == 0 || kept + bytes <= self.max_bytes {
                kept += bytes;
                continue;
            }

            std::fs::remove_file(&path).ok();
            sqlx::query("DELETE FROM audio_cache WHERE hash = ?")
                .bind(&hash)
                .execute(&self.pool)
                .await
                .map_err(|e| SynthError::Engine(format!("usuwanie wpisu cache: {e}")))?;
            freed += bytes;
        }

        if freed > 0 {
            tracing::info!(freed_bytes = freed, "cache audio wyczyszczony");
        }
        Ok(freed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::synth::FakeSynth;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn tmp_dir(tag: &str) -> Tmp {
        let dir = std::env::temp_dir().join(format!("aars-cache-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }

    #[tokio::test]
    async fn the_same_text_is_synthesized_only_once() {
        let pool = aars_core::db::test_pool().await;
        let dir = tmp_dir("hit");
        let cache = AudioCache::new(pool, dir.0.clone(), 10 * 1024 * 1024);
        let synth = FakeSynth::new(48_000);
        let voice = VoiceParams::default();

        let (path_a, pcm_a) = cache.render(&synth, "ostrzeżenie", &voice).await.unwrap();
        let (path_b, pcm_b) = cache.render(&synth, "ostrzeżenie", &voice).await.unwrap();

        assert_eq!(path_a, path_b);
        assert_eq!(pcm_a, pcm_b);
        assert_eq!(synth.requests().len(), 1, "drugie wywołanie musi trafić w cache");
    }

    #[tokio::test]
    async fn different_voice_params_are_different_cache_entries() {
        let pool = aars_core::db::test_pool().await;
        let dir = tmp_dir("voice");
        let cache = AudioCache::new(pool, dir.0.clone(), 10 * 1024 * 1024);
        let synth = FakeSynth::new(48_000);

        cache.render(&synth, "test", &VoiceParams::default()).await.unwrap();
        cache
            .render(&synth, "test", &VoiceParams { voice: None, rate: 1.2 })
            .await
            .unwrap();

        assert_eq!(synth.requests().len(), 2, "inne tempo to inne audio");
    }

    #[tokio::test]
    async fn a_cache_entry_whose_file_vanished_is_resynthesized() {
        let pool = aars_core::db::test_pool().await;
        let dir = tmp_dir("gone");
        let cache = AudioCache::new(pool, dir.0.clone(), 10 * 1024 * 1024);
        let synth = FakeSynth::new(48_000);
        let voice = VoiceParams::default();

        let (path, _) = cache.render(&synth, "test", &voice).await.unwrap();
        std::fs::remove_file(&path).unwrap();

        let (again, _) = cache.render(&synth, "test", &voice).await.unwrap();
        assert!(again.exists());
        assert_eq!(synth.requests().len(), 2);
    }

    #[tokio::test]
    async fn prune_evicts_least_recently_used_until_under_the_limit() {
        let pool = aars_core::db::test_pool().await;
        let dir = tmp_dir("prune");
        let synth = FakeSynth::new(48_000);
        let voice = VoiceParams::default();

        // Limit na tyle mały, że zmieści się jeden wpis, ale nie dwa:
        // FakeSynth daje ~2400 próbek na znak, czyli ~70 kB na komunikat.
        let cache = AudioCache::new(pool.clone(), dir.0.clone(), 100_000);
        let (old_path, _) = cache.render(&synth, "stary komunikat", &voice).await.unwrap();
        let (new_path, _) = cache.render(&synth, "nowy komunikat", &voice).await.unwrap();

        // Cofnij czas użycia starszego wpisu, by LRU miało co wybrać.
        sqlx::query("UPDATE audio_cache SET last_used_at = '2000-01-01T00:00:00Z' WHERE path = ?")
            .bind(old_path.to_string_lossy().as_ref())
            .execute(&pool)
            .await
            .unwrap();

        let freed = cache.prune().await.unwrap();

        assert!(freed > 0, "prune musiał coś zwolnić");
        assert!(!old_path.exists(), "najdawniej używany wpis powinien zniknąć");
        assert!(new_path.exists(), "najnowszy wpis musi zostać");
    }

    #[tokio::test]
    async fn prune_never_evicts_the_only_entry_even_if_it_exceeds_the_limit() {
        let pool = aars_core::db::test_pool().await;
        let dir = tmp_dir("single");
        let synth = FakeSynth::new(48_000);

        // Limit mniejszy niż jeden komunikat. Wyrzucenie go zamieniłoby cache
        // w maszynę do ponownej syntezy tego samego tekstu.
        let cache = AudioCache::new(pool, dir.0.clone(), 1_000);
        let (path, _) = cache.render(&synth, "komunikat", &VoiceParams::default()).await.unwrap();

        cache.prune().await.unwrap();

        assert!(path.exists(), "ostatni wpis zostaje, choć przekracza limit");
    }
}
