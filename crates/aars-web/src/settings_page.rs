//! Ustawienia stacji (admin) i okno ciszy.

use aars_core::settings::{QuietHours, SettingsRepo, StationSettings};
use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{Local, NaiveTime, Offset};
use serde::Deserialize;

use crate::audit;
use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::chrome::{Chrome, parse_priority, priority_value};
use crate::render::page;
use crate::state::AppState;

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    chrome: Chrome,
    callsign: String,
    id_mode: String,
    id_interval_s: String,
    tx_max_duration_s: String,
    misfire_grace_s: String,
    quiet_hours: String,
    quiet_min_priority: String,
    quiet_hint: String,
    pending: Vec<String>,
    has_station: bool,
    error: Option<String>,
    saved: bool,
    no_callsign: bool,
}

#[derive(Debug, Deserialize)]
pub struct SettingsForm {
    csrf: String,
    callsign: String,
    id_mode: String,
    id_interval_s: String,
    tx_max_duration_s: String,
    misfire_grace_s: String,
    #[serde(default)]
    quiet_hours: String,
    quiet_min_priority: String,
}

/// `22:00–06:00 lokalnie = 21:00–05:00 UTC` dla podanego przesunięcia strefy.
pub fn quiet_window_both(window: &QuietHours, offset_secs: i32) -> String {
    let shift = |t: NaiveTime| {
        t.overflowing_sub_signed(chrono::Duration::seconds(offset_secs.into()))
            .0
    };
    format!(
        "{}–{} lokalnie = {}–{} UTC",
        window.start.format("%H:%M"),
        window.end.format("%H:%M"),
        shift(window.start).format("%H:%M"),
        shift(window.end).format("%H:%M"),
    )
}

fn quiet_hint(window: Option<QuietHours>) -> String {
    let offset = Local::now().offset().fix().local_minus_utc();
    window
        .map(|w| quiet_window_both(&w, offset))
        .unwrap_or_default()
}

async fn view(
    state: &AppState,
    current: &CurrentUser,
    s: &StationSettings,
    error: Option<String>,
    saved: bool,
) -> SettingsPage {
    SettingsPage {
        chrome: Chrome::from(current),
        callsign: s.callsign.clone(),
        id_mode: if s.id_mode == aars_core::ident::IdMode::Cw {
            "cw"
        } else {
            "voice"
        }
        .to_owned(),
        id_interval_s: s.id_interval_s.to_string(),
        tx_max_duration_s: s.tx_max_duration_s.to_string(),
        misfire_grace_s: s.misfire_grace_s.to_string(),
        quiet_hours: s.quiet_hours.map(|q| q.as_text()).unwrap_or_default(),
        quiet_min_priority: priority_value(s.quiet_min_priority).to_owned(),
        quiet_hint: quiet_hint(s.quiet_hours),
        pending: crate::hardware_page::pending_for(state, s).await,
        has_station: state.station.is_some(),
        error,
        saved,
        no_callsign: s.callsign.is_empty(),
    }
}

fn server_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "błąd odczytu lub zapisu ustawień",
    )
        .into_response()
}

pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, aars_core::user::Role::Admin)?;
    match SettingsRepo::new(state.pool.clone()).load().await {
        Ok(s) => Ok(page(&view(&state, &current, &s, None, false).await)),
        Err(err) => {
            tracing::error!(%err, "odczyt ustawień zawiódł");
            Ok(server_error())
        }
    }
}

fn parse_form(form: &SettingsForm) -> Result<StationSettings, String> {
    let number = |text: &str, name: &str| {
        text.trim()
            .parse::<u64>()
            .map_err(|_| format!("{name}: podaj liczbę całkowitą"))
    };
    let id_mode = match form.id_mode.as_str() {
        "cw" => aars_core::ident::IdMode::Cw,
        "voice" => aars_core::ident::IdMode::Voice,
        _ => return Err("tryb identyfikacji: cw albo voice".to_owned()),
    };
    let quiet_hours = match form.quiet_hours.trim() {
        "" => None,
        text => Some(QuietHours::parse(text)?),
    };
    let mut parsed = StationSettings {
        callsign: form.callsign.clone(),
        id_mode,
        id_interval_s: number(&form.id_interval_s, "odstęp identyfikacji")?,
        tx_max_duration_s: number(&form.tx_max_duration_s, "limit czasu nadawania")?,
        misfire_grace_s: number(&form.misfire_grace_s, "tolerancja spóźnień")?,
        quiet_hours,
        quiet_min_priority: parse_priority(&form.quiet_min_priority)
            .ok_or_else(|| "nieznany priorytet progu ciszy".to_owned())?,
    };
    parsed.validate()?;
    Ok(parsed)
}

/// Klucze, których wartość się zmieniła.
fn changed_keys(old: &StationSettings, new: &StationSettings) -> Vec<&'static str> {
    let mut keys = Vec::new();
    let mut note = |differs: bool, key| {
        if differs {
            keys.push(key);
        }
    };
    note(old.callsign != new.callsign, "callsign");
    note(old.id_mode != new.id_mode, "id_mode");
    note(old.id_interval_s != new.id_interval_s, "id_interval_s");
    note(
        old.tx_max_duration_s != new.tx_max_duration_s,
        "tx_max_duration_s",
    );
    note(
        old.misfire_grace_s != new.misfire_grace_s,
        "misfire_grace_s",
    );
    note(old.quiet_hours != new.quiet_hours, "quiet_hours");
    note(
        old.quiet_min_priority != new.quiet_min_priority,
        "quiet_min_priority",
    );
    keys
}

pub async fn submit(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<SettingsForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, aars_core::user::Role::Admin)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let repo = SettingsRepo::new(state.pool.clone());
    let stored = match repo.load().await {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(%err, "odczyt ustawień zawiódł");
            return Ok(server_error());
        }
    };

    let new = match parse_form(&form) {
        Ok(new) => new,
        Err(message) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                page(&view(&state, &current, &stored, Some(message), false).await),
            )
                .into_response());
        }
    };

    if let Err(err) = repo.save(&new).await {
        tracing::error!(%err, "zapis ustawień zawiódł");
        return Ok(server_error());
    }
    audit(
        &state,
        current.user.id,
        "settings",
        &changed_keys(&stored, &new).join(","),
    )
    .await;
    Ok(page(&view(&state, &current, &new, None, true).await))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router;
    use aars_core::user::{Role, UserRepo};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    async fn logged_in(role: Role) -> (Router, String, String, AppState) {
        let state = AppState::for_tests(aars_core::db::test_pool().await);
        UserRepo::new(state.pool.clone())
            .create("u", "haslo", role)
            .await
            .unwrap();
        let app = router(state.clone());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("username=u&password=haslo"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or_default().to_owned())
            .unwrap();
        let (_, html) = get(&app, "/", &cookie).await;
        let csrf = html
            .split("name=\"csrf\" value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .to_owned();
        (app, cookie, csrf, state)
    }

    async fn get(app: &Router, uri: &str, cookie: &str) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn post_settings(
        app: &Router,
        cookie: &str,
        csrf: &str,
        overrides: &[(&str, &str)],
    ) -> StatusCode {
        let mut fields = [
            ("csrf", csrf),
            ("callsign", "SP7TQ"),
            ("id_mode", "voice"),
            ("id_interval_s", "600"),
            ("tx_max_duration_s", "180"),
            ("misfire_grace_s", "600"),
            ("quiet_hours", ""),
            ("quiet_min_priority", "emergency"),
        ];
        for (key, value) in overrides {
            fields.iter_mut().find(|(k, _)| k == key).unwrap().1 = value;
        }
        let body = fields
            .iter()
            .map(|(k, v)| format!("{k}={}", v.replace(':', "%3A")))
            .collect::<Vec<_>>()
            .join("&");
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/settings")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::COOKIE, cookie)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    async fn stored(state: &AppState, key: &str) -> String {
        sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_one(&state.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_admin_saves_the_callsign_uppercased() {
        let (app, cookie, csrf, state) = logged_in(Role::Admin).await;
        let status = post_settings(
            &app,
            &cookie,
            &csrf,
            &[
                ("callsign", "sp7tq"),
                ("quiet_hours", "22:00-06:00"),
                ("quiet_min_priority", "high"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stored(&state, "callsign").await, "SP7TQ");
        assert_eq!(stored(&state, "quiet_hours").await, "22:00-06:00");
        assert_eq!(stored(&state, "quiet_min_priority").await, "1");
        let audit: String =
            sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = 'settings'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert!(
            audit.contains("callsign") && audit.contains("quiet_hours"),
            "{audit}"
        );
    }

    #[tokio::test]
    async fn saving_does_not_touch_tx_enabled_or_poisoned() {
        let (app, cookie, csrf, state) = logged_in(Role::Admin).await;
        sqlx::query("UPDATE settings SET value = 'false' WHERE key = 'tx_enabled'")
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE settings SET value = 'true' WHERE key = 'poisoned'")
            .execute(&state.pool)
            .await
            .unwrap();
        post_settings(&app, &cookie, &csrf, &[]).await;
        assert_eq!(stored(&state, "tx_enabled").await, "false");
        assert_eq!(stored(&state, "poisoned").await, "true");
    }

    #[tokio::test]
    async fn out_of_range_values_are_refused_and_nothing_changes() {
        let (app, cookie, csrf, state) = logged_in(Role::Admin).await;
        let status = post_settings(
            &app,
            &cookie,
            &csrf,
            &[("tx_max_duration_s", "0"), ("callsign", "ZZ9ZZ")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(stored(&state, "tx_max_duration_s").await, "180");
        assert_eq!(stored(&state, "callsign").await, "");
        assert_eq!(
            post_settings(&app, &cookie, &csrf, &[("quiet_hours", "25:00-06:00")]).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post_settings(&app, &cookie, &csrf, &[("id_mode", "morse")]).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn a_wrong_csrf_token_is_refused() {
        let (app, cookie, _, state) = logged_in(Role::Admin).await;
        assert_eq!(
            post_settings(&app, &cookie, "zly", &[("callsign", "SP7TQ")]).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(stored(&state, "callsign").await, "");
    }

    #[tokio::test]
    async fn an_operator_cannot_open_settings() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
        assert_eq!(
            get(&app, "/settings", &cookie).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            post_settings(&app, &cookie, &csrf, &[("callsign", "SP7TQ")]).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(stored(&state, "callsign").await, "");
    }

    #[tokio::test]
    async fn the_settings_page_links_to_hardware_and_shows_pending_restart() {
        let (app, cookie, _, state) = logged_in(Role::Admin).await;
        let (_, html) = get(&app, "/settings", &cookie).await;
        assert!(html.contains("href=\"/settings/hardware\""), "{html}");
        assert!(html.contains("Nic nie czeka na restart"), "{html}");
        assert!(html.contains("tryb testowy"), "{html}");

        // Stacja działa na VOX, zapisane jest GPIO: różnica czeka na restart.
        let running = aars_core::hardware::HardwareSettings::from_config(&state.base_config);
        let mut saved = running.clone();
        saved.ptt_backend = aars_core::config::PttBackendKind::Gpio;
        aars_core::hardware::HardwareRepo::new(state.pool.clone())
            .save(&saved)
            .await
            .unwrap();
        let station = crate::state::testing::FakeStation::new(Some(running));
        let state = state.with_station(std::sync::Arc::new(station));
        let app = router(state);
        let (_, html) = get(&app, "/settings", &cookie).await;
        assert!(html.contains("ptt.backend: vox → gpio"), "{html}");
        assert!(html.contains("/station/restart"), "{html}");
    }

    #[test]
    fn the_quiet_window_is_shown_in_utc_too() {
        let window = QuietHours::parse("22:00-06:00").unwrap();
        assert_eq!(
            quiet_window_both(&window, 3600),
            "22:00–06:00 lokalnie = 21:00–05:00 UTC"
        );
        assert_eq!(
            quiet_window_both(&window, -7200),
            "22:00–06:00 lokalnie = 00:00–08:00 UTC"
        );
    }
}
