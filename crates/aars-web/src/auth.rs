use aars_core::user::{Role, User, UserRepo};
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::header;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};

use crate::state::AppState;

pub const SESSION_COOKIE: &str = "aars_session";

/// Użytkownik stojący za żądaniem wraz z tokenem CSRF jego sesji. Brak
/// sesji kończy się przekierowaniem na stronę logowania, nie pustą stroną.
pub struct CurrentUser {
    pub user: User,
    pub csrf_token: String,
    /// Wersja produktu do stopki strony (z `AppState`).
    pub version: &'static str,
}

/// Odpowiedź dla żądania bez sesji, bez uprawnień albo bez tokenu CSRF.
pub enum AuthRejection {
    Anonymous,
    Forbidden,
    CsrfMismatch,
    /// Konto ma hasło tymczasowe: do czasu zmiany wolno tylko formularz zmiany.
    MustChangePassword,
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        match self {
            Self::Anonymous => Redirect::to("/login").into_response(),
            Self::Forbidden => {
                (StatusCode::FORBIDDEN, "brak uprawnień do tej operacji").into_response()
            }
            Self::MustChangePassword => Redirect::to("/account/password").into_response(),
            Self::CsrfMismatch => {
                (StatusCode::FORBIDDEN, "niepoprawny token formularza").into_response()
            }
        }
    }
}

pub fn session_id_from_cookies(header_value: Option<&str>) -> Option<String> {
    let header_value = header_value?;
    header_value.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == SESSION_COOKIE).then(|| value.to_owned())
    })
}

fn session_id_from(parts: &Parts) -> Option<String> {
    session_id_from_cookies(
        parts
            .headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    )
}

/// Ścieżki dostępne przy niezmienionym haśle tymczasowym. `/assets/*` i
/// `/login` nie używają `CurrentUser`, więc nie wymagają wyjątku.
fn allowed_while_password_unchanged(path: &str) -> bool {
    matches!(path, "/account/password" | "/logout")
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session_id = session_id_from(parts).ok_or(AuthRejection::Anonymous)?;
        let (user, csrf_token) = UserRepo::new(state.pool.clone())
            .session_user(&session_id)
            .await
            .map_err(|err| {
                tracing::error!(%err, "odczyt sesji zawiódł");
                AuthRejection::Anonymous
            })?
            .ok_or(AuthRejection::Anonymous)?;
        if user.must_change_password && !allowed_while_password_unchanged(parts.uri.path()) {
            return Err(AuthRejection::MustChangePassword);
        }
        Ok(CurrentUser {
            user,
            csrf_token,
            version: state.version,
        })
    }
}

/// Sprawdza, czy użytkownik ma co najmniej wskazaną rolę.
pub fn require_role(user: &User, required: Role) -> Result<(), AuthRejection> {
    if user.role.can(required) {
        Ok(())
    } else {
        tracing::warn!(
            user = %user.username,
            role = user.role.as_str(),
            required = required.as_str(),
            "odmowa dostępu"
        );
        Err(AuthRejection::Forbidden)
    }
}

/// Porównanie tokenu CSRF w czasie stałym — token jest sekretem sesji.
pub fn require_csrf(expected: &str, provided: Option<&str>) -> Result<(), AuthRejection> {
    let provided = provided.unwrap_or_default();
    if expected.len() == provided.len()
        && expected
            .bytes()
            .zip(provided.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
        && !expected.is_empty()
    {
        Ok(())
    } else {
        tracing::warn!("odrzucono żądanie bez poprawnego tokenu CSRF");
        Err(AuthRejection::CsrfMismatch)
    }
}

/// Buduje wartość nagłówka `Set-Cookie` dla sesji.
pub fn session_cookie(session_id: &str, secure: bool) -> String {
    let mut cookie =
        format!("{SESSION_COOKIE}={session_id}; Path=/; HttpOnly; SameSite=Lax; Max-Age=43200");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn clear_cookie(secure: bool) -> String {
    let mut cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}
