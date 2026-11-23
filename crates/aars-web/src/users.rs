//! Zarządzanie użytkownikami (admin): konta, role, reset hasła.
//!
//! Hasło tymczasowe pojawia się tylko w odpowiedzi na żądanie, które je
//! wygenerowało. Nie trafia do logów (`tracing`) ani do `audit_log`.

use aars_core::error::DbError;
use aars_core::user::{AuthError, Role, UserChangeError, UserRepo};
use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::audit;
use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::chrome::{Chrome, role_label};
use crate::render::page;
use crate::state::AppState;

struct Row {
    id: i64,
    username: String,
    role: &'static str,
    role_label: &'static str,
    disabled: bool,
    must_change: bool,
    is_self: bool,
}

#[derive(Template)]
#[template(path = "users.html")]
struct UsersPage {
    chrome: Chrome,
    rows: Vec<Row>,
    error: Option<String>,
    /// Nazwa i hasło tymczasowe, pokazywane jeden raz.
    secret: Option<(String, String)>,
    new_username: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateForm {
    csrf: String,
    username: String,
    role: String,
}

#[derive(Debug, Deserialize)]
pub struct RoleForm {
    csrf: String,
    role: String,
}

#[derive(Debug, Deserialize)]
pub struct DisabledForm {
    csrf: String,
    disabled: String,
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

const SELF_MESSAGE: &str = "nie możesz odebrać uprawnień sam sobie";

/// `[a-z0-9_.-]{3,32}`.
fn valid_username(name: &str) -> bool {
    (3..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
}

async fn render(
    state: &AppState,
    current: &CurrentUser,
    status: StatusCode,
    error: Option<String>,
    secret: Option<(String, String)>,
    new_username: &str,
) -> Response {
    let users = match UserRepo::new(state.pool.clone()).list().await {
        Ok(users) => users,
        Err(err) => {
            tracing::error!(%err, "odczyt użytkowników zawiódł");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "błąd odczytu użytkowników",
            )
                .into_response();
        }
    };
    let rows = users
        .into_iter()
        .map(|u| Row {
            id: u.id,
            is_self: u.id == current.user.id,
            role: u.role.as_str(),
            role_label: role_label(u.role),
            username: u.username,
            disabled: u.disabled,
            must_change: u.must_change_password,
        })
        .collect();
    let body = page(&UsersPage {
        chrome: Chrome::load(state, current).await,
        rows,
        error,
        secret,
        new_username: new_username.to_owned(),
    });
    let mut response = (status, body).into_response();
    // Strona może zawierać hasło: nie wolno jej zapisywać w pamięci podręcznej.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().expect("stała"));
    response
}

fn db_failure(what: &str, err: &dyn std::fmt::Display) -> Response {
    tracing::error!(%err, "{what}");
    (StatusCode::INTERNAL_SERVER_ERROR, "błąd zapisu użytkownika").into_response()
}

/// Odmowa ochrony ostatniego admina to 400 z komunikatem, reszta to 500.
async fn change_failure(
    state: &AppState,
    current: &CurrentUser,
    what: &str,
    err: UserChangeError,
) -> Response {
    match err {
        UserChangeError::LastAdmin => {
            render(
                state,
                current,
                StatusCode::BAD_REQUEST,
                Some(err.to_string()),
                None,
                "",
            )
            .await
        }
        UserChangeError::Db(err) => db_failure(what, &err),
    }
}

fn is_unique_violation(err: &AuthError) -> bool {
    matches!(err, AuthError::Db(DbError::Sqlx(sqlx::Error::Database(e))) if e.is_unique_violation())
}

pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Admin)?;
    Ok(render(&state, &current, StatusCode::OK, None, None, "").await)
}

pub async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<CreateForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Admin)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let username = form.username.trim();
    let bad_request = |message: &str| {
        render(
            &state,
            &current,
            StatusCode::BAD_REQUEST,
            Some(message.to_owned()),
            None,
            username,
        )
    };
    if !valid_username(username) {
        return Ok(bad_request("nazwa: 3–32 znaki, tylko a–z, 0–9, _ . -").await);
    }
    let Some(role) = Role::parse(&form.role) else {
        return Ok(bad_request("nieznana rola").await);
    };

    let repo = UserRepo::new(state.pool.clone());
    match repo.create_with_temporary_password(username, role).await {
        Ok((_, password)) => {
            audit(
                &state,
                current.user.id,
                "user_create",
                &format!("{username} ({})", role.as_str()),
            )
            .await;
            Ok(render(
                &state,
                &current,
                StatusCode::OK,
                None,
                Some((username.to_owned(), password)),
                "",
            )
            .await)
        }
        Err(err) if is_unique_violation(&err) => {
            Ok(bad_request("taki użytkownik już istnieje").await)
        }
        Err(err) => Ok(db_failure("tworzenie użytkownika zawiodło", &err)),
    }
}

/// Wspólny początek akcji na koncie: rola, CSRF, ochrona przed samym sobą,
/// istnienie konta. `Err` niesie gotową odpowiedź.
async fn target(
    state: &AppState,
    current: &CurrentUser,
    csrf: &str,
    id: i64,
    protect_self: bool,
) -> Result<aars_core::user::User, Response> {
    require_role(&current.user, Role::Admin).map_err(IntoResponse::into_response)?;
    require_csrf(&current.csrf_token, Some(csrf)).map_err(IntoResponse::into_response)?;
    if protect_self && id == current.user.id {
        return Err(render(
            state,
            current,
            StatusCode::BAD_REQUEST,
            Some(SELF_MESSAGE.to_owned()),
            None,
            "",
        )
        .await);
    }
    match UserRepo::new(state.pool.clone()).by_id(id).await {
        Ok(Some(user)) => Ok(user),
        Ok(None) => Err((StatusCode::NOT_FOUND, "nie ma takiego użytkownika").into_response()),
        Err(err) => Err(db_failure("odczyt użytkownika zawiódł", &err)),
    }
}

pub async fn set_role(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<RoleForm>,
) -> Response {
    let user = match target(&state, &current, &form.csrf, id, true).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let Some(role) = Role::parse(&form.role) else {
        return render(
            &state,
            &current,
            StatusCode::BAD_REQUEST,
            Some("nieznana rola".to_owned()),
            None,
            "",
        )
        .await;
    };
    if let Err(err) = UserRepo::new(state.pool.clone()).set_role(id, role).await {
        return change_failure(&state, &current, "zmiana roli zawiodła", err).await;
    }
    audit(
        &state,
        current.user.id,
        "user_role",
        &format!(
            "{}: {} -> {}",
            user.username,
            user.role.as_str(),
            role.as_str()
        ),
    )
    .await;
    render(&state, &current, StatusCode::OK, None, None, "").await
}

pub async fn set_disabled(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<DisabledForm>,
) -> Response {
    let user = match target(&state, &current, &form.csrf, id, true).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let disabled = form.disabled == "1";
    if let Err(err) = UserRepo::new(state.pool.clone())
        .set_disabled(id, disabled)
        .await
    {
        return change_failure(&state, &current, "zmiana stanu konta zawiodła", err).await;
    }
    audit(
        &state,
        current.user.id,
        "user_disabled",
        &format!(
            "{}: {}",
            user.username,
            if disabled {
                "wyłączony"
            } else {
                "włączony"
            }
        ),
    )
    .await;
    render(&state, &current, StatusCode::OK, None, None, "").await
}

pub async fn reset(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Response {
    // Reset własnego hasła kończyłby własną sesję; do tego służy /account/password.
    let user = match target(&state, &current, &form.csrf, id, true).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    match UserRepo::new(state.pool.clone()).reset_password(id).await {
        Ok(password) => {
            audit(&state, current.user.id, "user_reset", &user.username).await;
            render(
                &state,
                &current,
                StatusCode::OK,
                None,
                Some((user.username, password)),
                "",
            )
            .await
        }
        Err(err) => db_failure("reset hasła zawiódł", &err),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Response {
    let user = match target(&state, &current, &form.csrf, id, true).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    if let Err(err) = UserRepo::new(state.pool.clone()).delete(id).await {
        return change_failure(&state, &current, "usunięcie użytkownika zawiodło", err).await;
    }
    audit(&state, current.user.id, "user_delete", &user.username).await;
    render(&state, &current, StatusCode::OK, None, None, "").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Option<String>, String) {
        let response = app.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            location,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    async fn get(app: &Router, uri: &str, cookie: &str) -> (StatusCode, Option<String>, String) {
        send(
            app,
            Request::builder()
                .uri(uri)
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post(
        app: &Router,
        uri: &str,
        cookie: &str,
        body: String,
    ) -> (StatusCode, Option<String>, String) {
        send(
            app,
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    async fn login(app: &Router, name: &str, password: &str) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("username={name}&password={password}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or_default().to_owned())
            .expect("logowanie musi dać ciasteczko")
    }

    async fn csrf_of(app: &Router, cookie: &str) -> String {
        let (_, _, html) = get(app, "/users", cookie).await;
        let html = if html.contains("name=\"csrf\"") {
            html
        } else {
            get(app, "/", cookie).await.2
        };
        html.split("name=\"csrf\" value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .to_owned()
    }

    async fn logged_in(role: Role) -> (Router, String, String, AppState) {
        let state = AppState::for_tests(aars_core::db::test_pool().await);
        UserRepo::new(state.pool.clone())
            .create("u", "haslo", role)
            .await
            .unwrap();
        let app = router(state.clone());
        let cookie = login(&app, "u", "haslo").await;
        let csrf = csrf_of(&app, &cookie).await;
        (app, cookie, csrf, state)
    }

    async fn user_id(state: &AppState, name: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(name)
            .fetch_one(&state.pool)
            .await
            .unwrap()
    }

    fn shown_password(html: &str) -> String {
        html.split("id=\"new-password\">")
            .nth(1)
            .unwrap()
            .split('<')
            .next()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn an_admin_creates_an_operator_who_must_change_the_password() {
        let (app, cookie, csrf, state) = logged_in(Role::Admin).await;
        let (status, _, html) = post(
            &app,
            "/users",
            &cookie,
            format!("csrf={csrf}&username=nowy&role=operator"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let password = shown_password(&html);
        assert_eq!(password.len(), 20);

        let fresh = login(&app, "nowy", &password).await;
        let (status, location, _) = get(&app, "/", &fresh).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.as_deref(), Some("/account/password"));

        // Hasło pokazano raz: ani ponowne wejście na stronę, ani audyt go nie mają.
        let (_, _, again) = get(&app, "/users", &cookie).await;
        assert!(!again.contains(&password));
        let details: Vec<String> = sqlx::query_scalar("SELECT detail FROM audit_log")
            .fetch_all(&state.pool)
            .await
            .unwrap();
        assert!(
            details.iter().all(|d| !d.contains(&password)),
            "{details:?}"
        );
        let action: String =
            sqlx::query_scalar("SELECT action FROM audit_log WHERE detail LIKE 'nowy%'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(action, "user_create");
    }

    #[tokio::test]
    async fn an_admin_cannot_demote_or_disable_themselves() {
        let (app, cookie, csrf, state) = logged_in(Role::Admin).await;
        let id = user_id(&state, "u").await;
        for (path, body) in [
            ("role", format!("csrf={csrf}&role=viewer")),
            ("disabled", format!("csrf={csrf}&disabled=1")),
            ("delete", format!("csrf={csrf}")),
            ("reset", format!("csrf={csrf}")),
        ] {
            let (status, _, html) = post(&app, &format!("/users/{id}/{path}"), &cookie, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
            assert!(
                html.contains("nie możesz odebrać uprawnień sam sobie"),
                "{path}"
            );
        }
        let (role, disabled): (String, i64) =
            sqlx::query_as("SELECT role, disabled FROM users WHERE id = ?")
                .bind(id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!((role.as_str(), disabled), ("admin", 0));
    }

    #[tokio::test]
    async fn a_duplicate_username_is_refused() {
        let (app, cookie, csrf, _) = logged_in(Role::Admin).await;
        let (status, _, _) = post(
            &app,
            "/users",
            &cookie,
            format!("csrf={csrf}&username=u&role=viewer"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = post(
            &app,
            "/users",
            &cookie,
            format!("csrf={csrf}&username=Ab&role=viewer"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_operator_cannot_manage_users() {
        let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
        assert_eq!(get(&app, "/users", &cookie).await.0, StatusCode::FORBIDDEN);
        let (status, _, _) = post(
            &app,
            "/users",
            &cookie,
            format!("csrf={csrf}&username=nowy&role=admin"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let id = user_id(&state, "u").await;
        let (status, _, _) = post(
            &app,
            &format!("/users/{id}/role"),
            &cookie,
            format!("csrf={csrf}&role=admin"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_admin_manages_another_account_and_it_is_audited() {
        let (app, cookie, csrf, state) = logged_in(Role::Admin).await;
        let repo = UserRepo::new(state.pool.clone());
        let id = repo.create("kolega", "stare", Role::Viewer).await.unwrap();
        let session = repo.start_session(id, None).await.unwrap();

        let (s, _, _) = post(
            &app,
            &format!("/users/{id}/role"),
            &cookie,
            format!("csrf={csrf}&role=operator"),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(repo.by_id(id).await.unwrap().unwrap().role, Role::Operator);

        let (s, _, html) = post(
            &app,
            &format!("/users/{id}/reset"),
            &cookie,
            format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let fresh = shown_password(&html);
        assert!(repo.session_user(&session.id).await.unwrap().is_none());
        assert!(
            repo.authenticate("kolega", &fresh)
                .await
                .unwrap()
                .must_change_password
        );

        let (s, _, _) = post(
            &app,
            &format!("/users/{id}/disabled"),
            &cookie,
            format!("csrf={csrf}&disabled=1"),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(repo.by_id(id).await.unwrap().unwrap().disabled);

        let (s, _, _) = post(
            &app,
            &format!("/users/{id}/delete"),
            &cookie,
            format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(repo.by_id(id).await.unwrap().is_none());

        let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_log ORDER BY id")
            .fetch_all(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            actions,
            ["user_role", "user_reset", "user_disabled", "user_delete"]
        );
        let details: Vec<String> = sqlx::query_scalar("SELECT detail FROM audit_log")
            .fetch_all(&state.pool)
            .await
            .unwrap();
        assert!(details.iter().all(|d| !d.contains(&fresh)));
    }

    #[tokio::test]
    async fn two_admins_demoting_each_other_at_once_leave_one_admin() {
        let (app, cookie_a, csrf_a, state) = logged_in(Role::Admin).await;
        let repo = UserRepo::new(state.pool.clone());
        let a = user_id(&state, "u").await;
        let b = repo.create("b", "haslo", Role::Admin).await.unwrap();
        let cookie_b = login(&app, "b", "haslo").await;
        let csrf_b = csrf_of(&app, &cookie_b).await;

        let (uri_b, uri_a) = (format!("/users/{b}/role"), format!("/users/{a}/role"));
        let (first, second) = tokio::join!(
            post(&app, &uri_b, &cookie_a, format!("csrf={csrf_a}&role=viewer")),
            post(&app, &uri_a, &cookie_b, format!("csrf={csrf_b}&role=viewer")),
        );
        let mut statuses = [first.0, second.0];
        statuses.sort();
        // Jeden wygrywa; drugi dostaje 403 (już nie admin) albo 400 (strażnik w SQL).
        assert_eq!(statuses[0], StatusCode::OK, "{statuses:?}");
        assert!(
            matches!(statuses[1], StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN),
            "{statuses:?}"
        );
        let admins: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(admins, 1);
    }

    #[tokio::test]
    async fn a_wrong_csrf_token_changes_nothing() {
        let (app, cookie, _, state) = logged_in(Role::Admin).await;
        let (status, _, _) = post(
            &app,
            "/users",
            &cookie,
            "csrf=zly&username=nowy&role=viewer".to_owned(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}
