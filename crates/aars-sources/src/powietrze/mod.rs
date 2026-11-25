//! Moduł `powietrze`: jakość powietrza dla pozycji stacji z Open-Meteo Air
//! Quality (model CAMS, licencja CC BY 4.0, dane: Open-Meteo, CAMS) i część
//! biuletynu „Jakość powietrza”. Źródło niczego samo nie ogłasza — włączone
//! odświeża dane w tle; część biuletynu pobiera je też sama, gdy pamięć jest
//! pusta albo stara.

pub mod parse;
pub mod speech;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use aars_core::module::{ModuleDescriptor, ModuleKind, Needs, SettingKind, SettingSpec, Toggle};
use chrono::{DateTime, TimeDelta, Utc};

use crate::bulletin::{BulletinPart, PartCtx, PartFuture, PartText};
use crate::http;
use crate::source::{DataSource, FetchFuture, SourceCtx, SourceError, SourceOutput};
pub use parse::{AirHour, AirQuality, Pollen};

pub const ID: &str = "powietrze";
pub const PART_ID: &str = "powietrze";
const MAX_BODY: usize = 128 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
/// Dane starsze niż to nie są używane przez część biuletynu.
const FRESH: TimeDelta = TimeDelta::minutes(60);

const YES_NO: &[(&str, &str)] = &[("tak", "Tak"), ("nie", "Nie")];

pub const DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
    id: ID,
    name: "Jakość powietrza (Open-Meteo, CAMS)",
    description: "Dane do klocka biuletynu „Jakość powietrza”: europejski indeks jakości powietrza dla pozycji stacji z prognozą na dobę i pyłki (model CAMS, dane: Open-Meteo, CAMS, licencja CC BY 4.0). Samo nie nadaje komunikatów; biuletyn pobiera dane także przy wyłączonym module, włączenie odświeża je w tle.",
    kind: ModuleKind::Source,
    toggle: Toggle::Switchable,
    default_enabled: false,
    needs: Needs::Position,
    auto_area: None,
    settings: &[
        SettingSpec {
            key: "pollen",
            label: "Pyłki",
            help: "Mówić o średnim i wysokim stężeniu pyłków (olcha, brzoza, trawy, bylica, ambrozja, oliwka). Dane tylko w sezonie pylenia.",
            kind: SettingKind::Choice(YES_NO),
            default: "tak",
        },
        SettingSpec {
            key: "interval_s",
            label: "Interwał odpytywania (s)",
            help: "Jak często odświeżać dane w tle, gdy moduł jest włączony.",
            kind: SettingKind::Integer {
                min: 900,
                max: 3600,
            },
            default: "1800",
        },
    ],
    see_also: Some(("Klocki biuletynu", "/bulletin")),
};

/// Pyłki z ustawień źródła; brak albo inna wartość niż „nie” → tak.
pub fn pollen_wanted(settings: &BTreeMap<String, String>) -> bool {
    settings.get("pollen").map(|v| v.trim()) != Some("nie")
}

/// Klucz pamięci: pozycja w setnych stopnia i to, czy pytano o pyłki.
type Key = (i64, i64, bool);

fn key(lat: f64, lon: f64, pollen: bool) -> Key {
    (
        (lat * 100.0).round() as i64,
        (lon * 100.0).round() as i64,
        pollen,
    )
}

struct Cached {
    data: AirQuality,
    fetched_at: DateTime<Utc>,
    key: Key,
}

pub struct Powietrze {
    client: reqwest::Client,
    url: String,
    cache: Mutex<Option<Cached>>,
}

impl Powietrze {
    pub fn new(cfg: &aars_core::config::SourcesConfig) -> Result<Self, SourceError> {
        Self::with_url(&cfg.open_meteo_aq_url, DEFAULT_TIMEOUT)
    }

    pub fn with_url(url: &str, timeout: Duration) -> Result<Self, SourceError> {
        Ok(Self {
            client: http::client(timeout)?,
            url: url.to_owned(),
            cache: Mutex::new(None),
        })
    }

    fn recall(&self, now: DateTime<Utc>, key: Key) -> Option<AirQuality> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .as_ref()
            .filter(|c| c.key == key && now - c.fetched_at <= FRESH && c.fetched_at <= now + FRESH)
            .map(|c| c.data.clone())
    }

    fn remember(&self, data: &AirQuality, now: DateTime<Utc>, key: Key) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        *cache = Some(Cached {
            data: data.clone(),
            fetched_at: now,
            key,
        });
    }

    async fn download(
        &self,
        lat: f64,
        lon: f64,
        pollen: bool,
        now: DateTime<Utc>,
    ) -> Result<AirQuality, SourceError> {
        let url = parse::url(&self.url, lat, lon, pollen);
        let body = http::get_limited(&self.client, &url, MAX_BODY).await?;
        let data = parse::parse(&body)?;
        self.remember(&data, now, key(lat, lon, pollen));
        Ok(data)
    }
}

impl DataSource for Powietrze {
    fn id(&self) -> &'static str {
        ID
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(1800)
    }

    fn needs(&self) -> Needs {
        Needs::Position
    }

    fn scope(&self, ctx: &SourceCtx) -> String {
        let pollen = pollen_wanted(&ctx.settings);
        match &ctx.position {
            Some(pos) => format!("{:.2},{:.2}|pyłki={pollen}", pos.lat, pos.lon),
            None => format!("brak pozycji|pyłki={pollen}"),
        }
    }

    /// Tylko odświeża pamięć dla biuletynu; komunikatów nie ma.
    fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a> {
        Box::pin(async move {
            let pos = ctx
                .position
                .as_ref()
                .ok_or_else(|| SourceError::Parse("brak pozycji stacji".to_owned()))?;
            self.download(pos.lat, pos.lon, pollen_wanted(&ctx.settings), ctx.now)
                .await?;
            Ok(SourceOutput {
                items: Vec::new(),
                complete: true,
            })
        })
    }
}

impl BulletinPart for Powietrze {
    fn id(&self) -> &'static str {
        PART_ID
    }

    fn max_chars(&self) -> usize {
        400
    }

    fn compose<'a>(&'a self, ctx: &'a PartCtx) -> PartFuture<'a> {
        Box::pin(async move {
            let Some(pos) = ctx.position.as_ref() else {
                return Ok(PartText::Empty("brak pozycji stacji"));
            };
            let pollen = pollen_wanted(&ctx.source_settings(ID));
            let data = match self.recall(ctx.now, key(pos.lat, pos.lon, pollen)) {
                Some(d) => d,
                None => self.download(pos.lat, pos.lon, pollen, ctx.now).await?,
            };
            Ok(match speech::text(&data, ctx.now, pollen) {
                Some(text) => PartText::Text(text),
                None => PartText::Empty("brak danych o jakości powietrza"),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::geo::StationPosition;
    use aars_core::source_state::AlertSourceHealth;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FIX: &[u8] =
        include_bytes!("../../tests/fixtures/powietrze/open-meteo-aq-rzeszow-2026-11-25.json");

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn part_ctx(now: &str, position: bool, sources: &[(&str, &str)]) -> PartCtx {
        PartCtx {
            now: at(now),
            position: position.then(|| StationPosition {
                lat: 50.04,
                lon: 21.99,
                locator: "KN20".to_owned(),
            }),
            settings: BTreeMap::new(),
            sources: sources
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            active_alerts: Vec::new(),
            alert_sources: AlertSourceHealth::default(),
            warnings_max_chars: None,
        }
    }

    #[test]
    fn the_descriptor_has_the_documented_settings() {
        let d = &DESCRIPTOR;
        assert_eq!(d.id, "powietrze");
        assert_eq!(d.needs, Needs::Position);
        assert!(!d.default_enabled);
        assert!(d.description.contains("CC BY 4.0"));
        let keys: Vec<_> = d.settings.iter().map(|s| (s.key, s.default)).collect();
        assert_eq!(keys, [("pollen", "tak"), ("interval_s", "1800")]);
        let mut input = BTreeMap::new();
        input.insert("pollen".to_owned(), "nie".to_owned());
        input.insert("interval_s".to_owned(), "900".to_owned());
        assert!(d.validate(&input).is_ok());
        input.insert("pollen".to_owned(), "może".to_owned());
        assert!(d.validate(&input).is_err());
    }

    #[test]
    fn pollen_is_on_unless_switched_off() {
        let m = |v: &str| BTreeMap::from([("pollen".to_owned(), v.to_owned())]);
        assert!(pollen_wanted(&BTreeMap::new()));
        assert!(pollen_wanted(&m("tak")));
        assert!(!pollen_wanted(&m("nie")));
    }

    async fn server(pollen: bool, times: u64) -> MockServer {
        let server = MockServer::start().await;
        let hourly = parse::url("", 0.0, 0.0, pollen);
        let hourly = hourly
            .split('&')
            .find_map(|kv| kv.strip_prefix("hourly="))
            .unwrap()
            .to_owned();
        Mock::given(method("GET"))
            .and(path("/v1/air-quality"))
            .and(query_param("latitude", "50.0400"))
            .and(query_param("longitude", "21.9900"))
            .and(query_param("hourly", hourly.as_str()))
            .and(query_param("timezone", "GMT"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(FIX, "application/json"))
            .expect(times)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn the_part_fetches_once_and_reuses_fresh_data() {
        let server = server(true, 1).await;
        let p = Powietrze::with_url(
            &format!("{}/v1/air-quality", server.uri()),
            Duration::from_secs(5),
        )
        .unwrap();
        let want = "Jakość powietrza umiarkowana. Dziś wieczorem dostateczna, pył zawieszony PM2,5 do 68 mikrogramów na metr sześcienny.";
        for now in ["2026-11-25T08:24:00Z", "2026-11-25T08:50:00Z"] {
            let got = p.compose(&part_ctx(now, true, &[])).await.unwrap();
            assert_eq!(got, PartText::Text(want.to_owned()), "{now}");
        }
        assert_eq!(BulletinPart::id(&p), "powietrze");
    }

    #[tokio::test]
    async fn the_pollen_setting_changes_the_question() {
        let server = server(false, 1).await;
        let p = Powietrze::with_url(
            &format!("{}/v1/air-quality", server.uri()),
            Duration::from_secs(5),
        )
        .unwrap();
        let ctx = part_ctx("2026-11-25T08:24:00Z", true, &[("powietrze:pollen", "nie")]);
        assert!(matches!(p.compose(&ctx).await.unwrap(), PartText::Text(_)));
    }

    #[tokio::test]
    async fn without_a_position_or_data_the_part_is_empty() {
        let p = Powietrze::with_url("http://127.0.0.1:1/x", Duration::from_secs(1)).unwrap();
        assert_eq!(
            p.compose(&part_ctx("2026-11-25T08:24:00Z", false, &[]))
                .await
                .unwrap(),
            PartText::Empty("brak pozycji stacji")
        );
        // Dane sprzed doby: nic bieżącego do powiedzenia.
        let server = server(true, 1).await;
        let p = Powietrze::with_url(
            &format!("{}/v1/air-quality", server.uri()),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            p.compose(&part_ctx("2026-11-27T08:24:00Z", true, &[]))
                .await
                .unwrap(),
            PartText::Empty("brak danych o jakości powietrza")
        );
    }

    #[tokio::test]
    async fn a_server_error_is_a_failure_not_silence() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string(r#"{"error":true,"reason":"x"}"#),
            )
            .mount(&server)
            .await;
        let p = Powietrze::with_url(
            &format!("{}/v1/air-quality", server.uri()),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(
            p.compose(&part_ctx("2026-11-25T08:24:00Z", true, &[]))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_source_refreshes_the_part_cache_without_items() {
        let server = server(true, 1).await;
        let p = Powietrze::with_url(
            &format!("{}/v1/air-quality", server.uri()),
            Duration::from_secs(5),
        )
        .unwrap();
        let ctx = SourceCtx {
            now: at("2026-11-25T08:20:00Z"),
            area: Vec::new(),
            area_text: String::new(),
            position: Some(StationPosition {
                lat: 50.04,
                lon: 21.99,
                locator: "KN20".to_owned(),
            }),
            settings: BTreeMap::new(),
            tracked_keys: Default::default(),
            tracked: Default::default(),
        };
        assert_eq!(p.scope(&ctx), "50.04,21.99|pyłki=true");
        let out = p.fetch(&ctx).await.unwrap();
        assert!(out.items.is_empty() && out.complete);
        // Część bierze dane z pamięci (serwer oczekuje jednego zapytania).
        assert!(matches!(
            p.compose(&part_ctx("2026-11-25T08:24:00Z", true, &[]))
                .await
                .unwrap(),
            PartText::Text(_)
        ));
    }
}
