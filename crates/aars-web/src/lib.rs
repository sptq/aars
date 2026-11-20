pub mod account;
pub mod assets;
pub mod auth;
pub mod chrome;
pub mod messages;
pub mod render;
pub mod state;

use aars_core::interlock::Interlocks;
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

use crate::chrome::{Chrome, both_times_now, parse_priority, priority_label};
use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::render::page;
pub use crate::state::AppState;

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    chrome: Chrome,
    tx_enabled: bool,
    poisoned: bool,
    pending: Vec<QueueRow>,
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
}

struct QueueRow {
    id: i64,
    priority: String,
    not_before: String,
    text: String,
    interrupting: bool,
}

fn queue_row(item: QueueItem) -> QueueRow {
    QueueRow {
        id: item.id,
        priority: priority_label(item.priority).to_owned(),
        not_before: both_times_now(item.not_before),
        text: item.rendered_text,
        interrupting: item.interrupting,
    }
}

async fn queue_rows(state: &AppState) -> Vec<QueueRow> {
    match state.queue.pending(50).await {
        Ok(items) => items.into_iter().map(queue_row).collect(),
        Err(err) => {
            tracing::error!(%err, "odczyt kolejki zawiódł");
            Vec::new()
        }
    }
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

async fn login_page() -> Response {
    page(&LoginPage { error: None })
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

    if let Some(session_id) = auth::session_id_from_cookies(
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()),
    ) {
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

async fn dashboard(State(state): State<AppState>, current: CurrentUser) -> Response {
    let interlocks = state.interlocks.snapshot();

    page(&DashboardPage {
        chrome: Chrome::from(&current),
        tx_enabled: interlocks.tx_enabled,
        poisoned: interlocks.poisoned,
        pending: queue_rows(&state).await,
    })
}

async fn compose_page(current: CurrentUser) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    Ok(page(&ComposePage {
        chrome: Chrome::from(&current),
    }))
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
        return Ok(
            (StatusCode::BAD_REQUEST, "treść komunikatu nie może być pusta").into_response()
        );
    }

    let Some(priority) = parse_priority(&form.priority) else {
        return Ok((StatusCode::BAD_REQUEST, "nieznany priorytet").into_response());
    };

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
    audit(
        &state,
        current.user.id,
        "tx_enabled",
        if enabled { "włączone" } else { "wyłączone" },
    )
    .await;

    Ok(Redirect::to("/").into_response())
}

async fn cancel_item(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: header::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    // HTMX wysyła token nagłówkiem, bo przycisk nie jest formularzem.
    require_csrf(
        &current.csrf_token,
        headers.get("x-csrf-token").and_then(|v| v.to_str().ok()),
    )?;

    match state.queue.cancel(id).await {
        Ok(count) => {
            tracing::info!(user = %current.user.username, id, count, "anulowanie zadania z panelu");
            audit(&state, current.user.id, "queue_cancel", &format!("zadanie {id}")).await;
        }
        Err(err) => tracing::error!(%err, id, "anulowanie zadania zawiodło"),
    }

    // Odpowiadamy fragmentem, więc HTMX podmienia tabelę bez przeładowania.
    Ok(page(&QueueFragment {
        pending: queue_rows(&state).await,
        chrome: Chrome::from(&current),
    }))
}

async fn queue_fragment(State(state): State<AppState>, current: CurrentUser) -> Response {
    page(&QueueFragment {
        pending: queue_rows(&state).await,
        chrome: Chrome::from(&current),
    })
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
        .route("/logout", post(logout))
        .route(
            "/account/password",
            get(account::password_page).post(account::password_submit),
        )
        .route("/compose", get(compose_page).post(compose_submit))
        .route("/messages", get(messages::list).post(messages::create))
        .route("/messages/new", get(messages::new_page))
        .route("/messages/next", get(messages::next_hint))
        .route("/messages/{id}/edit", get(messages::edit_page))
        .route("/messages/{id}", post(messages::update))
        .route("/messages/{id}/enabled", post(messages::set_enabled))
        .route("/messages/{id}/delete", post(messages::delete))
        .route("/messages/{id}/send", post(messages::send))
        .route("/tx-enabled", post(set_tx_enabled))
        .route("/queue/{id}/cancel", post(cancel_item))
        .route("/fragments/queue", get(queue_fragment))
        .route("/healthz", get(|| async { "ok" }))
        .route("/assets/{*path}", get(assets::serve))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::priority::Priority;
    use aars_core::user::{Role, UserRepo};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    async fn state() -> AppState {
        AppState::for_tests(aars_core::db::test_pool().await)
    }

    /// Loguje się i zwraca cookie sesji wraz z tokenem CSRF odczytanym ze
    /// strony pulpitu — dokładnie tak, jak robi to przeglądarka.
    async fn login(app: &Router, username: &str, password: &str) -> (String, String) {
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

    async fn logged_in(role: Role) -> (Router, String, String, AppState) {
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
                    .body(Body::from(format!("csrf={csrf}&text=proba&priority=normal&repeat=1&every=0")))
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
                    .body(Body::from("csrf=podrobiony&text=atak&priority=emergency&repeat=1&every=0"))
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
                    .body(Body::from(format!("csrf={csrf}&text=+++&priority=normal&repeat=1&every=0")))
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
        let id = aars_core::message::MessageRepo::new(state.pool.clone())
            .list()
            .await
            .unwrap()[0]
            .id;

        let response =
            post_form(&app, &cookie, &format!("/messages/{id}/send"), format!("csrf={csrf}")).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

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
}
