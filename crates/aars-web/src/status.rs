//! Dane stałego paska stanu i najbliższego biuletynu. Czyste funkcje plus
//! odczyty wyłącznie lokalne (baza, pola `AppState`); nic tu nie sięga do sieci.

use aars_core::error::DbError;
use aars_core::hardware::ConfigOrigin;
use aars_core::interlock::{InterlockState, Interlocks};
use aars_core::module::{BULLETIN, ModuleRepo};
use aars_core::settings::SettingsRepo;
use chrono::NaiveTime;

use crate::auth::{AuthRejection, CurrentUser};
use crate::chrome::Chrome;
use crate::render::page;
use crate::state::AppState;
use askama::Template;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

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

/// Cel powrotu po wyłączniku (D6): tylko lokalna ścieżka — od `/`, bez `//`
/// na początku, bez `\`, znaków sterujących i `://`; inaczej pulpit. Chroni
/// też `Redirect::to`, który panikuje na nagłówku z CR/LF.
pub fn safe_back(back: Option<&str>) -> &str {
    match back {
        Some(path)
            if path.starts_with('/')
                && !path.starts_with("//")
                && !path.contains('\\')
                && !path.contains("://")
                && !path.chars().any(char::is_control) =>
        {
            path
        }
        _ => "/",
    }
}

#[derive(Debug, Deserialize)]
pub struct BackQuery {
    pub back: Option<String>,
}

#[derive(Template)]
#[template(path = "status_bar.html")]
pub struct StatusFragment {
    pub chrome: Chrome,
}

/// `GET /fragments/status`: sam pasek stanu, odświeżany przez htmx (D7).
/// Wygasła sesja przy żądaniu htmx kończy się `HX-Redirect`, żeby
/// przeglądarka przeszła na logowanie, a pasek nie zamienił się w formularz.
pub async fn fragment(
    State(state): State<AppState>,
    current: Result<CurrentUser, AuthRejection>,
    headers: HeaderMap,
    Query(query): Query<BackQuery>,
) -> Response {
    let current = match current {
        Ok(current) => current,
        Err(rejection) => {
            let htmx = headers
                .get("HX-Request")
                .is_some_and(|v| v.as_bytes() == b"true");
            let target = match rejection {
                AuthRejection::Anonymous => Some("/login"),
                AuthRejection::MustChangePassword => Some("/account/password"),
                _ => None,
            };
            return match (htmx, target) {
                (true, Some(target)) => (StatusCode::OK, [("HX-Redirect", target)]).into_response(),
                _ => rejection.into_response(),
            };
        }
    };
    let mut chrome = Chrome::load(&state, &current).await;
    chrome.back = safe_back(query.back.as_deref()).to_owned();
    page(&StatusFragment { chrome })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testing::FakeStation;
    use crate::tests::logged_in;
    use aars_core::priority::Priority;
    use aars_core::user::Role;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use chrono::{Local, TimeZone};
    use std::sync::Arc;
    use tower::ServiceExt;

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
    fn the_bar_state_follows_the_interlocks() {
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

    #[test]
    fn safe_back_accepts_only_local_paths() {
        for ok in ["/", "/sources", "/messages/3/edit", "/settings/hardware"] {
            assert_eq!(safe_back(Some(ok)), ok);
        }
        for bad in [
            "",
            "sources",
            "//evil.example",
            "/\\evil.example",
            "https://evil.example",
            "/x\r\nSet-Cookie: a=b",
            "/a\tb",
            "/redirect?to=http://evil",
        ] {
            assert_eq!(safe_back(Some(bad)), "/", "{bad:?}");
        }
        assert_eq!(safe_back(None), "/");
    }

    async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let response = app.clone().oneshot(request).await.unwrap();
        let (status, headers) = (response.status(), response.headers().clone());
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    async fn switch(app: &Router, cookie: &str, body: String) -> (StatusCode, String) {
        let request = Request::builder()
            .method("POST")
            .uri("/tx-enabled")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(body))
            .unwrap();
        let (status, headers, _) = send(app, request).await;
        let location = headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        (status, location)
    }

    async fn fragment_html(app: &Router, cookie: &str, back: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .uri(format!("/fragments/status?back={back}"))
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap();
        let (status, _, html) = send(app, request).await;
        (status, html)
    }

    #[tokio::test]
    async fn the_kill_switch_returns_to_the_page_it_was_used_on() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
        let (status, location) = switch(
            &app,
            &cookie,
            format!("csrf={csrf}&enabled=0&back=%2Fsources"),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, "/sources");
        assert!(!state.interlocks.snapshot().tx_enabled);

        let (status, location) = switch(&app, &cookie, format!("csrf={csrf}&enabled=1")).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, "/");
        assert!(state.interlocks.snapshot().tx_enabled);
    }

    #[tokio::test]
    async fn a_foreign_back_target_redirects_to_the_dashboard() {
        let (app, cookie, csrf, _state) = logged_in(Role::Operator).await;
        for back in [
            "%2F%2Fevil.example",
            "https%3A%2F%2Fevil.example",
            "%2F%5Cevil.example",
            "%2Fx%0D%0ASet-Cookie%3A%20a%3Db",
        ] {
            let (status, location) =
                switch(&app, &cookie, format!("csrf={csrf}&enabled=0&back={back}")).await;
            assert_eq!(status, StatusCode::SEE_OTHER, "{back}");
            assert_eq!(location, "/", "{back}");
        }
    }

    #[tokio::test]
    async fn the_status_fragment_shows_the_switch_only_to_operators() {
        let (app, cookie, _csrf, _state) = logged_in(Role::Viewer).await;
        let (status, html) = fragment_html(&app, &cookie, "%2Fmap").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!html.contains("<html"), "{html}");
        assert!(html.contains("id=\"statusbar\""), "{html}");
        assert!(html.contains("Nadawanie:"), "{html}");
        assert!(html.contains("włączone"), "{html}");
        assert!(!html.contains("action=\"/tx-enabled\""), "{html}");
        assert!(html.contains("u · Podgląd"), "{html}");
        assert!(html.contains("bez znaku"), "{html}");

        let (app, cookie, _csrf, state) = logged_in(Role::Operator).await;
        let (_, html) = fragment_html(&app, &cookie, "%2Fmap").await;
        assert!(html.contains("Zatrzymaj nadawanie"), "{html}");
        assert!(html.contains("name=\"enabled\" value=\"0\""), "{html}");
        assert!(html.contains("name=\"back\" value=\"/map\""), "{html}");

        state.interlocks.set_tx_enabled(false);
        let (_, html) = fragment_html(&app, &cookie, "%2Fmap").await;
        assert!(!html.contains("action=\"/tx-enabled\""), "{html}");
        assert!(!html.contains("Włącz nadawanie"), "{html}");

        state.interlocks.set_poisoned(true);
        let (_, html) = fragment_html(&app, &cookie, "%2Fmap").await;
        assert!(!html.contains("action=\"/tx-enabled\""), "{html}");
        assert!(!html.contains("/watchdog/clear"), "{html}");
    }

    #[tokio::test]
    async fn the_status_fragment_ignores_a_foreign_back() {
        let (app, cookie, _csrf, _state) = logged_in(Role::Operator).await;
        let (_, html) = fragment_html(&app, &cookie, "%2F%2Fevil.example").await;
        assert!(html.contains("name=\"back\" value=\"/\""), "{html}");
        assert!(!html.contains("evil.example"), "{html}");
    }

    #[tokio::test]
    async fn the_status_fragment_survives_a_broken_settings_table() {
        let (app, cookie, _csrf, state) = logged_in(Role::Operator).await;
        sqlx::query("DROP TABLE settings")
            .execute(&state.pool)
            .await
            .unwrap();
        let (status, html) = fragment_html(&app, &cookie, "%2F").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Zatrzymaj nadawanie"), "{html}");
        assert!(!html.contains("Cisza nocna"), "{html}");
    }

    #[tokio::test]
    async fn an_expired_session_sends_htmx_to_the_login_page() {
        let app = crate::router(state().await);
        let request = Request::builder()
            .uri("/fragments/status?back=%2F")
            .header("HX-Request", "true")
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("HX-Redirect").unwrap(), "/login");
        assert!(body.is_empty(), "{body}");

        let request = Request::builder()
            .uri("/fragments/status?back=%2F")
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = send(&app, request).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers.get(header::LOCATION).unwrap(), "/login");
    }

    #[tokio::test]
    async fn the_dashboard_switch_carries_its_back_target() {
        let (app, cookie, _csrf, _state) = logged_in(Role::Operator).await;
        let request = Request::builder()
            .uri("/")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let (_, _, html) = send(&app, request).await;
        let back = html
            .find("name=\"back\" value=\"/\"")
            .expect("pulpit musi nieść cel powrotu");
        assert!(html[back..].contains("Zatrzymaj nadawanie"), "{html}");
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

    #[tokio::test]
    async fn the_bar_offers_only_stop_when_enabled_under_the_watchdog() {
        let (app, cookie, _csrf, state) = logged_in(Role::Operator).await;
        state.interlocks.set_poisoned(true);
        assert!(state.interlocks.snapshot().tx_enabled);
        let (_, html) = fragment_html(&app, &cookie, "%2F").await;
        assert!(html.contains("zablokowane — watchdog"), "{html}");
        assert!(html.contains("Zatrzymaj nadawanie"), "{html}");
        assert!(!html.contains("Włącz nadawanie"), "{html}");

        let request = Request::builder()
            .uri("/")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let (_, _, page) = send(&app, request).await;
        let bar = page.find("id=\"statusbar\"").expect("pasek stanu");
        let end = page[bar..]
            .find("</header>")
            .map_or(page.len(), |e| bar + e);
        assert!(page[bar..end].contains("Zatrzymaj nadawanie"), "{page}");
        assert!(!page[bar..end].contains("Włącz nadawanie"), "{page}");
    }

    #[tokio::test]
    async fn a_temporary_password_gets_neither_the_status_nor_the_switch() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
        sqlx::query("UPDATE users SET must_change_password = 1")
            .execute(&state.pool)
            .await
            .unwrap();

        let request = Request::builder()
            .uri("/fragments/status?back=%2F")
            .header(header::COOKIE, &cookie)
            .header("HX-Request", "true")
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("HX-Redirect").unwrap(), "/account/password");
        assert!(body.is_empty(), "{body}");

        let (status, headers, body) = send(&app, fragment_request(&cookie)).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers.get(header::LOCATION).unwrap(), "/account/password");
        assert!(!body.contains("statusbar"), "{body}");

        for htmx in [false, true] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/tx-enabled")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, &cookie);
            if htmx {
                request = request.header("HX-Request", "true");
            }
            let request = request
                .body(Body::from(format!("csrf={csrf}&enabled=0&back=%2F")))
                .unwrap();
            let (status, headers, _) = send(&app, request).await;
            assert_eq!(status, StatusCode::SEE_OTHER, "htmx={htmx}");
            assert_eq!(
                headers.get(header::LOCATION).unwrap(),
                "/account/password",
                "htmx={htmx}"
            );
            assert!(state.interlocks.snapshot().tx_enabled, "htmx={htmx}");
        }
    }

    /// Sam pasek stanu z pełnej strony (od `id="statusbar"` do `</header>`).
    fn bar_of(page: &str) -> &str {
        let start = page.find("id=\"statusbar\"").expect("pasek stanu");
        let end = page[start..]
            .find("</header>")
            .map_or(page.len(), |e| start + e);
        &page[start..end]
    }

    #[tokio::test]
    async fn the_bar_never_offers_to_start_transmission() {
        for role in [Role::Operator, Role::Admin] {
            for poisoned in [false, true] {
                let (app, cookie, _csrf, state) = logged_in(role).await;
                state.interlocks.set_tx_enabled(false);
                state.interlocks.set_poisoned(poisoned);
                let (_, fragment) = fragment_html(&app, &cookie, "%2F").await;
                let (_, _, page) = send(&app, page_request(&cookie)).await;
                for (what, html) in [("fragment", fragment.as_str()), ("strona", bar_of(&page))] {
                    let case = format!("{role:?} watchdog={poisoned} {what}");
                    assert!(html.contains("Nadawanie:"), "{case}: {html}");
                    assert!(!html.contains("action=\"/tx-enabled\""), "{case}: {html}");
                    assert!(!html.contains("Włącz nadawanie"), "{case}: {html}");
                }

                state.interlocks.set_tx_enabled(true);
                let (_, fragment) = fragment_html(&app, &cookie, "%2F").await;
                let (_, _, page) = send(&app, page_request(&cookie)).await;
                for (what, html) in [("fragment", fragment.as_str()), ("strona", bar_of(&page))] {
                    let case = format!("{role:?} watchdog={poisoned} {what}");
                    assert!(html.contains("Zatrzymaj nadawanie"), "{case}: {html}");
                    assert!(
                        html.contains("name=\"enabled\" value=\"0\""),
                        "{case}: {html}"
                    );
                    assert!(!html.contains("Włącz nadawanie"), "{case}: {html}");
                }
            }
        }
    }

    #[tokio::test]
    async fn only_the_tx_badge_is_a_polite_live_region() {
        let (app, cookie, _csrf, _state) = logged_in(Role::Operator).await;
        let (_, fragment) = fragment_html(&app, &cookie, "%2F").await;
        let (_, _, page) = send(&app, page_request(&cookie)).await;
        for html in [fragment.as_str(), bar_of(&page)] {
            assert_eq!(html.matches("aria-live").count(), 1, "{html}");
            let at = html.find("aria-live=\"polite\"").expect("aria-live");
            let open = html[..at].rfind('<').unwrap_or(0);
            assert!(html[open..at].contains("class=\"badge "), "{html}");
            assert!(!html[open..at].contains("statusbar"), "{html}");
        }
    }

    fn page_request(cookie: &str) -> Request<Body> {
        Request::builder()
            .uri("/")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    }

    fn fragment_request(cookie: &str) -> Request<Body> {
        Request::builder()
            .uri("/fragments/status?back=%2F")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    }
}
