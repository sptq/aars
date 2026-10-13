use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use aars_core::config::Config;
use aars_core::interlock::{InMemoryInterlocks, Interlocks};
use aars_core::priority::Priority;
use aars_core::sequencer::{Sequencer, SequencerConfig, SequencerHandle};
use aars_core::watchdog::{Watchdog, WatchdogHandle};

/// Złożony stos nadawczy: PTT, wyjście audio, sekwencer i watchdog.
pub struct RadioStack {
    pub sequencer: SequencerHandle,
    pub watchdog: WatchdogHandle,
    pub interlocks: Arc<InMemoryInterlocks>,
}

/// Wczytuje parametr operacyjny z tabeli `settings` (spec §9).
async fn read_setting(pool: &SqlitePool, key: &str) -> Result<String> {
    let value: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_one(pool)
        .await
        .with_context(|| format!("odczyt ustawienia {key}"))?;
    Ok(value)
}

pub async fn build_stack(config: &Config, pool: &SqlitePool) -> Result<RadioStack> {
    let ptt = aars_ptt::build(&config.ptt).context("budowa backendu PTT")?;
    let sink: Arc<dyn aars_core::sink::AudioSink> = Arc::new(
        aars_audio::output::CpalSink::open(&config.audio.device, config.audio.sample_rate)
            .context("otwarcie urządzenia audio")?,
    );

    let tx_max_s: u64 = read_setting(pool, "tx_max_duration_s").await?.parse().unwrap_or(180);
    let id_interval_s: u64 = read_setting(pool, "id_interval_s").await?.parse().unwrap_or(600);
    let tx_enabled = read_setting(pool, "tx_enabled").await? == "true";
    let poisoned = read_setting(pool, "poisoned").await? == "true";
    let quiet_min: i64 = read_setting(pool, "quiet_min_priority").await?.parse().unwrap_or(0);

    let interlocks = Arc::new(InMemoryInterlocks::permissive());
    interlocks.set_tx_enabled(tx_enabled);
    interlocks.set_poisoned(poisoned);
    interlocks.set_quiet(false, Priority::from_i64(quiet_min).unwrap_or(Priority::Emergency));

    let watchdog = Watchdog::spawn(ptt.clone(), Duration::from_secs(tx_max_s));

    let sequencer = Sequencer::spawn(
        ptt,
        sink,
        interlocks.clone() as Arc<dyn Interlocks>,
        SequencerConfig {
            tx_tail: Duration::from_millis(config.ptt.tx_tail_ms),
            tx_max_duration: Duration::from_secs(tx_max_s),
            id_interval: Duration::from_secs(id_interval_s),
        },
    );

    Ok(RadioStack { sequencer, watchdog, interlocks })
}
