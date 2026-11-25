//! Macierz rola × endpoint (spec §8) i kryterium wyjścia etapu 7:
//! harmonogram utworzony w panelu trafia do kolejki.

use crate::tests::{login, state};
use crate::{AppState, router};
use aars_core::message::{MessageRepo, NewMessage, ScheduleKind};
use aars_core::priority::Priority;
use aars_core::queue::QueueRepo;
use aars_core::scheduler::{Scheduler, SchedulerConfig};
use aars_core::user::{Role, UserRepo};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const ANON: u8 = 0;
const VIEWER: u8 = 1;
const OPERATOR: u8 = 2;
const ADMIN: u8 = 3;
const ROLES: [(&str, u8); 4] = [
    ("anon", ANON),
    ("viewer", VIEWER),
    ("operator", OPERATOR),
    ("admin", ADMIN),
];

/// Wiersz GET: ścieżka, minimalna rola i status „dopuszczony".
/// Podgląd audio zwraca 503, bo stan testowy nie ma renderera — ważne, że
/// nie 401/403/303.
struct Get<'a> {
    path: &'a str,
    min: u8,
    allowed: StatusCode,
}

const fn get(path: &str, min: u8) -> Get<'_> {
    Get {
        path,
        min,
        allowed: StatusCode::OK,
    }
}

/// Wszystkie trasy POST; ciało zawiera sumę pól wszystkich formularzy
/// (nadmiarowe pola serde ignoruje), a token CSRF jest błędny.
const POSTS: &[(&str, u8)] = &[
    ("/logout", VIEWER),
    ("/account/password", VIEWER),
    ("/compose", OPERATOR),
    ("/messages", OPERATOR),
    ("/messages/1", OPERATOR),
    ("/messages/1/enabled", OPERATOR),
    ("/messages/1/delete", OPERATOR),
    ("/messages/1/send", OPERATOR),
    ("/preview", OPERATOR),
    ("/tx-enabled", OPERATOR),
    ("/queue/1/cancel", OPERATOR),
    ("/queue/group/g/cancel", OPERATOR),
    ("/sources/imgw-meteo/enabled", OPERATOR),
    ("/sources/imgw-meteo/refresh", OPERATOR),
    ("/sources/rso/enabled", OPERATOR),
    ("/sources/rso/refresh", OPERATOR),
    ("/sources/swpc/enabled", OPERATOR),
    ("/sources/swpc/refresh", OPERATOR),
    ("/sources/gios-smog/enabled", OPERATOR),
    ("/sources/gios-smog/refresh", OPERATOR),
    ("/sources/emsc/enabled", OPERATOR),
    ("/sources/emsc/refresh", OPERATOR),
    ("/sources/pogoda/enabled", OPERATOR),
    ("/sources/pogoda/refresh", OPERATOR),
    ("/bulletin/preview", OPERATOR),
    ("/bulletin/send", OPERATOR),
    ("/bulletin/layout", ADMIN),
    ("/watchdog/clear", ADMIN),
    ("/settings", ADMIN),
    ("/settings/hardware", ADMIN),
    ("/settings/hardware/test-ptt", ADMIN),
    ("/settings/hardware/test-audio", ADMIN),
    ("/settings/position", ADMIN),
    ("/station/restart", ADMIN),
    ("/settings/update/check", ADMIN),
    ("/settings/update/install", ADMIN),
    ("/modules/scheduler/enabled", ADMIN),
    ("/modules/imgw-meteo/enabled", ADMIN),
    ("/modules/imgw-meteo/settings", ADMIN),
    ("/modules/rso/enabled", ADMIN),
    ("/modules/rso/settings", ADMIN),
    ("/modules/swpc/enabled", ADMIN),
    ("/modules/swpc/settings", ADMIN),
    ("/modules/gios-smog/enabled", ADMIN),
    ("/modules/gios-smog/settings", ADMIN),
    ("/modules/emsc/enabled", ADMIN),
    ("/modules/emsc/settings", ADMIN),
    ("/modules/pogoda/enabled", ADMIN),
    ("/modules/pogoda/settings", ADMIN),
    ("/modules/bulletin/enabled", ADMIN),
    ("/modules/bulletin/settings", ADMIN),
    ("/users", ADMIN),
    ("/users/1/role", ADMIN),
    ("/users/1/disabled", ADMIN),
    ("/users/1/reset", ADMIN),
    ("/users/1/delete", ADMIN),
];

const POST_BODY: &str = "csrf=zly&text=x&priority=normal&title=t&username=nowy&role=viewer\
&disabled=1&enabled=1&schedule_kind=immediate&current=a&new=b&confirm=b&callsign=SP0XXX\
&id_mode=off&id_interval_s=600&tx_max_duration_s=60&misfire_grace_s=60&quiet_min_priority=normal&area=&interval_s=5";

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    csrf: &str,
    body: &str,
) -> (StatusCode, Option<String>) {
    // `Host` jak z przeglądarki: kreator `/setup` odrzuca żądania bez niego.
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "127.0.0.1:8080");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    if method == "POST" {
        builder = builder
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("x-csrf-token", csrf);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (response.status(), location)
}

type Session = (u8, Option<(String, String)>);

async fn sessions() -> (Router, AppState, Vec<Session>, i64) {
    let state = state().await;
    // Migracja zasiewa tylko `imgw-meteo`; reszta źródeł rejestruje się jak na stacji.
    aars_core::module::ModuleRepo::new(state.pool.clone())
        .register(&state.modules)
        .await
        .unwrap();
    let users = UserRepo::new(state.pool.clone());
    for (name, role) in [
        ("v", Role::Viewer),
        ("o", Role::Operator),
        ("a", Role::Admin),
    ] {
        users.create(name, "haslo", role).await.unwrap();
    }
    let message_id = MessageRepo::new(state.pool.clone())
        .create(NewMessage {
            title: "t".into(),
            body_template: "treść".into(),
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
        .unwrap();
    let app = router(state.clone());
    let mut cookies = vec![(ANON, None)];
    for (name, level) in [("v", VIEWER), ("o", OPERATOR), ("a", ADMIN)] {
        cookies.push((level, Some(login(&app, name, "haslo").await)));
    }
    (app, state, cookies, message_id)
}

#[tokio::test]
async fn every_get_route_enforces_its_minimum_role() {
    let (app, _state, cookies, id) = sessions().await;
    let preview = format!("/messages/{id}/preview.wav");
    let edit = format!("/messages/{id}/edit");
    let routes = [
        get("/", VIEWER),
        get("/logs", VIEWER),
        get("/map", VIEWER),
        get("/map/data", VIEWER),
        get("/fragments/queue", VIEWER),
        get("/fragments/status", VIEWER),
        get("/account/password", VIEWER),
        get("/compose", OPERATOR),
        get("/messages", OPERATOR),
        get("/messages/new", OPERATOR),
        get(
            "/messages/next?schedule_kind=cron&cron=*%20*%20*%20*%20*",
            OPERATOR,
        ),
        get(&edit, OPERATOR),
        Get {
            path: &preview,
            min: OPERATOR,
            allowed: StatusCode::SERVICE_UNAVAILABLE,
        },
        get("/queue", OPERATOR),
        get("/sources", OPERATOR),
        get("/modules", OPERATOR),
        get("/modules?m=bulletin", OPERATOR),
        get("/bulletin", OPERATOR),
        get("/settings", ADMIN),
        get("/settings/hardware", ADMIN),
        get("/settings/update", ADMIN),
        get("/users", ADMIN),
    ];
    let mut failures = Vec::new();
    for route in &routes {
        for (name, level) in ROLES {
            let cookie = cookies
                .iter()
                .find(|(l, _)| *l == level)
                .and_then(|(_, c)| c.as_ref().map(|(cookie, _)| cookie.clone()));
            let (status, location) =
                request(&app, "GET", route.path, cookie.as_deref(), "", "").await;
            let ok = if level == ANON {
                status == StatusCode::SEE_OTHER && location.as_deref() == Some("/login")
            } else if level < route.min {
                status == StatusCode::FORBIDDEN
            } else {
                status == route.allowed
            };
            if !ok {
                failures.push(format!(
                    "GET {} jako {name}: {status} {location:?}",
                    route.path
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Pierwsze wczytanie każdej strony w ramie przez każdą rolę, która ma do
/// niej prawo: (ścieżka, poziom, status, HTML).
async fn framed_pages() -> Vec<(String, u8, StatusCode, String)> {
    let (app, _state, cookies, id) = sessions().await;
    let edit = format!("/messages/{id}/edit");
    let pages = [
        ("/", VIEWER),
        ("/logs", VIEWER),
        ("/map", VIEWER),
        ("/account/password", VIEWER),
        ("/compose", OPERATOR),
        ("/messages", OPERATOR),
        ("/messages/new", OPERATOR),
        (edit.as_str(), OPERATOR),
        ("/queue", OPERATOR),
        ("/sources", OPERATOR),
        ("/modules", OPERATOR),
        ("/bulletin", OPERATOR),
        ("/settings", ADMIN),
        ("/settings/hardware", ADMIN),
        ("/settings/update", ADMIN),
        ("/users", ADMIN),
    ];
    let mut out = Vec::new();
    for (path, min) in pages {
        for (level, session) in &cookies {
            let Some((cookie, _)) = session else { continue };
            if *level < min {
                continue;
            }
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::COOKIE, cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let html = String::from_utf8_lossy(&body).into_owned();
            out.push((path.to_owned(), *level, status, html));
        }
    }
    out
}

#[tokio::test]
async fn every_page_shows_the_status_bar_on_first_load() {
    let mut failures = Vec::new();
    for (path, level, status, html) in framed_pages().await {
        let ok = status == StatusCode::OK
            && html.contains("id=\"statusbar\"")
            && html.contains("Nadawanie:")
            && html.contains("class=\"badge ");
        if !ok {
            failures.push(format!("GET {path} (poziom {level}): {status}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[tokio::test]
async fn every_page_has_exactly_one_page_title() {
    let mut failures = Vec::new();
    for (path, level, status, html) in framed_pages().await {
        let h1 = html.matches("<h1").count();
        let in_main = html
            .find("<main")
            .and_then(|main| html[main..].find('>').map(|e| main + e + 1))
            .is_some_and(|open| {
                html[open..]
                    .trim_start()
                    .starts_with("<h1 class=\"page-title\">")
            });
        if status != StatusCode::OK || h1 != 1 || !in_main {
            failures.push(format!(
                "GET {path} (poziom {level}): {status}, <h1: {h1}, w <main>: {in_main}"
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[tokio::test]
async fn every_post_route_refuses_a_missing_csrf_token_and_a_too_low_role() {
    let (app, _state, cookies, _id) = sessions().await;
    let mut failures = Vec::new();
    for (path, min) in POSTS {
        for (name, level) in ROLES {
            let cookie = cookies
                .iter()
                .find(|(l, _)| *l == level)
                .and_then(|(_, c)| c.as_ref().map(|(cookie, _)| cookie.clone()));
            let (status, location) =
                request(&app, "POST", path, cookie.as_deref(), "zly", POST_BODY).await;
            let ok = if level == ANON {
                status == StatusCode::SEE_OTHER && location.as_deref() == Some("/login")
            } else {
                // Rola za niska i brak tokenu dają to samo: 403.
                status == StatusCode::FORBIDDEN
            };
            if !ok {
                failures.push(format!(
                    "POST {path} (min {min}) jako {name}: {status} {location:?}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[tokio::test]
async fn a_cron_schedule_created_in_the_panel_reaches_the_queue_once_per_minute() {
    let state = state().await;
    UserRepo::new(state.pool.clone())
        .create("op", "haslo", Role::Operator)
        .await
        .unwrap();
    let app = router(state.clone());
    let (cookie, csrf) = login(&app, "op", "haslo").await;

    let body = format!(
        "csrf={csrf}&title=Co+minute&text=Test+harmonogramu&priority=normal&schedule_kind=cron&cron=*+*+*+*+*"
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/messages")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let messages = MessageRepo::new(state.pool.clone());
    let queue = Arc::new(QueueRepo::new(state.pool.clone()));
    let scheduler = Scheduler::new(
        MessageRepo::new(state.pool.clone()),
        queue.clone(),
        SchedulerConfig {
            misfire_grace: Duration::from_secs(300),
            tick: Duration::from_secs(1),
        },
    );
    // Początek bieżącej minuty + 10 s, żeby +1 s na pewno został w tej samej minucie.
    let now = DateTime::from_timestamp(Utc::now().timestamp() / 60 * 60 + 10, 0).unwrap();
    scheduler.tick(now).await.unwrap();
    scheduler
        .tick(now + chrono::Duration::seconds(1))
        .await
        .unwrap();

    let pending = queue.pending(10).await.unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].rendered_text, "Test harmonogramu");
    let defined = messages.scheduled().await.unwrap();
    assert_eq!(pending[0].message_id, Some(defined[0].id));
}

/// Drugi przebieg POST: każda rola z WŁASNYM, poprawnym tokenem CSRF. Dzięki
/// temu brak `require_role` na trasie nie jest maskowany przez 403 z CSRF.
/// Id spoza bazy (999) i świeży stan na wiersz — żaden wiersz nie szkodzi
/// następnym.
const VALID_BODY: &str = "text=x&priority=normal&title=t&username=nowy&role=viewer\
&disabled=1&enabled=1&schedule_kind=immediate&current=a&new=b&confirm=b&callsign=SP0XXX\
&id_mode=off&id_interval_s=600&tx_max_duration_s=60&misfire_grace_s=60&quiet_min_priority=normal&area=&interval_s=5";

#[tokio::test]
async fn every_post_route_enforces_its_role_with_a_valid_csrf_token() {
    // (ścieżka, minimalna rola, dokładny status dla roli dopuszczonej)
    let rows: &[(&str, u8, u16)] = &[
        ("/account/password", VIEWER, 400),
        ("/compose", OPERATOR, 303),
        ("/messages", OPERATOR, 303),
        ("/messages/999", OPERATOR, 404),
        ("/messages/999/enabled", OPERATOR, 404),
        ("/messages/999/delete", OPERATOR, 404),
        ("/messages/999/send", OPERATOR, 404),
        ("/preview", OPERATOR, 503),
        ("/tx-enabled", OPERATOR, 303),
        ("/queue/999/cancel", OPERATOR, 200),
        ("/queue/group/g/cancel", OPERATOR, 200),
        ("/sources/imgw-meteo/enabled", OPERATOR, 303),
        ("/sources/imgw-meteo/refresh", OPERATOR, 503),
        ("/sources/rso/enabled", OPERATOR, 303),
        ("/sources/rso/refresh", OPERATOR, 503),
        ("/sources/swpc/enabled", OPERATOR, 303),
        ("/sources/swpc/refresh", OPERATOR, 503),
        ("/sources/gios-smog/enabled", OPERATOR, 303),
        ("/sources/gios-smog/refresh", OPERATOR, 503),
        ("/sources/emsc/enabled", OPERATOR, 303),
        ("/sources/emsc/refresh", OPERATOR, 503),
        ("/sources/pogoda/enabled", OPERATOR, 303),
        ("/sources/pogoda/refresh", OPERATOR, 503),
        // Stan testowy nie ma wywołania zwrotnego podglądu.
        ("/bulletin/preview", OPERATOR, 503),
        ("/bulletin/send", OPERATOR, 503),
        // Ciało nie ma pól biuletynu: bez zmian.
        ("/bulletin/layout", ADMIN, 303),
        ("/watchdog/clear", ADMIN, 303),
        ("/settings", ADMIN, 400),
        ("/settings/hardware", ADMIN, 400),
        // Brak potwierdzenia w ciele: 400 przed sprawdzeniem stacji.
        ("/settings/hardware/test-ptt", ADMIN, 400),
        ("/settings/hardware/test-audio", ADMIN, 503),
        // Ciało nie ma pól pozycji: wszystkie puste kasują pozycję.
        ("/settings/position", ADMIN, 303),
        ("/station/restart", ADMIN, 503),
        // Stan testowy nie ma sterownika aktualizacji.
        ("/settings/update/check", ADMIN, 503),
        // `confirm=b` to nie „tak”: 400 przed sprawdzeniem sterownika.
        ("/settings/update/install", ADMIN, 400),
        ("/modules/scheduler/enabled", ADMIN, 303),
        ("/modules/imgw-meteo/enabled", ADMIN, 303),
        // `interval_s=5` jest poza zakresem 60–3600.
        ("/modules/imgw-meteo/settings", ADMIN, 400),
        ("/modules/rso/enabled", ADMIN, 303),
        ("/modules/rso/settings", ADMIN, 400),
        ("/modules/swpc/enabled", ADMIN, 303),
        ("/modules/swpc/settings", ADMIN, 400),
        ("/modules/gios-smog/enabled", ADMIN, 303),
        ("/modules/gios-smog/settings", ADMIN, 400),
        ("/modules/emsc/enabled", ADMIN, 303),
        ("/modules/emsc/settings", ADMIN, 400),
        ("/modules/pogoda/enabled", ADMIN, 303),
        ("/modules/pogoda/settings", ADMIN, 400),
        ("/modules/bulletin/enabled", ADMIN, 303),
        // Ciało nie ma pól biuletynu: bez zmian.
        ("/modules/bulletin/settings", ADMIN, 303),
        ("/users", ADMIN, 200),
        ("/users/999/role", ADMIN, 404),
        ("/users/999/disabled", ADMIN, 404),
        ("/users/999/reset", ADMIN, 404),
        ("/users/999/delete", ADMIN, 404),
        // Wylogowanie kończy sesję, więc idzie na końcu: 303 na /login.
        ("/logout", VIEWER, 303),
    ];

    let mut failures = Vec::new();
    let (app, _state, sessions, _id) = sessions().await;
    for (path, min, expected) in rows {
        for (name, level) in ROLES {
            let session = sessions
                .iter()
                .find(|(l, _)| *l == level)
                .and_then(|(_, s)| s.as_ref());
            let (cookie, csrf) = session.map_or((None, String::new()), |(c, t)| {
                (Some(c.as_str()), t.clone())
            });
            let body = format!("csrf={csrf}&{VALID_BODY}");
            let (status, location) = request(&app, "POST", path, cookie, &csrf, &body).await;
            let ok = if level == ANON {
                status == StatusCode::SEE_OTHER && location.as_deref() == Some("/login")
            } else if level < *min {
                status == StatusCode::FORBIDDEN
            } else if *path == "/logout" {
                status.as_u16() == *expected && location.as_deref() == Some("/login")
            } else {
                status.as_u16() == *expected && location.as_deref() != Some("/login")
            };
            if !ok {
                failures.push(format!("POST {path} (min {min}, oczekiwano {expected}) jako {name}: {status} {location:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[tokio::test]
async fn a_post_without_the_csrf_field_is_refused_and_changes_nothing() {
    let (app, state, sessions, _id) = sessions().await;
    let cookie = sessions[2].1.as_ref().map(|(c, _)| c.clone());
    // Operator bez pola csrf w formularzu i bez nagłówka.
    let body = "title=t&text=Nowy&priority=normal&schedule_kind=immediate";
    let (status, _) = request(&app, "POST", "/messages", cookie.as_deref(), "", body).await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::UNPROCESSABLE_ENTITY,
        "{status}"
    );
    let count = MessageRepo::new(state.pool.clone())
        .list()
        .await
        .unwrap()
        .len();
    assert_eq!(count, 1, "brak tokenu nie może utworzyć definicji");
}

/// Wyjątek od macierzy: `/setup` (kreator pierwszego konta, R12) działa bez
/// sesji, więc nie ma go w listach wyżej. Chronią go własne warunki: tryb
/// `wizard`, brak włączonego admina, sieć lokalna i CSRF double-submit
/// (testy w `setup_page`). W domyślnym trybie `console` i przy istniejącym
/// adminie nie istnieje dla nikogo — także dla zalogowanego admina.
#[tokio::test]
async fn setup_is_outside_the_session_matrix_and_hidden_by_default() {
    let (app, _state, cookies, _id) = sessions().await;
    for (name, level) in ROLES {
        let cookie = cookies
            .iter()
            .find(|(l, _)| *l == level)
            .and_then(|(_, c)| c.as_ref().map(|(cookie, _)| cookie.clone()));
        for method in ["GET", "POST"] {
            let (status, location) =
                request(&app, method, "/setup", cookie.as_deref(), "zly", POST_BODY).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{method} /setup jako {name}: {location:?}"
            );
        }
    }
}
