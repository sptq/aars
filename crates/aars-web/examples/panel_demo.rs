//! Panel z danymi przykładowymi, bez sprzętu i bez stacji.
//!
//! `cargo run -p aars-web --example panel_demo`, potem http://127.0.0.1:8088
//! (port z `PANEL_DEMO_PORT`). Baza leży w katalogu tymczasowym i znika razem
//! z procesem; nic nie dotyka karty dźwiękowej ani nadajnika.

use std::sync::Arc;
use std::time::Duration;

use aars_core::geo::{PositionRepo, StationPosition};
use aars_core::ident::IdMode;
use aars_core::interlock::InMemoryInterlocks;
use aars_core::message::{MessageRepo, NewMessage, ScheduleKind};
use aars_core::module::{
    BULLETIN, ModuleDescriptor, ModuleRegistry, ModuleRepo, builtin, sample_source,
};
use aars_core::priority::Priority;
use aars_core::queue::{EnqueueRequest, QueueRepo};
use aars_core::settings::{QuietHours, SettingsRepo, StationSettings};
use aars_core::source_state::{NewTracked, SourceStateRepo};
use aars_core::txlog::{TxLogEntry, TxLogRepo};
use aars_core::user::{Role, UserRepo};
use aars_web::AppState;
use aars_web::state::{BulletinPreviewView, MapWarning, WarningsSnapshot};
use chrono::Utc;
use sqlx::SqlitePool;

/// Teksty zasianych zadań kolejki; pierwszy widać na pulpicie.
const QUEUE_TEXTS: [&str; 3] = [
    "Komunikat próbny stacji SP0DEMO.",
    "Przypomnienie o cotygodniowej sieci.",
    "Ostrzeżenie o burzach w powiecie rzeszowskim.",
];

fn request(text: &str, not_before: chrono::Duration, repeat_count: u32) -> EnqueueRequest {
    EnqueueRequest {
        message_id: None,
        rendered_text: text.to_owned(),
        priority: Priority::Normal,
        interrupting: false,
        not_before: Some(Utc::now() + not_before),
        expires_at: None,
        repeat_count,
        repeat_interval: Duration::from_secs(600),
        created_by: None,
    }
}

/// Zasiewa bazę danymi przykładowymi i składa stan panelu bez stacji,
/// sondy sprzętu i aktualizacji.
async fn demo_state(pool: SqlitePool) -> AppState {
    let mut all = builtin();
    all.push(ModuleDescriptor {
        name: "Ostrzeżenia meteorologiczne IMGW",
        ..sample_source("imgw-meteo")
    });
    for id in ["rso", "swpc", "gios-smog", "emsc", "pogoda"] {
        all.push(sample_source(id));
    }
    let registry = Arc::new(ModuleRegistry::new(all));
    ModuleRepo::new(pool.clone())
        .register(&registry)
        .await
        .expect("rejestr modułów");

    let users = UserRepo::new(pool.clone());
    for (name, role) in [
        ("admin", Role::Admin),
        ("operator", Role::Operator),
        ("podglad", Role::Viewer),
    ] {
        users.create(name, "demo", role).await.expect("konto demo");
    }

    SettingsRepo::new(pool.clone())
        .save(&StationSettings {
            callsign: "SP0DEMO".to_owned(),
            id_mode: IdMode::Voice,
            id_interval_s: 600,
            tx_max_duration_s: 180,
            misfire_grace_s: 600,
            quiet_hours: QuietHours::parse("22:00-06:00").ok(),
            quiet_min_priority: Priority::High,
        })
        .await
        .expect("ustawienia stacji");
    PositionRepo::new(pool.clone())
        .save(Some(&StationPosition {
            lat: 50.04,
            lon: 21.99,
            locator: "KN09BX".to_owned(),
        }))
        .await
        .expect("pozycja");

    let now = Utc::now();
    let sources = SourceStateRepo::new(pool.clone());
    sources
        .save_config("imgw-meteo", "18 podkarpackie", 300)
        .await
        .expect("obszar");
    sources
        .set_enabled("imgw-meteo", true)
        .await
        .expect("włączenie imgw");
    sources
        .record_success("imgw-meteo", now)
        .await
        .expect("sukces imgw");
    sources
        .upsert_active(
            &NewTracked {
                source_id: "imgw-meteo".to_owned(),
                item_key: "demo-burze-1863".to_owned(),
                topic: "burze".to_owned(),
                fingerprint: "demo-1".to_owned(),
                area: "1863".to_owned(),
                summary: "Burze z gradem, 1. stopień".to_owned(),
                cancel_text: "Odwołano ostrzeżenie o burzach z gradem.".to_owned(),
                priority: Priority::High,
                valid_from: Some(now),
                valid_until: Some(now + chrono::Duration::hours(6)),
                aired: true,
                first_seen_at: now,
                last_seen_at: now,
                queue_item_id: None,
                superseded: Vec::new(),
            },
            now,
        )
        .await
        .expect("ostrzeżenie");
    sources
        .set_enabled("rso", true)
        .await
        .expect("włączenie rso");
    sources
        .record_failure("rso", now, "brak połączenia (demo)")
        .await
        .expect("porażka rso");

    let messages = MessageRepo::new(pool.clone());
    let new_message = |title: &str, body: &str, schedule| NewMessage {
        title: title.to_owned(),
        body_template: body.to_owned(),
        priority: Priority::Normal,
        interrupting: false,
        repeat_count: 1,
        repeat_interval: Duration::ZERO,
        schedule,
        valid_from: None,
        valid_until: None,
        source_id: None,
        created_by: None,
    };
    for (title, body, schedule) in [
        (
            "Poranna pogoda",
            "Dzień dobry, tu stacja {callsign}.",
            ScheduleKind::Cron("0 8 * * *".to_owned()),
        ),
        (
            "Sieć niedzielna",
            "Zapraszamy na niedzielną sieć.",
            ScheduleKind::Immediate,
        ),
        ("Komunikat próbny", QUEUE_TEXTS[0], ScheduleKind::Immediate),
    ] {
        messages
            .create(new_message(title, body, schedule))
            .await
            .expect("komunikat");
    }

    let queue = Arc::new(QueueRepo::new(pool.clone()));
    queue
        .enqueue(request(QUEUE_TEXTS[0], chrono::Duration::minutes(10), 1))
        .await
        .expect("kolejka 1");
    queue
        .enqueue(request(QUEUE_TEXTS[1], chrono::Duration::hours(1), 1))
        .await
        .expect("kolejka 2");
    queue
        .enqueue(request(QUEUE_TEXTS[2], chrono::Duration::minutes(30), 2))
        .await
        .expect("kolejka 3");

    let txlog = TxLogRepo::new(pool.clone());
    for ago in [90, 20] {
        let started = now - chrono::Duration::minutes(ago);
        txlog
            .record(TxLogEntry {
                queue_item_id: None,
                started_at: started,
                ended_at: started + chrono::Duration::seconds(25),
                ptt_backend: "demo".to_owned(),
                result: "ok",
                note: None,
            })
            .await
            .expect("dziennik nadań");
    }

    ModuleRepo::new(pool.clone())
        .set_enabled(&BULLETIN, true)
        .await
        .expect("biuletyn");

    let warnings_at = now;
    AppState::new(pool, queue, Arc::new(InMemoryInterlocks::permissive()), false)
        .with_modules(registry)
        .with_version("demo")
        .with_warnings_map(Arc::new(move || {
            let until = warnings_at + chrono::Duration::hours(6);
            let warning = |teryt: &str, level: u8, event: &str| MapWarning {
                teryt: teryt.to_owned(),
                level,
                event: event.to_owned(),
                until,
            };
            Some(WarningsSnapshot {
                fetched_at: Utc::now(),
                warnings: vec![warning("1863", 2, "Burze z gradem"), warning("1816", 1, "Burze z gradem")],
            })
        }))
        .with_bulletin_preview(Arc::new(|| {
            Box::pin(async {
                Ok(BulletinPreviewView {
                    text: Some("Biuletyn stacji SP0DEMO. Pogoda: pogodnie, 18 stopni. Obowiązuje ostrzeżenie o burzach z gradem.".to_owned()),
                    parts: vec![("pogoda".to_owned(), "pogodnie, 18 stopni".to_owned())],
                    skipped_reason: None,
                })
            })
        }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = aars_core::db::connect(&dir.path().join("demo.db")).await?;
    aars_core::db::migrate(&pool).await?;
    let state = demo_state(pool).await;

    let port: u16 = std::env::var("PANEL_DEMO_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8088);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("Panel demo: http://127.0.0.1:{port}");
    println!("Konta (hasło demo): admin, operator, podglad");
    axum::serve(listener, aars_web::router(state)).await?;
    drop(dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    async fn demo() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = aars_core::db::connect(&dir.path().join("demo.db"))
            .await
            .unwrap();
        aars_core::db::migrate(&pool).await.unwrap();
        (demo_state(pool).await, dir)
    }

    async fn get(state: AppState, username: &str, uri: &str) -> (StatusCode, String) {
        let repo = UserRepo::new(state.pool.clone());
        let user = repo.authenticate(username, "demo").await.unwrap();
        let session = repo.start_session(user.id, None).await.unwrap();
        let response = aars_web::router(state)
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, format!("aars_session={}", session.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn the_demo_dashboard_renders_the_sample_data() {
        let (state, _dir) = demo().await;
        let (status, body) = get(state, "admin", "/").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("Zatrzymaj nadawanie"), "{body}");
        assert!(body.contains(QUEUE_TEXTS[0]), "{body}");
        // Pulpit na tym etapie liczy ostrzeżenia; ich treść stoi na stronie źródeł.
        assert!(body.contains("aktywne ostrzeżenia: 1"), "{body}");
    }

    #[tokio::test]
    async fn the_demo_sources_page_lists_the_warning() {
        let (state, _dir) = demo().await;
        let (status, body) = get(state, "admin", "/sources").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("Burze z gradem, 1. stopień"), "{body}");
    }

    #[tokio::test]
    async fn the_demo_map_has_warnings() {
        let (state, _dir) = demo().await;
        let (status, body) = get(state, "podglad", "/map/data").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["state"], "ok", "{body}");
        assert!(!v["warnings"].as_array().unwrap().is_empty(), "{body}");
    }

    #[tokio::test]
    async fn the_demo_has_no_station_probe_or_update() {
        let (state, _dir) = demo().await;
        assert!(state.station.is_none() && state.probe.is_none() && state.update.is_none());
        assert!(state.preview.is_none() && state.watchdog_clear.is_none());
    }
}
