//! Kreator pierwszego konta administratora `/setup` (etap 10, R12). Bez
//! logowania, ale tylko w trybie `setup.first_admin = "wizard"`, tylko gdy
//! nie ma włączonego admina i tylko z sieci lokalnej stacji. CSRF jako
//! double-submit: losowe ciasteczko `aars_setup` i to samo w polu formularza.
//! Hasło nigdy nie trafia do logu ani do audytu.

use std::net::{IpAddr, SocketAddr};

use aars_core::config::FirstAdmin;
use aars_core::netlocal::is_local_client;
use aars_core::user::{FirstAdminError, UserRepo, random_token};
use askama::Template;
use axum::Form;
use axum::extract::connect_info::{ConnectInfo, MockConnectInfo};
use axum::extract::State;
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::auth;
use crate::render::page;
use crate::state::AppState;

pub const SETUP_COOKIE: &str = "aars_setup";
/// Ważność ciasteczka kreatora (R12: 15 min).
const SETUP_COOKIE_MAX_AGE_S: u32 = 15 * 60;

pub const NOT_LOCAL: &str = "Kreator pierwszego konta działa tylko z sieci lokalnej stacji.";
pub const EXPIRED: &str = "Sesja kreatora wygasła — odśwież stronę.";
pub const ALREADY_CREATED: &str = "Konto administratora już założono.";
const NAME_RULE: &str = "nazwa: 3–32 znaki: małe litery, cyfry, . _ -";
const PASSWORD_RULE: &str = "hasło: co najmniej 10 znaków";
const MISMATCH: &str = "hasła się różnią";
const NAME_TAKEN: &str = "nazwa zajęta przez konto innej roli — wybierz inną";
const MIN_PASSWORD_CHARS: usize = 10;

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupPage {
    csrf: String,
    username: String,
    errors: Vec<String>,
    version: &'static str,
}

/// Pola formularza. Bez `Debug`, żeby hasło nie mogło trafić do logu.
#[derive(Default, Deserialize)]
pub struct SetupForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    password_again: String,
}

/// Adres klienta z `ConnectInfo` (stacja) albo `MockConnectInfo` (testy).
/// Brak obu znaczy, że serwer nie podaje adresów — wtedy kreator odmawia.
fn client_ip(extensions: &Extensions) -> Option<IpAddr> {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .or_else(|| {
            extensions
                .get::<MockConnectInfo<SocketAddr>>()
                .map(|m| m.0.ip())
        })
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
}

/// Porównanie w czasie stałym; pusty token nigdy nie pasuje.
fn tokens_match(a: &str, b: &str) -> bool {
    !a.is_empty()
        && a.len() == b.len()
        && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn setup_cookie(token: &str, secure: bool) -> String {
    let mut cookie = format!(
        "{SETUP_COOKIE}={token}; Path=/setup; HttpOnly; SameSite=Strict; Max-Age={SETUP_COOKIE_MAX_AGE_S}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn clear_setup_cookie(secure: bool) -> String {
    let mut cookie =
        format!("{SETUP_COOKIE}=; Path=/setup; HttpOnly; SameSite=Strict; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "nie ma takiej strony").into_response()
}

/// Czy kreator jest teraz otwarty: tryb `wizard` i brak włączonego admina.
async fn wizard_open(state: &AppState) -> Result<bool, Response> {
    if state.first_admin != FirstAdmin::Wizard {
        return Ok(false);
    }
    match UserRepo::new(state.pool.clone()).has_enabled_admin().await {
        Ok(has) => Ok(!has),
        Err(err) => {
            tracing::error!(%err, "odczyt kont administratora zawiódł");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "błąd bazy danych").into_response())
        }
    }
}

/// Warunki wspólne dla GET i POST: `404` poza kreatorem, `403` spoza sieci
/// lokalnej. Zwraca adres klienta.
async fn guard(state: &AppState, extensions: &Extensions) -> Result<IpAddr, Response> {
    if !wizard_open(state).await? {
        return Err(not_found());
    }
    match client_ip(extensions) {
        Some(ip) if is_local_client(ip, &state.local_nets) => Ok(ip),
        other => {
            tracing::warn!(client = ?other, "kreator pierwszego konta: adres spoza sieci lokalnej");
            Err((StatusCode::FORBIDDEN, NOT_LOCAL).into_response())
        }
    }
}

/// W trybie `wizard` bez włączonego admina `/` i `/login` prowadzą do
/// kreatora. Błąd bazy nie przekierowuje (strona pokaże swój błąd).
pub async fn wizard_redirect(state: &AppState) -> Option<Response> {
    match wizard_open(state).await {
        Ok(true) => Some(Redirect::to("/setup").into_response()),
        _ => None,
    }
}

fn form_page(status: StatusCode, page_data: &SetupPage) -> Response {
    let mut response = page(page_data);
    if response.status() == StatusCode::OK {
        *response.status_mut() = status;
    }
    response
}

pub async fn page_view(State(state): State<AppState>, extensions: Extensions) -> Response {
    if let Err(response) = guard(&state, &extensions).await {
        return response;
    }
    let token = random_token();
    let mut response = form_page(
        StatusCode::OK,
        &SetupPage {
            csrf: token.clone(),
            username: String::new(),
            errors: Vec::new(),
            version: state.version,
        },
    );
    if let Ok(value) = HeaderValue::from_str(&setup_cookie(&token, state.secure_cookies)) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

fn validate(form: &SetupForm) -> Vec<String> {
    let mut errors = Vec::new();
    let name = &form.username;
    let name_ok = (3..=32).contains(&name.chars().count())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if !name_ok {
        errors.push(NAME_RULE.to_owned());
    }
    if form.password.chars().count() < MIN_PASSWORD_CHARS {
        errors.push(PASSWORD_RULE.to_owned());
    }
    if form.password != form.password_again {
        errors.push(MISMATCH.to_owned());
    }
    errors
}

pub async fn submit(
    State(state): State<AppState>,
    extensions: Extensions,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Response {
    let ip = match guard(&state, &extensions).await {
        Ok(ip) => ip,
        Err(response) => return response,
    };
    let cookie = cookie_value(&headers, SETUP_COOKIE).unwrap_or_default();
    if !tokens_match(&cookie, &form.csrf) {
        tracing::warn!(%ip, "kreator pierwszego konta: brak albo zły token");
        return (StatusCode::BAD_REQUEST, EXPIRED).into_response();
    }
    let refuse = |errors: Vec<String>| {
        form_page(
            StatusCode::BAD_REQUEST,
            &SetupPage {
                csrf: cookie.clone(),
                username: form.username.clone(),
                errors,
                version: state.version,
            },
        )
    };
    let errors = validate(&form);
    if !errors.is_empty() {
        return refuse(errors);
    }

    let repo = UserRepo::new(state.pool.clone());
    let id = match repo.create_first_admin(&form.username, &form.password).await {
        Ok(id) => id,
        Err(FirstAdminError::AlreadyExists) => {
            return (StatusCode::CONFLICT, ALREADY_CREATED).into_response();
        }
        Err(FirstAdminError::NameTaken) => return refuse(vec![NAME_TAKEN.to_owned()]),
        Err(FirstAdminError::Auth(err)) => {
            tracing::error!(%err, "założenie pierwszego admina zawiodło");
            return (StatusCode::INTERNAL_SERVER_ERROR, "błąd zapisu konta").into_response();
        }
    };
    tracing::warn!(username = %form.username, %ip, "pierwsze konto administratora założone z kreatora");
    crate::audit(&state, id, "first_admin", &format!("{} z {ip}", form.username)).await;

    // Od razu sesja, jak po `/login`.
    let session = match repo.start_session(id, None).await {
        Ok(session) => session,
        Err(err) => {
            tracing::error!(%err, "utworzenie sesji zawiodło");
            // Konto już jest: logowanie zadziała zwykłą drogą.
            return Redirect::to("/login").into_response();
        }
    };
    let mut response = (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
    for cookie in [
        auth::session_cookie(&session.id, state.secure_cookies),
        clear_setup_cookie(state.secure_cookies),
    ] {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router;
    use aars_core::netlocal::LocalNet;
    use aars_core::user::Role;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn wizard_state() -> AppState {
        AppState::for_tests(aars_core::db::test_pool().await)
            .with_first_admin(FirstAdmin::Wizard)
            .with_local_nets(vec![LocalNet {
                addr: "192.168.1.2".parse().unwrap(),
                prefix: 24,
            }])
    }

    fn app_from(state: &AppState, client: &str) -> Router {
        let addr: SocketAddr = client.parse().unwrap();
        router(state.clone()).layer(MockConnectInfo(addr))
    }

    struct Reply {
        status: StatusCode,
        body: String,
        location: Option<String>,
        cookies: Vec<String>,
    }

    async fn send(app: &Router, method: &str, uri: &str, cookie: Option<&str>, body: &str) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        if method == "POST" {
            builder = builder.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let cookies = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_owned)
            .collect();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        Reply {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            location,
            cookies,
        }
    }

    /// Otwiera kreator i zwraca ciasteczko `aars_setup=…` oraz token.
    async fn open(app: &Router) -> (String, String) {
        let reply = send(app, "GET", "/setup", None, "").await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let cookie = reply
            .cookies
            .iter()
            .find(|c| c.starts_with("aars_setup="))
            .expect("ciasteczko kreatora")
            .clone();
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("Max-Age=900"), "{cookie}");
        let pair = cookie.split(';').next().unwrap().to_owned();
        let token = pair.trim_start_matches("aars_setup=").to_owned();
        assert!(reply.body.contains(&format!("value=\"{token}\"")), "{}", reply.body);
        (pair, token)
    }

    fn body(token: &str, name: &str, password: &str, again: &str) -> String {
        format!("csrf={token}&username={name}&password={password}&password_again={again}")
    }

    async fn user_count(state: &AppState) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&state.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_wizard_is_hidden_in_console_mode() {
        let state = AppState::for_tests(aars_core::db::test_pool().await);
        let app = app_from(&state, "192.168.1.10:5555");
        assert_eq!(send(&app, "GET", "/setup", None, "").await.status, StatusCode::NOT_FOUND);
        let reply = send(
            &app,
            "POST",
            "/setup",
            Some("aars_setup=t"),
            &body("t", "szef", "dlugie-haslo", "dlugie-haslo"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(user_count(&state).await, 0);
        // Bez kreatora `/` prowadzi na zwykłe logowanie.
        let reply = send(&app, "GET", "/", None, "").await;
        assert_eq!(reply.location.as_deref(), Some("/login"));
        assert_eq!(send(&app, "GET", "/login", None, "").await.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn without_an_admin_root_redirects_to_setup() {
        let state = wizard_state().await;
        let app = app_from(&state, "192.168.1.10:5555");
        for path in ["/", "/login"] {
            let reply = send(&app, "GET", path, None, "").await;
            assert_eq!(reply.status, StatusCode::SEE_OTHER, "{path}");
            assert_eq!(reply.location.as_deref(), Some("/setup"), "{path}");
        }
        let reply = send(&app, "GET", "/setup", None, "").await;
        assert!(
            reply.body.contains(
                "Pierwsze uruchomienie stacji AARS. Załóż konto administratora. Kreator działa \
                 tylko z sieci lokalnej i znika po założeniu konta."
            ),
            "{}",
            reply.body
        );
    }

    #[tokio::test]
    async fn a_local_client_creates_the_admin_and_is_logged_in() {
        let state = wizard_state().await;
        let app = app_from(&state, "192.168.1.10:5555");
        let (cookie, token) = open(&app).await;
        let reply = send(
            &app,
            "POST",
            "/setup",
            Some(&cookie),
            &body(&token, "szef", "dlugie-haslo", "dlugie-haslo"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        assert_eq!(reply.location.as_deref(), Some("/"));
        let session = reply
            .cookies
            .iter()
            .find(|c| c.starts_with("aars_session="))
            .expect("ciasteczko sesji")
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert!(
            reply.cookies.iter().any(|c| c.starts_with("aars_setup=;") && c.contains("Max-Age=0")),
            "{:?}",
            reply.cookies
        );

        // Sesja działa od razu: pulpit admina z linkiem do ustawień.
        let reply = send(&app, "GET", "/", Some(&session), "").await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains("szef"), "{}", reply.body);
        assert!(reply.body.contains("/settings/hardware"), "{}", reply.body);

        let user = UserRepo::new(state.pool.clone())
            .authenticate("szef", "dlugie-haslo")
            .await
            .unwrap();
        assert_eq!(user.role, Role::Admin);
        assert!(!user.must_change_password);
        let audit: Vec<(Option<i64>, String)> =
            sqlx::query_as("SELECT user_id, detail FROM audit_log WHERE action = 'first_admin'")
                .fetch_all(&state.pool)
                .await
                .unwrap();
        assert_eq!(audit, vec![(Some(user.id), "szef z 192.168.1.10".to_owned())]);
        // Hasło nie trafia do audytu.
        let all: Vec<String> = sqlx::query_scalar("SELECT detail FROM audit_log")
            .fetch_all(&state.pool)
            .await
            .unwrap();
        assert!(all.iter().all(|d| !d.contains("dlugie-haslo")));
    }

    #[tokio::test]
    async fn a_remote_client_is_refused() {
        let state = wizard_state().await;
        let local = app_from(&state, "192.168.1.10:5555");
        let (cookie, token) = open(&local).await;

        let remote = app_from(&state, "8.8.8.8:5555");
        let reply = send(&remote, "GET", "/setup", None, "").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(reply.body, NOT_LOCAL);
        assert!(reply.cookies.is_empty());
        let reply = send(
            &remote,
            "POST",
            "/setup",
            Some(&cookie),
            &body(&token, "szef", "dlugie-haslo", "dlugie-haslo"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(user_count(&state).await, 0);

        // Globalne IPv6 z tego samego /64 co interfejs stacji jest lokalne.
        let state = wizard_state().await.with_local_nets(vec![LocalNet {
            addr: "2a01:110::1".parse().unwrap(),
            prefix: 64,
        }]);
        let app = app_from(&state, "[2a01:110::5]:5555");
        assert_eq!(send(&app, "GET", "/setup", None, "").await.status, StatusCode::OK);
        let app = app_from(&state, "[2a01:111::5]:5555");
        assert_eq!(send(&app, "GET", "/setup", None, "").await.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn without_a_client_address_the_wizard_refuses() {
        // Serwer bez `ConnectInfo`: kreator nie zgaduje, że klient jest lokalny.
        let state = wizard_state().await;
        let app = router(state.clone());
        assert_eq!(send(&app, "GET", "/setup", None, "").await.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_post_without_the_setup_cookie_is_refused() {
        let state = wizard_state().await;
        let app = app_from(&state, "192.168.1.10:5555");
        let (cookie, token) = open(&app).await;
        let form = body(&token, "szef", "dlugie-haslo", "dlugie-haslo");

        for (cookie, form) in [
            (None, form.clone()),
            (Some("aars_setup=inny".to_owned()), form.clone()),
            (Some(cookie.clone()), body("inny", "szef", "dlugie-haslo", "dlugie-haslo")),
            (Some("aars_setup=".to_owned()), body("", "szef", "dlugie-haslo", "dlugie-haslo")),
        ] {
            let reply = send(&app, "POST", "/setup", cookie.as_deref(), &form).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{cookie:?}");
            assert_eq!(reply.body, EXPIRED);
        }
        assert_eq!(user_count(&state).await, 0);
    }

    #[tokio::test]
    async fn short_passwords_and_mismatches_are_refused() {
        let state = wizard_state().await;
        let app = app_from(&state, "192.168.1.10:5555");
        let (cookie, token) = open(&app).await;
        for (name, password, again, expected) in [
            ("szef", "krotkie", "krotkie", PASSWORD_RULE),
            ("szef", "dlugie-haslo", "inne-haslo-1", MISMATCH),
            ("Szef", "dlugie-haslo", "dlugie-haslo", NAME_RULE),
            ("ab", "dlugie-haslo", "dlugie-haslo", NAME_RULE),
            ("szef%20admin", "dlugie-haslo", "dlugie-haslo", NAME_RULE),
        ] {
            let reply = send(&app, "POST", "/setup", Some(&cookie), &body(&token, name, password, again))
                .await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{name}");
            assert!(reply.body.contains(expected), "{name}: {}", reply.body);
            // Wpisana nazwa wraca do pola, hasło nie.
            assert!(!reply.body.contains(password), "{}", reply.body);
        }
        let reply = send(
            &app,
            "POST",
            "/setup",
            Some(&cookie),
            &body(&token, "szef.1", "krotkie", "krotkie"),
        )
        .await;
        assert!(reply.body.contains("value=\"szef.1\""), "{}", reply.body);
        assert_eq!(user_count(&state).await, 0);
    }

    #[tokio::test]
    async fn after_the_admin_exists_setup_is_404() {
        let state = wizard_state().await;
        let app = app_from(&state, "192.168.1.10:5555");
        let (cookie, token) = open(&app).await;
        UserRepo::new(state.pool.clone())
            .create("ktos", "haslo-ktosia-1", Role::Admin)
            .await
            .unwrap();
        assert_eq!(send(&app, "GET", "/setup", None, "").await.status, StatusCode::NOT_FOUND);
        let reply = send(
            &app,
            "POST",
            "/setup",
            Some(&cookie),
            &body(&token, "szef", "dlugie-haslo", "dlugie-haslo"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(user_count(&state).await, 1);
        // `/` i `/login` już nie prowadzą do kreatora.
        assert_eq!(
            send(&app, "GET", "/", None, "").await.location.as_deref(),
            Some("/login")
        );
        assert_eq!(send(&app, "GET", "/login", None, "").await.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_second_browser_finds_the_wizard_closed() {
        // Obie przeglądarki przeszły sprawdzenie „brak admina”, zanim
        // którakolwiek zapisała konto: rozstrzyga transakcja.
        let state = wizard_state().await;
        let app = app_from(&state, "192.168.1.10:5555");
        let (cookie, token) = open(&app).await;
        UserRepo::new(state.pool.clone())
            .create_first_admin("pierwszy", "dlugie-haslo")
            .await
            .unwrap();
        let reply = send(
            &app,
            "POST",
            "/setup",
            Some(&cookie),
            &body(&token, "drugi", "dlugie-haslo", "dlugie-haslo"),
        )
        .await;
        // Po zapisie pierwszego kreator już jest zamknięty.
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(user_count(&state).await, 1);
    }
}
