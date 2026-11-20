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

use crate::station::HardwareFactory;

/// O ile watchdog czeka dłużej niż sekwencer (ponad `tx_tail`). Sekwencer
/// ucina nadanie na `tx_max_duration` sam; strażnik z tym samym limitem
/// ścigałby się z nim o kilkadziesiąt milisekund i zatruwał stację przy
/// każdym zbyt długim komunikacie. Zapas wystarcza na zamknięcie strumienia
/// audio i `unkey()`.
const WATCHDOG_GRACE: Duration = Duration::from_secs(2);

/// Limit strażnika: PTT trzyma `tx_tail` przed audio i do `tx_max` audio,
/// więc strażnik musi dać obu, plus zapas. Bez `tx_tail` długi ogon radia
/// zatruwałby stację na zwykłym komunikacie o maksymalnej długości.
fn watchdog_limit(tx_max: Duration, tx_tail: Duration) -> Duration {
    tx_max + tx_tail + WATCHDOG_GRACE
}

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

/// Interlocki odtworzone z tabeli `settings`: wyłącznik i lepka blokada
/// strażnika przetrwują restart. Okno ciszy ustawia `quiet_hours_task`.
pub async fn load_interlocks(pool: &SqlitePool) -> Result<Arc<InMemoryInterlocks>> {
    let tx_enabled = read_setting(pool, "tx_enabled").await? == "true";
    let poisoned = read_setting(pool, "poisoned").await? == "true";
    let quiet_min: i64 = read_setting(pool, "quiet_min_priority").await?.parse().unwrap_or(0);

    let interlocks = Arc::new(InMemoryInterlocks::permissive());
    interlocks.set_tx_enabled(tx_enabled);
    interlocks.set_poisoned(poisoned);
    interlocks.set_quiet(false, Priority::from_i64(quiet_min).unwrap_or(Priority::Emergency));
    Ok(interlocks)
}

/// Jedyne miejsce, które otwiera PTT i wyjście audio (D2), przez fabrykę,
/// żeby testy podmieniły sprzęt. Ustawienia czytamy przed otwarciem sprzętu;
/// gdy wyjście audio zawiedzie po udanym PTT, PTT ginie razem z tą funkcją,
/// zanim ktokolwiek spróbuje następnego kandydata.
pub async fn build_stack(
    config: &Config,
    pool: &SqlitePool,
    factory: &dyn HardwareFactory,
) -> Result<RadioStack> {
    let tx_max_s: u64 = read_setting(pool, "tx_max_duration_s").await?.parse().unwrap_or(180);
    let id_interval_s: u64 = read_setting(pool, "id_interval_s").await?.parse().unwrap_or(600);
    let interlocks = load_interlocks(pool).await?;

    let ptt = factory
        .ptt(&config.ptt)
        .map_err(anyhow::Error::msg)
        .context("budowa backendu PTT")?;
    let sink = match factory.sink(&config.audio) {
        Ok(sink) => sink,
        Err(err) => {
            drop(ptt);
            return Err(anyhow::Error::msg(err).context("otwarcie urządzenia audio"));
        }
    };

    let tx_tail = Duration::from_millis(config.ptt.tx_tail_ms);
    let watchdog =
        Watchdog::spawn(ptt.clone(), watchdog_limit(Duration::from_secs(tx_max_s), tx_tail));

    let sequencer = Sequencer::spawn(
        ptt,
        sink,
        interlocks.clone() as Arc<dyn Interlocks>,
        SequencerConfig {
            tx_tail,
            tx_max_duration: Duration::from_secs(tx_max_s),
            id_interval: Duration::from_secs(id_interval_s),
        },
        Some(watchdog.marker()),
    );

    Ok(RadioStack { sequencer, watchdog, interlocks })
}

/// Utrwala interwencję strażnika: interlocki od razu, `settings.poisoned`
/// tak, by blokada przetrwała restart (spec §4.3, „celowo lepka”).
/// Zwraca, czy interwencja jest utrwalona.
///
/// Admin może zdjąć blokadę w trakcie tego zapisu (panel czyści strażnika,
/// bazę i interlocki). Dlatego po zapisie sprawdzamy strażnika jeszcze raz:
/// jeśli jest już czysty, cofamy własny zapis, zamiast zatruć stację na nowo.
pub async fn record_poison(
    marker: &WatchdogMarker,
    interlocks: &InMemoryInterlocks,
    pool: &SqlitePool,
) -> bool {
    interlocks.set_poisoned(true);
    match sqlx::query(
        "UPDATE settings SET value = 'true', updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
          WHERE key = 'poisoned'",
    )
    .execute(pool)
    .await
    {
        Ok(_) if marker.is_poisoned() => true,
        Ok(_) => {
            tracing::warn!("blokadę zdjęto podczas zapisu interwencji — cofam zapis");
            interlocks.set_poisoned(false);
            if let Err(err) = sqlx::query(
                "UPDATE settings SET value = 'false',
                        updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
                  WHERE key = 'poisoned'",
            )
            .execute(pool)
            .await
            {
                tracing::error!(%err, "cofnięcie flagi poisoned zawiodło");
            }
            false
        }
        Err(err) => {
            tracing::error!(%err, "zapis flagi poisoned zawiódł, ponowię");
            false
        }
    }
}

/// Co `POISON_POLL` przenosi flagę strażnika do interlocków i bazy.
/// Działa poziomem, nie zboczem: dopóki strażnik jest zatruty, a stan nie
/// jest utrwalony (interlocki czyste albo zapis do bazy zawiódł), ponawia.
/// Nie walczy z ręcznym zdjęciem blokady, bo panel czyści najpierw samego
/// strażnika — wtedy most nie ma już czego przenosić.
pub async fn watch_poison(
    marker: WatchdogMarker,
    interlocks: Arc<InMemoryInterlocks>,
    pool: SqlitePool,
    shutdown: CancelToken,
) {
    // Czy bieżąca interwencja jest już zapisana w bazie.
    let mut persisted = false;
    while !shutdown.is_cancelled() {
        if marker.is_poisoned() {
            if !persisted || !interlocks.snapshot().poisoned {
                if !persisted {
                    tracing::error!(
                        "watchdog interweniował — nadawanie zablokowane do ręcznego zdjęcia"
                    );
                }
                persisted = record_poison(&marker, &interlocks, &pool).await;
            }
        } else {
            persisted = false;
        }
        tokio::time::sleep(POISON_POLL).await;
    }
}

/// Jak często sprawdzamy, czy trwa cisza nocna.
const QUIET_POLL: Duration = Duration::from_secs(30);

/// Jedno sprawdzenie okna ciszy (czas lokalny `now`). Dotyka wyłącznie
/// `set_quiet`, więc nie walczy z innymi piszącymi do interlocków.
pub async fn apply_quiet_hours(
    pool: &SqlitePool,
    interlocks: &InMemoryInterlocks,
    now: chrono::NaiveTime,
) {
    match aars_core::settings::SettingsRepo::new(pool.clone()).load().await {
        Ok(settings) => {
            let quiet = settings.quiet_hours.is_some_and(|window| window.contains(now));
            interlocks.set_quiet(quiet, settings.quiet_min_priority);
        }
        Err(err) => tracing::error!(%err, "odczyt okna ciszy zawiódł"),
    }
}

pub async fn quiet_hours_task(
    pool: SqlitePool,
    interlocks: Arc<InMemoryInterlocks>,
    shutdown: CancelToken,
) {
    while !shutdown.is_cancelled() {
        apply_quiet_hours(&pool, &interlocks, chrono::Local::now().time()).await;
        sleep_unless_shutdown(QUIET_POLL, &shutdown).await;
    }
}

/// Jak często długi sen sprawdza sygnał zatrzymania.
const SHUTDOWN_POLL: Duration = Duration::from_millis(200);

/// Śpi `total`, ale wraca najpóźniej `SHUTDOWN_POLL` po sygnale zatrzymania:
/// `CancelToken` to flaga, nie future, więc `select!` ściga sen z odpytywaniem.
async fn sleep_unless_shutdown(total: Duration, shutdown: &CancelToken) {
    let cancelled = async {
        while !shutdown.is_cancelled() {
            tokio::time::sleep(SHUTDOWN_POLL).await;
        }
    };
    tokio::select! {
        () = tokio::time::sleep(total) => {}
        () = cancelled => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::ptt::{FakePtt, PttBackend};

    #[tokio::test]
    async fn the_quiet_window_switches_the_interlocks_with_the_clock() {
        let pool = aars_core::db::test_pool().await;
        sqlx::query("UPDATE settings SET value = '22:00-06:00' WHERE key = 'quiet_hours'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE settings SET value = '1' WHERE key = 'quiet_min_priority'")
            .execute(&pool)
            .await
            .unwrap();
        let locks = InMemoryInterlocks::permissive();
        let at = |h| chrono::NaiveTime::from_hms_opt(h, 0, 0).unwrap();

        apply_quiet_hours(&pool, &locks, at(23)).await;
        assert!(locks.snapshot().quiet_now);
        assert_eq!(locks.check(Priority::Normal), Err(aars_core::interlock::RefusalReason::QuietHours));
        assert!(locks.check(Priority::High).is_ok());

        apply_quiet_hours(&pool, &locks, at(12)).await;
        assert!(!locks.snapshot().quiet_now);
        assert!(locks.check(Priority::Normal).is_ok());
    }

    #[test]
    fn the_watchdog_waits_for_the_tail_the_audio_and_a_grace() {
        assert_eq!(
            watchdog_limit(Duration::from_secs(180), Duration::from_millis(1500)),
            Duration::from_millis(183_500)
        );
    }

    #[tokio::test]
    async fn the_quiet_hours_task_stops_promptly_on_shutdown() {
        let pool = aars_core::db::test_pool().await;
        let shutdown = CancelToken::new();
        let task = tokio::spawn(quiet_hours_task(
            pool,
            Arc::new(InMemoryInterlocks::permissive()),
            shutdown.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("zadanie ciszy nie może czekać pełnych 30 s na zatrzymanie")
            .unwrap();
    }

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

    #[tokio::test]
    async fn a_clear_that_lands_during_record_poison_is_not_undone() {
        let pool = aars_core::db::test_pool().await;
        let ptt = Arc::new(FakePtt::new());
        let watchdog = Watchdog::spawn(ptt, Duration::from_secs(60));
        // Strażnik czysty: admin zdjął blokadę, zanim most skończył zapis.
        let marker = watchdog.marker();
        let interlocks = InMemoryInterlocks::permissive();

        let persisted = record_poison(&marker, &interlocks, &pool).await;

        assert!(!persisted, "cofnięty zapis nie jest utrwaloną interwencją");
        assert!(!interlocks.snapshot().poisoned, "most nie może cofnąć zdjęcia blokady");
        assert_eq!(stored_poison(&pool).await, "false");
        watchdog.stop();
    }

    #[tokio::test]
    async fn a_failed_poison_write_is_retried_until_it_lands() {
        let pool = aars_core::db::test_pool().await;
        // Baza odmawia zapisu flagi, dopóki trigger istnieje.
        sqlx::query(
            "CREATE TRIGGER refuse_poison BEFORE UPDATE ON settings WHEN NEW.key = 'poisoned'
             BEGIN SELECT RAISE(ABORT, 'zapis zablokowany w teście'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

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
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(interlocks.snapshot().poisoned, "interlocki nie mogą czekać na bazę");
        assert_eq!(stored_poison(&pool).await, "false", "zapis miał się nie udać");

        sqlx::query("DROP TRIGGER refuse_poison").execute(&pool).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(stored_poison(&pool).await, "true", "most musi ponawiać zapis do skutku");

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(3), task).await.unwrap().unwrap();
        watchdog.stop();
    }
}
