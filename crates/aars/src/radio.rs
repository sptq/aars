use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use aars_core::config::Config;
use aars_core::interlock::{InMemoryInterlocks, Interlocks};
use aars_core::priority::Priority;
use aars_core::sequencer::{Sequencer, SequencerConfig, SequencerHandle};
use aars_core::sink::CancelToken;
use aars_core::watchdog::{Watchdog, WatchdogHandle, WatchdogMarker};

/// O ile watchdog czeka dłużej niż sekwencer. Sekwencer ucina nadanie na
/// `tx_max_duration` sam; strażnik z tym samym limitem ścigałby się z nim o
/// kilkadziesiąt milisekund i zatruwał stację przy każdym zbyt długim
/// komunikacie. Zapas wystarcza na zamknięcie strumienia audio i `unkey()`.
const WATCHDOG_GRACE: Duration = Duration::from_secs(2);

/// Jak często binarka przenosi flagę strażnika do interlocków i bazy.
const POISON_POLL: Duration = Duration::from_millis(500);

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

    let watchdog = Watchdog::spawn(ptt.clone(), Duration::from_secs(tx_max_s) + WATCHDOG_GRACE);

    let sequencer = Sequencer::spawn(
        ptt,
        sink,
        interlocks.clone() as Arc<dyn Interlocks>,
        SequencerConfig {
            tx_tail: Duration::from_millis(config.ptt.tx_tail_ms),
            tx_max_duration: Duration::from_secs(tx_max_s),
            id_interval: Duration::from_secs(id_interval_s),
        },
        Some(watchdog.marker()),
    );

    Ok(RadioStack { sequencer, watchdog, interlocks })
}

/// Utrwala interwencję strażnika: interlocki od razu, `settings.poisoned`
/// tak, by blokada przetrwała restart (spec §4.3, „celowo lepka”).
pub async fn record_poison(interlocks: &InMemoryInterlocks, pool: &SqlitePool) {
    interlocks.set_poisoned(true);
    if let Err(err) = sqlx::query(
        "UPDATE settings SET value = 'true', updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
          WHERE key = 'poisoned'",
    )
    .execute(pool)
    .await
    {
        tracing::error!(%err, "zapis flagi poisoned zawiódł");
    }
}

/// Co `POISON_POLL` przenosi flagę strażnika do interlocków i bazy.
/// Reaguje na zbocze „czysty → zatruty”, więc nie walczy z ręcznym
/// zdjęciem blokady w panelu, które czyści wszystkie trzy miejsca.
pub async fn watch_poison(
    marker: WatchdogMarker,
    interlocks: Arc<InMemoryInterlocks>,
    pool: SqlitePool,
    shutdown: CancelToken,
) {
    let mut was_poisoned = false;
    while !shutdown.is_cancelled() {
        let poisoned = marker.is_poisoned();
        if poisoned && !was_poisoned {
            tracing::error!("watchdog interweniował — nadawanie zablokowane do ręcznego zdjęcia");
            record_poison(&interlocks, &pool).await;
        }
        was_poisoned = poisoned;
        tokio::time::sleep(POISON_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::ptt::{FakePtt, PttBackend};

    async fn stored_poison(pool: &SqlitePool) -> String {
        read_setting(pool, "poisoned").await.unwrap()
    }

    #[tokio::test]
    async fn a_watchdog_intervention_reaches_the_interlocks_and_the_database() {
        let pool = aars_core::db::test_pool().await;
        let ptt = Arc::new(FakePtt::new());
        let watchdog = Watchdog::spawn(ptt.clone(), Duration::from_millis(50));
        let marker = watchdog.marker();
        let interlocks = Arc::new(InMemoryInterlocks::permissive());
        let shutdown = CancelToken::new();
        let task = tokio::spawn(watch_poison(
            marker.clone(),
            interlocks.clone(),
            pool.clone(),
            shutdown.clone(),
        ));

        ptt.key().unwrap();
        marker.mark_keyed();
        // Interwencja po ~100 ms, przeniesienie najpóźniej po kolejnych 500 ms.
        tokio::time::sleep(Duration::from_millis(1500)).await;

        assert!(marker.is_poisoned());
        assert!(interlocks.snapshot().poisoned, "interlocki muszą znać interwencję strażnika");
        assert_eq!(stored_poison(&pool).await, "true", "blokada musi przetrwać restart");

        // Ręczne zdjęcie w panelu czyści wszystkie trzy miejsca; most nie
        // może go cofnąć.
        marker.clear_poison();
        interlocks.set_poisoned(false);
        sqlx::query("UPDATE settings SET value = 'false' WHERE key = 'poisoned'")
            .execute(&pool)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!interlocks.snapshot().poisoned);
        assert_eq!(stored_poison(&pool).await, "false");

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("most kończy się po sygnale zatrzymania")
            .unwrap();
        watchdog.stop();
    }
}
