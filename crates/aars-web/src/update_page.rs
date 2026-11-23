//! Aktualizacja z panelu (admin): wersja, sprawdzenie wydania, instalacja
//! z potwierdzeniem, próba nowej wersji i ostatni wynik (etap 10, R11, R18).
//! Pobieranie, weryfikacja i audyt `update_install` żyją w binarce za
//! traitem `UpdateControl`; strona tylko pyta i zleca.

use aars_core::update::{Available, MAX_ATTEMPTS, ResultKind, UpdateStatus};
use aars_core::user::Role;
use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::chrome::Chrome;
use crate::render::page;
use crate::state::AppState;

/// Odpowiedź, gdy proces nie ma sterownika aktualizacji.
pub const NO_UPDATE: &str = "aktualizacja nie działa w tym procesie";
const CONFIRM_NEEDED: &str = "Zaznacz potwierdzenie — aktualizacja restartuje stację.";
const NO_VERSION: &str = "Brak wersji do instalacji — sprawdź aktualizacje jeszcze raz.";

/// Wynik „Sprawdź aktualizacje”.
enum CheckView {
    Latest(String),
    Available {
        version: String,
        size_mb: String,
        notes: String,
    },
    Error(String),
}

#[derive(Template)]
#[template(path = "update.html")]
struct UpdatePage {
    chrome: Chrome,
    current: String,
    previous: String,
    /// (wersja, numer startu, wersja, do której stacja wróci).
    trial: Option<(String, u32, String)>,
    last: Option<(&'static str, String)>,
    /// Powód, dla którego aktualizacja z panelu jest wyłączona.
    unsupported: Option<String>,
    check: Option<CheckView>,
    /// (klasa CSS, tekst) po zleceniu albo odmowie instalacji.
    notice: Option<(&'static str, String)>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CsrfOnly {
    #[serde(default)]
    csrf: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct InstallForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    confirm: String,
}

/// Rozmiar w MB z przecinkiem dziesiętnym.
fn megabytes(size: u64) -> String {
    format!("{:.1}", size as f64 / 1_000_000.0).replace('.', ",")
}

fn last_text(status: &UpdateStatus) -> Option<(&'static str, String)> {
    let last = status.last.as_ref()?;
    Some(match last.kind {
        ResultKind::Installed => ("badge on", format!("Zainstalowano {}", last.version)),
        ResultKind::RolledBack => (
            "error",
            format!(
                "Cofnięto {} → {}: {}. Zmiany z czasu próby przepadły \
                 (kopia bazy sprzed aktualizacji).",
                last.version, last.from, last.detail
            ),
        ),
        ResultKind::Failed => (
            "error",
            format!("Aktualizacja nie powiodła się: {}", last.detail),
        ),
    })
}

fn status_of(state: &AppState) -> UpdateStatus {
    match &state.update {
        Some(update) => update.status(),
        None => UpdateStatus {
            current: state.version.to_owned(),
            previous: None,
            trial: None,
            unsupported: Some(NO_UPDATE.to_owned()),
            last: None,
        },
    }
}

fn show(
    state: &AppState,
    chrome: Chrome,
    status: StatusCode,
    check: Option<CheckView>,
    notice: Option<(&'static str, String)>,
) -> Response {
    let s = status_of(state);
    let last = last_text(&s);
    // `attempts` to numer bieżącego startu (przebieg z instalacją = 1, R8).
    let trial = s.trial.as_ref().map(|t| {
        (
            t.version.clone(),
            t.attempts.clamp(1, MAX_ATTEMPTS),
            t.previous.clone(),
        )
    });
    let template = UpdatePage {
        chrome,
        current: s.current,
        previous: s.previous.unwrap_or_else(|| "brak".to_owned()),
        trial,
        last,
        unsupported: s.unsupported,
        check,
        notice,
    };
    let mut response = page(&template);
    if response.status() == StatusCode::OK {
        *response.status_mut() = status;
    }
    response
}

pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Admin)?;
    Ok(show(&state, Chrome::load(&state, &current).await, StatusCode::OK, None, None))
}

pub async fn check(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<CsrfOnly>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Admin)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;
    let Some(update) = &state.update else {
        return Ok((StatusCode::SERVICE_UNAVAILABLE, NO_UPDATE).into_response());
    };
    let view = match update.check().await {
        Ok(None) => CheckView::Latest(update.status().current),
        Ok(Some(Available {
            version,
            notes,
            size,
        })) => CheckView::Available {
            version,
            size_mb: megabytes(size),
            notes,
        },
        Err(err) => CheckView::Error(err.to_string()),
    };
    let chrome = Chrome::load(&state, &current).await;
    Ok(show(&state, chrome, StatusCode::OK, Some(view), None))
}

pub async fn install(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<InstallForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Admin)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;
    let chrome = Chrome::load(&state, &current).await;
    let refuse = |status, text: &str| {
        show(
            &state,
            chrome.clone(),
            status,
            None,
            Some(("error", text.to_owned())),
        )
    };
    if form.confirm != "tak" {
        return Ok(refuse(StatusCode::BAD_REQUEST, CONFIRM_NEEDED));
    }
    let Some(update) = &state.update else {
        return Ok((StatusCode::SERVICE_UNAVAILABLE, NO_UPDATE).into_response());
    };
    let version = form.version.trim();
    if version.is_empty() {
        return Ok(refuse(StatusCode::BAD_REQUEST, NO_VERSION));
    }
    match update.install(version, &current.user.username).await {
        Ok(()) => {
            tracing::warn!(user = %current.user.username, version, "aktualizacja zlecona z panelu");
            Ok(show(
                &state,
                chrome.clone(),
                StatusCode::OK,
                None,
                Some((
                    "badge on",
                    format!(
                        "Aktualizacja do {version} zlecona. Stacja dokończy bieżące nadanie \
                         i uruchomi się ponownie — odśwież stronę za minutę."
                    ),
                )),
            ))
        }
        Err(refusal) => {
            tracing::warn!(user = %current.user.username, version, %refusal, "odmowa aktualizacji");
            Ok(refuse(StatusCode::CONFLICT, &refusal.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router;
    use crate::state::testing::FakeUpdate;
    use crate::tests::login;
    use aars_core::update::{LastResult, Trial, UpdateRefusal};
    use aars_core::user::UserRepo;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, header};
    use std::sync::Arc;
    use tower::ServiceExt;

    struct Env {
        app: Router,
        cookie: String,
        csrf: String,
        update: Option<Arc<FakeUpdate>>,
    }

    async fn env_with(role: Role, update: Option<Arc<FakeUpdate>>) -> Env {
        let mut state = AppState::for_tests(aars_core::db::test_pool().await);
        if let Some(update) = &update {
            state = state.with_update(update.clone());
        }
        UserRepo::new(state.pool.clone())
            .create("admin", "haslo", role)
            .await
            .unwrap();
        let app = router(state);
        let (cookie, csrf) = login(&app, "admin", "haslo").await;
        Env {
            app,
            cookie,
            csrf,
            update,
        }
    }

    async fn admin_env(update: FakeUpdate) -> Env {
        env_with(Role::Admin, Some(Arc::new(update))).await
    }

    fn available(notes: &str) -> Available {
        Available {
            version: "0.11.0".to_owned(),
            notes: notes.to_owned(),
            size: 12_300_000,
        }
    }

    async fn send(env: &Env, method: &str, uri: &str, body: String) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, &env.cookie);
        if method == "POST" {
            builder = builder.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        let response = env
            .app
            .clone()
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn get(env: &Env) -> (StatusCode, String) {
        send(env, "GET", "/settings/update", String::new()).await
    }

    async fn post(env: &Env, uri: &str, extra: &str) -> (StatusCode, String) {
        let body = format!("csrf={}{extra}", env.csrf);
        send(env, "POST", uri, body).await
    }

    fn installs(env: &Env) -> Vec<(String, String)> {
        env.update
            .as_ref()
            .map(|u| u.installs.lock().unwrap().clone())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn the_page_shows_the_running_and_previous_version() {
        let fake = FakeUpdate::new("0.10.0");
        fake.status.lock().unwrap().previous = Some("0.9.0".to_owned());
        let env = admin_env(fake).await;
        let (status, body) = get(&env).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Działa: 0.10.0"), "{body}");
        assert!(body.contains("Poprzednia: 0.9.0"), "{body}");
        assert!(body.contains("Sprawdź aktualizacje"), "{body}");
        assert!(body.contains("action=\"/settings/update/check\""), "{body}");

        let env = env_with(Role::Admin, Some(Arc::new(FakeUpdate::new("0.10.0")))).await;
        let (_, body) = get(&env).await;
        assert!(body.contains("Poprzednia: brak"), "{body}");
    }

    #[tokio::test]
    async fn a_check_with_a_newer_version_offers_the_install_form() {
        let fake = FakeUpdate::new("0.10.0");
        *fake.available.lock().unwrap() = Ok(Some(available("Poprawki.")));
        let env = admin_env(fake).await;
        let (status, body) = post(&env, "/settings/update/check", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Dostępna wersja 0.11.0 (12,3 MB)"), "{body}");
        assert!(body.contains("Poprawki."), "{body}");
        assert!(body.contains("Zainstaluj 0.11.0"), "{body}");
        assert!(body.contains("name=\"confirm\""), "{body}");
        assert!(body.contains("name=\"version\" value=\"0.11.0\""), "{body}");
        assert!(
            body.contains("i wstanie na wersji 0.11.0. Gdy nowa wersja nie wstanie, wróci do 0.10.0."),
            "{body}"
        );
        assert!(installs(&env).is_empty(), "sprawdzenie niczego nie instaluje");
    }

    #[tokio::test]
    async fn a_check_without_news_says_so() {
        let env = admin_env(FakeUpdate::new("0.10.0")).await;
        let (status, body) = post(&env, "/settings/update/check", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Działa najnowsza wersja (0.10.0)."), "{body}");
        assert!(!body.contains("name=\"confirm\""), "{body}");
    }

    #[tokio::test]
    async fn a_check_error_is_shown() {
        let fake = FakeUpdate::new("0.10.0");
        *fake.available.lock().unwrap() = Err(UpdateRefusal::Failed(
            "błąd TLS — sprawdź datę i godzinę stacji (synchronizacja NTP)".to_owned(),
        ));
        let env = admin_env(fake).await;
        let (status, body) = post(&env, "/settings/update/check", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Nie udało się sprawdzić: błąd TLS"), "{body}");
        assert!(!body.contains("name=\"confirm\""), "{body}");
    }

    #[tokio::test]
    async fn release_notes_are_escaped() {
        let fake = FakeUpdate::new("0.10.0");
        *fake.available.lock().unwrap() =
            Ok(Some(available("<script>alert(1)</script>\nlinia 2")));
        let env = admin_env(fake).await;
        let (_, body) = post(&env, "/settings/update/check", "").await;
        assert!(!body.contains("<script>alert"), "{body}");
        // Askama escapuje encjami liczbowymi (`&#60;` zamiast `&lt;`).
        assert!(body.contains("&#60;script&#62;alert(1)"), "{body}");
        assert!(body.contains("white-space:pre-wrap"), "{body}");
    }

    #[tokio::test]
    async fn install_needs_the_confirmation() {
        let env = admin_env(FakeUpdate::new("0.10.0")).await;
        let (status, body) = post(&env, "/settings/update/install", "&version=0.11.0").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains(CONFIRM_NEEDED), "{body}");
        let (status, _) = post(
            &env,
            "/settings/update/install",
            "&version=0.11.0&confirm=tak2",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(installs(&env).is_empty());
    }

    #[tokio::test]
    async fn a_confirmed_install_reaches_the_controller() {
        let env = admin_env(FakeUpdate::new("0.10.0")).await;
        let (status, body) = post(
            &env,
            "/settings/update/install",
            "&version=0.11.0&confirm=tak",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            installs(&env),
            vec![("0.11.0".to_owned(), "admin".to_owned())]
        );
        assert!(
            body.contains(
                "Aktualizacja do 0.11.0 zlecona. Stacja dokończy bieżące nadanie i uruchomi się \
                 ponownie — odśwież stronę za minutę."
            ),
            "{body}"
        );
    }

    #[tokio::test]
    async fn an_empty_version_is_refused_before_the_controller() {
        let env = admin_env(FakeUpdate::new("0.10.0")).await;
        let (status, body) = post(&env, "/settings/update/install", "&confirm=tak").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains(NO_VERSION), "{body}");
        assert!(installs(&env).is_empty());
    }

    #[tokio::test]
    async fn a_refusal_is_409_with_its_text() {
        let fake = FakeUpdate::new("0.10.0");
        *fake.install_result.lock().unwrap() = Err(UpdateRefusal::UrgentPending);
        let env = admin_env(fake).await;
        let (status, body) = post(
            &env,
            "/settings/update/install",
            "&version=0.11.0&confirm=tak",
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.contains("Czeka pilny komunikat"), "{body}");
    }

    #[tokio::test]
    async fn an_unsupported_station_shows_the_reason_without_forms() {
        let fake = FakeUpdate::new("0.10.0");
        fake.status.lock().unwrap().unsupported = Some(UpdateRefusal::Unsupported.to_string());
        let env = admin_env(fake).await;
        let (status, body) = get(&env).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("Aktualizacja z panelu działa tylko w obrazie Raspberry Pi"),
            "{body}"
        );
        assert!(body.contains("Działa: 0.10.0"), "{body}");
        assert!(!body.contains("/settings/update/check"), "{body}");
        assert!(!body.contains("/settings/update/install"), "{body}");
    }

    #[tokio::test]
    async fn a_trial_warns_against_hardware_changes() {
        let fake = FakeUpdate::new("0.11.0");
        fake.status.lock().unwrap().trial = Some(Trial {
            format: 1,
            version: "0.11.0".to_owned(),
            previous: "0.10.0".to_owned(),
            installed_at: "2026-11-21T10:00:00Z".to_owned(),
            attempts: 1,
            radio_was_up: true,
        });
        let env = admin_env(fake).await;
        let (_, body) = get(&env).await;
        assert!(
            body.contains(
                "Próba wersji 0.11.0: start 1 z 3. Stacja potwierdzi ją po minucie pracy; \
                 gdy nie wstanie, wróci do 0.10.0."
            ),
            "{body}"
        );
        assert!(
            body.contains(
                "Nie zmieniaj teraz sprzętu — nieudany start w czasie próby cofnie aktualizację."
            ),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_rollback_result_says_trial_data_was_lost() {
        let fake = FakeUpdate::new("0.10.0");
        fake.status.lock().unwrap().last = Some(LastResult {
            format: 1,
            at: "2026-11-21T10:05:00Z".to_owned(),
            kind: ResultKind::RolledBack,
            version: "0.11.0".to_owned(),
            from: "0.10.0".to_owned(),
            detail: "trzy starty bez potwierdzenia".to_owned(),
        });
        let env = admin_env(fake).await;
        let (_, body) = get(&env).await;
        assert!(
            body.contains(
                "Cofnięto 0.11.0 → 0.10.0: trzy starty bez potwierdzenia. Zmiany z czasu próby \
                 przepadły (kopia bazy sprzed aktualizacji)."
            ),
            "{body}"
        );

        let fake = FakeUpdate::new("0.11.0");
        fake.status.lock().unwrap().last = Some(LastResult {
            format: 1,
            at: "2026-11-21T10:05:00Z".to_owned(),
            kind: ResultKind::Installed,
            version: "0.11.0".to_owned(),
            from: "0.10.0".to_owned(),
            detail: String::new(),
        });
        let env = admin_env(fake).await;
        let (_, body) = get(&env).await;
        assert!(body.contains("Zainstalowano 0.11.0"), "{body}");
    }

    #[tokio::test]
    async fn an_operator_cannot_open_or_post_update() {
        let env = env_with(Role::Operator, Some(Arc::new(FakeUpdate::new("0.10.0")))).await;
        let (status, _) = get(&env).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = post(&env, "/settings/update/check", "").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = post(
            &env,
            "/settings/update/install",
            "&version=0.11.0&confirm=tak",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(installs(&env).is_empty());
    }

    #[tokio::test]
    async fn without_an_update_controller_install_is_503() {
        let env = env_with(Role::Admin, None).await;
        let (status, body) = post(
            &env,
            "/settings/update/install",
            "&version=0.11.0&confirm=tak",
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, NO_UPDATE);
        let (status, _) = post(&env, "/settings/update/check", "").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        // Strona dalej pokazuje wersję i powód.
        let (status, body) = get(&env).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Działa: 0.0.0-test"), "{body}");
        assert!(body.contains(NO_UPDATE), "{body}");
    }

    #[tokio::test]
    async fn a_wrong_csrf_token_installs_nothing() {
        let env = admin_env(FakeUpdate::new("0.10.0")).await;
        let (status, _) = send(
            &env,
            "POST",
            "/settings/update/install",
            "csrf=zly&version=0.11.0&confirm=tak".to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(installs(&env).is_empty());
    }

    #[test]
    fn audit_labels_for_update_actions() {
        use crate::chrome::audit_action_label;
        for (action, label) in [
            ("update_install", "aktualizacja zlecona"),
            ("update_installed", "aktualizacja zainstalowana"),
            ("update_rolled_back", "aktualizacja cofnięta"),
            ("update_failed", "aktualizacja nieudana"),
            ("first_admin", "pierwsze konto admina"),
            ("admin_reset", "reset kont admina"),
        ] {
            assert_eq!(audit_action_label(action), label);
        }
    }
}
