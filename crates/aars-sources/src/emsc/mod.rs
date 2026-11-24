//! Źródło wstrząsów w pobliżu stacji (EMSC FDSN).

pub mod parse;
pub mod speech;

use std::time::Duration;

use aars_core::module::{ModuleDescriptor, ModuleKind, Needs, SettingKind, SettingSpec, Toggle};
use aars_core::priority::Priority;
use chrono::TimeDelta;

use crate::http;

use crate::source::{DataSource, FetchFuture, SourceCtx, SourceError, SourceItem, SourceOutput};
use parse::Quake;

pub const MAX_BODY: usize = 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

const DEFAULT_MIN_MAG: f64 = 3.0;
const DEFAULT_RADIUS_KM: u32 = 150;
/// Kilometrów na stopień łuku wielkiego koła.
const KM_PER_DEGREE: f64 = 111.195;
const LOOKBACK: TimeDelta = TimeDelta::hours(6);
const VALIDITY: TimeDelta = TimeDelta::minutes(30);
const HIGH_MAG: f64 = 4.5;

pub const DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
    id: "emsc",
    name: "Wstrząsy sejsmiczne (EMSC)",
    description: "Wstrząsy ziemi w promieniu od stacji według Europejsko-Śródziemnomorskiego Centrum Sejsmologicznego (EMSC).",
    kind: ModuleKind::Source,
    toggle: Toggle::Switchable,
    default_enabled: false,
    needs: Needs::Position,
    auto_area: None,
    settings: &[
        SettingSpec {
            key: "min_mag",
            label: "Najniższa magnituda do ogłoszenia",
            help: "Wstrząsy o magnitudzie od tej wartości (1,0–9,0) są ogłaszane.",
            kind: SettingKind::Decimal { min: 1.0, max: 9.0 },
            default: "3.0",
        },
        SettingSpec {
            key: "radius_km",
            label: "Promień od stacji (km)",
            help: "Wstrząsy bliżej niż tyle kilometrów od stacji są ogłaszane.",
            kind: SettingKind::Integer { min: 10, max: 1000 },
            default: "150",
        },
        SettingSpec {
            key: "interval_s",
            label: "Interwał odpytywania (s)",
            help: "Jak często odpytywać źródło.",
            kind: SettingKind::Integer { min: 60, max: 3600 },
            default: "120",
        },
    ],
    see_also: None,
};

fn min_mag(ctx: &SourceCtx) -> f64 {
    ctx.settings
        .get("min_mag")
        .and_then(|v| v.trim().replace(',', ".").parse::<f64>().ok())
        .filter(|n| (1.0..=9.0).contains(n))
        .unwrap_or(DEFAULT_MIN_MAG)
}

fn radius_km(ctx: &SourceCtx) -> u32 {
    ctx.settings
        .get("radius_km")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| (10..=1000).contains(n))
        .unwrap_or(DEFAULT_RADIUS_KM)
}

/// Adres zapytania FDSN dla pozycji stacji; `None` bez pozycji.
pub fn query_url(base: &str, ctx: &SourceCtx) -> Option<String> {
    let pos = ctx.position.as_ref()?;
    let start = (ctx.now - LOOKBACK).format("%Y-%m-%dT%H:%M:%S");
    Some(format!(
        "{base}?format=json&lat={:.3}&lon={:.3}&maxradius={:.3}&minmag={:.1}&start={start}&orderby=time&limit=100",
        pos.lat,
        pos.lon,
        f64::from(radius_km(ctx)) / KM_PER_DEGREE,
        min_mag(ctx),
    ))
}

pub struct Emsc {
    client: reqwest::Client,
    url: String,
}

impl Emsc {
    pub fn new(url: &str) -> Result<Self, SourceError> {
        Self::with_timeout(url, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(url: &str, timeout: Duration) -> Result<Self, SourceError> {
        Ok(Self {
            client: http::client(timeout)?,
            url: url.to_owned(),
        })
    }
}

impl DataSource for Emsc {
    fn id(&self) -> &'static str {
        "emsc"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(120)
    }

    fn needs(&self) -> Needs {
        Needs::Position
    }

    fn scope(&self, ctx: &SourceCtx) -> String {
        let (lat, lon) = ctx.position.as_ref().map_or((0.0, 0.0), |p| (p.lat, p.lon));
        format!(
            "{lat:.2},{lon:.2}|r={}|m={:.1}",
            radius_km(ctx),
            min_mag(ctx)
        )
    }

    fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a> {
        Box::pin(async move {
            let url = query_url(&self.url, ctx)
                .ok_or_else(|| SourceError::Parse("brak pozycji stacji".to_owned()))?;
            let body = http::get_limited(&self.client, &url, MAX_BODY).await?;
            let parsed = parse::parse_events(&body)?;
            Ok(to_output(&parsed.events, parsed.skipped, ctx))
        })
    }
}

/// Czysta funkcja: zdarzenia → pozycje w promieniu stacji, jeszcze ważne.
pub fn to_output(events: &[Quake], skipped: usize, ctx: &SourceCtx) -> SourceOutput {
    let complete = skipped == 0;
    let Some(pos) = ctx.position.as_ref() else {
        return SourceOutput {
            items: Vec::new(),
            complete,
        };
    };
    let station = (pos.lat, pos.lon);
    let (min_mag, radius) = (min_mag(ctx), f64::from(radius_km(ctx)));
    let mut items = Vec::new();
    for q in events {
        let valid_until = q.time + VALIDITY;
        if valid_until <= ctx.now || q.mag < min_mag {
            continue;
        }
        let at = (q.lat, q.lon);
        let distance = crate::speech::haversine_km(station, at);
        if distance > radius {
            continue;
        }
        let km = distance.round() as u32;
        let direction = crate::speech::compass8(station, at);
        let step = (q.mag * 2.0).floor() / 2.0;
        items.push(SourceItem {
            key: q.unid.clone(),
            topic: format!("emsc:{}", q.unid),
            fingerprint: format!("{}|{step:.1}", q.unid),
            text: speech::announcement(q, km, direction),
            change_text: speech::change(q, km, direction),
            cancel_text: String::new(),
            summary: speech::summary(q, km),
            priority: if q.mag >= HIGH_MAG {
                Priority::High
            } else {
                Priority::Normal
            },
            interrupting: false,
            valid_from: Some(q.time),
            valid_until: Some(valid_until),
        });
    }
    SourceOutput { items, complete }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::geo::StationPosition;
    use aars_core::priority::Priority;
    use chrono::{DateTime, TimeZone, Utc};

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/emsc/pl-bbox-2026-11-22.json");

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 22, h, m, 0).unwrap()
    }

    fn ctx_at(now: DateTime<Utc>, settings: &[(&str, &str)]) -> SourceCtx {
        SourceCtx {
            now,
            area: Vec::new(),
            area_text: String::new(),
            position: Some(StationPosition {
                lat: 51.77,
                lon: 19.46,
                locator: "JO91".into(),
            }),
            settings: settings
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            tracked_keys: Default::default(),
            tracked: Default::default(),
        }
    }

    fn run(body: &[u8], ctx: &SourceCtx) -> SourceOutput {
        let p = parse::parse_events(body).unwrap();
        to_output(&p.events, p.skipped, ctx)
    }

    fn one_event(mag: f64, region: &str) -> Vec<u8> {
        let mut v: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
        let mut f = v["features"][0].clone();
        f["properties"]["mag"] = mag.into();
        f["properties"]["flynn_region"] = region.into();
        v["features"] = serde_json::json!([f]);
        serde_json::to_vec(&v).unwrap()
    }

    fn strong_ctx() -> SourceCtx {
        ctx_at(at(12, 40), &[("min_mag", "2.0"), ("radius_km", "300")])
    }

    #[test]
    fn the_descriptor_has_the_documented_settings() {
        let d = &DESCRIPTOR;
        assert_eq!((d.id, d.name), ("emsc", "Wstrząsy sejsmiczne (EMSC)"));
        assert_eq!(d.needs, Needs::Position);
        assert!(!d.default_enabled);
        let keys: Vec<_> = d.settings.iter().map(|s| (s.key, s.default)).collect();
        assert_eq!(
            keys,
            [
                ("min_mag", "3.0"),
                ("radius_km", "150"),
                ("interval_s", "120")
            ]
        );
        assert_eq!(
            d.settings[0].kind,
            SettingKind::Decimal { min: 1.0, max: 9.0 }
        );
        assert_eq!(
            d.settings[1].kind,
            SettingKind::Integer { min: 10, max: 1000 }
        );
        assert_eq!(
            d.settings[2].kind,
            SettingKind::Integer { min: 60, max: 3600 }
        );
    }

    #[test]
    fn only_recent_strong_close_events_become_items() {
        let out = run(FIXTURE, &strong_ctx());
        assert!(out.complete);
        assert_eq!(out.items.len(), 1);
        let item = &out.items[0];
        let km = crate::speech::haversine_km((51.77, 19.46), (51.5552, 16.1286)).round() as u32;
        assert_eq!(
            item.text,
            format!(
                "Wstrząs sejsmiczny o magnitudzie 2,1, {} na zachód od stacji, Polska, o 13:26. Dane: EMSC.",
                crate::speech::km_phrase(km)
            )
        );
        assert_eq!(
            (
                item.key.as_str(),
                item.topic.as_str(),
                item.fingerprint.as_str()
            ),
            (
                "20261122_0000158",
                "emsc:20261122_0000158",
                "20261122_0000158|2.0"
            )
        );
        assert_eq!(item.priority, Priority::Normal);
        assert!(!item.interrupting);
        assert_eq!(item.cancel_text, "");
        assert_eq!(
            item.valid_from,
            Some(
                Utc.with_ymd_and_hms(2026, 11, 22, 12, 26, 38).unwrap()
                    + TimeDelta::milliseconds(654)
            )
        );
        assert_eq!(
            item.valid_until,
            Some(
                Utc.with_ymd_and_hms(2026, 11, 22, 12, 56, 38).unwrap()
                    + TimeDelta::milliseconds(654)
            )
        );
        assert!(
            item.change_text
                .starts_with("Poprawka danych wstrząsu. Wstrząs sejsmiczny")
        );
        assert_eq!(item.summary, format!("M2,1, {km} km, Polska"));
        let late = ctx_at(at(13, 0), &[("min_mag", "2.0"), ("radius_km", "300")]);
        assert!(run(FIXTURE, &late).items.is_empty());
    }

    #[test]
    fn emsc_texts_survive_normalization() {
        let out = run(FIXTURE, &strong_ctx());
        let item = &out.items[0];
        crate::speech::assert_speakable(&[&item.text, &item.change_text, &item.cancel_text]);
        let spoken = aars_audio::text::normalize(&item.text);
        assert!(spoken.contains("Polska, o godzinie trzynastej dwadzieścia sześć."), "{spoken}");
        assert!(spoken.ends_with("Dane: E M S C."), "{spoken}");
    }

    #[test]
    fn min_mag_and_radius_filter_locally() {
        assert!(
            run(
                FIXTURE,
                &ctx_at(at(12, 40), &[("min_mag", "3.0"), ("radius_km", "300")])
            )
            .items
            .is_empty()
        );
        assert!(
            run(
                FIXTURE,
                &ctx_at(at(12, 40), &[("min_mag", "2.0"), ("radius_km", "100")])
            )
            .items
            .is_empty()
        );
    }

    #[test]
    fn the_fingerprint_steps_by_half_a_magnitude() {
        let fp = |m: f64| {
            run(&one_event(m, "POLAND"), &strong_ctx()).items[0]
                .fingerprint
                .clone()
        };
        assert_eq!(fp(3.3), "20261122_0000158|3.0");
        assert_eq!(fp(3.4), "20261122_0000158|3.0");
        assert_eq!(fp(3.5), "20261122_0000158|3.5");
    }

    #[test]
    fn a_strong_quake_is_high_priority() {
        let p = |m: f64| run(&one_event(m, "POLAND"), &strong_ctx()).items[0].priority;
        assert_eq!(p(4.4), Priority::Normal);
        assert_eq!(p(4.5), Priority::High);
    }

    #[test]
    fn an_unknown_region_is_not_spoken() {
        let out = run(&one_event(2.1, "ROMANIA"), &strong_ctx());
        let t = &out.items[0].text;
        assert!(t.contains("od stacji, o "), "{t}");
        assert!(!out.items[0].summary.ends_with(','));
    }

    #[test]
    fn skipped_events_make_the_output_incomplete() {
        let p = parse::parse_events(FIXTURE).unwrap();
        assert!(!to_output(&p.events, 1, &strong_ctx()).complete);
    }

    #[test]
    fn the_query_url_carries_position_radius_and_threshold() {
        let url = query_url("http://h/q", &ctx_at(at(12, 40), &[])).unwrap();
        assert_eq!(
            url,
            "http://h/q?format=json&lat=51.770&lon=19.460&maxradius=1.349&minmag=3.0&start=2026-11-22T06:40:00&orderby=time&limit=100"
        );
        let mut c = ctx_at(at(12, 40), &[]);
        c.position = None;
        assert!(query_url("http://h/q", &c).is_none());
    }

    #[test]
    fn the_scope_uses_resolved_settings() {
        let src = Emsc::new("http://127.0.0.1:1/x").unwrap();
        assert_eq!(
            src.scope(&ctx_at(at(12, 40), &[])),
            "51.77,19.46|r=150|m=3.0"
        );
        assert_eq!(src.scope(&strong_ctx()), "51.77,19.46|r=300|m=2.0");
        assert_eq!(
            src.scope(&ctx_at(at(12, 40), &[("min_mag", "x"), ("radius_km", "5")])),
            "51.77,19.46|r=150|m=3.0"
        );
    }

    mod http_tests {
        use super::*;
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[tokio::test]
        async fn the_query_reaches_the_server_and_the_fixture_is_parsed() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/q"))
                .and(query_param("format", "json"))
                .and(query_param("minmag", "2.0"))
                .and(query_param("maxradius", "2.698"))
                .and(query_param("start", "2026-11-22T06:40:00"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(FIXTURE.to_vec(), "application/json"),
                )
                .mount(&server)
                .await;
            let src = Emsc::new(&format!("{}/q", server.uri())).unwrap();
            let out = src.fetch(&strong_ctx()).await.unwrap();
            assert_eq!(out.items.len(), 1);
            assert_eq!(src.id(), "emsc");
            assert_eq!(src.needs(), Needs::Position);
            assert_eq!(src.default_interval(), Duration::from_secs(120));
        }

        #[tokio::test]
        async fn a_server_error_is_a_status_error() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&server)
                .await;
            let src = Emsc::new(&format!("{}/q", server.uri())).unwrap();
            let err = src.fetch(&strong_ctx()).await.err().unwrap();
            assert!(matches!(err, SourceError::Status(503)), "{err:?}");
        }

        #[tokio::test]
        async fn no_position_is_a_parse_error_not_a_panic() {
            let src = Emsc::new("http://127.0.0.1:1/q").unwrap();
            let mut c = strong_ctx();
            c.position = None;
            assert!(matches!(src.fetch(&c).await, Err(SourceError::Parse(_))));
        }
    }
}
