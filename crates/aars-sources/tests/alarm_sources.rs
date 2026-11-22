//! Kryterium wyjścia etapu 11a: dwa źródła alarmowe (SWPC z G3 i EMSC ze
//! świeżym wstrząsem) po jednym cyklu dają w kolejce dwa komunikaty o
//! oczekiwanych priorytetach. Tylko wiremock na 127.0.0.1; bez dyspozytora,
//! PTT i audio, więc nic nie może kluczować nadajnika.

use std::sync::Arc;

use aars_core::geo::{PositionRepo, parse_position};
use aars_core::module::{ModuleRegistry, ModuleRepo};
use aars_core::priority::Priority;
use aars_core::queue::QueueRepo;
use aars_core::source_state::SourceStateRepo;
use aars_sources::emsc::{self, Emsc};
use aars_sources::swpc::{self, Swpc};
use aars_sources::SourceRunner;
use chrono::{DateTime, Duration, TimeZone, Utc};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 22, 15, 0, 0).unwrap()
}

async fn serve(server: &MockServer, route: &str, body: String) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
        .mount(server)
        .await;
}

fn swpc_body(stamp: DateTime<Utc>) -> String {
    serde_json::json!({
        "0": {
            "DateStamp": stamp.format("%Y-%m-%d").to_string(),
            "TimeStamp": stamp.format("%H:%M:%S").to_string(),
            "R": {"Scale": "0", "Text": "none"},
            "S": {"Scale": "0", "Text": "none"},
            "G": {"Scale": "3", "Text": "strong"}
        }
    })
    .to_string()
}

fn emsc_body(time: DateTime<Utc>) -> String {
    serde_json::json!({
        "type": "FeatureCollection",
        "features": [{
            "type": "Feature",
            "geometry": {"type": "Point", "coordinates": [16.1286, 51.5552, -7.3]},
            "id": "20261122_0000999",
            "properties": {
                "time": time.format("%Y-%m-%dT%H:%M:%S.000Z").to_string(),
                "flynn_region": "POLAND",
                "lat": 51.5552, "lon": 16.1286, "depth": 7.3,
                "mag": 3.4, "magtype": "m",
                "unid": "20261122_0000999"
            }
        }]
    })
    .to_string()
}

#[tokio::test]
async fn a_g3_storm_and_a_fresh_quake_reach_the_queue_with_the_right_priorities() {
    let pool = aars_core::db::test_pool().await;
    let registry = ModuleRegistry::new(vec![swpc::DESCRIPTOR, emsc::DESCRIPTOR]);
    ModuleRepo::new(pool.clone()).register(&registry).await.unwrap();
    let state = SourceStateRepo::new(pool.clone());
    state.set_enabled("swpc", true).await.unwrap();
    state.set_enabled("emsc", true).await.unwrap();
    let position = parse_position("51.5", "16.1", "").unwrap();
    PositionRepo::new(pool.clone()).save(position.as_ref()).await.unwrap();
    let queue = Arc::new(QueueRepo::new(pool));

    let server = MockServer::start().await;
    serve(&server, "/scales.json", swpc_body(now() - Duration::minutes(1))).await;
    serve(&server, "/query", emsc_body(now() - Duration::minutes(5))).await;

    let swpc = Swpc::new(&format!("{}/scales.json", server.uri())).unwrap();
    let emsc = Emsc::new(&format!("{}/query", server.uri())).unwrap();
    let swpc_runner = SourceRunner::new(Arc::new(swpc), state.clone(), queue.clone());
    let emsc_runner = SourceRunner::new(Arc::new(emsc), state, queue.clone());

    let a = swpc_runner.cycle(now()).await;
    let b = emsc_runner.cycle(now()).await;
    assert_eq!(a.announced, 1, "{a:?}");
    assert_eq!(b.announced, 1, "{b:?}");

    let pending = queue.pending(10).await.unwrap();
    assert_eq!(pending.len(), 2, "{pending:?}");
    let swpc_item = pending.iter().find(|i| i.rendered_text.contains("NOAA")).expect("SWPC");
    assert_eq!(swpc_item.priority, Priority::High);
    let emsc_item = pending.iter().find(|i| i.rendered_text.contains("EMSC")).expect("EMSC");
    assert_eq!(emsc_item.priority, Priority::Normal);
}
