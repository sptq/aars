//! Źródło `pogoda`: alerty progowe z prognozy Open-Meteo i część biuletynu
//! (z zapasem MET Norway) — jedna instancja, jedna pamięć prognozy.

pub mod alerts;
pub mod speech;

use std::sync::Mutex;
use std::time::Duration;

use aars_core::module::{ModuleDescriptor, ModuleKind, Needs, SettingKind, SettingSpec, Toggle};
use chrono::{DateTime, TimeDelta, Utc};

use crate::bulletin::{BulletinPart, PartCtx, PartFuture, PartText};
use crate::forecast::{Forecast, met_no, open_meteo};
use crate::http;
use crate::source::{DataSource, FetchFuture, SourceCtx, SourceError};

pub const ID: &str = "pogoda";
const OPEN_METEO_MAX_BODY: usize = 256 * 1024;
const MET_MAX_BODY: usize = 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
/// Prognoza starsza niż to nie jest używana przez część biuletynu.
const FRESH: TimeDelta = TimeDelta::minutes(60);

pub const DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
    id: ID,
    name: "Pogoda (Open-Meteo)",
    description: "Komunikat, gdy prognoza dla pozycji stacji przekroczy progi: porywy, mróz, opady. Biuletyn korzysta z tej samej prognozy.",
    kind: ModuleKind::Source,
    toggle: Toggle::Switchable,
    default_enabled: false,
    needs: Needs::Position,
    settings: &[
        SettingSpec {
            key: "gust_kmh",
            label: "Porywy wiatru od (km/h)",
            help: "Porywy od tej wartości w prognozie na dziś lub jutro są ogłaszane.",
            kind: SettingKind::Integer { min: 40, max: 150 },
            default: "75",
        },
        SettingSpec {
            key: "frost_c",
            label: "Mróz od (°C)",
            help: "Temperatura minimalna równa tej wartości lub niższa jest ogłaszana.",
            kind: SettingKind::Integer { min: -40, max: 0 },
            default: "-15",
        },
        SettingSpec {
            key: "precip_mm",
            label: "Suma opadów dobowych od (mm)",
            help: "Dobowa suma opadów od tej wartości jest ogłaszana.",
            kind: SettingKind::Integer { min: 5, max: 200 },
            default: "30",
        },
        SettingSpec {
            key: "interval_s",
            label: "Interwał odpytywania (s)",
            help: "Jak często odpytywać źródło.",
            kind: SettingKind::Integer {
                min: 900,
                max: 3600,
            },
            default: "1800",
        },
    ],
    see_also: None,
};

/// Prognoza w pamięci; `pos` to pozycja stacji w setnych stopnia.
#[derive(Debug, Clone)]
pub struct Cached {
    pub forecast: Forecast,
    pub fetched_at: DateTime<Utc>,
    pub pos: (i64, i64),
}

fn pos_key(lat: f64, lon: f64) -> (i64, i64) {
    ((lat * 100.0).round() as i64, (lon * 100.0).round() as i64)
}

pub struct Pogoda {
    open_meteo_client: reqwest::Client,
    met_client: reqwest::Client,
    open_meteo_url: String,
    met_no_url: String,
    cache: Mutex<Option<Cached>>,
}

impl Pogoda {
    pub fn new(cfg: &aars_core::config::SourcesConfig) -> Result<Self, SourceError> {
        Self::with_urls(&cfg.open_meteo_url, &cfg.met_no_url, DEFAULT_TIMEOUT)
    }

    pub fn with_urls(
        open_meteo: &str,
        met_no: &str,
        timeout: Duration,
    ) -> Result<Self, SourceError> {
        Ok(Self {
            open_meteo_client: http::client(timeout)?,
            met_client: http::client_with_agent(timeout, http::MET_USER_AGENT)?,
            open_meteo_url: open_meteo.to_owned(),
            met_no_url: met_no.to_owned(),
            cache: Mutex::new(None),
        })
    }

    fn remember(&self, forecast: &Forecast, now: DateTime<Utc>, pos: (i64, i64)) {
        // Blokada tylko wokół zapisu, nigdy przez await; zatruty Mutex odzyskujemy.
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        *cache = Some(Cached {
            forecast: forecast.clone(),
            fetched_at: now,
            pos,
        });
    }

    fn recall(&self, now: DateTime<Utc>, pos: (i64, i64)) -> Option<Forecast> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .as_ref()
            .filter(|c| c.pos == pos && now - c.fetched_at <= FRESH)
            .map(|c| c.forecast.clone())
    }

    async fn ask_open_meteo(
        &self,
        lat: f64,
        lon: f64,
        now: DateTime<Utc>,
    ) -> Result<Forecast, SourceError> {
        let url = open_meteo::url(&self.open_meteo_url, lat, lon);
        let body = http::get_limited(&self.open_meteo_client, &url, OPEN_METEO_MAX_BODY).await?;
        open_meteo::parse(&body, now)
    }

    async fn ask_met(
        &self,
        lat: f64,
        lon: f64,
        now: DateTime<Utc>,
    ) -> Result<Forecast, SourceError> {
        let url = met_no::url(&self.met_no_url, lat, lon);
        let body = http::get_limited(&self.met_client, &url, MET_MAX_BODY).await?;
        met_no::parse(&body, now)
    }
}

fn threshold(ctx: &SourceCtx, key: &str, default: i64, min: i64, max: i64) -> i64 {
    ctx.settings
        .get(key)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| (min..=max).contains(n))
        .unwrap_or(default)
}

/// Progi (porywy km/h, mróz °C, opady mm) z ustawień; błędna wartość → domyślna.
pub(crate) fn thresholds(ctx: &SourceCtx) -> (i64, i64, i64) {
    (
        threshold(ctx, "gust_kmh", 75, 40, 150),
        threshold(ctx, "frost_c", -15, -40, 0),
        threshold(ctx, "precip_mm", 30, 5, 200),
    )
}

impl DataSource for Pogoda {
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
        let (g, f, p) = thresholds(ctx);
        match &ctx.position {
            Some(pos) => format!("{:.2},{:.2}|g={g}|f={f}|p={p}", pos.lat, pos.lon),
            None => format!("brak pozycji|g={g}|f={f}|p={p}"),
        }
    }

    fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a> {
        Box::pin(async move {
            let pos = ctx
                .position
                .as_ref()
                .ok_or_else(|| SourceError::Parse("brak pozycji stacji".to_owned()))?;
            let forecast = self.ask_open_meteo(pos.lat, pos.lon, ctx.now).await?;
            self.remember(&forecast, ctx.now, pos_key(pos.lat, pos.lon));
            Ok(alerts::to_output(&forecast, ctx))
        })
    }
}

impl BulletinPart for Pogoda {
    fn id(&self) -> &'static str {
        ID
    }

    fn max_chars(&self) -> usize {
        450
    }

    fn compose<'a>(&'a self, ctx: &'a PartCtx) -> PartFuture<'a> {
        Box::pin(async move {
            let Some(pos) = ctx.position.as_ref() else {
                return Ok(PartText::Empty("brak pozycji stacji"));
            };
            let key = pos_key(pos.lat, pos.lon);
            let forecast = match self.recall(ctx.now, key) {
                Some(f) => f,
                None => {
                    let forecast = match self.ask_open_meteo(pos.lat, pos.lon, ctx.now).await {
                        Ok(f) => f,
                        Err(_) => self.ask_met(pos.lat, pos.lon, ctx.now).await?,
                    };
                    self.remember(&forecast, ctx.now, key);
                    forecast
                }
            };
            Ok(match speech::bulletin(&forecast, ctx.now) {
                Some(text) => PartText::Text(text),
                None => PartText::Empty("brak prognozy"),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech::assert_speakable;
    use aars_core::geo::StationPosition;
    use std::collections::BTreeMap;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const OPEN_METEO: &[u8] =
        include_bytes!("../../tests/fixtures/pogoda/open-meteo-rzeszow-2026-11-22.json");
    const MET: &[u8] = include_bytes!("../../tests/fixtures/pogoda/met-no-rzeszow-2026-11-22.json");

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn position(lat: f64, lon: f64) -> StationPosition {
        StationPosition {
            lat,
            lon,
            locator: "KN00".to_owned(),
        }
    }

    fn ctx_at(now: &str, settings: &[(&str, &str)]) -> SourceCtx {
        SourceCtx {
            now: at(now),
            area: Vec::new(),
            area_text: String::new(),
            position: Some(position(50.04, 21.99)),
            settings: settings
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            tracked_keys: Default::default(),
            tracked: Default::default(),
        }
    }

    fn open_meteo_forecast() -> Forecast {
        open_meteo::parse(OPEN_METEO, at("2026-11-22T18:50:00Z")).unwrap()
    }

    /// Fiks z podmienioną wartością w `daily`.
    fn edited(edit: impl FnOnce(&mut serde_json::Value)) -> Forecast {
        let mut v: serde_json::Value = serde_json::from_slice(OPEN_METEO).unwrap();
        edit(&mut v);
        open_meteo::parse(&serde_json::to_vec(&v).unwrap(), at("2026-11-22T18:50:00Z")).unwrap()
    }

    fn item<'a>(out: &'a crate::source::SourceOutput, key: &str) -> &'a crate::source::SourceItem {
        out.items
            .iter()
            .find(|i| i.key == key)
            .unwrap_or_else(|| panic!("brak {key}"))
    }

    #[test]
    fn the_descriptor_has_the_documented_settings() {
        let d = &DESCRIPTOR;
        assert_eq!((d.id, d.name), ("pogoda", "Pogoda (Open-Meteo)"));
        assert_eq!(d.needs, Needs::Position);
        assert!(!d.default_enabled);
        let keys: Vec<_> = d.settings.iter().map(|s| (s.key, s.default)).collect();
        assert_eq!(
            keys,
            [
                ("gust_kmh", "75"),
                ("frost_c", "-15"),
                ("precip_mm", "30"),
                ("interval_s", "1800")
            ]
        );
    }

    #[test]
    fn the_quiet_fixture_has_no_alerts_at_default_thresholds() {
        let out = alerts::to_output(&open_meteo_forecast(), &ctx_at("2026-11-22T18:50:00Z", &[]));
        assert!(out.items.is_empty());
        assert!(out.complete);
    }

    #[test]
    fn gusts_over_the_threshold_are_one_item_per_day() {
        let f = open_meteo_forecast();
        // 11:50 lokalnie: dziś liczy się od 11:00, a 12:00 ma porywy 40,3 km/h.
        let out = alerts::to_output(&f, &ctx_at("2026-11-22T10:50:00Z", &[("gust_kmh", "40")]));
        assert_eq!(out.items.len(), 2);
        let today = item(&out, "gust:2026-11-22");
        assert_eq!(
            today.text,
            "Prognoza pogody: dziś porywy wiatru do 40 km/h. Dane: Open-Meteo."
        );
        assert_eq!(
            today.change_text,
            "Zmiana prognozy: dziś porywy wiatru do 40 km/h. Dane: Open-Meteo."
        );
        assert_eq!(today.fingerprint, "gust:40");
        assert_eq!(today.topic, "gust:2026-11-22");
        assert_eq!(today.valid_until, Some(at("2026-11-22T23:00:00Z")));
        assert_eq!(today.valid_from, None);
        assert_eq!(today.summary, "porywy wiatru do 40 km/h, 22 listopada");
        assert_eq!(today.cancel_text, "");
        assert_eq!(today.priority, aars_core::priority::Priority::Normal);
        assert!(!today.interrupting);
        let tomorrow = item(&out, "gust:2026-11-23");
        assert_eq!(
            tomorrow.text,
            "Prognoza pogody: jutro porywy wiatru do 45 km/h. Dane: Open-Meteo."
        );
        assert_eq!(tomorrow.fingerprint, "gust:40");
        assert_eq!(tomorrow.valid_until, Some(at("2026-11-23T23:00:00Z")));

        let out = alerts::to_output(&f, &ctx_at("2026-11-22T10:50:00Z", &[("gust_kmh", "41")]));
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].key, "gust:2026-11-23");
        // Wieczorem poranne porywy już minęły: zostaje tylko jutro.
        let out = alerts::to_output(&f, &ctx_at("2026-11-22T18:50:00Z", &[("gust_kmh", "40")]));
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].key, "gust:2026-11-23");
    }

    #[test]
    fn frost_and_rain_use_their_steps() {
        let ctx = ctx_at("2026-11-22T18:50:00Z", &[]);
        let f = edited(|v| v["daily"]["temperature_2m_min"][1] = (-17.3).into());
        let out = alerts::to_output(&f, &ctx);
        let frost = item(&out, "frost:2026-11-23");
        assert_eq!(frost.fingerprint, "frost:-15");
        assert!(
            frost.text.contains("jutro spadek temperatury do -17 °C."),
            "{}",
            frost.text
        );
        assert_eq!(frost.summary, "spadek temperatury do -17 °C, 23 listopada");

        let f = edited(|v| v["daily"]["temperature_2m_min"][1] = (-21.0).into());
        assert_eq!(
            item(&alerts::to_output(&f, &ctx), "frost:2026-11-23").fingerprint,
            "frost:-20"
        );

        // Dziś: suma opadów z godzin od bieżącej (19:00 lokalnie) do końca doby.
        let f = edited(|v| {
            v["hourly"]["precipitation"][20] = 30.0.into();
            v["hourly"]["precipitation"][23] = 5.2.into();
        });
        let out = alerts::to_output(&f, &ctx);
        let rain = item(&out, "precip:2026-11-22");
        assert_eq!(rain.fingerprint, "precip:30");
        assert!(rain.text.contains("dziś opady do 35 mm."), "{}", rain.text);
        assert_eq!(rain.summary, "opady do 35 mm, 22 listopada");

        assert_eq!(speech::precip_text(7.26), "7,3");
        assert_eq!(speech::precip_text(35.2), "35");
        assert_eq!(speech::precip_text(2.0), "2");
    }

    #[test]
    fn in_the_evening_the_morning_minimum_is_not_announced() {
        // Mróz o 6:00 rano (dobowe minimum też −18 °C), wieczorem już minął.
        let f = edited(|v| {
            v["hourly"]["temperature_2m"][6] = (-18.0).into();
            v["daily"]["temperature_2m_min"][0] = (-18.0).into();
        });
        let evening = alerts::to_output(&f, &ctx_at("2026-11-22T18:50:00Z", &[]));
        assert!(
            evening.items.iter().all(|i| i.key != "frost:2026-11-22"),
            "{:?}",
            evening.items
        );
        let dawn = alerts::to_output(&f, &ctx_at("2026-11-22T04:10:00Z", &[]));
        let frost = item(&dawn, "frost:2026-11-22");
        assert_eq!(frost.fingerprint, "frost:-15");
        assert!(
            frost.text.contains("dziś spadek temperatury do -18 °C."),
            "{}",
            frost.text
        );
    }

    #[test]
    fn the_current_hour_still_counts_for_today() {
        let f = edited(|v| {
            v["hourly"]["wind_gusts_10m"][18] = 90.0.into();
            v["hourly"]["wind_gusts_10m"][19] = 80.0.into();
        });
        // 19:30 lokalnie: 19:00 się liczy, 18:00 już nie.
        let out = alerts::to_output(&f, &ctx_at("2026-11-22T18:30:00Z", &[]));
        let gust = item(&out, "gust:2026-11-22");
        assert!(
            gust.text.contains("dziś porywy wiatru do 80 km/h."),
            "{}",
            gust.text
        );
        assert_eq!(gust.fingerprint, "gust:80");
    }

    fn ctx_tracked(now: &str, tracked: &[(&str, &str)]) -> SourceCtx {
        let mut ctx = ctx_at(now, &[]);
        ctx.tracked = tracked
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        ctx
    }

    #[test]
    fn a_weaker_value_today_keeps_the_announced_fingerprint() {
        let now = "2026-11-22T07:00:00Z";
        let f = edited(|v| v["hourly"]["wind_gusts_10m"][10] = 85.0.into());
        // Bez śledzenia: odcisk z bieżącej wartości.
        let free = alerts::to_output(&f, &ctx_at(now, &[]));
        assert_eq!(item(&free, "gust:2026-11-22").fingerprint, "gust:80");
        let held = alerts::to_output(&f, &ctx_tracked(now, &[("gust:2026-11-22", "gust:90")]));
        assert_eq!(item(&held, "gust:2026-11-22").fingerprint, "gust:90");

        // Pod progiem: pozycja zostaje, z odciskiem i wartością kubełka.
        let f = edited(|v| v["hourly"]["wind_gusts_10m"][10] = 60.0.into());
        let out = alerts::to_output(&f, &ctx_tracked(now, &[("gust:2026-11-22", "gust:80")]));
        let gust = item(&out, "gust:2026-11-22");
        assert_eq!(gust.fingerprint, "gust:80");
        assert_eq!(gust.topic, "gust:2026-11-22");
        assert!(gust.text.contains("dziś porywy wiatru do 80 km/h."), "{}", gust.text);
        assert_eq!(gust.valid_until, Some(at("2026-11-22T23:00:00Z")));

        let out = alerts::to_output(&f, &ctx_tracked(now, &[("frost:2026-11-22", "frost:-15")]));
        let frost = item(&out, "frost:2026-11-22");
        assert_eq!(frost.fingerprint, "frost:-15");
        assert!(frost.text.contains("do -15 °C."), "{}", frost.text);
    }

    #[test]
    fn a_stronger_value_today_is_a_new_fingerprint() {
        let now = "2026-11-22T07:00:00Z";
        let f = edited(|v| {
            v["hourly"]["wind_gusts_10m"][10] = 95.0.into();
            v["hourly"]["temperature_2m"][10] = (-21.0).into();
        });
        let out = alerts::to_output(
            &f,
            &ctx_tracked(now, &[("gust:2026-11-22", "gust:80"), ("frost:2026-11-22", "frost:-15")]),
        );
        assert_eq!(item(&out, "gust:2026-11-22").fingerprint, "gust:90");
        assert_eq!(item(&out, "frost:2026-11-22").fingerprint, "frost:-20");
    }

    #[test]
    fn tomorrow_is_not_held() {
        let out = alerts::to_output(
            &open_meteo_forecast(),
            &ctx_tracked("2026-11-22T07:00:00Z", &[("gust:2026-11-23", "gust:90")]),
        );
        assert!(out.items.iter().all(|i| i.key != "gust:2026-11-23"), "{:?}", out.items);
    }

    #[test]
    fn without_hourly_data_today_has_no_alerts() {
        let f = edited(|v| {
            v.as_object_mut().unwrap().remove("hourly");
            v["daily"]["wind_gusts_10m_max"][0] = 99.0.into();
            v["daily"]["wind_gusts_10m_max"][1] = 99.0.into();
        });
        let out = alerts::to_output(&f, &ctx_at("2026-11-22T10:50:00Z", &[]));
        let keys: Vec<_> = out.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["gust:2026-11-23"]);
    }

    #[test]
    fn crossing_midnight_keeps_key_and_fingerprint() {
        let f = open_meteo_forecast();
        let before = alerts::to_output(&f, &ctx_at("2026-11-22T22:50:00Z", &[("gust_kmh", "41")]));
        let after = alerts::to_output(&f, &ctx_at("2026-11-22T23:10:00Z", &[("gust_kmh", "41")]));
        let (a, b) = (
            item(&before, "gust:2026-11-23"),
            item(&after, "gust:2026-11-23"),
        );
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.fingerprint, "gust:40");
        assert!(a.text.contains("jutro") && b.text.contains("dziś"));
        assert_eq!(a.text.replace("jutro", "dziś"), b.text);
    }

    #[test]
    fn invalid_thresholds_fall_back_to_the_defaults() {
        let f = open_meteo_forecast();
        let out = alerts::to_output(
            &f,
            &ctx_at(
                "2026-11-22T18:50:00Z",
                &[("gust_kmh", "abc"), ("frost_c", "99")],
            ),
        );
        assert!(out.items.is_empty());
    }

    #[test]
    fn the_scope_carries_position_and_thresholds() {
        let src = Pogoda::with_urls(
            "http://127.0.0.1:9/a",
            "http://127.0.0.1:9/b",
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            src.scope(&ctx_at("2026-11-22T18:50:00Z", &[])),
            "50.04,21.99|g=75|f=-15|p=30"
        );
        assert_eq!(
            (DataSource::id(&src), src.needs(), src.default_interval()),
            ("pogoda", Needs::Position, Duration::from_secs(1800))
        );
    }

    #[test]
    fn the_bulletin_text_after_two_pm() {
        let now = at("2026-11-22T18:50:00Z");
        assert_eq!(
            speech::bulletin(&open_meteo_forecast(), now).unwrap(),
            "Pogoda dla okolic stacji. Teraz 2 °C, bezchmurnie. W nocy do 1 °C. Jutro śnieg, do 3 °C, opady do 2,4 mm, porywy wiatru do 45 km/h. Dane: Open-Meteo."
        );
    }

    #[test]
    fn the_bulletin_text_before_two_pm() {
        let now = at("2026-11-22T09:00:00Z");
        assert_eq!(
            speech::bulletin(&open_meteo_forecast(), now).unwrap(),
            "Pogoda dla okolic stacji. Teraz 2 °C, bezchmurnie. Dziś mżawka, do 6 °C, opady do 1,2 mm, porywy wiatru do 40 km/h. W nocy do 1 °C. Dane: Open-Meteo."
        );
    }

    #[test]
    fn the_met_fallback_speaks_mean_wind() {
        let now = at("2026-11-22T19:00:00Z");
        let f = met_no::parse(MET, now).unwrap();
        assert_eq!(
            speech::bulletin(&f, now).unwrap(),
            "Pogoda dla okolic stacji. Teraz -1 °C, małe zachmurzenie. W nocy do -2 °C. Jutro śnieg, do 2 °C, opady do 4,6 mm, wiatr do 26 km/h. Dane: MET Norway."
        );
    }

    #[test]
    fn gaps_are_left_out_not_invented() {
        let f = edited(|v| {
            v["daily"]["precipitation_sum"][1] = serde_json::Value::Null;
            v["daily"]["wind_gusts_10m_max"][1] = serde_json::Value::Null;
            v["daily"]["weather_code"][1] = serde_json::Value::Null;
            v["current"]["weather_code"] = serde_json::Value::Null;
        });
        assert_eq!(
            speech::bulletin(&f, at("2026-11-22T18:50:00Z")).unwrap(),
            "Pogoda dla okolic stacji. Teraz 2 °C. W nocy do 1 °C. Jutro do 3 °C. Dane: Open-Meteo."
        );
        let mut empty = open_meteo_forecast();
        empty.current = None;
        empty.days.clear();
        assert_eq!(speech::bulletin(&empty, at("2026-11-22T18:50:00Z")), None);
    }

    #[test]
    fn the_bulletin_texts_are_speakable() {
        let evening = at("2026-11-22T18:50:00Z");
        let morning = at("2026-11-22T09:00:00Z");
        let met_now = at("2026-11-22T19:00:00Z");
        let a = speech::bulletin(&open_meteo_forecast(), evening).unwrap();
        let b = speech::bulletin(&open_meteo_forecast(), morning).unwrap();
        let c = speech::bulletin(&met_no::parse(MET, met_now).unwrap(), met_now).unwrap();
        assert_speakable(&[&a, &b, &c]);
        let spoken = aars_audio::text::normalize(&a);
        assert!(spoken.contains("Teraz dwa stopnie Celsjusza"), "{spoken}");
        assert!(
            spoken.contains("do czterdziestu pięciu kilometrów na godzinę"),
            "{spoken}"
        );
    }

    mod http_tests {
        use super::*;

        fn ok(b: &[u8]) -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_raw(b.to_vec(), "application/json")
        }

        fn source(server: &MockServer) -> Pogoda {
            Pogoda::with_urls(
                &format!("{}/v1/forecast", server.uri()),
                &format!("{}/compact", server.uri()),
                Duration::from_secs(5),
            )
            .unwrap()
        }

        fn part_ctx(now: &str, pos: Option<StationPosition>) -> PartCtx {
            PartCtx {
                now: at(now),
                position: pos,
                settings: BTreeMap::new(),
                active_alerts: Vec::new(),
                alert_sources: Default::default(),
            }
        }

        #[tokio::test]
        async fn the_source_asks_open_meteo_for_the_station_position() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/forecast"))
                .and(query_param("latitude", "50.0400"))
                .and(query_param("longitude", "21.9900"))
                .and(query_param("timezone", "Europe/Warsaw"))
                .and(query_param("forecast_days", "3"))
                .respond_with(ok(OPEN_METEO))
                .expect(1)
                .mount(&server)
                .await;
            let out = source(&server)
                .fetch(&ctx_at("2026-11-22T18:50:00Z", &[]))
                .await
                .unwrap();
            assert!(out.items.is_empty() && out.complete);
        }

        #[tokio::test]
        async fn the_part_falls_back_to_met_with_its_user_agent() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/forecast"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/compact"))
                .and(header("user-agent", http::MET_USER_AGENT))
                .and(query_param("lat", "50.0400"))
                .and(query_param("lon", "21.9900"))
                .respond_with(ok(MET))
                .expect(1)
                .mount(&server)
                .await;
            let src = source(&server);
            let PartText::Text(text) = src
                .compose(&part_ctx(
                    "2026-11-22T19:00:00Z",
                    Some(position(50.04, 21.99)),
                ))
                .await
                .unwrap()
            else {
                panic!("pusta część");
            };
            assert!(text.ends_with("Dane: MET Norway."), "{text}");

            server.reset().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&server)
                .await;
            let fresh = source(&server);
            let err = fresh
                .compose(&part_ctx(
                    "2026-11-22T19:00:00Z",
                    Some(position(50.04, 21.99)),
                ))
                .await
                .unwrap_err();
            assert!(matches!(err, SourceError::Status(503)), "{err}");
        }

        #[tokio::test]
        async fn the_part_reuses_a_fresh_forecast() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/forecast"))
                .respond_with(ok(OPEN_METEO))
                .expect(2)
                .mount(&server)
                .await;
            let src = source(&server);
            src.fetch(&ctx_at("2026-11-22T18:00:00Z", &[]))
                .await
                .unwrap();
            let p = Some(position(50.04, 21.99));
            let t = src
                .compose(&part_ctx("2026-11-22T18:30:00Z", p.clone()))
                .await
                .unwrap();
            assert!(matches!(t, PartText::Text(_)));
            // 61 min po pobraniu: nowe żądanie (łącznie 2).
            src.compose(&part_ctx("2026-11-22T19:01:00Z", p))
                .await
                .unwrap();
            server.verify().await;
        }

        #[tokio::test]
        async fn a_moved_station_does_not_reuse_the_cached_forecast() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/forecast"))
                .and(query_param("latitude", "50.0600"))
                .respond_with(ok(OPEN_METEO))
                .expect(1)
                .mount(&server)
                .await;
            let src = source(&server);
            src.remember(
                &open_meteo_forecast(),
                at("2026-11-22T18:50:00Z"),
                pos_key(50.04, 21.99),
            );
            let t = src
                .compose(&part_ctx(
                    "2026-11-22T19:00:00Z",
                    Some(position(50.06, 21.99)),
                ))
                .await
                .unwrap();
            assert!(matches!(t, PartText::Text(_)));
            server.verify().await;
        }

        #[tokio::test]
        async fn without_a_position_the_part_is_empty() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ok(OPEN_METEO))
                .expect(0)
                .mount(&server)
                .await;
            let t = source(&server)
                .compose(&part_ctx("2026-11-22T19:00:00Z", None))
                .await
                .unwrap();
            assert_eq!(t, PartText::Empty("brak pozycji stacji"));
            server.verify().await;
        }
    }
}
