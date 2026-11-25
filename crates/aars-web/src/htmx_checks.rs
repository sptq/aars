//! Panel bez przeładowań (etap 13, D): formularz wysłany przez htmx dostaje
//! fragment strony (bez `<html>`) i powiadomienie w `HX-Trigger`; błąd
//! walidacji to 422 z kartą. Bez `HX-Request` zostaje dotychczasowe
//! przekierowanie — to sprawdzają testy poszczególnych stron.

use crate::tests::{logged_in, state};
use crate::{AppState, router};
use aars_core::message::{MessageRepo, NewMessage, ScheduleKind};
use aars_core::priority::Priority;
use aars_core::queue::EnqueueRequest;
use aars_core::user::{Role, UserRepo};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

/// Rodzaj żądania htmx.
#[derive(Clone, Copy)]
enum Hx {
    /// Formularz z `hx-post`.
    Form,
    /// Link albo formularz pod `hx-boost`.
    Boosted,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn toast(&self) -> serde_json::Value {
        let raw = self
            .headers
            .get("HX-Trigger")
            .unwrap_or_else(|| panic!("brak HX-Trigger ({}): {}", self.status, self.body))
            .to_str()
            .unwrap();
        serde_json::from_str::<serde_json::Value>(raw).unwrap()["toast"].clone()
    }

    /// 200, fragment bez ramy strony i powiadomienie „ok” o tekście `text`.
    fn assert_card(&self, card: &str, text: &str) {
        assert_eq!(self.status, StatusCode::OK, "{}", self.body);
        assert!(
            !self.body.contains("<html"),
            "fragment z <html>: {}",
            self.body
        );
        assert!(!self.body.contains("class=\"sidebar\""), "fragment z menu");
        assert!(
            self.body.contains(&format!("id=\"{card}\"")),
            "brak karty {card}: {}",
            self.body
        );
        assert_eq!(self.toast()["kind"], "ok", "{:?}", self.headers);
        assert_eq!(self.toast()["text"], text);
    }

    /// 422, karta z błędem i powiadomienie o błędzie.
    fn assert_refused(&self, card: &str) {
        assert_eq!(
            self.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            self.body
        );
        assert!(!self.body.contains("<html"), "fragment z <html>");
        assert!(
            self.body.contains(&format!("id=\"{card}\"")),
            "brak karty {card}"
        );
        assert!(self.body.contains("class=\"error\""), "brak błędu w karcie");
        assert_eq!(self.toast()["kind"], "error");
    }
}

async fn send(
    app: &Router,
    cookie: &str,
    hx: Option<Hx>,
    method: &str,
    path: &str,
    body: String,
) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(hx) = hx {
        request = request.header("HX-Request", "true");
        if matches!(hx, Hx::Boosted) {
            request = request.header("HX-Boosted", "true");
        }
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn hx_post(app: &Router, cookie: &str, csrf: &str, path: &str, fields: &str) -> Reply {
    send(
        app,
        cookie,
        Some(Hx::Form),
        "POST",
        path,
        format!("csrf={csrf}&{fields}"),
    )
    .await
}

async fn plain_post(app: &Router, cookie: &str, csrf: &str, path: &str, fields: &str) -> Reply {
    send(
        app,
        cookie,
        None,
        "POST",
        path,
        format!("csrf={csrf}&{fields}"),
    )
    .await
}

async fn admin() -> (Router, String, String, AppState) {
    logged_in(Role::Admin).await
}

#[tokio::test]
async fn module_toggle_and_settings_answer_with_the_card() {
    let (app, cookie, csrf, _) = admin().await;
    let reply = hx_post(
        &app,
        &cookie,
        &csrf,
        "/modules/scheduler/enabled",
        "enabled=0",
    )
    .await;
    reply.assert_card("scheduler", "Wyłączono");
    assert!(reply.body.contains("wyłączony"), "{}", reply.body);

    let reply = hx_post(
        &app,
        &cookie,
        &csrf,
        "/modules/imgw-meteo/settings",
        "interval_s=900",
    )
    .await;
    reply.assert_card("imgw-meteo", "Zapisano");
    assert!(reply.body.contains("value=\"900\""));

    // bez zmian: ta sama karta, inne słowo
    let reply = hx_post(
        &app,
        &cookie,
        &csrf,
        "/modules/imgw-meteo/settings",
        "interval_s=900",
    )
    .await;
    reply.assert_card("imgw-meteo", "Bez zmian");

    // bez htmx — przekierowanie jak dotąd
    let reply = plain_post(
        &app,
        &cookie,
        &csrf,
        "/modules/scheduler/enabled",
        "enabled=1",
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(reply.headers.get("HX-Trigger").is_none());
}

#[tokio::test]
async fn an_invalid_module_setting_is_a_422_card_with_the_entered_value() {
    let (app, cookie, csrf, _) = admin().await;
    let reply = hx_post(
        &app,
        &cookie,
        &csrf,
        "/modules/imgw-meteo/settings",
        "interval_s=5",
    )
    .await;
    reply.assert_refused("imgw-meteo");
    assert!(
        reply.toast()["text"].as_str().unwrap().contains("60"),
        "{:?}",
        reply.toast()
    );
    assert!(
        reply.body.contains("value=\"5\""),
        "wpisana wartość zostaje"
    );
    // bez htmx dalej 400 i cała strona
    let reply = plain_post(
        &app,
        &cookie,
        &csrf,
        "/modules/imgw-meteo/settings",
        "interval_s=5",
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("<html"));
}

#[tokio::test]
async fn an_always_on_module_refuses_with_its_reason() {
    let (app, cookie, csrf, _) = admin().await;
    let reply = hx_post(
        &app,
        &cookie,
        &csrf,
        "/modules/auto-id/enabled",
        "enabled=0",
    )
    .await;
    reply.assert_refused("auto-id");
    assert!(
        reply.toast()["text"]
            .as_str()
            .unwrap()
            .contains("nie da się wyłączyć"),
        "{:?}",
        reply.toast()
    );
}

#[tokio::test]
async fn source_toggle_and_refresh_answer_with_the_card() {
    let state = state().await.with_source_refresh(Arc::new(|_| true));
    UserRepo::new(state.pool.clone())
        .create("u", "haslo", Role::Operator)
        .await
        .unwrap();
    let app = router(state);
    let (cookie, csrf) = crate::tests::login(&app, "u", "haslo").await;

    let reply = hx_post(
        &app,
        &cookie,
        &csrf,
        "/sources/imgw-meteo/enabled",
        "enabled=0",
    )
    .await;
    reply.assert_card("imgw-meteo", "Wyłączono");
    let reply = hx_post(&app, &cookie, &csrf, "/sources/imgw-meteo/refresh", "").await;
    reply.assert_card("imgw-meteo", "Odświeżanie zlecone");
    let reply = plain_post(&app, &cookie, &csrf, "/sources/imgw-meteo/refresh", "").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
}

const STATION: &str = "callsign=SP0ABC&id_mode=voice&id_interval_s=600&tx_max_duration_s=120\
                       &misfire_grace_s=300&quiet_hours=&quiet_min_priority=high&speech_rate_pct=90";

#[tokio::test]
async fn station_settings_and_position_answer_with_their_cards() {
    let (app, cookie, csrf, _) = admin().await;
    let reply = hx_post(&app, &cookie, &csrf, "/settings", STATION).await;
    reply.assert_card("stacja", "Zapisano");
    assert!(
        reply.body.contains("id=\"restart\""),
        "karta restartu do hx-select-oob"
    );

    let bad = STATION.replace("id_interval_s=600", "id_interval_s=7");
    let reply = hx_post(&app, &cookie, &csrf, "/settings", &bad).await;
    reply.assert_refused("stacja");
    assert!(
        reply.body.contains("value=\"7\""),
        "wpisana wartość zostaje: {}",
        reply.body
    );

    let reply = hx_post(
        &app,
        &cookie,
        &csrf,
        "/settings/position",
        "lat=&lon=&locator=JO91RS",
    )
    .await;
    reply.assert_card("polozenie", "Zapisano położenie");
    assert!(reply.body.contains("JO91RS"));
    let reply = hx_post(
        &app,
        &cookie,
        &csrf,
        "/settings/position",
        "lat=999&lon=1&locator=",
    )
    .await;
    reply.assert_refused("polozenie");

    let reply = plain_post(
        &app,
        &cookie,
        &csrf,
        "/settings/position",
        "lat=&lon=&locator=",
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
}

async fn a_message(state: &AppState) -> i64 {
    MessageRepo::new(state.pool.clone())
        .create(NewMessage {
            title: "Próba".to_owned(),
            body_template: "Komunikat próbny".to_owned(),
            priority: Priority::Normal,
            interrupting: false,
            repeat_count: 1,
            repeat_interval: Duration::ZERO,
            schedule: ScheduleKind::Immediate,
            valid_from: None,
            valid_until: None,
            source_id: None,
            created_by: None,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn message_actions_answer_with_the_list_card() {
    let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
    let id = a_message(&state).await;
    let reply = hx_post(
        &app,
        &cookie,
        &csrf,
        &format!("/messages/{id}/enabled"),
        "enabled=0",
    )
    .await;
    reply.assert_card("komunikaty", "Wyłączono");
    let reply = hx_post(&app, &cookie, &csrf, &format!("/messages/{id}/send"), "").await;
    reply.assert_card("komunikaty", "Komunikat dodany do kolejki");
    assert_eq!(state.queue.pending(10).await.unwrap().len(), 1);
    let reply = hx_post(&app, &cookie, &csrf, &format!("/messages/{id}/delete"), "").await;
    reply.assert_card("komunikaty", "Usunięto");
    assert!(!reply.body.contains("Próba"));
    // bez htmx „Nadaj teraz” dalej prowadzi do kolejki
    let id = a_message(&state).await;
    let reply = plain_post(&app, &cookie, &csrf, &format!("/messages/{id}/send"), "").await;
    assert_eq!(reply.headers.get(header::LOCATION).unwrap(), "/queue");
}

#[tokio::test]
async fn queue_cancel_carries_a_toast() {
    let (app, cookie, csrf, state) = logged_in(Role::Operator).await;
    let ids = state
        .queue
        .enqueue(EnqueueRequest {
            message_id: None,
            rendered_text: "do anulowania".to_owned(),
            priority: Priority::Normal,
            interrupting: false,
            not_before: None,
            expires_at: None,
            repeat_count: 1,
            repeat_interval: Duration::ZERO,
            created_by: None,
        })
        .await
        .unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(format!("/queue/{}/cancel?full=1", ids[0]))
        .header(header::COOKIE, &cookie)
        .header("HX-Request", "true")
        .header("x-csrf-token", &csrf)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let trigger = response
        .headers()
        .get("HX-Trigger")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        trigger.contains("\"kind\":\"ok\"") && trigger.contains("Anulowano"),
        "{trigger}"
    );
}

#[tokio::test]
async fn user_actions_answer_with_the_users_panel() {
    let (app, cookie, csrf, _) = admin().await;
    let reply = hx_post(
        &app,
        &cookie,
        &csrf,
        "/users",
        "username=nowy&role=operator",
    )
    .await;
    reply.assert_card("uzytkownicy", "Hasło tymczasowe gotowe");
    assert!(reply.body.contains("id=\"new-password\""));
    assert_eq!(
        reply.headers.get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );

    let reply = hx_post(&app, &cookie, &csrf, "/users", "username=x&role=operator").await;
    reply.assert_refused("uzytkownicy");
    assert!(reply.toast()["text"].as_str().unwrap().starts_with("nazwa"));
}

#[tokio::test]
async fn boosted_requests_get_the_whole_page_and_the_old_redirect() {
    let (app, cookie, csrf, _) = admin().await;
    let reply = send(
        &app,
        &cookie,
        Some(Hx::Boosted),
        "GET",
        "/modules",
        String::new(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("<html") && reply.body.contains("data-panel"));
    let reply = send(
        &app,
        &cookie,
        Some(Hx::Boosted),
        "POST",
        "/modules/scheduler/enabled",
        format!("csrf={csrf}&enabled=0"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    // strona z mapą prosi panel.js o zwykłe wejście
    let reply = send(
        &app,
        &cookie,
        Some(Hx::Boosted),
        "GET",
        "/map",
        String::new(),
    )
    .await;
    assert!(
        reply.body.contains("data-panel data-full-load"),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn the_frame_keeps_safety_forms_out_of_htmx() {
    let (app, cookie, _, state) = logged_in(Role::Operator).await;
    state.interlocks.set_tx_enabled(true);
    let reply = send(&app, &cookie, None, "GET", "/", String::new()).await;
    let html = reply.body;
    assert!(
        html.contains("<body hx-boost=\"true\" data-panel"),
        "{html}"
    );
    assert!(html.contains("id=\"toasts\" class=\"toasts\" role=\"status\" aria-live=\"polite\""));
    // wyłącznik nadawania i wylogowanie: zwykły POST, nigdy przez htmx
    for form in html.split("<form").skip(1) {
        let tag = &form[..form.find('>').unwrap()];
        if tag.contains("/tx-enabled") || tag.contains("/logout") {
            assert!(tag.contains("hx-boost=\"false\""), "{tag}");
            assert!(!tag.contains("hx-post"), "{tag}");
        }
    }
    assert!(html.contains("action=\"/tx-enabled\""));
}
