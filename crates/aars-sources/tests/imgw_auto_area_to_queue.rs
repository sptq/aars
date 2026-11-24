//! Źródło obszarowe z pustym ręcznym obszarem liczy obszar z pozycji stacji
//! (etap 13). Tylko wiremock na 127.0.0.1; bez dyspozytora, PTT i audio.

use std::sync::Arc;

use aars_core::geo::{PositionRepo, parse_position};
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

async fn station(
    position: Option<(&str, &str)>,
) -> (MockServer, SourceRunner, Arc<QueueRepo>, SourceStateRepo) {
    let pool = aars_core::db::test_pool().await;
    let state = SourceStateRepo::new(pool.clone());
    state.set_enabled("imgw-meteo", true).await.unwrap();
    state.save_config("imgw-meteo", "", 300).await.unwrap();
    if let Some((lat, lon)) = position {
        let p = parse_position(lat, lon, "").unwrap();
        PositionRepo::new(pool.clone())
            .save(p.as_ref())
            .await
            .unwrap();
    }
    let queue = Arc::new(QueueRepo::new(pool));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/warningsmeteo"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(JULY.to_vec(), "application/json; charset=utf-8"),
        )
        .mount(&server)
        .await;
    let source = ImgwMeteo::new(&format!("{}/warningsmeteo", server.uri())).unwrap();
    let runner = SourceRunner::new(Arc::new(source), state.clone(), queue.clone());
    (server, runner, queue, state)
}

#[tokio::test]
async fn an_empty_manual_area_is_taken_from_the_station_position() {
    // Opoczno, powiat 1007: leży w jednym ostrzeżeniu z fiksu.
    let (_server, runner, queue, _state) = station(Some(("51.38", "20.28"))).await;
    let report = runner.cycle(now()).await;
    assert_eq!(report.skipped, None, "{report:?}");
    assert_eq!(report.announced, 1, "{report:?}");
    let pending = queue.pending(10).await.unwrap();
    assert!(
        pending[0]
            .rendered_text
            .contains("Obszar: powiat opoczyński"),
        "{}",
        pending[0].rendered_text
    );
}

#[tokio::test]
async fn without_a_position_the_cycle_is_skipped() {
    let (_server, runner, queue, _state) = station(None).await;
    let report = runner.cycle(now()).await;
    assert_eq!(report.skipped, Some("brak pozycji stacji"), "{report:?}");
    assert!(!report.fetched);
    assert!(queue.pending(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_manual_area_wins_over_the_automatic_one() {
    // Pozycja w Opocznie (1007), ale ręcznie Łódź (1061): ogłoszenie dla Łodzi.
    let (_server, runner, queue, state) = station(Some(("51.38", "20.28"))).await;
    state
        .save_config("imgw-meteo", "1061 Łódź", 300)
        .await
        .unwrap();
    let report = runner.cycle(now()).await;
    assert_eq!(report.announced, 1, "{report:?}");
    let pending = queue.pending(10).await.unwrap();
    assert!(
        pending[0].rendered_text.contains("Obszar: Łódź"),
        "{}",
        pending[0].rendered_text
    );
    assert!(
        !pending[0].rendered_text.contains("opoczyński"),
        "{}",
        pending[0].rendered_text
    );
}

#[tokio::test]
async fn a_position_outside_poland_skips_the_cycle() {
    let (_server, runner, queue, _state) = station(Some(("52.52", "13.40"))).await;
    let report = runner.cycle(now()).await;
    assert_eq!(
        report.skipped,
        Some("pozycja poza obszarem Polski"),
        "{report:?}"
    );
    assert!(!report.fetched);
    assert!(queue.pending(10).await.unwrap().is_empty());
}
