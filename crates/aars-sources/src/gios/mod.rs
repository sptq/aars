//! Źródło alertów smogowych GIOŚ (ostrzeżenia ze stacji w promieniu od stacji).

pub mod parse;
pub mod speech;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aars_core::module::{ModuleDescriptor, ModuleKind, Needs, SettingKind, SettingSpec, Toggle};
use aars_core::priority::Priority;
use chrono::{DateTime, TimeDelta, Utc};

use crate::http;
use crate::source::{DataSource, FetchFuture, SourceCtx, SourceError, SourceItem, SourceOutput};
use parse::{Station, Warning};

pub const MAX_BODY: usize = 2 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_RADIUS_KM: u32 = 25;
const STATIONS_TTL: TimeDelta = TimeDelta::hours(24);
const MAX_STATION_PAGES: u32 = 10;

pub const DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
    id: "gios-smog",
    name: "Alerty smogowe GIOŚ",
    description: "Alerty o przekroczeniu poziomu informowania i alarmowego dla pyłów w pobliżu stacji według GIOŚ.",
    kind: ModuleKind::Source,
    toggle: Toggle::Switchable,
    default_enabled: false,
    needs: Needs::Position,
    settings: &[
        SettingSpec {
            key: "radius_km",
            label: "Promień od stacji (km)",
            help: "Alerty ze stacji pomiarowych bliżej niż tyle kilometrów od stacji są ogłaszane.",
            kind: SettingKind::Integer { min: 5, max: 100 },
            default: "25",
        },
        SettingSpec {
            key: "interval_s",
            label: "Interwał odpytywania (s)",
            help: "Jak często odpytywać źródło.",
            kind: SettingKind::Integer {
                min: 300,
                max: 3600,
            },
            default: "600",
        },
    ],
    see_also: None,
};

fn radius_km(ctx: &SourceCtx) -> u32 {
    ctx.settings
        .get("radius_km")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| (5..=100).contains(n))
        .unwrap_or(DEFAULT_RADIUS_KM)
}

type StationCache = Option<(DateTime<Utc>, Arc<Vec<Station>>)>;

pub struct Gios {
    client: reqwest::Client,
    base: String,
    stations: Mutex<StationCache>,
}

impl Gios {
    pub fn new(base: &str) -> Result<Self, SourceError> {
        Self::with_timeout(base, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(base: &str, timeout: Duration) -> Result<Self, SourceError> {
        Ok(Self {
            client: http::client(timeout)?,
            base: base.trim_end_matches('/').to_owned(),
            stations: Mutex::new(None),
        })
    }

    fn cached(&self) -> StationCache {
        // Zatruty Mutex: pamięć podręczna jest prosta, więc ją odzyskujemy.
        self.stations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    async fn download_stations(&self) -> Result<Vec<Station>, SourceError> {
        let mut all = Vec::new();
        let mut pages = 1;
        let mut page = 0;
        while page < pages.min(MAX_STATION_PAGES) {
            let url = format!("{}/station/findAll?size=500&page={page}", self.base);
            let body = http::get_limited(&self.client, &url, MAX_BODY).await?;
            let (stations, total) = parse::parse_stations(&body)?;
            all.extend(stations);
            pages = total;
            page += 1;
        }
        Ok(all)
    }

    /// Lista stacji z pamięci (do 24 h wg `ctx.now`) albo świeżo pobrana;
    /// nieudane odświeżenie przy starej kopii zostawia starą kopię.
    async fn stations(&self, now: DateTime<Utc>) -> Result<Arc<Vec<Station>>, SourceError> {
        let cached = self.cached();
        if let Some((at, list)) = &cached
            && now - *at < STATIONS_TTL
        {
            return Ok(Arc::clone(list));
        }
        match self.download_stations().await {
            Ok(list) => {
                let list = Arc::new(list);
                *self.stations.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((now, Arc::clone(&list)));
                Ok(list)
            }
            Err(e) => match cached {
                Some((_, old)) => {
                    tracing::warn!("odświeżenie listy stacji GIOŚ nie powiodło się: {e}");
                    Ok(old)
                }
                None => Err(e),
            },
        }
    }
}

impl DataSource for Gios {
    fn id(&self) -> &'static str {
        "gios-smog"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(600)
    }

    fn needs(&self) -> Needs {
        Needs::Position
    }

    fn scope(&self, ctx: &SourceCtx) -> String {
        let (lat, lon) = ctx.position.as_ref().map_or((0.0, 0.0), |p| (p.lat, p.lon));
        format!("{lat:.2},{lon:.2}|r={}", radius_km(ctx))
    }

    fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a> {
        Box::pin(async move {
            if ctx.position.is_none() {
                return Err(SourceError::Parse("brak pozycji stacji".to_owned()));
            }
            // Najpierw stacje, potem ostrzeżenia; Mutex nigdy przez await.
            let stations = self.stations(ctx.now).await?;
            let url = format!("{}/levels/getWarnings", self.base);
            let body = http::get_limited(&self.client, &url, MAX_BODY).await?;
            let (warnings, skipped) = parse::parse_warnings(&body)?;
            Ok(to_output(&warnings, skipped, &stations, ctx))
        })
    }
}

/// Czysta funkcja: ostrzeżenia → pozycje na `(strefa, poziom, parametr)` dla
/// stacji w promieniu; ostrzeżenie ze stacji spoza listy jest ignorowane.
pub fn to_output(
    warnings: &[Warning],
    skipped: usize,
    stations: &[Station],
    ctx: &SourceCtx,
) -> SourceOutput {
    let complete = skipped == 0;
    let Some(pos) = ctx.position.as_ref() else {
        return SourceOutput {
            items: Vec::new(),
            complete,
        };
    };
    let station = (pos.lat, pos.lon);
    let radius = f64::from(radius_km(ctx));
    let by_id: HashMap<i64, &Station> = stations.iter().map(|s| (s.id, s)).collect();
    let mut items: Vec<SourceItem> = Vec::new();
    for w in warnings {
        let Some(st) = by_id.get(&w.station_id) else {
            continue;
        };
        if crate::speech::haversine_km(station, (st.lat, st.lon)) > radius {
            continue;
        }
        let key = format!("{}|{}|{}", w.zone_code, w.level, w.param_code);
        if items.iter().any(|i| i.key == key) {
            continue;
        }
        let text = speech::announcement(w);
        items.push(SourceItem {
            topic: key.clone(),
            fingerprint: key.clone(),
            key,
            change_text: text.clone(),
            text,
            cancel_text: speech::cancel(&w.zone_name),
            summary: speech::summary(w),
            priority: if w.level == "PA" {
                Priority::High
            } else {
                Priority::Normal
            },
            interrupting: false,
            valid_from: None,
            valid_until: None,
        });
    }
    SourceOutput { items, complete }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::geo::StationPosition;
    use chrono::TimeZone;

    const STATIONS: &[u8] =
        include_bytes!("../../tests/fixtures/gios/findAll-size500-2026-11-22.json");
    const WARNINGS: &[u8] = include_bytes!("../../tests/fixtures/gios/getWarnings-2026-11-22.json");

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 22, h, 0, 0).unwrap()
    }

    fn ctx_at(now: DateTime<Utc>, radius: &str) -> SourceCtx {
        SourceCtx {
            now,
            area: Vec::new(),
            area_text: String::new(),
            position: Some(StationPosition {
                lat: 50.04,
                lon: 22.00,
                locator: "KN00".into(),
            }),
            settings: [("radius_km".to_owned(), radius.to_owned())].into(),
            tracked_keys: Default::default(),
        }
    }

    fn run_body(warnings: &[u8], ctx: &SourceCtx) -> SourceOutput {
        let (stations, _) = parse::parse_stations(STATIONS).unwrap();
        let (w, skipped) = parse::parse_warnings(warnings).unwrap();
        to_output(&w, skipped, &stations, ctx)
    }

    fn run(radius: &str) -> SourceOutput {
        run_body(WARNINGS, &ctx_at(at(12), radius))
    }

    #[test]
    fn the_descriptor_has_the_documented_settings() {
        let d = &DESCRIPTOR;
        assert_eq!((d.id, d.name), ("gios-smog", "Alerty smogowe GIOŚ"));
        assert_eq!(d.needs, Needs::Position);
        assert!(!d.default_enabled);
        let keys: Vec<_> = d.settings.iter().map(|s| (s.key, s.default)).collect();
        assert_eq!(keys, [("radius_km", "25"), ("interval_s", "600")]);
        assert_eq!(
            d.settings[0].kind,
            SettingKind::Integer { min: 5, max: 100 }
        );
        assert_eq!(
            d.settings[1].kind,
            SettingKind::Integer {
                min: 300,
                max: 3600
            }
        );
    }

    #[test]
    fn warnings_near_the_station_become_one_item_per_zone() {
        // Stacja Krasne (strefa PL1802) leży 6,5 km od Rzeszowa, więc dla
        // dokładnie jednej pozycji promień to 5 km (Starzyńskiego: 2,7 km).
        let wider = run("10");
        let keys: Vec<_> = wider.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["PL1802|PI|PM10", "PL1801|PI|PM10"]);
        let out = run("5");
        assert!(out.complete);
        assert_eq!(out.items.len(), 1);
        let item = &out.items[0];
        assert_eq!(item.key, "PL1801|PI|PM10");
        assert_eq!(item.topic, "PL1801|PI|PM10");
        assert_eq!(item.fingerprint, "PL1801|PI|PM10");
        assert!(
            item.text.starts_with(
                "Alert smogowy GIOŚ, miasto Rzeszów. Przekroczony poziom informowania dla pył zawieszony PM10."
            ),
            "{}",
            item.text
        );
        assert!(item.text.ends_with("Źródło: GIOŚ."), "{}", item.text);
        assert!(!item.text.contains('\n'));
        assert!(item.text.contains("Ogół ludności: rozważ"), "{}", item.text);
        assert!(
            item.text
                .contains(" Pełne zalecenia na stronie GIOŚ. Źródło")
        );
        assert_eq!(
            item.cancel_text,
            "Alert smogowy dla miasto Rzeszów odwołany. Źródło: GIOŚ."
        );
        assert_eq!(item.summary, "PM10, poziom informowania, miasto Rzeszów");
        assert_eq!(item.priority, Priority::Normal);
        assert!(!item.interrupting);
        assert_eq!((item.valid_from, item.valid_until), (None, None));
    }

    #[test]
    fn a_wide_radius_merges_stations_of_one_zone() {
        let out = run("100");
        let mut keys: Vec<_> = out.items.iter().map(|i| i.key.clone()).collect();
        let n = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), n);
        assert!(n >= 2);
        assert_eq!(keys.iter().filter(|k| k.starts_with("PL1802|")).count(), 1);
        assert!(keys.contains(&"PL1801|PI|PM10".to_owned()));
    }

    #[test]
    fn pa_is_high_priority() {
        let mut v: serde_json::Value = serde_json::from_slice(WARNINGS).unwrap();
        let i = v
            .as_array()
            .unwrap()
            .iter()
            .position(|w| w["zone"]["zoneCode"] == "PL1801")
            .unwrap();
        v[i]["standard"]["standardTypeCode"] = "PA".into();
        let out = run_body(&serde_json::to_vec(&v).unwrap(), &ctx_at(at(12), "5"));
        let item = &out.items[0];
        assert_eq!(item.key, "PL1801|PA|PM10");
        assert_eq!(item.priority, Priority::High);
        assert!(item.text.contains("Przekroczony poziom alarmowy dla"));
        assert_eq!(item.summary, "PM10, poziom alarmowy, miasto Rzeszów");
    }

    #[test]
    fn another_level_uses_the_standard_type_from_the_response() {
        let mut v: serde_json::Value = serde_json::from_slice(WARNINGS).unwrap();
        for w in v.as_array_mut().unwrap() {
            w["standard"]["standardTypeCode"] = "PX".into();
            w["standard"]["standardType"] = "Poziom docelowy".into();
        }
        let out = run_body(&serde_json::to_vec(&v).unwrap(), &ctx_at(at(12), "10"));
        assert!(
            out.items[0]
                .text
                .contains("Przekroczony poziom Poziom docelowy dla")
        );
        assert_eq!(out.items[0].priority, Priority::Normal);
    }

    #[test]
    fn a_short_precaution_is_not_cut() {
        let mut v: serde_json::Value = serde_json::from_slice(WARNINGS).unwrap();
        for w in v.as_array_mut().unwrap() {
            w["recommendedPrecaution"] = "Zostań w domu,".into();
        }
        let out = run_body(&serde_json::to_vec(&v).unwrap(), &ctx_at(at(12), "10"));
        assert!(
            out.items[0]
                .text
                .ends_with("pył zawieszony PM10. Zostań w domu. Źródło: GIOŚ."),
            "{}",
            out.items[0].text
        );
    }

    #[test]
    fn a_warning_for_an_unknown_station_is_ignored() {
        let mut v: serde_json::Value = serde_json::from_slice(WARNINGS).unwrap();
        for w in v.as_array_mut().unwrap() {
            w["sensor"]["stationId"] = 999_999.into();
        }
        let out = run_body(&serde_json::to_vec(&v).unwrap(), &ctx_at(at(12), "100"));
        assert!(out.items.is_empty());
        assert!(out.complete);
    }

    #[test]
    fn skipped_warnings_make_the_output_incomplete() {
        let (stations, _) = parse::parse_stations(STATIONS).unwrap();
        assert!(!to_output(&[], 1, &stations, &ctx_at(at(12), "10")).complete);
    }

    #[test]
    fn the_scope_uses_position_and_resolved_radius() {
        let src = Gios::new("http://127.0.0.1:1").unwrap();
        assert_eq!(src.scope(&ctx_at(at(12), "10")), "50.04,22.00|r=10");
        assert_eq!(src.scope(&ctx_at(at(12), "x")), "50.04,22.00|r=25");
        let mut c = ctx_at(at(12), "10");
        c.settings.clear();
        assert_eq!(src.scope(&c), "50.04,22.00|r=25");
    }

    mod http_tests {
        use super::*;
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn json(body: &[u8]) -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_raw(body.to_vec(), "application/json")
        }

        async fn stations_mock(server: &MockServer, expect: u64) {
            Mock::given(method("GET"))
                .and(path("/station/findAll"))
                .and(query_param("size", "500"))
                .and(query_param("page", "0"))
                .respond_with(json(STATIONS))
                .expect(expect)
                .mount(server)
                .await;
        }

        async fn warnings_mock(server: &MockServer) {
            Mock::given(method("GET"))
                .and(path("/levels/getWarnings"))
                .respond_with(json(WARNINGS))
                .mount(server)
                .await;
        }

        #[tokio::test]
        async fn the_station_list_is_fetched_once_a_day() {
            let server = MockServer::start().await;
            stations_mock(&server, 2).await;
            warnings_mock(&server).await;
            let src = Gios::new(&server.uri()).unwrap();
            let first = src.fetch(&ctx_at(at(12), "5")).await.unwrap();
            assert_eq!(first.items.len(), 1);
            src.fetch(&ctx_at(at(13), "5")).await.unwrap();
            let later = Utc.with_ymd_and_hms(2026, 11, 23, 13, 0, 0).unwrap();
            let third = src.fetch(&ctx_at(later, "5")).await.unwrap();
            assert_eq!(third.items.len(), 1);
            assert_eq!(src.id(), "gios-smog");
            assert_eq!(src.needs(), Needs::Position);
            assert_eq!(src.default_interval(), Duration::from_secs(600));
        }

        #[tokio::test]
        async fn a_failed_refresh_keeps_the_old_station_list() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/station/findAll"))
                .respond_with(json(STATIONS))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/station/findAll"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&server)
                .await;
            warnings_mock(&server).await;
            let src = Gios::new(&server.uri()).unwrap();
            src.fetch(&ctx_at(at(12), "5")).await.unwrap();
            let later = Utc.with_ymd_and_hms(2026, 11, 24, 12, 0, 0).unwrap();
            let out = src.fetch(&ctx_at(later, "5")).await.unwrap();
            assert_eq!(out.items.len(), 1);
        }

        #[tokio::test]
        async fn a_failed_first_station_fetch_is_an_error() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/station/findAll"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&server)
                .await;
            warnings_mock(&server).await;
            let src = Gios::new(&server.uri()).unwrap();
            let err = src.fetch(&ctx_at(at(12), "10")).await.err().unwrap();
            assert!(matches!(err, SourceError::Status(503)), "{err:?}");
        }

        #[tokio::test]
        async fn a_warnings_error_is_an_error() {
            let server = MockServer::start().await;
            stations_mock(&server, 1).await;
            Mock::given(method("GET"))
                .and(path("/levels/getWarnings"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;
            let src = Gios::new(&server.uri()).unwrap();
            let err = src.fetch(&ctx_at(at(12), "10")).await.err().unwrap();
            assert!(matches!(err, SourceError::Status(500)), "{err:?}");
        }

        #[tokio::test]
        async fn no_position_is_a_parse_error_not_a_panic() {
            let src = Gios::new("http://127.0.0.1:1").unwrap();
            let mut c = ctx_at(at(12), "10");
            c.position = None;
            assert!(matches!(src.fetch(&c).await, Err(SourceError::Parse(_))));
        }
    }
}
