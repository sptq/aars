//! Kryterium wyjścia etapu 8: prawdziwy fiks IMGW z lipca trafia do kolejki
//! dokładnie raz, bez ruchu sieciowego (serwer wiremock).

use std::sync::Arc;

use aars_core::priority::Priority;
use aars_core::queue::QueueRepo;
use aars_core::source_state::SourceStateRepo;
use aars_sources::SourceRunner;
use aars_sources::imgw_meteo::ImgwMeteo;
use chrono::{DateTime, TimeZone, Utc};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const JULY: &[u8] = include_bytes!("fixtures/imgw/warningsmeteo-2026-07-23.json");

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 23, 6, 0, 0).unwrap()
}

async fn serve(server: &MockServer, body: Vec<u8>) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/warningsmeteo"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body, "application/json; charset=utf-8"),
        )
        .mount(server)
        .await;
}

async fn station() -> (MockServer, SourceRunner, Arc<QueueRepo>) {
    let pool = aars_core::db::test_pool().await;
    let state = SourceStateRepo::new(pool.clone());
    state.set_enabled("imgw-meteo", true).await.unwrap();
    state
        .save_config("imgw-meteo", "1061 Łódź", 300)
        .await
        .unwrap();
    let queue = Arc::new(QueueRepo::new(pool));
    let server = MockServer::start().await;
    serve(&server, JULY.to_vec()).await;
    let source = ImgwMeteo::new(&format!("{}/warningsmeteo", server.uri())).unwrap();
    let runner = SourceRunner::new(Arc::new(source), state, queue.clone());
    (server, runner, queue)
}

#[tokio::test]
async fn a_real_imgw_warning_reaches_the_queue_exactly_once() {
    let (_server, runner, queue) = station().await;

    let first = runner.cycle(now()).await;
    assert_eq!(first.announced, 1, "{first:?}");
    let pending = queue.pending(10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].priority, Priority::Normal);
    assert!(!pending[0].interrupting);
    assert!(
        pending[0].rendered_text.starts_with(
            "Ostrzeżenie meteorologiczne pierwszego stopnia: Burze. Obszar: Łódź. \
             Obowiązuje 23 lipca od 10:00 do 22:00."
        ),
        "{}",
        pending[0].rendered_text
    );
    assert_eq!(
        pending[0].expires_at,
        Some(Utc.with_ymd_and_hms(2026, 7, 23, 20, 0, 0).unwrap())
    );

    let second = runner.cycle(now() + chrono::Duration::minutes(5)).await;
    assert_eq!(
        second.announced + second.changed + second.cancelled,
        0,
        "{second:?}"
    );
    assert_eq!(
        queue.pending(10).await.unwrap().len(),
        1,
        "powtórne pobranie nic nie dokłada"
    );
}

#[tokio::test]
async fn a_raised_level_is_announced_once_with_a_higher_priority() {
    let (server, runner, queue) = station().await;
    runner.cycle(now()).await;
    assert_eq!(queue.pending(10).await.unwrap().len(), 1);

    let mut json: serde_json::Value = serde_json::from_slice(JULY).unwrap();
    let warning = json
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|w| w["id"] == "Sk20260722203344412")
        .expect("ostrzeżenie dla Łodzi jest w fiksie");
    warning["stopien"] = "2".into();
    serve(&server, serde_json::to_vec(&json).unwrap()).await;

    let third = runner.cycle(now() + chrono::Duration::minutes(5)).await;
    assert_eq!(third.changed, 1, "{third:?}");
    let pending = queue.pending(10).await.unwrap();
    let raised: Vec<_> = pending
        .iter()
        .filter(|i| i.priority == Priority::High)
        .collect();
    assert_eq!(raised.len(), 1, "{pending:?}");
    assert!(
        raised[0].rendered_text.starts_with("Zmiana ostrzeżenia."),
        "{}",
        raised[0].rendered_text
    );

    let fourth = runner.cycle(now() + chrono::Duration::minutes(10)).await;
    assert_eq!(
        fourth.announced + fourth.changed + fourth.cancelled,
        0,
        "{fourth:?}"
    );
    assert_eq!(
        queue
            .pending(10)
            .await
            .unwrap()
            .iter()
            .filter(|i| i.priority == Priority::High)
            .count(),
        1
    );
}
