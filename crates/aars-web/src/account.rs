use aars_core::user::{AuthError, UserRepo};
use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::audit;
use crate::auth::{AuthRejection, CurrentUser, require_csrf};
use crate::chrome::Chrome;
use crate::render::page;
use crate::state::AppState;

pub const MIN_PASSWORD_LEN: usize = 10;

#[derive(Template)]
#[template(path = "password.html")]
struct PasswordPage {
    chrome: Chrome,
    must_change: bool,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PasswordForm {
    csrf: String,
    current: String,
    new: String,
    confirm: String,
}

pub async fn password_page(current: CurrentUser) -> Response {
    page(&PasswordPage {
        must_change: current.user.must_change_password,
        chrome: Chrome::from(&current),
        error: None,
    })
}

pub async fn password_submit(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<PasswordForm>,
) -> Result<Response, AuthRejection> {
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let refuse = |message: &str| {
        (
            StatusCode::BAD_REQUEST,
            page(&PasswordPage {
                must_change: current.user.must_change_password,
                chrome: Chrome::from(&current),
                error: Some(message.to_owned()),
            }),
        )
            .into_response()
    };

    if form.new.chars().count() < MIN_PASSWORD_LEN {
        return Ok(refuse("Nowe hasło musi mieć co najmniej 10 znaków."));
    }
    if form.new != form.confirm {
        return Ok(refuse("Powtórzone hasło różni się od nowego."));
    }

    let repo = UserRepo::new(state.pool.clone());
    match repo
        .authenticate(&current.user.username, &form.current)
        .await
    {
        Ok(_) => {}
        Err(AuthError::InvalidCredentials | AuthError::Disabled) => {
            tracing::warn!(user = %current.user.username, "zmiana hasła: złe obecne hasło");
            return Ok(refuse("Obecne hasło jest niepoprawne."));
        }
        Err(err) => {
            tracing::error!(%err, "zmiana hasła: błąd uwierzytelniania");
            return Ok((StatusCode::INTERNAL_SERVER_ERROR, "błąd uwierzytelniania").into_response());
        }
    }

    if let Err(err) = repo.set_password(current.user.id, &form.new).await {
        tracing::error!(%err, "zapis nowego hasła zawiódł");
        return Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            "nie udało się zmienić hasła",
        )
            .into_response());
    }
    audit(
        &state,
        current.user.id,
        "password_change",
        &current.user.username,
    )
    .await;

    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router;
    use aars_core::user::Role;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn setup(must_change: bool) -> (Router, AppState) {
        let state = AppState::for_tests(aars_core::db::test_pool().await);
        UserRepo::new(state.pool.clone())
            .create("u", "stare-haslo", Role::Viewer)
            .await
            .unwrap();
        if must_change {
            sqlx::query("UPDATE users SET must_change_password = 1")
                .execute(&state.pool)
                .await
                .unwrap();
        }
        (router(state.clone()), state)
    }

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

    async fn login(app: &Router, password: &str) -> (StatusCode, Option<String>) {
        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("username=u&password={password}")))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or_default().to_owned());
        (response.status(), cookie)
    }

    fn get(uri: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    }

    /// Cookie sesji i token CSRF wzięty z formularza zmiany hasła.
    async fn session(app: &Router) -> (String, String) {
        let (status, cookie) = login(app, "stare-haslo").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let cookie = cookie.unwrap();
        let (status, _, html) = send(app, get("/account/password", &cookie)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        let csrf = html
            .split("name=\"csrf\" value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("formularz musi zawierać token CSRF")
            .to_owned();
        (cookie, csrf)
    }

    async fn change(
        app: &Router,
        cookie: &str,
        csrf: &str,
        current: &str,
        new: &str,
        confirm: &str,
    ) -> (StatusCode, Option<String>) {
        let req = Request::builder()
            .method("POST")
            .uri("/account/password")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(format!(
                "csrf={csrf}&current={current}&new={new}&confirm={confirm}"
            )))
            .unwrap();
        let (status, location, _) = send(app, req).await;
        (status, location)
    }

    #[tokio::test]
    async fn a_user_who_must_change_the_password_is_sent_to_the_change_form() {
        let (app, _) = setup(true).await;
        let (_, cookie) = login(&app, "stare-haslo").await;
        let (status, location, _) = send(&app, get("/", &cookie.unwrap())).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.as_deref(), Some("/account/password"));
    }

    #[tokio::test]
    async fn a_user_who_need_not_change_the_password_sees_the_dashboard() {
        let (app, _) = setup(false).await;
        let (_, cookie) = login(&app, "stare-haslo").await;
        let (status, _, _) = send(&app, get("/", &cookie.unwrap())).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn changing_the_password_lifts_the_redirect() {
        let (app, state) = setup(true).await;
        let (cookie, csrf) = session(&app).await;

        let (status, location) = change(
            &app,
            &cookie,
            &csrf,
            "stare-haslo",
            "nowe-haslo-123",
            "nowe-haslo-123",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.as_deref(), Some("/"));

        let (status, _, _) = send(&app, get("/", &cookie)).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = login(&app, "stare-haslo").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = login(&app, "nowe-haslo-123").await;
        assert_eq!(status, StatusCode::SEE_OTHER);

        let audited: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE action = 'password_change'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(audited, 1);
    }

    async fn assert_refused(current: &str, new: &str, confirm: &str) {
        let (app, _) = setup(true).await;
        let (cookie, csrf) = session(&app).await;
        let (status, _) = change(&app, &cookie, &csrf, current, new, confirm).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = login(&app, "stare-haslo").await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "stare hasło ma nadal działać"
        );
        let (status, location, _) = send(&app, get("/", &cookie)).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.as_deref(), Some("/account/password"));
    }

    #[tokio::test]
    async fn a_wrong_current_password_is_refused() {
        assert_refused("zle-haslo", "nowe-haslo-123", "nowe-haslo-123").await;
    }

    #[tokio::test]
    async fn a_new_password_shorter_than_10_characters_is_refused() {
        assert_refused("stare-haslo", "krotkie", "krotkie").await;
    }

    #[tokio::test]
    async fn mismatched_confirmation_is_refused() {
        assert_refused("stare-haslo", "nowe-haslo-123", "inne-haslo-123").await;
    }

    #[tokio::test]
    async fn a_missing_csrf_token_is_refused() {
        let (app, _) = setup(true).await;
        let (cookie, _) = session(&app).await;
        let (status, _) = change(
            &app,
            &cookie,
            "zly",
            "stare-haslo",
            "nowe-haslo-123",
            "nowe-haslo-123",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn logout_and_assets_stay_reachable_while_the_password_is_unchanged() {
        let (app, _) = setup(true).await;
        let (cookie, csrf) = session(&app).await;
        let (status, _, _) = send(&app, get("/assets/app.css", &cookie)).await;
        assert_eq!(status, StatusCode::OK);
        let req = Request::builder()
            .method("POST")
            .uri("/logout")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from(format!("csrf={csrf}")))
            .unwrap();
        let (status, location, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.as_deref(), Some("/login"));
    }
}
