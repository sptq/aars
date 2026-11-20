//! Odsłuch komunikatu przed nadaniem, tym samym torem syntezy co nadanie.

use std::future::Future;
use std::pin::Pin;

use aars_core::message::MessageRepo;
use aars_core::user::Role;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::state::AppState;

pub type PreviewFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>;

/// Błąd renderera zajętego innym podglądem — panel mapuje go na 503.
pub const BUSY: &str = "podgląd zajęty, spróbuj za chwilę";

/// Najdłuższy tekst przyjmowany do podglądu (znaki).
pub const MAX_CHARS: usize = 2000;

/// Renderuje tekst do bajtów WAV. Bez `async_trait`, jak `Renderer` w rdzeniu.
pub trait PreviewRenderer: Send + Sync {
    fn render_wav<'a>(&'a self, text: &'a str) -> PreviewFuture<'a>;
}

fn wav_response(bytes: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "audio/wav")], bytes).into_response()
}

fn text_response(status: StatusCode, text: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response()
}

async fn render(state: &AppState, text: &str) -> Response {
    let Some(renderer) = state.preview.as_ref() else {
        return text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "podgląd niedostępny: synteza nie jest skonfigurowana".to_owned(),
        );
    };
    if text.trim().is_empty() {
        return text_response(
            StatusCode::BAD_REQUEST,
            "pusty tekst — nie ma czego odsłuchać".to_owned(),
        );
    }
    if text.chars().count() > MAX_CHARS {
        return text_response(
            StatusCode::BAD_REQUEST,
            "treść za długa do podglądu".to_owned(),
        );
    }
    match renderer.render_wav(text).await {
        Ok(bytes) => wav_response(bytes),
        Err(err) if err == BUSY => text_response(StatusCode::SERVICE_UNAVAILABLE, err),
        Err(err) => {
            tracing::error!(%err, "synteza podglądu zawiodła");
            text_response(StatusCode::BAD_GATEWAY, format!("synteza zawiodła: {err}"))
        }
    }
}

/// Odsłuch zapisanej definicji; tylko do odczytu, więc bez CSRF.
pub async fn stored(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    match MessageRepo::new(state.pool.clone()).get(id).await {
        Ok(Some(message)) => Ok(render(&state, &message.body_template).await),
        Ok(None) => Ok(text_response(
            StatusCode::NOT_FOUND,
            "nie ma takiego komunikatu".to_owned(),
        )),
        Err(err) => {
            tracing::error!(%err, "odczyt definicji do podglądu zawiódł");
            Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "błąd odczytu definicji".to_owned(),
            ))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PreviewForm {
    #[serde(default)]
    text: String,
    #[serde(default)]
    csrf: String,
}

/// Odsłuch tekstu z formularza (jeszcze niezapisanego).
pub async fn from_text(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<PreviewForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;
    Ok(render(&state, &form.text).await)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aars_core::message::{MessageRepo, NewMessage, ScheduleKind};
    use aars_core::priority::Priority;
    use aars_core::user::{Role, UserRepo};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;
    use crate::{AppState, router};

    struct Fake(Result<Vec<u8>, String>);
    impl PreviewRenderer for Fake {
        fn render_wav<'a>(&'a self, _text: &'a str) -> PreviewFuture<'a> {
            Box::pin(async move { self.0.clone() })
        }
    }

    async fn setup(
        role: Role,
        renderer: Option<Result<Vec<u8>, String>>,
    ) -> (Router, String, String, AppState) {
        let mut state = AppState::for_tests(aars_core::db::test_pool().await);
        if let Some(r) = renderer {
            state = state.with_preview(Arc::new(Fake(r)));
        }
        UserRepo::new(state.pool.clone())
            .create("u", "haslo", role)
            .await
            .unwrap();
        let app = router(state.clone());
        let login = app
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
        let cookie = login
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let dash = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = String::from_utf8_lossy(
            &axum::body::to_bytes(dash.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .into_owned();
        let csrf = html
            .split("name=\"csrf\" value=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap()
            .to_owned();
        (app, cookie, csrf, state)
    }

    async fn store_message(state: &AppState) -> i64 {
        MessageRepo::new(state.pool.clone())
            .create(NewMessage {
                title: "t".into(),
                body_template: "Uwaga".into(),
                priority: Priority::Normal,
                interrupting: false,
                repeat_count: 1,
                repeat_interval: std::time::Duration::ZERO,
                schedule: ScheduleKind::Immediate,
                valid_from: None,
                valid_until: None,
                source_id: None,
                created_by: None,
            })
            .await
            .unwrap()
    }

    async fn get(app: &Router, cookie: &str, uri: &str) -> (StatusCode, Option<String>, Vec<u8>) {
        let r = app
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
        let status = r.status();
        let ct = r
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_owned());
        (
            status,
            ct,
            axum::body::to_bytes(r.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
    }

    async fn post(
        app: &Router,
        cookie: &str,
        body: String,
    ) -> (StatusCode, Option<String>, Vec<u8>) {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/preview")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::COOKIE, cookie)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = r.status();
        let ct = r
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_owned());
        (
            status,
            ct,
            axum::body::to_bytes(r.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
    }

    #[tokio::test]
    async fn the_preview_returns_wav_audio() {
        let (app, cookie, csrf, state) =
            setup(Role::Operator, Some(Ok(b"RIFF-fake".to_vec()))).await;
        let id = store_message(&state).await;

        let (status, ct, body) = get(&app, &cookie, &format!("/messages/{id}/preview.wav")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ct.as_deref(), Some("audio/wav"));
        assert_eq!(body, b"RIFF-fake");

        let (status, ct, body) = post(&app, &cookie, format!("csrf={csrf}&text=Uwaga")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ct.as_deref(), Some("audio/wav"));
        assert_eq!(body, b"RIFF-fake");
    }

    #[tokio::test]
    async fn the_preview_without_a_renderer_says_why() {
        let (app, cookie, csrf, _state) = setup(Role::Operator, None).await;
        let (status, _, body) = post(&app, &cookie, format!("csrf={csrf}&text=Uwaga")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(String::from_utf8_lossy(&body).contains("podgląd niedostępny"));
    }

    #[tokio::test]
    async fn a_viewer_cannot_preview() {
        let (app, cookie, csrf, state) = setup(Role::Viewer, Some(Ok(b"RIFF-fake".to_vec()))).await;
        let id = store_message(&state).await;
        let (status, _, _) = get(&app, &cookie, &format!("/messages/{id}/preview.wav")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = post(&app, &cookie, format!("csrf={csrf}&text=Uwaga")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_preview_of_a_missing_message_is_404() {
        let (app, cookie, _csrf, _state) =
            setup(Role::Operator, Some(Ok(b"RIFF-fake".to_vec()))).await;
        let (status, _, _) = get(&app, &cookie, "/messages/999/preview.wav").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_empty_text_is_400_a_synthesis_failure_is_502_and_csrf_is_required() {
        let (app, cookie, csrf, _s) = setup(Role::Operator, Some(Ok(b"RIFF-fake".to_vec()))).await;
        let (status, _, _) = post(&app, &cookie, format!("csrf={csrf}&text=+++")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = post(&app, &cookie, "csrf=zly&text=Uwaga".into()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (app, cookie, csrf, _s) = setup(Role::Operator, Some(Err("piper padł".into()))).await;
        let (status, _, body) = post(&app, &cookie, format!("csrf={csrf}&text=Uwaga")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(String::from_utf8_lossy(&body).contains("piper padł"));
    }

    #[tokio::test]
    async fn a_text_over_the_limit_is_400() {
        let (app, cookie, csrf, _s) = setup(Role::Operator, Some(Ok(b"RIFF-fake".to_vec()))).await;
        let long = "a".repeat(MAX_CHARS + 1);
        let (status, _, body) = post(&app, &cookie, format!("csrf={csrf}&text={long}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("treść za długa do podglądu"));
        let ok = "a".repeat(MAX_CHARS);
        let (status, _, _) = post(&app, &cookie, format!("csrf={csrf}&text={ok}")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_busy_renderer_is_503() {
        let (app, cookie, csrf, _s) = setup(Role::Operator, Some(Err(BUSY.into()))).await;
        let (status, _, body) = post(&app, &cookie, format!("csrf={csrf}&text=Uwaga")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(String::from_utf8_lossy(&body).contains("podgląd zajęty"));
    }
}
