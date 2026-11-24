//! Biuletyn z prawdziwych fiksów: pogoda (Open-Meteo), propagacja (NOAA,
//! KC2G) i ostrzeżenia złożone w jeden tekst; runner wstawia go do kolejki.
//! Tylko wiremock na 127.0.0.1; bez dyspozytora, PTT i audio, więc nic nie
//! może kluczować nadajnika.

use std::sync::Arc;

use aars_core::config::Config;
use aars_core::geo::{PositionRepo, parse_position};
use aars_core::module::{BULLETIN, ModuleRegistry, ModuleRepo};
use aars_core::priority::Priority;
use aars_core::queue::QueueRepo;
use aars_core::settings::SettingsRepo;
use aars_core::source_state::{NewTracked, SourceStateRepo};
use aars_sources::bulletin::{BulletinRunner, build};
use aars_sources::speech::warsaw_to_utc;
use aars_sources::swpc;
use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::SqlitePool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const EXPECTED: &str = "Biuletyn stacji SP9XYZ. 20:00. Pogoda dla okolic stacji. Obecnie bezchmurnie, temperatura 2 °C. Wiatr 3 m/s, w porywach 5 m/s. W najbliższych godzinach pochmurno, bez opadów. W nocy do 1 °C. Jutro śnieg, od 1 do 3 °C, opady do 2 mm, porywy wiatru do 13 m/s. Dane: Open-Meteo. Propagacja. Strumień słoneczny 98, umiarkowany. Indeks K 0,3, warunki spokojne. Pogoda kosmiczna bez zaburzeń. Jonosonda Warszawa: częstotliwość krytyczna 3,2 megaherca, MUF 10,6 megaherca. Łączności bliskie do pasma 160 metrów, dalekie do pasma 40 metrów. Dane: NOAA, GIRO przez KC2G. Brak obowiązujących ostrzeżeń dla okolic stacji. Koniec biuletynu.";

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn local(s: &str) -> DateTime<Utc> {
    warsaw_to_utc(NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()).unwrap()
}

fn fixture(name: &str) -> String {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
}

async fn server() -> MockServer {
    let server = MockServer::start().await;
    for (route, file) in [
        ("/v1/forecast", "pogoda/open-meteo-rzeszow-2026-11-22.json"),
        ("/compact", "pogoda/met-no-rzeszow-2026-11-22.json"),
        ("/f107", "propagacja/f107-2026-11-22.json"),
        ("/kp", "propagacja/kp-2026-11-22.json"),
        ("/scales", "swpc/noaa-scales-2026-11-22.json"),
        ("/stations", "propagacja/kc2g-stations-2026-11-22.json"),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(fixture(file), "application/json"),
            )
            .mount(&server)
            .await;
    }
    server
}

fn config(server: &MockServer) -> Config {
    let u = |p: &str| format!("{}{p}", server.uri());
    let mut cfg = Config::default();
    cfg.sources.open_meteo_url = u("/v1/forecast");
    cfg.sources.met_no_url = u("/compact");
    cfg.sources.swpc_f107_url = u("/f107");
    cfg.sources.swpc_kp_url = u("/kp");
    cfg.sources.swpc_url = u("/scales");
    cfg.sources.kc2g_url = u("/stations");
    cfg
}

async fn station() -> SqlitePool {
    let pool = aars_core::db::test_pool().await;
    let modules = ModuleRepo::new(pool.clone());
    modules
        .register(&ModuleRegistry::new(vec![BULLETIN, swpc::DESCRIPTOR]))
        .await
        .unwrap();
    modules.set_enabled(&BULLETIN, true).await.unwrap();
    SettingsRepo::new(pool.clone())
        .kv_put("callsign", "SP9XYZ")
        .await
        .unwrap();
    let rzeszow = parse_position("50.04", "21.99", "").unwrap();
    PositionRepo::new(pool.clone())
        .save(rzeszow.as_ref())
        .await
        .unwrap();
    pool
}

/// Warunki z `speech::assert_speakable` (tam tylko dla testów jednostkowych).
fn assert_speakable(text: &str) {
    const RAW: &[&str] = &["NOAA", "MUF", "KC2G", "SWPC"];
    let spoken = aars_audio::text::normalize(text);
    assert!(!spoken.contains("godzinie godzina"), "{spoken}");
    assert!(!spoken.contains(",;"), "{spoken}");
    for word in spoken.split_whitespace() {
        let core = word.trim_matches(|c: char| !c.is_alphanumeric());
        assert!(!RAW.contains(&core), "surowy skrót {core:?} w: {spoken}");
    }
}

#[tokio::test]
async fn the_bulletin_reads_the_fixtures_into_one_text() {
    let server = server().await;
    let pool = station().await;
    let bulletin = build(&config(&server), pool.clone()).unwrap().bulletin;

    let now = at("2026-11-22T19:00:00Z");
    // Bez włączonego źródła ostrzeżeń część milczy — nie mówi „brak ostrzeżeń”.
    let c = bulletin.compose(now).await.unwrap();
    assert_eq!(
        c.text.as_deref(),
        Some(
            EXPECTED
                .replace(" Brak obowiązujących ostrzeżeń dla okolic stacji.", "")
                .as_str()
        ),
        "{:?}",
        c.parts
    );
    assert!(
        c.parts.iter().any(|(id, o)| *id == "ostrzezenia"
            && o.describe().contains("brak włączonych źródeł ostrzeżeń")),
        "{:?}",
        c.parts
    );

    // Włączone, świeże źródło bez ostrzeżeń: „Brak obowiązujących ostrzeżeń dla okolic stacji.”
    let state = SourceStateRepo::new(pool.clone());
    state.set_enabled("swpc", true).await.unwrap();
    state
        .record_success("swpc", now - chrono::Duration::minutes(5))
        .await
        .unwrap();
    let c = bulletin.compose(now).await.unwrap();
    assert_eq!(c.text.as_deref(), Some(EXPECTED), "{:?}", c.parts);
    assert_speakable(EXPECTED);

    // Nadane ostrzeżenie z włączonego, świeżego źródła wchodzi do biuletynu.
    state
        .upsert_active(
            &NewTracked {
                source_id: "swpc".into(),
                item_key: "G".into(),
                topic: "G".into(),
                fingerprint: "G3".into(),
                area: String::new(),
                summary: "G3 — silna burza geomagnetyczna".into(),
                text: "Uwaga, burza geomagnetyczna G3, silna.".into(),
                cancel_text: String::new(),
                priority: Priority::High,
                valid_from: None,
                valid_until: None,
                aired: true,
                first_seen_at: now - chrono::Duration::minutes(30),
                last_seen_at: now - chrono::Duration::minutes(5),
                queue_item_id: None,
                superseded: Vec::new(),
            },
            now - chrono::Duration::minutes(5),
        )
        .await
        .unwrap();
    let text = bulletin.compose(now).await.unwrap().text.unwrap();
    assert!(
        text.contains("Obowiązujące ostrzeżenia. Uwaga, burza geomagnetyczna G3, silna."),
        "{text}"
    );
    assert!(!text.contains("Brak obowiązujących ostrzeżeń"), "{text}");
    assert_speakable(&text);

    // Runner o porze 19:00 lokalnie wstawia dokładnie jeden element.
    let queue = Arc::new(QueueRepo::new(pool.clone()));
    let runner = BulletinRunner::new(bulletin.clone(), queue.clone());
    let tick = runner.tick(local("2026-11-22 19:00:05")).await.unwrap();
    assert!(tick.queued.is_some(), "{tick:?}");
    runner.tick(local("2026-11-22 19:00:15")).await.unwrap();
    let items = queue.pending(10).await.unwrap();
    assert_eq!(items.len(), 1);
    assert!(
        items[0]
            .rendered_text
            .starts_with("Biuletyn stacji SP9XYZ. 19:00. "),
        "{}",
        items[0].rendered_text
    );
    assert_speakable(&items[0].rendered_text);
}
