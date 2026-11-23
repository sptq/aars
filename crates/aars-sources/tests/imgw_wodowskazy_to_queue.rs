//! Wodowskaz przy samym progu nie migocze na antenie: śledzona pozycja nie
//! obniża odcisku i trzyma się z histerezą 5 cm; odwołanie dopiero po
//! prawdziwym spadku. Tylko wiremock na 127.0.0.1; bez dyspozytora, PTT i
//! audio, więc nic nie może kluczować nadajnika.

use std::sync::Arc;

use aars_core::geo::{PositionRepo, parse_position};
use aars_core::module::{ModuleRegistry, ModuleRepo};
use aars_core::queue::QueueRepo;
use aars_core::settings::SettingsRepo;
use aars_core::source_state::SourceStateRepo;
use aars_sources::imgw_wodowskazy::{self, ImgwWodowskazy};
use aars_sources::{CycleReport, SourceRunner};
use chrono::{DateTime, Duration, TimeZone, Utc};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIX: &[u8] = include_bytes!("fixtures/imgw/hydro-2026-11-22.json");
/// Gdańsk, morze (`rzeka "-"`): ostrzegawczy 550, alarmowy 570.
const GDANSK: &str = "154180140";

fn now() -> DateTime<Utc> {
    // Pomiar w fiksie: 20:20 czasu Warszawy = 19:20 UTC.
    Utc.with_ymd_and_hms(2026, 11, 22, 19, 30, 0).unwrap()
}

/// Fiks ze stanem Gdańska `level` cm.
fn body(level: u32) -> Vec<u8> {
    let mut v: serde_json::Value = serde_json::from_slice(FIX).unwrap();
    let gauge = v
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|g| g["id_stacji"] == GDANSK)
        .expect("Gdańsk jest w fiksie");
    gauge["stan_wody"] = level.to_string().into();
    serde_json::to_vec(&v).unwrap()
}

async fn serve(server: &MockServer, level: u32) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/hydro"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body(level), "application/json"))
        .mount(server)
        .await;
}

async fn station() -> (MockServer, SourceRunner, Arc<QueueRepo>) {
    let pool = aars_core::db::test_pool().await;
    ModuleRepo::new(pool.clone())
        .register(&ModuleRegistry::new(vec![imgw_wodowskazy::DESCRIPTOR]))
        .await
        .unwrap();
    let state = SourceStateRepo::new(pool.clone());
    assert_eq!(
        state.set_enabled(imgw_wodowskazy::ID, true).await.unwrap(),
        1
    );
    SettingsRepo::new(pool.clone())
        .kv_put("source:imgw-wodowskazy:stations", GDANSK)
        .await
        .unwrap();
    let rzeszow = parse_position("50.04", "21.99", "").unwrap();
    PositionRepo::new(pool.clone())
        .save(rzeszow.as_ref())
        .await
        .unwrap();
    let queue = Arc::new(QueueRepo::new(pool));
    let server = MockServer::start().await;
    let source = ImgwWodowskazy::new(&format!("{}/hydro", server.uri())).unwrap();
    let runner = SourceRunner::new(Arc::new(source), state, queue.clone());
    (server, runner, queue)
}

fn quiet(r: &CycleReport) -> bool {
    r.error.is_none() && r.announced + r.changed + r.cancelled == 0
}

#[tokio::test]
async fn a_gauge_hovering_at_the_alarm_level_is_announced_once_and_cancelled_after_a_real_fall() {
    let (server, runner, queue) = station().await;
    let mut t = now();

    serve(&server, 571).await;
    let first = runner.cycle(t).await;
    assert_eq!(first.announced, 1, "{first:?}");
    // Ogłoszenie poszło na antenę: odwołanie będzie nadane.
    let aired = queue
        .claim_next()
        .await
        .unwrap()
        .expect("ogłoszenie w kolejce");
    assert!(
        aired
            .rendered_text
            .starts_with("Wodowskaz Gdańsk: stan wody 571 cm, powyżej stanu alarmowego"),
        "{}",
        aired.rendered_text
    );
    queue.mark_done(aired.id).await.unwrap();

    // 569/571 przy alarmowym 570, potem 560 (nad ostrzegawczym) i 546
    // (w histerezie ostrzegawczy − 5): ani zmiany, ani odwołania.
    for level in [569, 571, 569, 571, 569, 560, 546, 545] {
        t += Duration::minutes(15);
        serve(&server, level).await;
        let r = runner.cycle(t).await;
        assert!(quiet(&r), "{level}: {r:?}");
    }
    assert!(queue.pending(10).await.unwrap().is_empty());

    // Prawdziwy spadek poniżej ostrzegawczy − 5: odwołanie po dwóch cyklach.
    serve(&server, 540).await;
    t += Duration::minutes(15);
    let miss = runner.cycle(t).await;
    assert!(quiet(&miss), "{miss:?}");
    t += Duration::minutes(15);
    let cancel = runner.cycle(t).await;
    assert_eq!(
        (cancel.cancelled, cancel.announced, cancel.changed),
        (1, 0, 0),
        "{cancel:?}"
    );
    let pending = queue.pending(10).await.unwrap();
    let texts: Vec<&str> = pending.iter().map(|i| i.rendered_text.as_str()).collect();
    assert_eq!(
        texts,
        ["Wodowskaz Gdańsk: stan wody spadł poniżej stanu ostrzegawczego."]
    );
}
