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
pub const WRONG_HOST: &str =
    "Kreator otwórz pod adresem stacji (IP albo nazwa stacji), nie przez obcą domenę.";
pub const CROSS_ORIGIN: &str = "Formularz kreatora przyszedł z innej strony — odmowa.";
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
/// Nazwa hosta z `Host` (albo z adresu żądania przy HTTP/2), bez portu,
/// małymi literami i bez kropki na końcu. IPv6 bez nawiasów.
pub(crate) fn request_host(headers: &HeaderMap, uri: &axum::http::Uri) -> Option<String> {
    let raw = match headers.get(header::HOST) {
        Some(value) => value.to_str().ok()?.trim().to_owned(),
        None => uri.authority()?.as_str().to_owned(),
    };
    let host = if let Some(rest) = raw.strip_prefix('[') {
        // `[v6]` albo `[v6]:port`.
        let (inside, after) = rest.split_once(']')?;
        if !(after.is_empty() || after.strip_prefix(':').is_some_and(is_port)) {
            return None;
        }
        inside.to_owned()
    } else {
        match raw.rsplit_once(':') {
            Some((name, port)) if is_port(port) && !name.contains(':') => name.to_owned(),
            Some(_) => return None,
            None => raw,
        }
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

fn is_port(port: &str) -> bool {
    !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit())
}

/// Obrona przed DNS rebinding: obca domena przepięta na adres stacji ma
/// w `Host` swoją nazwę. Przechodzą tylko literały IP, `localhost`, nazwy
/// stacji z `AppState::host_names` (nazwa hosta, `{nazwa}.local` i inne
/// domeny lokalne, host z `server.bind`, `server.allowed_hosts`) oraz
/// `{nazwa}-{N}.local`, którą avahi nadaje po kolizji nazw w sieci
/// (przegląd końcowy M-1). `.local` rozwiązuje tylko mDNS w sieci lokalnej,
/// więc tej nazwy nie da się podpiąć z publicznego DNS.
pub fn host_allowed(host: &str, names: &[String]) -> bool {
    let named = |host: &str| {
        names
            .iter()
            .any(|n| n.trim_end_matches('.').eq_ignore_ascii_case(host))
    };
    host.parse::<IpAddr>().is_ok() || host == "localhost" || named(host) || avahi_renamed(host).is_some_and(|h| named(&h))
}

/// `aars-2.local` → `aars.local` (avahi: `{nazwa}-{N}`, N to same cyfry).
fn avahi_renamed(host: &str) -> Option<String> {
    let label = host.strip_suffix(".local")?;
    let (base, n) = label.rsplit_once('-')?;
    let digits = !n.is_empty() && n.len() <= 4 && n.bytes().all(|b| b.is_ascii_digit());
    (digits && !base.is_empty() && !base.contains('.')).then(|| format!("{base}.local"))
}

/// `POST` z innej strony: `Sec-Fetch-Site: cross-site` albo `Origin`, który
/// nie wskazuje tego samego hosta i portu co `Host`.
fn cross_origin(headers: &HeaderMap) -> bool {
    let header_str = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if header_str("sec-fetch-site").is_some_and(|v| v.eq_ignore_ascii_case("cross-site")) {
        return true;
    }
    let Some(origin) = header_str("origin") else {
        return false;
    };
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    match (authority, header_str("host")) {
        (Some(authority), Some(host)) => !authority.eq_ignore_ascii_case(host.trim()),
        // `Origin: null` albo bez `Host`: nie da się potwierdzić tej samej strony.
        _ => true,
    }
}

async fn guard(
    state: &AppState,
    extensions: &Extensions,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Result<IpAddr, Response> {
    match request_host(headers, uri) {
        Some(host) if host_allowed(&host, &state.host_names) => {}
        other => {
            tracing::warn!(host = ?other, "kreator pierwszego konta: obca nazwa w Host");
            return Err((StatusCode::MISDIRECTED_REQUEST, WRONG_HOST).into_response());
        }
    }
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

pub async fn page_view(
    State(state): State<AppState>,
    extensions: Extensions,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if let Err(response) = guard(&state, &extensions, &headers, &uri).await {
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
    uri: axum::http::Uri,
    Form(form): Form<SetupForm>,
) -> Response {
    let ip = match guard(&state, &extensions, &headers, &uri).await {
        Ok(ip) => ip,
        Err(response) => return response,
    };
    if cross_origin(&headers) {
        tracing::warn!(%ip, "kreator pierwszego konta: formularz z innej strony");
        return (StatusCode::FORBIDDEN, CROSS_ORIGIN).into_response();
    }
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
            .with_host_names(vec!["aars".to_owned(), "aars.local".to_owned()])
    }

    #[test]
    fn host_header_parsing_and_the_allow_list() {
        let names = vec!["aars".to_owned(), "aars.local".to_owned()];
        let host = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, value.parse().unwrap());
            request_host(&headers, &axum::http::Uri::from_static("/setup"))
        };
        let allowed = |value: &str| host(value).is_some_and(|h| host_allowed(&h, &names));
        for ok in [
            "192.168.1.10",
            "192.168.1.10:8080",
            "[fe80::1]:8080",
            "[2a01:110::5]",
            "aars.local",
            "AARS.local.:8080",
            "aars",
            "localhost:8080",
            // avahi po kolizji nazw (M-1).
            "aars-2.local",
            "AARS-13.local:8080",
            "[::ffff:192.168.1.10]:8080",
        ] {
            assert!(allowed(ok), "{ok}");
        }
        for bad in [
            "evil.example",
            "evil.example:8080",
            "aars.local.evil.example",
            "192.168.1.10.nip.io",
            "aars.local:80:80",
            "[fe80::1]x",
            "",
            "aars-.local",
            "aars-2x.local",
            "aars-2.lan",
            "inna-2.local",
            "evil.aars-2.local",
            "aars-2.local.evil.example",
            "aars-12345.local",
        ] {
            assert!(!allowed(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_rebound_foreign_host_is_refused() {
        let state = wizard_state().await;
        let app = app_from(&state, "192.168.1.10:5555");
        for method in ["GET", "POST"] {
            let reply = send_with(&app, method, "/setup", None, "", &[("host", "evil.example")]).await;
            assert_eq!(reply.status, StatusCode::MISDIRECTED_REQUEST, "{method}");
            assert!(reply.cookies.is_empty());
        }
        // Brak `Host` też odpada.
        let reply = send_with(&app, "GET", "/setup", None, "", &[]).await;
        assert_eq!(reply.status, StatusCode::MISDIRECTED_REQUEST);
        for host in ["192.168.1.10:8080", "aars.local", "aars.local:8080"] {
            let reply = send_with(&app, "GET", "/setup", None, "", &[("host", host)]).await;
            assert_eq!(reply.status, StatusCode::OK, "{host}");
        }
    }

    #[tokio::test]
    async fn an_ipv4_client_on_a_dual_stack_socket_passes_both_checks() {
        // Obraz słucha na `[::]:8080` (przegląd końcowy M-2): klient IPv4
        // przychodzi jako `::ffff:a.b.c.d`, a `Host` ma literał IPv4 albo
        // nazwę stacji, także po kolizji w avahi (M-1).
        let state = wizard_state().await;
        let app = app_from(&state, "[::ffff:192.168.1.10]:5555");
        for host in ["192.168.1.2:8080", "aars.local:8080", "aars-2.local:8080", "[::ffff:192.168.1.2]:8080"] {
            let reply = send_with(&app, "GET", "/setup", None, "", &[("host", host)]).await;
            assert_eq!(reply.status, StatusCode::OK, "{host}");
        }
        // Publiczny IPv4 zmapowany na IPv6 dalej nie jest lokalny.
        let app = app_from(&state, "[::ffff:8.8.8.8]:5555");
        let reply = send(&app, "GET", "/setup", None, "").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_cross_origin_post_is_refused() {
        let state = wizard_state().await;
        let app = app_from(&state, "192.168.1.10:5555");
        let (cookie, token) = open(&app).await;
        let form = body(&token, "szef", "dlugie-haslo", "dlugie-haslo");
        for extra in [
            vec![("host", "192.168.1.2:8080"), ("origin", "http://evil.example")],
            vec![("host", "192.168.1.2:8080"), ("origin", "null")],
            vec![("host", "192.168.1.2:8080"), ("origin", "http://192.168.1.2:9999")],
            vec![("host", "192.168.1.2:8080"), ("sec-fetch-site", "cross-site")],
        ] {
            let reply = send_with(&app, "POST", "/setup", Some(&cookie), &form, &extra).await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{extra:?}");
            assert_eq!(reply.body, CROSS_ORIGIN);
        }
        assert_eq!(user_count(&state).await, 0);
        // Ta sama strona: `Origin` zgodny z `Host` i `same-origin` przechodzą.
        let reply = send_with(
            &app,
            "POST",
            "/setup",
            Some(&cookie),
            &form,
            &[
                ("host", "192.168.1.2:8080"),
                ("origin", "http://192.168.1.2:8080"),
                ("sec-fetch-site", "same-origin"),
            ],
        )
        .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        assert_eq!(user_count(&state).await, 1);
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
        send_with(app, method, uri, cookie, body, &[("host", "192.168.1.2:8080")]).await
    }

    async fn send_with(
        app: &Router,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        body: &str,
        extra: &[(&str, &str)],
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
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

    /// Przegląd 8–9 M-4: kontrola `Host` na całym panelu. `POST /login`
    /// pod przepiętą nazwą nie dostaje sesji.
    #[tokio::test]
    async fn the_whole_panel_refuses_a_foreign_host() {
        let state = AppState::for_tests(aars_core::db::test_pool().await)
            .with_host_names(vec!["aars".to_owned(), "aars.local".to_owned(), "panel.lan".to_owned()])
            .with_host_check(true);
        UserRepo::new(state.pool.clone())
            .create("op", "dlugie-haslo-1", aars_core::user::Role::Operator)
            .await
            .unwrap();
        let app = app_from(&state, "192.168.1.10:5555");
        let login = "username=op&password=dlugie-haslo-1";
        let evil = [("host", "evil.example")];
        let reply = send_with(&app, "POST", "/login", None, login, &evil).await;
        assert_eq!(reply.status, StatusCode::MISDIRECTED_REQUEST);
        assert!(reply.cookies.is_empty(), "{:?}", reply.cookies);
        for path in ["/", "/login", "/healthz"] {
            let reply = send_with(&app, "GET", path, None, "", &[("host", "evil.example:8080")]).await;
            assert_eq!(reply.status, StatusCode::MISDIRECTED_REQUEST, "{path}");
        }
        let reply = send_with(&app, "GET", "/healthz", None, "", &[]).await;
        assert_eq!(reply.status, StatusCode::MISDIRECTED_REQUEST, "bez Host");
        for host in ["192.168.1.2:8080", "[fe80::1]:8080", "localhost:8080", "aars.local", "PANEL.lan:8080"] {
            let reply = send_with(&app, "GET", "/healthz", None, "", &[("host", host)]).await;
            assert_eq!(reply.status, StatusCode::OK, "{host}");
        }
        let ok = [("host", "aars.local:8080")];
        let reply = send_with(&app, "POST", "/login", None, login, &ok).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        assert!(!reply.cookies.is_empty());
    }

    /// Przegląd 8–9 M-3: zalogowany operator zostaje na pulpicie z
    /// wyłącznikiem nadawania, a pasek prowadzi do kreatora.
    #[tokio::test]
    async fn a_logged_in_operator_keeps_the_dashboard_without_an_admin() {
        let state = wizard_state().await;
        let repo = UserRepo::new(state.pool.clone());
        let id = repo
            .create("op", "dlugie-haslo-1", aars_core::user::Role::Operator)
            .await
            .unwrap();
        let session = repo.start_session(id, None).await.unwrap();
        let app = app_from(&state, "203.0.113.7:5555");
        let cookie = format!("{}={}", crate::auth::SESSION_COOKIE, session.id);
        let reply = send(&app, "GET", "/", Some(&cookie), "").await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.location);
        assert!(reply.body.contains("Zatrzymaj nadawanie"), "{}", reply.body);
        assert!(reply.body.contains("Brak konta administratora"), "{}", reply.body);
        let reply = send(&app, "GET", "/", None, "").await;
        assert_eq!(reply.location.as_deref(), Some("/setup"));
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
