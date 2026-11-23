//! Dane stałego paska stanu i najbliższego biuletynu. Czyste funkcje plus
//! odczyty wyłącznie lokalne (baza, pola `AppState`); nic tu nie sięga do sieci.

use aars_core::error::DbError;
use aars_core::hardware::ConfigOrigin;
use aars_core::interlock::{InterlockState, Interlocks};
use aars_core::module::{BULLETIN, ModuleRepo};
use aars_core::settings::SettingsRepo;
use chrono::NaiveTime;

use crate::state::AppState;

/// Lokalna kopia `parse_times` z runnera biuletynu (`aars-web` nie zależy od
/// `aars-sources`): ta sama składnia, pusta lista albo błąd = `None`.
fn parse_times(text: &str) -> Option<Vec<NaiveTime>> {
    let times: Option<Vec<NaiveTime>> = text
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| NaiveTime::parse_from_str(v, "%H:%M").ok())
        .collect();
    times.filter(|t| !t.is_empty())
}

/// Pory biuletynu, gdy lista w ustawieniach jest pusta albo nieczytelna;
/// te same, których używa runner biuletynu w `aars-sources`.
const DEFAULT_TIMES: &str = "07:00,19:00";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxState {
    Watchdog,
    Unavailable,
    Disabled,
    Enabled,
}

impl TxState {
    pub fn label(self) -> &'static str {
        match self {
            TxState::Watchdog => "zablokowane — watchdog",
            TxState::Unavailable => "niedostępne",
            TxState::Disabled => "wyłączone",
            TxState::Enabled => "włączone",
        }
    }

    pub fn css(self) -> &'static str {
        match self {
            TxState::Watchdog | TxState::Unavailable => "err",
            TxState::Disabled => "off",
            TxState::Enabled => "on",
        }
    }
}

pub fn tx_state(tx_enabled: bool, poisoned: bool, unavailable: bool) -> TxState {
    if poisoned {
        TxState::Watchdog
    } else if unavailable {
        TxState::Unavailable
    } else if !tx_enabled {
        TxState::Disabled
    } else {
        TxState::Enabled
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusBar {
    pub tx: TxState,
    pub tx_enabled: bool,
    pub can_start: bool,
    pub quiet: Option<bool>,
    pub callsign: String,
    pub clock: String,
}

pub fn status_bar(
    interlocks: &InterlockState,
    unavailable: bool,
    quiet: Option<bool>,
    callsign: String,
    now: NaiveTime,
) -> StatusBar {
    StatusBar {
        tx: tx_state(interlocks.tx_enabled, interlocks.poisoned, unavailable),
        tx_enabled: interlocks.tx_enabled,
        can_start: !interlocks.tx_enabled && !interlocks.poisoned && !unavailable,
        quiet,
        callsign,
        clock: now.format("%H:%M").to_string(),
    }
}

pub(crate) fn apply_notes(state: &AppState) -> (Option<String>, Option<String>) {
    let (mut tx_unavailable, mut last_good_note) = (None, None);
    if let Some(station) = &state.station {
        let status = station.apply_status();
        let error = status
            .error
            .unwrap_or_else(|| "brak działającej konfiguracji sprzętu".to_owned());
        match status.origin {
            ConfigOrigin::None => tx_unavailable = Some(error),
            ConfigOrigin::LastGood => last_good_note = Some(error),
            _ => {}
        }
    }
    (tx_unavailable, last_good_note)
}

pub async fn load_status(state: &AppState, now: chrono::DateTime<chrono::Local>) -> StatusBar {
    let interlocks = state.interlocks.snapshot();
    let unavailable = apply_notes(state).0.is_some();
    let (quiet, stored_callsign) = match SettingsRepo::new(state.pool.clone()).load().await {
        Ok(settings) => (
            settings.quiet_hours.map(|_| settings.quiet_now(now.time())),
            settings.callsign,
        ),
        Err(err) => {
            tracing::warn!(%err, "pasek stanu: nie odczytano ustawień stacji");
            (None, String::new())
        }
    };
    let callsign = match &state.station {
        Some(station) => station.running_station().callsign,
        None => stored_callsign,
    };
    status_bar(&interlocks, unavailable, quiet, callsign, now.time())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextBulletin {
    Off,
    Today(NaiveTime),
    Tomorrow(NaiveTime),
}

impl NextBulletin {
    pub fn label(&self) -> String {
        match self {
            NextBulletin::Off => "wyłączony".to_owned(),
            NextBulletin::Today(t) => t.format("%H:%M").to_string(),
            NextBulletin::Tomorrow(t) => format!("jutro {}", t.format("%H:%M")),
        }
    }
}

pub fn next_bulletin(enabled: bool, times: &str, now: NaiveTime) -> NextBulletin {
    if !enabled {
        return NextBulletin::Off;
    }
    let mut list = parse_times(times)
        .or_else(|| parse_times(DEFAULT_TIMES))
        .unwrap_or_default();
    list.sort();
    if let Some(next) = list.iter().find(|t| **t > now) {
        return NextBulletin::Today(*next);
    }
    match list.first() {
        Some(first) => NextBulletin::Tomorrow(*first),
        None => NextBulletin::Off,
    }
}

pub async fn load_next_bulletin(state: &AppState, now: NaiveTime) -> Result<NextBulletin, DbError> {
    let repo = ModuleRepo::new(state.pool.clone());
    if !repo.is_enabled(&BULLETIN).await? {
        return Ok(NextBulletin::Off);
    }
    let settings = repo.settings(&BULLETIN).await?;
    let times = settings
        .get("times")
        .map(String::as_str)
        .unwrap_or(DEFAULT_TIMES);
    Ok(next_bulletin(true, times, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testing::FakeStation;
    use aars_core::priority::Priority;
    use chrono::{Local, TimeZone};
    use std::sync::Arc;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn local(h: u32, m: u32) -> chrono::DateTime<Local> {
        Local.with_ymd_and_hms(2026, 11, 23, h, m, 0).unwrap()
    }

    async fn state() -> AppState {
        AppState::for_tests(aars_core::db::test_pool().await)
    }

    #[test]
    fn the_badge_prefers_watchdog_then_unavailable_then_disabled() {
        assert_eq!(tx_state(true, true, true), TxState::Watchdog);
        assert_eq!(tx_state(false, true, false), TxState::Watchdog);
        assert_eq!(tx_state(true, false, true), TxState::Unavailable);
        assert_eq!(tx_state(false, false, true), TxState::Unavailable);
        assert_eq!(tx_state(false, false, false), TxState::Disabled);
        assert_eq!(tx_state(true, false, false), TxState::Enabled);
        assert_eq!(
            (TxState::Watchdog.label(), TxState::Watchdog.css()),
            ("zablokowane — watchdog", "err")
        );
        assert_eq!(
            (TxState::Unavailable.label(), TxState::Unavailable.css()),
            ("niedostępne", "err")
        );
        assert_eq!(
            (TxState::Disabled.label(), TxState::Disabled.css()),
            ("wyłączone", "off")
        );
        assert_eq!(
            (TxState::Enabled.label(), TxState::Enabled.css()),
            ("włączone", "on")
        );
    }

    #[test]
    fn stop_is_offered_whenever_enabled_and_start_only_when_nothing_blocks() {
        for bits in 0..8u8 {
            let (tx_enabled, poisoned, unavailable) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
            let locks = InterlockState {
                poisoned,
                tx_enabled,
                inhibited: false,
                quiet_now: false,
                quiet_min_priority: Priority::Emergency,
            };
            let bar = status_bar(&locks, unavailable, None, "SP7TQ".into(), t(12, 0));
            assert_eq!(bar.tx_enabled, tx_enabled, "{bits}");
            assert_eq!(
                bar.can_start,
                !tx_enabled && !poisoned && !unavailable,
                "{bits}"
            );
            assert_eq!(
                bar.tx,
                tx_state(tx_enabled, poisoned, unavailable),
                "{bits}"
            );
        }
    }

    #[test]
    fn next_bulletin_is_the_next_time_today_or_tomorrow() {
        assert_eq!(
            next_bulletin(true, "07:00,19:00", t(12, 0)),
            NextBulletin::Today(t(19, 0))
        );
        assert_eq!(
            next_bulletin(true, "07:00,19:00", t(6, 59)),
            NextBulletin::Today(t(7, 0))
        );
        assert_eq!(
            next_bulletin(
                true,
                "07:00,19:00",
                NaiveTime::from_hms_opt(19, 0, 30).unwrap()
            ),
            NextBulletin::Tomorrow(t(7, 0))
        );
        assert_eq!(
            next_bulletin(false, "07:00,19:00", t(12, 0)),
            NextBulletin::Off
        );
        assert_eq!(
            next_bulletin(true, "zła", t(12, 0)),
            NextBulletin::Today(t(19, 0))
        );
        assert_eq!(
            next_bulletin(true, "", t(20, 0)),
            NextBulletin::Tomorrow(t(7, 0))
        );
        assert_eq!(NextBulletin::Off.label(), "wyłączony");
        assert_eq!(NextBulletin::Today(t(19, 0)).label(), "19:00");
        assert_eq!(NextBulletin::Tomorrow(t(7, 0)).label(), "jutro 07:00");
    }

    #[tokio::test]
    async fn quiet_is_reported_only_when_configured() {
        let state = state().await;
        let repo = SettingsRepo::new(state.pool.clone());
        let mut s = repo.load().await.unwrap();
        s.quiet_hours = Some(aars_core::settings::QuietHours::parse("22:00-06:00").unwrap());
        repo.save(&s).await.unwrap();
        let bar = load_status(&state, local(23, 0)).await;
        assert_eq!(bar.quiet, Some(true));
        assert_eq!(bar.clock, "23:00");
        assert_eq!(load_status(&state, local(12, 0)).await.quiet, Some(false));

        s.quiet_hours = None;
        repo.save(&s).await.unwrap();
        assert_eq!(load_status(&state, local(23, 0)).await.quiet, None);
    }

    #[tokio::test]
    async fn the_callsign_is_the_running_one() {
        let state = state().await;
        let repo = SettingsRepo::new(state.pool.clone());
        let mut s = repo.load().await.unwrap();
        s.callsign = "SP9ABC".into();
        repo.save(&s).await.unwrap();
        assert_eq!(load_status(&state, local(12, 0)).await.callsign, "SP9ABC");

        let mut station = FakeStation::new(None);
        station.station.callsign = "SP7TQ".into();
        let state = state.with_station(Arc::new(station));
        assert_eq!(load_status(&state, local(12, 0)).await.callsign, "SP7TQ");
    }

    #[tokio::test]
    async fn a_station_without_hardware_is_unavailable() {
        let mut station = FakeStation::new(None);
        station.status.origin = ConfigOrigin::None;
        let state = state().await.with_station(Arc::new(station));
        assert_eq!(
            load_status(&state, local(12, 0)).await.tx,
            TxState::Unavailable
        );
    }

    #[tokio::test]
    async fn the_status_survives_a_broken_settings_table() {
        let state = state().await;
        sqlx::query("DROP TABLE settings")
            .execute(&state.pool)
            .await
            .unwrap();
        let bar = load_status(&state, local(12, 0)).await;
        assert_eq!(bar.tx, TxState::Enabled);
        assert_eq!(bar.quiet, None);
        assert_eq!(bar.callsign, "");
    }

    #[tokio::test]
    async fn the_next_bulletin_follows_the_module_settings() {
        let state = state().await;
        let modules = ModuleRepo::new(state.pool.clone());
        modules.register(&state.modules).await.unwrap();
        assert_eq!(
            load_next_bulletin(&state, t(12, 0)).await.unwrap(),
            NextBulletin::Off
        );
        modules.set_enabled(&BULLETIN, true).await.unwrap();
        assert_eq!(
            load_next_bulletin(&state, t(12, 0)).await.unwrap(),
            next_bulletin(true, "07:00,19:00", t(12, 0))
        );
        assert_eq!(
            load_next_bulletin(&state, t(20, 0)).await.unwrap(),
            NextBulletin::Tomorrow(t(7, 0))
        );
    }
}
