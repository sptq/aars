//! Strona źródeł danych: stan, aktywne ostrzeżenia, włączanie i „odśwież
//! teraz”. Obszar i interwał są tu tylko do odczytu — zmienia je administrator
//! na stronie „Moduły” (etap 9, R15). Panel nie zna crate'a źródeł — wołanie
//! runnera idzie przez `AppState::source_refresh`.

use aars_core::error::DbError;
use aars_core::source_state::{SourceConfig, SourceStateRepo, SourceStatus};
use aars_core::user::Role;
use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::audit;
use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::chrome::{Chrome, both_times_now, plural, priority_label, source_label};
use crate::render::page;
use crate::state::AppState;

/// Plakietka zdrowia źródła: (klasa CSS, tekst). Przestarzałe wygrywa z „działa”.
pub fn health(
    status: &SourceStatus,
    cfg: &SourceConfig,
    now: DateTime<Utc>,
) -> (&'static str, String) {
    if !cfg.enabled {
        ("off", "wyłączone".to_owned())
    } else if cfg.area.is_empty() {
        ("warn", "brak obszaru".to_owned())
    } else if status.consecutive_failures > 0 {
        let n = status.consecutive_failures.unsigned_abs();
        ("err", format!("błąd ({n} {})", plural(n, "próba", "próby", "prób")))
    } else if status.is_stale(cfg.interval_s, now) {
        ("warn", "przestarzałe".to_owned())
    } else {
        ("on", "działa".to_owned())
    }
}

pub(crate) struct WarningRow {
    pub(crate) summary: String,
    pub(crate) priority: &'static str,
    pub(crate) valid_from: String,
    pub(crate) valid_until: String,
}

pub(crate) struct SourceView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) css: &'static str,
    pub(crate) health: String,
    pub(crate) enabled: bool,
    pub(crate) last_run: String,
    pub(crate) last_success: String,
    pub(crate) last_error: Option<String>,
    pub(crate) area_text: String,
    pub(crate) interval_s: u64,
    pub(crate) warnings: Vec<WarningRow>,
}

fn time_or_never(at: Option<DateTime<Utc>>) -> String {
    at.map_or_else(|| "nigdy".to_owned(), both_times_now)
}

/// Widoki wszystkich źródeł; błąd bazy dostaje wołający (pulpit robi z niego notkę).
pub(crate) async fn load_views(state: &AppState) -> Result<Vec<SourceView>, DbError> {
    let repo = SourceStateRepo::new(state.pool.clone());
    let now = Utc::now();
    let mut views = Vec::new();
    for status in repo.statuses().await? {
        let Some(cfg) = repo.config(&status.source_id).await? else {
            continue;
        };
        let (css, health_text) = health(&status, &cfg, now);
        let warnings = repo
            .active_items(&status.source_id)
            .await?
            .into_iter()
            // Wiersz zostaje `active` do udanego cyklu; gdy źródło stoi,
            // ostrzeżenie po terminie nie jest już aktywne.
            .filter(|item| item.valid_until.is_none_or(|until| until > now))
            .map(|item| WarningRow {
                summary: item.summary,
                priority: priority_label(item.priority),
                valid_from: time_or_never(item.valid_from),
                valid_until: time_or_never(item.valid_until),
            })
            .collect();
        views.push(SourceView {
            name: source_label(&status.source_id).to_owned(),
            id: status.source_id.clone(),
            css,
            health: health_text,
            enabled: cfg.enabled,
            last_run: time_or_never(status.last_run_at),
            last_success: time_or_never(status.last_success_at),
            last_error: status.last_error.clone(),
            area_text: cfg.area_text,
            interval_s: cfg.interval_s,
            warnings,
        });
    }
    Ok(views)
}

#[derive(Template)]
#[template(path = "sources.html")]
struct SourcesPage {
    chrome: Chrome,
    sources: Vec<SourceView>,
    can_refresh: bool,
}

pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    match load_views(&state).await {
        Ok(sources) => Ok(page(&SourcesPage {
            chrome: Chrome::from(&current),
            sources,
            can_refresh: state.source_refresh.is_some(),
        })),
        Err(err) => Ok(crate::render::db_error("odczyt stanu źródeł zawiódł", &err)),
    }
}

#[derive(Debug, Deserialize)]
pub struct EnabledForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    enabled: String,
}

#[derive(Debug, Deserialize)]
pub struct CsrfOnly {
    #[serde(default)]
    csrf: String,
}

fn unknown_source() -> Response {
    (StatusCode::NOT_FOUND, "nie ma takiego źródła").into_response()
}

fn server_error(what: &str, err: &dyn std::fmt::Display) -> Response {
    tracing::error!(%err, "{what}");
    (StatusCode::INTERNAL_SERVER_ERROR, "błąd bazy danych").into_response()
}

/// `Ok(true)` — źródło istnieje.
async fn exists(state: &AppState, id: &str) -> Result<bool, DbError> {
    Ok(SourceStateRepo::new(state.pool.clone())
        .config(id)
        .await?
        .is_some())
}

pub async fn set_enabled(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<String>,
    Form(form): Form<EnabledForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let enabled = matches!(form.enabled.as_str(), "1" | "true" | "on");
    match SourceStateRepo::new(state.pool.clone())
        .set_enabled(&id, enabled)
        .await
    {
        Ok(0) => return Ok(unknown_source()),
        Ok(_) => {}
        Err(err) => return Ok(server_error("zmiana stanu źródła zawiodła", &err)),
    }
    audit(
        &state,
        current.user.id,
        "source_enabled",
        &format!("{id}: {}", if enabled { "włączone" } else { "wyłączone" }),
    )
    .await;
    if enabled && let Some(refresh) = &state.source_refresh {
        refresh(&id);
    }
    Ok(Redirect::to("/sources").into_response())
}

pub async fn refresh(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<String>,
    Form(form): Form<CsrfOnly>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    match exists(&state, &id).await {
        Ok(true) => {}
        Ok(false) => return Ok(unknown_source()),
        Err(err) => return Ok(server_error("odczyt źródła zawiódł", &err)),
    }
    let Some(wake) = &state.source_refresh else {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            "odświeżanie niedostępne: źródła nie są uruchomione",
        )
            .into_response());
    };
    if !wake(&id) {
        return Ok(unknown_source());
    }
    audit(&state, current.user.id, "source_refresh", &id).await;
    Ok(Redirect::to("/sources").into_response())
}

#[cfg(test)]
mod tests {
    use crate::router;
    use crate::tests::{login, state};
    use aars_core::priority::Priority;
    use aars_core::source_state::{NewTracked, SourceStateRepo};
    use aars_core::user::{Role, UserRepo};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use chrono::{DateTime, TimeZone, Utc};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    use crate::AppState;

    async fn app_as(role: Role, state: AppState) -> (Router, String, String, AppState) {
        UserRepo::new(state.pool.clone())
            .create("u", "haslo", role)
            .await
            .unwrap();
        let app = router(state.clone());
        let (cookie, csrf) = login(&app, "u", "haslo").await;
        (app, cookie, csrf, state)
    }

    async fn get(app: &Router, cookie: &str, path: &str) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn post(
        app: &Router,
        cookie: &str,
        csrf: &str,
        path: &str,
        fields: &str,
    ) -> (StatusCode, String) {
        let body = format!("csrf={csrf}&{fields}");
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn encode(text: &str) -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    async fn audit_count(state: &AppState, action: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE action = ?")
            .bind(action)
            .fetch_one(&state.pool)
            .await
            .unwrap()
    }

    async fn active_warning(state: &AppState) {
        tracked_warning(state, "Sk1", "Burze, 1. stopień", Utc::now() + chrono::Duration::hours(10)).await;
    }

    async fn tracked_warning(state: &AppState, key: &str, summary: &str, valid_until: DateTime<Utc>) {
        let at = Utc.with_ymd_and_hms(2026, 7, 23, 8, 0, 0).unwrap();
        SourceStateRepo::new(state.pool.clone())
            .upsert_active(
                &NewTracked {
                    source_id: "imgw-meteo".into(),
                    item_key: key.into(),
                    topic: "Burze".into(),
                    fingerprint: "f".into(),
                    area: "1061".into(),
                    summary: summary.into(),
                    cancel_text: "Odwołanie".into(),
                    priority: Priority::Normal,
                    valid_from: Some(at),
                    valid_until: Some(valid_until),
                    aired: true,
                    first_seen_at: at,
                    last_seen_at: at,
                    queue_item_id: None,
                    superseded: Vec::new(),
                },
                at,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_operator_sees_the_source_status_and_active_warnings() {
        let (app, cookie, _csrf, state) = app_as(Role::Operator, state().await).await;
        active_warning(&state).await;
        let (status, html) = get(&app, &cookie, "/sources").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Ostrzeżenia meteorologiczne IMGW"), "{html}");
        assert!(html.contains("Burze, 1. stopień"), "{html}");
        assert!(html.contains("Zwykły"), "{html}");
        assert!(html.contains("wyłączone"), "{html}");
        assert!(
            !html.contains("name=\"area\""),
            "operator nie dostaje pól obszaru"
        );
    }

    /// Zapisuje wyrenderowaną stronę dla kontrolera (`SOURCES_PAGE_OUT`).
    #[tokio::test]
    async fn the_admin_page_renders_a_working_source_with_warnings() {
        let state = state().await.with_source_refresh(Arc::new(|_| true));
        let (app, cookie, _csrf, state) = app_as(Role::Admin, state).await;
        let repo = SourceStateRepo::new(state.pool.clone());
        repo.save_config("imgw-meteo", "1061 Łódź", 300).await.unwrap();
        repo.set_enabled("imgw-meteo", true).await.unwrap();
        repo.record_success("imgw-meteo", Utc::now()).await.unwrap();
        active_warning(&state).await;
        let (status, html) = get(&app, &cookie, "/sources").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("działa"), "{html}");
        assert!(html.contains("Zmień w Moduły"), "{html}");
        if let Ok(path) = std::env::var("SOURCES_PAGE_OUT") {
            std::fs::write(path, &html).unwrap();
        }
    }

    #[tokio::test]
    async fn an_operator_enables_the_source_and_it_is_woken() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let state = state().await.with_source_refresh(Arc::new(move |id| {
            assert_eq!(id, "imgw-meteo");
            seen.fetch_add(1, Ordering::SeqCst);
            true
        }));
        let (app, cookie, csrf, state) = app_as(Role::Operator, state).await;
        let (status, _) = post(
            &app,
            &cookie,
            &csrf,
            "/sources/imgw-meteo/enabled",
            "enabled=1",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let cfg = SourceStateRepo::new(state.pool.clone())
            .config("imgw-meteo")
            .await
            .unwrap()
            .unwrap();
        assert!(cfg.enabled);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(audit_count(&state, "source_enabled").await, 1);
    }

    #[tokio::test]
    async fn refresh_without_running_sources_is_503() {
        let (app, cookie, csrf, _state) = app_as(Role::Operator, state().await).await;
        let (status, body) = post(&app, &cookie, &csrf, "/sources/imgw-meteo/refresh", "").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("odświeżanie niedostępne"), "{body}");
        let (_, html) = get(&app, &cookie, "/sources").await;
        assert!(html.contains("źródła nie są uruchomione"), "{html}");
    }

    #[tokio::test]
    async fn refresh_wakes_the_source_and_is_audited() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let state = state().await.with_source_refresh(Arc::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            true
        }));
        let (app, cookie, csrf, state) = app_as(Role::Operator, state).await;
        let (status, _) = post(&app, &cookie, &csrf, "/sources/imgw-meteo/refresh", "").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(audit_count(&state, "source_refresh").await, 1);
    }

    #[tokio::test]
    async fn the_sources_page_shows_the_area_read_only_with_a_link_for_the_admin() {
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        SourceStateRepo::new(state.pool.clone())
            .save_config("imgw-meteo", "1061 Łódź", 600)
            .await
            .unwrap();
        let (_, html) = get(&app, &cookie, "/sources").await;
        assert!(html.contains("1061 Łódź"), "{html}");
        assert!(html.contains("600 s"), "{html}");
        assert!(html.contains("href=\"/modules#imgw-meteo\""), "{html}");
        assert!(html.contains("Zmień w Moduły"), "{html}");
        assert!(!html.contains("name=\"area\""), "{html}");
        assert!(!html.contains("/sources/imgw-meteo/settings"), "{html}");
        // Stara trasa zapisu zniknęła (R15).
        let fields = format!("area={}&interval_s=900", encode("10"));
        let (status, _) = post(
            &app,
            &cookie,
            &csrf,
            "/sources/imgw-meteo/settings",
            &fields,
        )
        .await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED,
            "{status}"
        );
        let cfg = SourceStateRepo::new(state.pool.clone())
            .config("imgw-meteo")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cfg.interval_s, 600);

        let (app, cookie, _, _) = app_as(Role::Operator, crate::tests::state().await).await;
        let (_, html) = get(&app, &cookie, "/sources").await;
        assert!(!html.contains("Zmień w Moduły"), "operator nie dostaje linku: {html}");
    }

    #[tokio::test]
    async fn a_viewer_cannot_open_sources() {
        let (app, cookie, _csrf, _state) = app_as(Role::Viewer, state().await).await;
        let (status, _) = get(&app, &cookie, "/sources").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_unknown_source_is_404() {
        let (app, cookie, csrf, _state) = app_as(Role::Admin, state().await).await;
        for path in ["enabled", "refresh"] {
            let (status, _) = post(
                &app,
                &cookie,
                &csrf,
                &format!("/sources/nie-ma/{path}"),
                "enabled=1",
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn the_dashboard_shows_source_health_to_a_viewer() {
        let (app, cookie, _csrf, _state) = app_as(Role::Viewer, state().await).await;
        let (status, html) = get(&app, &cookie, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Źródła danych"), "{html}");
        assert!(html.contains("Ostrzeżenia meteorologiczne IMGW"), "{html}");
        assert!(html.contains("wyłączone"), "{html}");
    }

    #[tokio::test]
    async fn a_warning_past_its_validity_is_neither_listed_nor_counted() {
        let (app, cookie, _csrf, state) = app_as(Role::Operator, state().await).await;
        active_warning(&state).await;
        tracked_warning(&state, "Sk0", "Upał, wczorajszy", Utc::now() - chrono::Duration::hours(1)).await;

        let (_, html) = get(&app, &cookie, "/sources").await;
        assert!(html.contains("Burze, 1. stopień"), "{html}");
        assert!(!html.contains("Upał, wczorajszy"), "{html}");
        let (_, html) = get(&app, &cookie, "/").await;
        assert!(html.contains("aktywne ostrzeżenia: 1"), "{html}");
    }

    #[tokio::test]
    async fn the_dashboard_survives_a_broken_source_table() {
        let (app, cookie, _csrf, state) = app_as(Role::Operator, state().await).await;
        sqlx::query("DROP TABLE source_state")
            .execute(&state.pool)
            .await
            .unwrap();
        let (status, html) = get(&app, &cookie, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Nie udało się odczytać stanu źródeł."),
            "{html}"
        );
        assert!(html.contains("action=\"/tx-enabled\""), "{html}");
    }

    #[tokio::test]
    async fn a_failing_source_does_not_wear_the_disabled_class() {
        let (app, cookie, _csrf, state) = app_as(Role::Operator, state().await).await;
        let repo = SourceStateRepo::new(state.pool.clone());
        repo.set_enabled("imgw-meteo", true).await.unwrap();
        repo.save_config("imgw-meteo", "10 łódzkie", 300).await.unwrap();
        repo.record_failure("imgw-meteo", Utc::now(), "boom").await.unwrap();
        let (_, html) = get(&app, &cookie, "/sources").await;
        assert!(html.contains("badge err"), "{html}");
        assert!(!html.contains("badge off"), "{html}");
    }

    #[test]
    fn health_prefers_stale_over_working_and_reports_failures() {
        use super::health;
        use aars_core::source_state::{SourceConfig, SourceStatus};
        let now = Utc.with_ymd_and_hms(2026, 7, 23, 12, 0, 0).unwrap();
        let area = aars_core::area::parse_area("10").unwrap();
        let cfg = |enabled, area: Vec<_>| SourceConfig {
            enabled,
            area_text: aars_core::area::area_text(&area),
            area,
            interval_s: 300,
        };
        let status = |success_ago_min: Option<i64>, failures| SourceStatus {
            source_id: "imgw-meteo".into(),
            enabled: true,
            last_run_at: Some(now),
            last_success_at: success_ago_min.map(|m| now - chrono::Duration::minutes(m)),
            consecutive_failures: failures,
            last_error: None,
        };
        assert_eq!(
            health(&status(Some(1), 0), &cfg(false, area.clone()), now).1,
            "wyłączone"
        );
        assert_eq!(
            health(&status(Some(1), 0), &cfg(true, vec![]), now).1,
            "brak obszaru"
        );
        assert_eq!(
            health(&status(Some(1), 0), &cfg(true, area.clone()), now).1,
            "działa"
        );
        assert_eq!(
            health(&status(Some(1), 3), &cfg(true, area.clone()), now).1,
            "błąd (3 próby)"
        );
        assert_eq!(
            health(&status(Some(90), 0), &cfg(true, area), now).1,
            "przestarzałe"
        );
    }
}
