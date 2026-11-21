pub mod account;
pub mod assets;
pub mod auth;
pub mod chrome;
pub mod hardware_page;
pub mod logs;
pub mod messages;
pub mod modules_page;
pub mod preview;
pub mod queue_page;
pub mod render;
pub mod settings_page;
pub mod setup_page;
pub mod sources_page;
pub mod state;
pub mod update_page;
pub mod users;

use aars_core::error::DbError;
use aars_core::interlock::Interlocks;
use std::collections::HashMap;

use aars_core::queue::{EnqueueRequest, QueueItem};
use aars_core::user::{AuthError, Role, UserRepo};
use askama::Template;
use axum::Form;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::chrome::{Chrome, both_times_now, parse_priority, priority_label};
use crate::render::{db_error, page};
pub use crate::state::AppState;

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    error: Option<String>,
    version: &'static str,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    chrome: Chrome,
    tx_enabled: bool,
    poisoned: bool,
    pending: Vec<QueueRow>,
    full: bool,
    /// Notka zamiast tabeli, gdy odczyt kolejki zawiódł.
    queue_error: Option<String>,
    sources: Vec<sources_page::SourceView>,
    /// Notka zamiast karty źródeł, gdy odczyt ich stanu zawiódł.
    sources_error: Option<String>,
    /// Brak stosu nadawczego (R6): powód do bannera dla każdej roli.
    tx_unavailable: Option<String>,
    /// Stacja działa na ostatniej działającej konfiguracji (tylko admin).
    last_good_note: Option<String>,
    /// Tryb kreatora bez włączonego admina (pasek dla zalogowanych).
    no_admin: bool,
}

#[derive(Template)]
#[template(path = "compose.html")]
struct ComposePage {
    chrome: Chrome,
}

#[derive(Template)]
#[template(path = "queue_fragment.html")]
struct QueueFragment {
    pending: Vec<QueueRow>,
    chrome: Chrome,
    /// Pełna strona kolejki: przyciski i odświeżanie zostają przy limicie 200.
    full: bool,
}

pub(crate) struct QueueRow {
    id: i64,
    group: Option<String>,
    repeat_label: String,
    priority: String,
    not_before: String,
    text: String,
    interrupting: bool,
    /// Powód wstrzymania po odmowie interlocka; surowe błędy nie trafiają do UI.
    held: Option<String>,
}

fn queue_row(item: QueueItem, sizes: &HashMap<String, i64>) -> QueueRow {
    let repeat_label = match item.repeat_group.as_ref().and_then(|g| sizes.get(g)) {
        Some(size) => format!("{}/{}", item.repeat_index + 1, size),
        None => String::new(),
    };
    QueueRow {
        id: item.id,
        group: item.repeat_group,
        repeat_label,
        priority: priority_label(item.priority).to_owned(),
        not_before: both_times_now(item.not_before),
        text: item.rendered_text,
        interrupting: item.interrupting,
        held: item.error.filter(|e| e.starts_with(aars_core::dispatcher::HELD_PREFIX)),
    }
}

/// Wiersze kolejki; błąd jest już w logu (`tracing::error!`).
async fn queue_rows(state: &AppState, limit: i64) -> Result<Vec<QueueRow>, DbError> {
    let sizes = state.queue.group_sizes().await.inspect_err(|err| {
        tracing::error!(%err, "odczyt rozmiarów serii zawiódł");
    })?;
    let items = state.queue.pending(limit).await.inspect_err(|err| {
        tracing::error!(%err, "odczyt kolejki zawiódł");
    })?;
    Ok(items.into_iter().map(|item| queue_row(item, &sizes)).collect())
}

#[derive(Debug, Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct CsrfForm {
    csrf: String,
}

#[derive(Debug, Deserialize)]
struct ComposeForm {
    csrf: String,
    text: String,
    priority: String,
    #[serde(default = "one")]
    repeat: u32,
    #[serde(default)]
    every: u64,
    #[serde(default)]
    interrupting: Option<String>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
struct TxEnabledForm {
    csrf: String,
    enabled: String,
}

async fn login_page(State(state): State<AppState>) -> Response {
    // R12: bez admina w trybie `wizard` logować się nie ma kim.
    if let Some(redirect) = setup_page::wizard_redirect(&state).await {
        return redirect;
    }
    page(&LoginPage {
        error: None,
        version: state.version,
    })
}

/// `/healthz`: żywy panel i wersja, która go serwuje (R17).
async fn healthz(State(state): State<AppState>) -> String {
    format!("ok {}", state.version)
}

async fn login_submit(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    let repo = UserRepo::new(state.pool.clone());

    match repo.authenticate(&form.username, &form.password).await {
        Ok(user) => match repo.start_session(user.id, None).await {
            Ok(session) => (
                StatusCode::SEE_OTHER,
                [
                    (header::LOCATION, "/".to_owned()),
                    (header::SET_COOKIE, auth::session_cookie(&session.id, state.secure_cookies)),
                ],
            )
                .into_response(),
            Err(err) => {
                tracing::error!(%err, "utworzenie sesji zawiodło");
                (StatusCode::INTERNAL_SERVER_ERROR, "błąd sesji").into_response()
            }
        },
        Err(AuthError::InvalidCredentials | AuthError::Disabled) => {
            tracing::warn!(username = %form.username, "nieudane logowanie");
            (
                StatusCode::UNAUTHORIZED,
                page(&LoginPage {
                    error: Some("Niepoprawna nazwa użytkownika lub hasło.".to_owned()),
                    version: state.version,
                }),
            )
                .into_response()
        }
        Err(err) => {
            tracing::error!(%err, "błąd uwierzytelniania");
            (StatusCode::INTERNAL_SERVER_ERROR, "błąd uwierzytelniania").into_response()
        }
    }
}

async fn logout(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: header::HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AuthRejection> {
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    if let Some(session_id) =
        auth::session_id_from_cookies(headers.get(header::COOKIE).and_then(|v| v.to_str().ok()))
    {
        let _ = UserRepo::new(state.pool.clone()).end_session(&session_id).await;
    }

    Ok((
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/login".to_owned()),
            (header::SET_COOKIE, auth::clear_cookie(state.secure_cookies)),
        ],
    )
        .into_response())
}

async fn dashboard(
    State(state): State<AppState>,
    current: Result<CurrentUser, AuthRejection>,
) -> Response {
    // R12: w trybie `wizard` bez admina pulpit prowadzi do kreatora tylko
    // żądania bez sesji (dlatego `Result`). Zalogowany operator zostaje na
    // pulpicie, bo tu jest wyłącznik nadawania (przegląd 8–9 M-3).
    let current = match current {
        Ok(current) => current,
        Err(rejection) => {
            if let Some(redirect) = setup_page::wizard_redirect(&state).await {
                return redirect;
            }
            return rejection.into_response();
        }
    };
    let no_admin = setup_page::wizard_redirect(&state).await.is_some();
    let interlocks = state.interlocks.snapshot();
    // Błąd kolejki nie może zabrać wyłącznika nadawania: notka zamiast tabeli.
    let (pending, queue_error) = match queue_rows(&state, 50).await {
        Ok(rows) => (rows, None),
        Err(_) => (
            Vec::new(),
            Some(
                "Nie udało się odczytać kolejki — szczegóły w dzienniku stacji. \
                 Wyłącznik nadawania działa."
                    .to_owned(),
            ),
        ),
    };

    let (sources, sources_error) = match sources_page::load_views(&state).await {
        Ok(views) => (views, None),
        Err(err) => {
            tracing::error!(%err, "odczyt stanu źródeł zawiódł");
            (Vec::new(), Some("Nie udało się odczytać stanu źródeł.".to_owned()))
        }
    };

    let (mut tx_unavailable, mut last_good_note) = (None, None);
    if let Some(station) = &state.station {
        let status = station.apply_status();
        let error = status
            .error
            .unwrap_or_else(|| "brak działającej konfiguracji sprzętu".to_owned());
        match status.origin {
            aars_core::hardware::ConfigOrigin::None => tx_unavailable = Some(error),
            aars_core::hardware::ConfigOrigin::LastGood => last_good_note = Some(error),
            _ => {}
        }
    }

    page(&DashboardPage {
        chrome: Chrome::from(&current),
        tx_unavailable,
        last_good_note,
        no_admin,
        tx_enabled: interlocks.tx_enabled,
        poisoned: interlocks.poisoned,
        pending,
        full: false,
        queue_error,
        sources,
        sources_error,
    })
}

async fn compose_page(current: CurrentUser) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    Ok(page(&ComposePage { chrome: Chrome::from(&current) }))
}

async fn compose_submit(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<ComposeForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let text = form.text.trim().to_owned();
    if text.is_empty() {
        // Puste nadanie to wejście na pasmo z ciszą — odmawiamy.
        return Ok((StatusCode::BAD_REQUEST, "treść komunikatu nie może być pusta").into_response());
    }

    let Some(priority) = parse_priority(&form.priority) else {
        return Ok((StatusCode::BAD_REQUEST, "nieznany priorytet").into_response());
    };

    if form.every > aars_core::message::MAX_REPEAT_INTERVAL_S {
        return Ok((StatusCode::BAD_REQUEST, aars_core::message::REPEAT_INTERVAL_ERROR)
            .into_response());
    }

    let request = EnqueueRequest {
        message_id: None,
        rendered_text: text,
        priority,
        interrupting: form.interrupting.is_some(),
        not_before: None,
        expires_at: None,
        repeat_count: form.repeat.clamp(1, 20),
        repeat_interval: std::time::Duration::from_secs(form.every),
        created_by: Some(current.user.id),
    };

    match state.queue.enqueue(request).await {
        Ok(ids) => {
            tracing::info!(
                user = %current.user.username,
                count = ids.len(),
                "komunikat dodany z panelu"
            );
            audit(&state, current.user.id, "compose", &format!("{} zadań", ids.len())).await;
            Ok(Redirect::to("/").into_response())
        }
        Err(err) => {
            tracing::error!(%err, "dodanie komunikatu z panelu zawiodło");
            Ok((StatusCode::INTERNAL_SERVER_ERROR, "błąd kolejki").into_response())
        }
    }
}

async fn set_tx_enabled(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<TxEnabledForm>,
) -> Result<Response, AuthRejection> {
    // Kill switch jest dostępny operatorowi: zatrzymanie nadawania nie może
    // wymagać admina (spec §8).
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let enabled = matches!(form.enabled.as_str(), "1" | "true" | "on");
    state.interlocks.set_tx_enabled(enabled);

    // Stan musi przetrwać restart, więc ląduje też w `settings`.
    if let Err(err) = sqlx::query(
        "UPDATE settings SET value = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
          WHERE key = 'tx_enabled'",
    )
    .bind(if enabled { "true" } else { "false" })
    .execute(&state.pool)
    .await
    {
        tracing::error!(%err, "zapis stanu nadawania zawiódł");
    }

    tracing::warn!(user = %current.user.username, enabled, "zmiana stanu nadawania z panelu");
    audit(&state, current.user.id, "tx_enabled", if enabled { "włączone" } else { "wyłączone" })
        .await;

    Ok(Redirect::to("/").into_response())
}

/// Zdejmuje blokadę po interwencji watchdoga. Tylko admin (Ruling R4):
/// zdjęcie przywraca automatyczne nadawanie po awarii sprzętowej.
async fn clear_watchdog(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Admin)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    // Najpierw sam strażnik: binarka przenosi jego flagę do interlocków,
    // więc wyczyszczenie interlocków przy wciąż zatrutym strażniku nic by
    // nie dało.
    if let Some(clear) = &state.watchdog_clear {
        clear();
    }

    if let Err(err) = sqlx::query(
        "UPDATE settings SET value = 'false', updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
          WHERE key = 'poisoned'",
    )
    .execute(&state.pool)
    .await
    {
        // Bez zapisu blokada wróciłaby po restarcie — nie udajemy sukcesu.
        tracing::error!(%err, "zapis zdjęcia blokady watchdoga zawiódł");
        return Ok((StatusCode::INTERNAL_SERVER_ERROR, "błąd bazy").into_response());
    }
    state.interlocks.set_poisoned(false);

    tracing::warn!(user = %current.user.username, "blokada watchdoga zdjęta z panelu");
    audit(&state, current.user.id, "watchdog_clear", "blokada zdjęta").await;

    Ok(Redirect::to("/").into_response())
}

async fn cancel_item(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: header::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<i64>,
    axum::extract::Query(view): axum::extract::Query<ViewQuery>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    // HTMX wysyła token nagłówkiem, bo przycisk nie jest formularzem.
    require_csrf(&current.csrf_token, headers.get("x-csrf-token").and_then(|v| v.to_str().ok()))?;

    match state.queue.cancel(id).await {
        Ok(count) => {
            tracing::info!(user = %current.user.username, id, count, "anulowanie zadania z panelu");
            audit(&state, current.user.id, "queue_cancel", &format!("zadanie {id}")).await;
        }
        Err(err) => tracing::error!(%err, id, "anulowanie zadania zawiodło"),
    }

    // Odpowiadamy fragmentem, więc HTMX podmienia tabelę bez przeładowania.
    Ok(queue_fragment_response(&state, &current, view.full).await)
}

/// `?full=1` oznacza fragment strony `/queue` (limit 200 zamiast 50).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ViewQuery {
    #[serde(default, deserialize_with = "flag")]
    pub(crate) full: bool,
}

/// Flaga z adresu: szablony wysyłają `?full=1`, a serde dla `bool`
/// przyjmuje tylko `true`/`false` (bez tego fragment pełnej kolejki to 400).
fn flag<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    Ok(matches!(value.as_str(), "1" | "true" | "on"))
}

pub(crate) async fn queue_fragment_response(
    state: &AppState,
    current: &CurrentUser,
    full: bool,
) -> Response {
    let pending = match queue_rows(state, if full { 200 } else { 50 }).await {
        Ok(rows) => rows,
        Err(err) => return db_error("odczyt kolejki zawiódł", &err),
    };
    page(&QueueFragment {
        pending,
        chrome: Chrome::from(current),
        full,
    })
}

async fn queue_fragment(
    State(state): State<AppState>,
    current: CurrentUser,
    axum::extract::Query(view): axum::extract::Query<ViewQuery>,
) -> Response {
    queue_fragment_response(&state, &current, view.full).await
}

/// Zapis do dziennika audytu: kto kazał i co (spec §5).
pub(crate) async fn audit(state: &AppState, user_id: i64, action: &str, detail: &str) {
    if let Err(err) =
        sqlx::query("INSERT INTO audit_log (user_id, action, detail) VALUES (?, ?, ?)")
            .bind(user_id)
            .bind(action)
            .bind(detail)
            .execute(&state.pool)
            .await
    {
        tracing::error!(%err, "zapis do dziennika audytu zawiódł");
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_page).post(login_submit))
        // Kreator pierwszego konta: bez sesji, z własnymi warunkami (R12).
        .route("/setup", get(setup_page::page_view).post(setup_page::submit))
        .route("/logout", post(logout))
        .route("/account/password", get(account::password_page).post(account::password_submit))
        .route("/compose", get(compose_page).post(compose_submit))
        .route("/messages", get(messages::list).post(messages::create))
        .route("/messages/new", get(messages::new_page))
        .route("/messages/next", get(messages::next_hint))
        .route("/messages/{id}/edit", get(messages::edit_page))
        .route("/messages/{id}", post(messages::update))
        .route("/messages/{id}/enabled", post(messages::set_enabled))
        .route("/messages/{id}/delete", post(messages::delete))
        .route("/messages/{id}/send", post(messages::send))
        .route("/messages/{id}/preview.wav", get(preview::stored))
        .route("/preview", post(preview::from_text))
        .route("/tx-enabled", post(set_tx_enabled))
        .route("/watchdog/clear", post(clear_watchdog))
        .route("/queue/{id}/cancel", post(cancel_item))
        .route("/fragments/queue", get(queue_fragment))
        .route("/queue", get(queue_page::page_view))
        .route("/logs", get(logs::page_view))
        .route("/settings", get(settings_page::page_view).post(settings_page::submit))
        .route("/settings/position", post(settings_page::submit_position))
        .route(
            "/settings/hardware",
            get(hardware_page::page_view).post(hardware_page::save),
        )
        .route("/settings/hardware/test-ptt", post(hardware_page::test_ptt))
        .route("/settings/hardware/test-audio", post(hardware_page::test_audio))
        .route("/station/restart", post(hardware_page::restart))
        .route("/settings/update", get(update_page::page_view))
        .route("/settings/update/check", post(update_page::check))
        .route("/settings/update/install", post(update_page::install))
        .route("/users", get(users::page_view).post(users::create))
        .route("/users/{id}/role", post(users::set_role))
        .route("/users/{id}/disabled", post(users::set_disabled))
        .route("/users/{id}/reset", post(users::reset))
        .route("/users/{id}/delete", post(users::delete))
        .route("/queue/group/{group}/cancel", post(queue_page::cancel_group))
        .route("/sources", get(sources_page::page_view))
        .route("/sources/{id}/enabled", post(sources_page::set_enabled))
        .route("/sources/{id}/refresh", post(sources_page::refresh))
        .route("/modules", get(modules_page::page_view))
        .route("/modules/{id}/enabled", post(modules_page::set_enabled))
        .route("/modules/{id}/settings", post(modules_page::save_settings))
        .route("/healthz", get(healthz))
        .route("/assets/{*path}", get(assets::serve))
        .layer(axum::middleware::from_fn_with_state(state.clone(), host_check))
        // Zewnętrzna warstwa: adres klienta przed `Host` i przed trasami.
        .layer(axum::middleware::from_fn_with_state(state.clone(), client_check))
        .with_state(state)
}

/// Odpowiedź dla klienta spoza sieci lokalnej (`403`, przegląd N-1).
pub const NOT_LOCAL_PAGE: &str = "<!doctype html>
<html lang=\"pl\"><head><meta charset=\"utf-8\">
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
<title>AARS: dostęp tylko z sieci lokalnej</title></head>
<body style=\"font-family: sans-serif; max-width: 40em; margin: 2em auto; padding: 0 1em; line-height: 1.5\">
<h1>Dostęp tylko z sieci lokalnej</h1>
<p>Panel stacji odpowiada wyłącznie urządzeniom z sieci lokalnej stacji
(adresy prywatne IPv4, link-local, ULA i ta sama sieć <code>/64</code> IPv6).
Twój adres jest spoza tej sieci.</p>
<p>Zdalny dostęp włącza administrator stacji: w pliku
<code>/etc/aars/aars.toml</code> w sekcji <code>[server]</code> ustawia
<code>allow_remote = true</code> i restartuje usługę
(<code>sudo systemctl restart aars</code>).</p>
<p><strong>Uwaga:</strong> panel działa po zwykłym HTTP, bez szyfrowania.
Hasła i sesja idą jawnym tekstem, a konta operatora i administratora
kluczują nadajnik. Nie wystawiaj panelu do internetu (przekierowanie portu,
otwarty IPv6). Do zdalnej pracy użyj VPN do sieci stacji, np. WireGuard,
wtedy <code>allow_remote</code> zwykle nie jest potrzebne.</p>
</body></html>
";

/// Cały panel tylko dla sieci lokalnej (przegląd N-1): ta sama reguła co
/// kreator `/setup`. Bez adresu klienta (serwer bez `ConnectInfo`) odmowa,
/// a nie zgadywanie. `/healthz` też podlega kontroli; pętla zwrotna jest
/// lokalna, więc nadzór na samej stacji działa.
async fn client_check(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !state.lan_only {
        return next.run(request).await;
    }
    match setup_page::client_ip(request.extensions()) {
        Some(ip) if aars_core::netlocal::is_local_client(ip, &state.local_nets) => {
            next.run(request).await
        }
        other => {
            tracing::warn!(client = ?other, path = %request.uri().path(), "klient spoza sieci lokalnej — odmowa");
            (
                StatusCode::FORBIDDEN,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                NOT_LOCAL_PAGE,
            )
                .into_response()
        }
    }
}

/// Odpowiedź na żądanie pod obcą nazwą (`421`).
pub const FOREIGN_HOST: &str = "Panel stacji odpowiada tylko pod nazwą albo adresem stacji. \
     Otwórz go pod adresem IP stacji (np. http://192.168.1.20:8080) albo dopisz tę nazwę \
     do server.allowed_hosts w aars.toml.";

/// Obrona przed DNS rebinding na całym panelu (przegląd 8–9 M-4): obca
/// domena przepięta na adres stacji ma w `Host` swoją nazwę, więc nie
/// dostanie ani strony logowania, ani sesji.
async fn host_check(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !state.check_hosts {
        return next.run(request).await;
    }
    match setup_page::request_host(request.headers(), request.uri()) {
        Some(host) if setup_page::host_allowed(&host, &state.host_names) => next.run(request).await,
        other => {
            tracing::warn!(host = ?other, path = %request.uri().path(), "obca nazwa w Host — odmowa");
            (StatusCode::MISDIRECTED_REQUEST, FOREIGN_HOST).into_response()
        }
    }
}

#[cfg(test)]
mod role_matrix;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use aars_core::priority::Priority;
    use aars_core::user::{Role, UserRepo};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use std::sync::Arc;
    use tower::ServiceExt;

    pub(crate) async fn state() -> AppState {
        AppState::for_tests(aars_core::db::test_pool().await)
    }

    /// Loguje się i zwraca cookie sesji wraz z tokenem CSRF odczytanym ze
    /// strony pulpitu — dokładnie tak, jak robi to przeglądarka.
    pub(crate) async fn login(app: &Router, username: &str, password: &str) -> (String, String) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("username={username}&password={password}")))
                    .unwrap(),
            )
            .await
            .unwrap();

        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or_default().to_owned())
            .expect("logowanie musi ustawić cookie");

        let dashboard = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::COOKIE, cookie.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(dashboard.into_body(), 1 << 20).await.unwrap();
        let html = String::from_utf8_lossy(&body);
        let csrf = html
            .split("name=\"csrf\" value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("pulpit musi zawierać token CSRF")
            .to_owned();

        (cookie, csrf)
    }

    pub(crate) async fn logged_in(role: Role) -> (Router, String, String, AppState) {
        let state = state().await;
        UserRepo::new(state.pool.clone()).create("u", "haslo", role).await.unwrap();
        let app = router(state.clone());
        let (cookie, csrf) = login(&app, "u", "haslo").await;
        (app, cookie, csrf, state)
    }

    fn enqueue(state: &AppState, text: &str) -> EnqueueRequest {
        let _ = state;
        EnqueueRequest {
            message_id: None,
            rendered_text: text.to_owned(),
            priority: Priority::Normal,
            interrupting: false,
            not_before: None,
            expires_at: None,
            repeat_count: 1,
            repeat_interval: std::time::Duration::ZERO,
            created_by: None,
        }
    }

    #[tokio::test]
    async fn the_dashboard_redirects_an_anonymous_visitor_to_the_login_page() {
        let response = router(state().await)
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/login");
    }

    #[tokio::test]
    async fn the_login_page_is_served_without_a_session() {
        let response = router(state().await)
            .oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_reports_the_version() {
        let app = router(state().await.with_version("0.10.0"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"ok 0.10.0");
    }

    #[tokio::test]
    async fn the_footer_shows_the_version_on_login() {
        let response = router(state().await)
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("AARS 0.0.0-test"), "{html}");
    }

    #[tokio::test]
    async fn the_footer_shows_the_version_to_every_role() {
        for role in [Role::Viewer, Role::Operator, Role::Admin] {
            let (app, cookie, _csrf, _state) = logged_in(role).await;
            let html = get_body(app, &cookie, "/").await;
            assert!(html.contains("AARS 0.0.0-test"), "{role:?}: {html}");
        }
    }

    #[tokio::test]
    async fn a_correct_login_sets_an_httponly_session_cookie() {
        let state = state().await;
        UserRepo::new(state.pool.clone())
            .create("operator", "haslo-operatora", Role::Operator)
            .await
            .unwrap();

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("username=operator&password=haslo-operatora"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
    }

    #[tokio::test]
    async fn a_wrong_password_does_not_set_a_session() {
        let state = state().await;
        UserRepo::new(state.pool.clone())
            .create("operator", "haslo-operatora", Role::Operator)
            .await
            .unwrap();

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("username=operator&password=zle"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn a_logged_in_viewer_sees_the_dashboard() {
        let (app, cookie, _csrf, _state) = logged_in(Role::Viewer).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_viewer_cannot_reach_operator_endpoints() {
        let (app, cookie, _csrf, _state) = logged_in(Role::Viewer).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/compose")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn logging_out_invalidates_the_cookie_immediately() {
        let (app, cookie, csrf, _state) = logged_in(Role::Operator).await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/logout")
                    .header(header::COOKIE, cookie.clone())
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf={csrf}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "sesja musi być unieważniona");
    }

    #[tokio::test]
    async fn vendored_assets_are_served_from_the_binary() {
        let app = router(state().await);

        for path in ["/assets/vendor/htmx.min.js", "/assets/app.css"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "brak assetu {path}");
        }
    }

    #[tokio::test]
    async fn an_operator_can_put_a_message_on_the_queue() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/compose")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf={csrf}&text=uwaga+ostrzezenie&priority=high&repeat=2&every=60&interrupting=1"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let pending = state.queue.pending(10).await.unwrap();
        assert_eq!(pending.len(), 2, "powtórzenia muszą się rozwinąć");
        assert_eq!(pending[0].rendered_text, "uwaga ostrzezenie");
        assert_eq!(pending[0].priority, Priority::High);
        assert!(pending[0].interrupting);
    }

    #[tokio::test]
    async fn a_viewer_cannot_put_anything_on_the_queue() {
        let (app, cookie, csrf, state) = logged_in(Role::Viewer).await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/compose")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf={csrf}&text=proba&priority=normal&repeat=1&every=0"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.queue.pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_request_without_a_csrf_token_is_refused() {
        let (app, cookie, _csrf, state) = logged_in(Role::Operator).await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/compose")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "csrf=podrobiony&text=atak&priority=emergency&repeat=1&every=0",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            state.queue.pending(10).await.unwrap().is_empty(),
            "żądanie międzywitrynowe nie może wejść na pasmo"
        );
    }

    #[tokio::test]
    async fn an_empty_message_is_rejected_instead_of_queuing_silence() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/compose")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf={csrf}&text=+++&priority=normal&repeat=1&every=0"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(state.queue.pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_operator_can_flip_the_kill_switch() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
        assert!(state.interlocks.snapshot().tx_enabled);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tx-enabled")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf={csrf}&enabled=0")))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            !state.interlocks.snapshot().tx_enabled,
            "zatrzymanie nadawania musi działać od razu"
        );

        let stored: String =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'tx_enabled'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(stored, "false", "stan kill switcha musi przetrwać restart");
    }

    #[tokio::test]
    async fn an_operator_can_cancel_a_queued_item() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
        let ids = state.queue.enqueue(enqueue(&state, "do anulowania")).await.unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/queue/{}/cancel", ids[0]))
                    .header(header::COOKIE, cookie)
                    .header("x-csrf-token", csrf)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(response.status().is_success(), "{:?}", response.status());
        assert!(state.queue.pending(10).await.unwrap().is_empty());
    }

    fn repeated(state: &AppState) -> EnqueueRequest {
        let mut request = enqueue(state, "seria");
        request.repeat_count = 3;
        request.repeat_interval = std::time::Duration::from_secs(60);
        request
    }

    async fn post_cancel(app: Router, cookie: &str, csrf: Option<&str>, uri: String) -> StatusCode {
        let mut builder = Request::builder().method("POST").uri(uri).header(header::COOKIE, cookie);
        if let Some(csrf) = csrf {
            builder = builder.header("x-csrf-token", csrf);
        }
        app.oneshot(builder.body(Body::empty()).unwrap()).await.unwrap().status()
    }

    #[tokio::test]
    async fn cancelling_a_group_removes_all_remaining_repeats() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
        state.queue.enqueue(repeated(&state)).await.unwrap();
        let group = state.queue.pending(10).await.unwrap()[0].repeat_group.clone().unwrap();

        let status =
            post_cancel(app, &cookie, Some(&csrf), format!("/queue/group/{group}/cancel")).await;

        assert!(status.is_success(), "{status:?}");
        assert!(state.queue.pending(10).await.unwrap().is_empty());
        let logged: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'queue_cancel_group'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert_eq!(logged, 1);
    }

    #[tokio::test]
    async fn cancelling_one_repeat_keeps_the_others() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
        let ids = state.queue.enqueue(repeated(&state)).await.unwrap();

        let status =
            post_cancel(app, &cookie, Some(&csrf), format!("/queue/{}/cancel", ids[0])).await;

        assert!(status.is_success(), "{status:?}");
        assert_eq!(state.queue.pending(10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_queue_page_shows_repeat_position() {
        let (app, cookie, _csrf, state) = logged_in(Role::Operator).await;
        state.queue.enqueue(repeated(&state)).await.unwrap();

        let html = get_body(app, &cookie, "/queue").await;

        assert!(html.contains("1/3"), "{html}");
        assert!(html.contains("3/3"), "{html}");
        assert!(html.contains("Anuluj serię"), "{html}");
    }

    #[tokio::test]
    async fn a_viewer_cannot_open_the_queue_page() {
        let (app, cookie, _csrf, _state) = logged_in(Role::Viewer).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/queue")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn group_cancel_without_csrf_is_refused() {
        let (app, cookie, _csrf, state) = logged_in(Role::Operator).await;
        state.queue.enqueue(repeated(&state)).await.unwrap();
        let group = state.queue.pending(10).await.unwrap()[0].repeat_group.clone().unwrap();

        let status = post_cancel(app, &cookie, None, format!("/queue/group/{group}/cancel")).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(state.queue.pending(10).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_viewer_cannot_cancel_a_group() {
        let (app, cookie, csrf, state) = logged_in(Role::Viewer).await;
        state.queue.enqueue(repeated(&state)).await.unwrap();
        let group = state.queue.pending(10).await.unwrap()[0].repeat_group.clone().unwrap();

        let status =
            post_cancel(app, &cookie, Some(&csrf), format!("/queue/group/{group}/cancel")).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(state.queue.pending(10).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn the_queue_fragment_is_served_for_live_updates() {
        let (app, cookie, _csrf, state) = logged_in(Role::Viewer).await;
        state.queue.enqueue(enqueue(&state, "w kolejce")).await.unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/fragments/queue")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("w kolejce"), "{html}");
        assert!(!html.contains("<html"), "fragment nie może być całą stroną");
    }

    async fn get_body(app: Router, cookie: &str, uri: &str) -> String {
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        String::from_utf8_lossy(&body).into_owned()
    }

    #[tokio::test]
    async fn the_queue_shows_why_an_item_is_held() {
        let (app, cookie, _csrf, state) = logged_in(Role::Viewer).await;
        state.queue.enqueue(enqueue(&state, "wstrzymany")).await.unwrap();
        let claimed = state.queue.claim_next().await.unwrap().unwrap();
        state
            .queue
            .hold(
                claimed.id,
                "wstrzymane: nadawanie wyłączone",
                chrono::Utc::now() + chrono::Duration::seconds(30),
            )
            .await
            .unwrap();
        state.queue.enqueue(enqueue(&state, "po błędzie")).await.unwrap();
        let other = state.queue.claim_next().await.unwrap().unwrap();
        state.queue.requeue(other.id, "surowy błąd sterownika", chrono::Utc::now()).await.unwrap();

        let html = get_body(app, &cookie, "/fragments/queue").await;
        assert!(html.contains("wstrzymane: nadawanie wyłączone"), "{html}");
        assert!(!html.contains("surowy błąd sterownika"), "{html}");
    }

    #[tokio::test]
    async fn the_queue_shows_polish_priority_names() {
        let (app, cookie, _csrf, state) = logged_in(Role::Viewer).await;
        let mut request = enqueue(&state, "w kolejce");
        request.priority = Priority::High;
        state.queue.enqueue(request).await.unwrap();

        let html = get_body(app, &cookie, "/fragments/queue").await;
        assert!(html.contains("Wysoki"), "{html}");
        assert!(!html.contains("High"), "{html}");
    }

    #[tokio::test]
    async fn an_admin_sees_admin_links_and_an_operator_does_not() {
        let (app, cookie, _csrf, _state) = logged_in(Role::Admin).await;
        let html = get_body(app, &cookie, "/").await;
        assert!(html.contains("href=\"/settings\""), "{html}");
        assert!(html.contains("href=\"/users\""), "{html}");

        let (app, cookie, _csrf, _state) = logged_in(Role::Operator).await;
        let html = get_body(app, &cookie, "/").await;
        assert!(!html.contains("href=\"/settings\""), "{html}");
        assert!(!html.contains("href=\"/users\""), "{html}");
        assert!(html.contains("href=\"/queue\""), "{html}");
    }

    async fn post_form(
        app: &Router,
        cookie: &str,
        uri: &str,
        body: String,
    ) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::COOKIE, cookie)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn get_html(app: &Router, cookie: &str, uri: &str) -> (StatusCode, String) {
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
        let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    fn message_form(csrf: &str, title: &str, text: &str, cron: &str) -> String {
        format!(
            "csrf={csrf}&title={title}&text={text}&priority=high&repeat=2&every=60\
             &schedule_kind=cron&at=&cron={}",
            cron.replace(' ', "+").replace('*', "%2A")
        )
    }

    async fn create_message(app: &Router, cookie: &str, csrf: &str) {
        let response = post_form(
            app,
            cookie,
            "/messages",
            message_form(csrf, "biuletyn", "Uwaga+wszystkim", "0 18 * * *"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn an_operator_creates_a_cron_message_and_sees_it_listed() {
        let (app, cookie, csrf, _state) = logged_in(Role::Operator).await;

        let response = post_form(
            &app,
            &cookie,
            "/messages",
            message_form(&csrf, "biuletyn", "Uwaga+wszystkim", "0 18 * * *"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/messages");

        let (status, html) = get_html(&app, &cookie, "/messages").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("biuletyn"), "{html}");
        assert!(html.contains("0 18 * * *"), "{html}");
        assert!(html.contains("Wysoki"), "{html}");
    }

    #[tokio::test]
    async fn a_huge_repeat_interval_is_refused_in_compose_and_in_definitions() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;

        let response = post_form(
            &app,
            &cookie,
            "/compose",
            format!("csrf={csrf}&text=uwaga&priority=normal&repeat=3&every=18446744073709551615"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("86400"));
        assert!(state.queue.pending(10).await.unwrap().is_empty());

        let response = post_form(
            &app,
            &cookie,
            "/messages",
            message_form(&csrf, "biuletyn", "tresc", "0 18 * * *")
                .replace("every=60", "every=86401"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let saved = aars_core::message::MessageRepo::new(state.pool.clone()).list().await.unwrap();
        assert!(saved.is_empty(), "{saved:?}");

        // Granica jest włącznie.
        let response = post_form(
            &app,
            &cookie,
            "/messages",
            message_form(&csrf, "biuletyn", "tresc", "0 18 * * *")
                .replace("every=60", "every=86400"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn the_full_queue_fragment_accepts_full_1_from_the_templates() {
        let (app, cookie, _csrf, _state) = logged_in(Role::Operator).await;
        let (status, html) = get_html(&app, &cookie, "/fragments/queue?full=1").await;
        assert_eq!(status, StatusCode::OK, "{html}");
    }

    #[tokio::test]
    async fn a_broken_queue_table_is_a_500_not_an_empty_queue() {
        let (app, cookie, _csrf, state) = logged_in(Role::Operator).await;
        sqlx::query("ALTER TABLE queue_items RENAME TO queue_items_gone")
            .execute(&state.pool)
            .await
            .unwrap();
        for uri in ["/fragments/queue", "/fragments/queue?full=1", "/queue"] {
            let (status, html) = get_html(&app, &cookie, uri).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{uri}");
            assert!(!html.contains("Kolejka pusta"), "{uri}: {html}");
        }
    }

    #[tokio::test]
    async fn broken_log_tables_are_a_500_not_empty_logs() {
        for table in ["tx_log", "audit_log"] {
            let (app, cookie, _csrf, state) = logged_in(Role::Viewer).await;
            sqlx::query(&format!("ALTER TABLE {table} RENAME TO {table}_gone"))
                .execute(&state.pool)
                .await
                .unwrap();
            let (status, html) = get_html(&app, &cookie, "/logs").await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{table}");
            assert!(!html.contains("Nic jeszcze nie nadano"), "{table}: {html}");
        }
    }

    #[tokio::test]
    async fn the_header_badge_and_the_audit_log_speak_polish() {
        let (app, cookie, csrf, _state) = logged_in(Role::Operator).await;
        let response = post_form(
            &app,
            &cookie,
            "/compose",
            format!("csrf={csrf}&text=uwaga&priority=normal&repeat=1&every=0"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let (status, html) = get_html(&app, &cookie, "/logs").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("u · Operator"), "{html}");
        assert!(html.contains("<td>nowe nadanie</td>"), "{html}");
        assert!(!html.contains("<td>compose</td>"), "{html}");
    }

    #[tokio::test]
    async fn links_use_the_panel_accent_colour() {
        let css = String::from_utf8(
            crate::assets::Assets::get("app.css").unwrap().data.into_owned(),
        )
        .unwrap();
        assert!(css.contains("a, a:visited { color: var(--accent); }"), "{css}");
        assert!(css.contains("a:hover"), "{css}");
    }

    #[tokio::test]
    async fn an_invalid_cron_is_refused_and_nothing_is_saved() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;

        let response = post_form(
            &app,
            &cookie,
            "/messages",
            message_form(&csrf, "zly", "tresc", "0 25 * * *"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let saved = aars_core::message::MessageRepo::new(state.pool.clone()).list().await.unwrap();
        assert!(saved.is_empty(), "{saved:?}");
    }

    #[tokio::test]
    async fn an_empty_message_text_is_refused() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;

        let response = post_form(
            &app,
            &cookie,
            "/messages",
            message_form(&csrf, "pusty", "+++", "0 18 * * *"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let saved = aars_core::message::MessageRepo::new(state.pool.clone()).list().await.unwrap();
        assert!(saved.is_empty(), "{saved:?}");
    }

    #[tokio::test]
    async fn an_operator_can_edit_a_message() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
        create_message(&app, &cookie, &csrf).await;
        let repo = aars_core::message::MessageRepo::new(state.pool.clone());
        let id = repo.list().await.unwrap()[0].id;

        let (status, html) = get_html(&app, &cookie, &format!("/messages/{id}/edit")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("0 18 * * *"), "{html}");

        let response = post_form(
            &app,
            &cookie,
            &format!("/messages/{id}"),
            message_form(&csrf, "nowy", "Inna+tresc", "30 6 * * *"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let message = repo.get(id).await.unwrap().unwrap();
        assert_eq!(message.title, "nowy");
        assert_eq!(
            message.schedule,
            aars_core::message::ScheduleKind::Cron("30 6 * * *".to_owned())
        );
    }

    #[tokio::test]
    async fn an_operator_can_disable_and_delete_a_message() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
        create_message(&app, &cookie, &csrf).await;
        let repo = aars_core::message::MessageRepo::new(state.pool.clone());
        let id = repo.list().await.unwrap()[0].id;

        let response = post_form(
            &app,
            &cookie,
            &format!("/messages/{id}/enabled"),
            format!("csrf={csrf}&enabled=0"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(!repo.get(id).await.unwrap().unwrap().enabled);

        let response =
            post_form(&app, &cookie, &format!("/messages/{id}/delete"), format!("csrf={csrf}"))
                .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(repo.get(id).await.unwrap().is_none());

        let audited: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_log ORDER BY id")
            .fetch_all(&state.pool)
            .await
            .unwrap();
        assert_eq!(audited, ["message_create", "message_enabled", "message_delete"]);
    }

    #[tokio::test]
    async fn send_now_puts_the_definition_on_the_queue() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
        create_message(&app, &cookie, &csrf).await;
        let id =
            aars_core::message::MessageRepo::new(state.pool.clone()).list().await.unwrap()[0].id;

        let response =
            post_form(&app, &cookie, &format!("/messages/{id}/send"), format!("csrf={csrf}")).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/queue");

        let pending = state.queue.pending(10).await.unwrap();
        assert_eq!(pending.len(), 2, "{pending:?}");
        assert!(pending.iter().all(|item| item.message_id == Some(id)));
        assert!(pending.iter().all(|item| item.rendered_text == "Uwaga wszystkim"));
        assert!(pending.iter().all(|item| item.priority == Priority::High));
    }

    #[tokio::test]
    async fn a_viewer_cannot_open_messages() {
        let (app, cookie, _csrf, _state) = logged_in(Role::Viewer).await;

        let (status, _) = get_html(&app, &cookie, "/messages").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_next_fire_hint_shows_both_times() {
        let (app, cookie, _csrf, _state) = logged_in(Role::Operator).await;

        let (status, text) =
            get_html(&app, &cookie, "/messages/next?schedule_kind=cron&at=&cron=0+18+%2A+%2A+%2A")
                .await;
        assert_eq!(status, StatusCode::OK);
        assert!(text.contains("18:00 UTC"), "{text}");
        assert!(text.contains("lokalnie"), "{text}");
    }

    #[tokio::test]
    async fn creating_a_message_without_csrf_is_forbidden_and_saves_nothing() {
        let (app, cookie, _csrf, state) = logged_in(Role::Operator).await;

        let response = post_form(
            &app,
            &cookie,
            "/messages",
            "csrf=zly&title=x&text=tresc&priority=normal&schedule_kind=immediate".to_owned(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let saved = aars_core::message::MessageRepo::new(state.pool.clone()).list().await.unwrap();
        assert!(saved.is_empty(), "{saved:?}");
    }

    #[tokio::test]
    async fn sending_an_unknown_message_is_not_found() {
        let (app, cookie, csrf, _state) = logged_in(Role::Operator).await;

        let response =
            post_form(&app, &cookie, "/messages/9999/send", format!("csrf={csrf}")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_viewer_sees_transmissions_and_audit_entries() {
        let (app, op_cookie, csrf, state) = logged_in(Role::Operator).await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/compose")
                    .header(header::COOKIE, op_cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf={csrf}&text=komunikat+dziennika&priority=normal")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let id = state.queue.pending(10).await.unwrap()[0].id;
        let now = chrono::Utc::now();
        aars_core::txlog::TxLogRepo::new(state.pool.clone())
            .record(aars_core::txlog::TxLogEntry {
                queue_item_id: Some(id),
                started_at: now - chrono::Duration::seconds(7),
                ended_at: now,
                ptt_backend: "gpio".to_owned(),
                result: "ok",
                note: None,
            })
            .await
            .unwrap();

        UserRepo::new(state.pool.clone()).create("v", "haslo", Role::Viewer).await.unwrap();
        let (viewer_cookie, _) = login(&app, "v", "haslo").await;
        let html = get_body(app, &viewer_cookie, "/logs").await;

        assert!(html.contains("nadane"), "{html}");
        assert!(html.contains("komunikat dziennika"), "{html}");
        assert!(html.contains("nowe nadanie"), "{html}");
        assert!(html.contains("UTC"), "{html}");
        assert!(html.contains("lokalnie"), "{html}");
    }

    #[tokio::test]
    async fn a_test_ptt_row_shows_its_note_instead_of_a_message() {
        let (app, cookie, _csrf, state) = logged_in(Role::Operator).await;
        let now = chrono::Utc::now();
        aars_core::txlog::TxLogRepo::new(state.pool.clone())
            .record(aars_core::txlog::TxLogEntry {
                queue_item_id: None,
                started_at: now - chrono::Duration::seconds(2),
                ended_at: now,
                ptt_backend: "gpio".to_owned(),
                result: "ok",
                note: Some("test PTT: admin".to_owned()),
            })
            .await
            .unwrap();

        let html = get_body(app, &cookie, "/logs").await;

        assert!(html.contains("test PTT: admin"), "{html}");
        assert!(html.contains("2.0 s"), "{html}");
    }

    #[tokio::test]
    async fn anonymous_visitors_are_sent_to_login() {
        let response = router(state().await)
            .oneshot(Request::builder().uri("/logs").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/login");
    }

    /// Stan zatruty po interwencji watchdoga, z licznikiem wywołań
    /// `clear_poison` po stronie strażnika.
    async fn poisoned_as(
        role: Role,
    ) -> (Router, String, String, AppState, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cleared = Arc::new(AtomicUsize::new(0));
        let counter = cleared.clone();
        let state = state().await.with_watchdog_clear(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        state.interlocks.set_poisoned(true);
        sqlx::query("UPDATE settings SET value = 'true' WHERE key = 'poisoned'")
            .execute(&state.pool)
            .await
            .unwrap();
        UserRepo::new(state.pool.clone()).create("u", "haslo", role).await.unwrap();
        let app = router(state.clone());
        let (cookie, csrf) = login(&app, "u", "haslo").await;
        (app, cookie, csrf, state, cleared)
    }

    fn clear_request(cookie: &str, csrf: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/watchdog/clear")
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("csrf={csrf}")))
            .unwrap()
    }

    async fn stored_poison(state: &AppState) -> String {
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'poisoned'")
            .fetch_one(&state.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_admin_clears_the_watchdog_block() {
        let (app, cookie, csrf, state, cleared) = poisoned_as(Role::Admin).await;

        let response = app.oneshot(clear_request(&cookie, &csrf)).await.unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(!state.interlocks.snapshot().poisoned, "interlocki muszą przepuszczać");
        assert_eq!(stored_poison(&state).await, "false", "zdjęcie musi przetrwać restart");
        assert_eq!(
            cleared.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "flaga musi zniknąć też w samym watchdogu"
        );
        let audited: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE action = 'watchdog_clear'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(audited, 1);
    }

    #[tokio::test]
    async fn an_operator_cannot_clear_the_watchdog_block() {
        let (app, cookie, csrf, state, cleared) = poisoned_as(Role::Operator).await;

        let response = app.oneshot(clear_request(&cookie, &csrf)).await.unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.interlocks.snapshot().poisoned);
        assert_eq!(stored_poison(&state).await, "true");
        assert_eq!(cleared.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn clearing_the_watchdog_block_needs_a_csrf_token() {
        let (app, cookie, _csrf, state, cleared) = poisoned_as(Role::Admin).await;

        let response = app.oneshot(clear_request(&cookie, "podrobiony")).await.unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.interlocks.snapshot().poisoned);
        assert_eq!(cleared.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    async fn dashboard_html(app: Router, cookie: &str) -> String {
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn the_dashboard_offers_the_watchdog_reset_only_to_an_admin() {
        let (app, cookie, _, _, _) = poisoned_as(Role::Admin).await;
        let html = dashboard_html(app, &cookie).await;
        assert!(html.contains("watchdog — wymaga resetu"));
        assert!(html.contains("action=\"/watchdog/clear\""), "{html}");
        assert!(html.contains("Zdejmij blokadę watchdoga"));

        let (app, cookie, _, _, _) = poisoned_as(Role::Operator).await;
        let html = dashboard_html(app, &cookie).await;
        assert!(html.contains("watchdog — wymaga resetu"), "operator widzi blokadę");
        assert!(!html.contains("/watchdog/clear"), "operator nie dostaje przycisku");
    }

    #[tokio::test]
    async fn the_kill_switch_is_shown_even_when_the_queue_cannot_be_read() {
        let (app, cookie, _csrf, state) = logged_in(Role::Operator).await;
        sqlx::query("DROP TABLE queue_items").execute(&state.pool).await.unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("action=\"/tx-enabled\""), "{html}");
        assert!(html.contains("Nie udało się odczytać kolejki"));
        assert!(!html.contains("Kolejka pusta."), "pusta tabela kłamałaby, że nic nie czeka");
    }
}
