//! Źródło `pogoda`: alerty progowe z prognozy Open-Meteo i część biuletynu
//! (z zapasem MET Norway) — jedna instancja, jedna pamięć prognozy.

pub mod alerts;
pub mod speech;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aars_core::module::{ModuleDescriptor, ModuleKind, Needs, SettingKind, SettingSpec, Toggle};
use chrono::{DateTime, TimeDelta, Utc};

use crate::bulletin::{BulletinPart, PartCtx, PartFuture, PartText};
use crate::forecast::{Forecast, met_no, open_meteo};
use crate::http;
use crate::source::{DataSource, FetchFuture, SourceCtx, SourceError};

pub const ID: &str = "pogoda";
/// Części biuletynu z jednej prognozy (klocki układu).
pub const PART_NOW: &str = "pogoda_teraz";
pub const PART_FORECAST: &str = "prognoza";
pub const PART_SUN: &str = "slonce";
const PARTS: [&str; 3] = [PART_NOW, PART_FORECAST, PART_SUN];
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
    auto_area: None,
    settings: &[
        SettingSpec {
            key: "gust_kmh",
            label: "Porywy wiatru od (km/h)",
            help: "Porywy od tej wartości w prognozie na dziś lub jutro są ogłaszane; na antenie w m/s (75 km/h ≈ 21 m/s).",
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
    /// Jedno pobranie na złożenie: części pogodowe biuletynu czekają tu na
    /// siebie, a porażka dla tej samej chwili i pozycji nie jest powtarzana.
    gate: tokio::sync::Mutex<Option<Failure>>,
}

/// Nieudane pobranie dla chwili złożenia i pozycji.
struct Failure {
    at: DateTime<Utc>,
    pos: (i64, i64),
    error: SourceError,
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
            gate: tokio::sync::Mutex::new(None),
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

impl Pogoda {
    /// Prognoza dla części biuletynu: świeża z pamięci albo jedno pobranie
    /// (Open-Meteo, w zapasie MET Norway) wspólne dla wszystkich części
    /// złożenia; `None` bez pozycji stacji.
    pub(crate) async fn forecast_for(&self, ctx: &PartCtx) -> Result<Option<Forecast>, SourceError> {
        let Some(pos) = ctx.position.as_ref() else {
            return Ok(None);
        };
        let key = pos_key(pos.lat, pos.lon);
        let mut failed = self.gate.lock().await;
        if let Some(f) = self.recall(ctx.now, key) {
            return Ok(Some(f));
        }
        if let Some(f) = failed.as_ref().filter(|f| f.at == ctx.now && f.pos == key) {
            return Err(f.error.clone());
        }
        let fetched = match self.ask_open_meteo(pos.lat, pos.lon, ctx.now).await {
            Ok(f) => Ok(f),
            Err(_) => self.ask_met(pos.lat, pos.lon, ctx.now).await,
        };
        match fetched {
            Ok(forecast) => {
                self.remember(&forecast, ctx.now, key);
                *failed = None;
                Ok(Some(forecast))
            }
            Err(error) => {
                *failed = Some(Failure {
                    at: ctx.now,
                    pos: key,
                    error: error.clone(),
                });
                Err(error)
            }
        }
    }

    /// Trzy części biuletynu (`pogoda_teraz`, `prognoza`, `slonce`) na tej
    /// samej instancji, więc z jedną prognozą.
    pub fn parts(self: &Arc<Self>) -> Vec<Arc<dyn BulletinPart>> {
        PARTS
            .iter()
            .map(|id| {
                Arc::new(WeatherPart {
                    id,
                    pogoda: Arc::clone(self),
                }) as Arc<dyn BulletinPart>
            })
            .collect()
    }
}

/// Jedna z części pogodowych biuletynu.
pub struct WeatherPart {
    id: &'static str,
    pogoda: Arc<Pogoda>,
}

impl WeatherPart {
    fn text(&self, forecast: &Forecast, now: DateTime<Utc>) -> Option<String> {
        match self.id {
            PART_NOW => speech::now_text(forecast, now),
            PART_FORECAST => speech::forecast_text(forecast, now),
            _ => speech::sun_text(forecast, now),
        }
    }

    fn empty_reason(&self) -> &'static str {
        match self.id {
            PART_NOW => "brak warunków bieżących",
            PART_FORECAST => "brak prognozy",
            _ => "brak godzin wschodu i zachodu",
        }
    }
}

impl BulletinPart for WeatherPart {
    fn id(&self) -> &'static str {
        self.id
    }

    fn max_chars(&self) -> usize {
        500
    }

    fn compose<'a>(&'a self, ctx: &'a PartCtx) -> PartFuture<'a> {
        Box::pin(async move {
            let Some(forecast) = self.pogoda.forecast_for(ctx).await? else {
                return Ok(PartText::Empty("brak pozycji stacji"));
            };
            // Każda część niesie zapowiedź z atrybucją; kompozytor zostawia ją
            // tylko w pierwszej dołączonej części pogodowej.
            Ok(match self.text(&forecast, ctx.now) {
                Some(text) => PartText::Text(format!("{} {text}", speech::heading(&forecast))),
                None => PartText::Empty(self.empty_reason()),
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
            "Prognoza pogody: dziś porywy wiatru do 11 m/s. Dane: Open-Meteo."
        );
        assert_eq!(
            today.change_text,
            "Zmiana prognozy: dziś porywy wiatru do 11 m/s. Dane: Open-Meteo."
        );
        assert_eq!(today.fingerprint, "gust:40");
        assert_eq!(today.topic, "gust:2026-11-22");
        assert_eq!(today.valid_until, Some(at("2026-11-22T23:00:00Z")));
        assert_eq!(today.valid_from, None);
        assert_eq!(today.summary, "porywy wiatru do 11 m/s, 22 listopada");
        assert_eq!(today.cancel_text, "");
        assert_eq!(today.priority, aars_core::priority::Priority::Normal);
        assert!(!today.interrupting);
        let tomorrow = item(&out, "gust:2026-11-23");
        assert_eq!(
            tomorrow.text,
            "Prognoza pogody: jutro porywy wiatru do 13 m/s. Dane: Open-Meteo."
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
    }

    #[test]
    fn a_tiny_rain_threshold_never_says_do_zero_mm() {
        let f = edited(|v| {
            v["hourly"]["precipitation"][20] = 0.3.into();
        });
        let out = alerts::to_output(&f, &ctx_at("2026-11-22T18:50:00Z", &[("precip_mm", "0")]));
        if let Some(rain) = out.items.iter().find(|i| i.key.starts_with("precip")) {
            assert!(!rain.text.contains("do 0 mm"), "{}", rain.text);
            assert!(rain.text.contains("słabe opady"), "{}", rain.text);
        }
    }

    #[test]
    fn rain_alerts_say_whole_millimetres_after_do() {
        // Ułamek po „do” normalizer czyta nieodmieniony, więc całkowite mm.
        let f = edited(|v| {
            v["hourly"]["precipitation"][20] = 7.26.into();
        });
        let ctx = ctx_at("2026-11-22T18:50:00Z", &[("precip_mm", "5")]);
        let out = alerts::to_output(&f, &ctx);
        let rain = item(&out, "precip:2026-11-22");
        assert_eq!(rain.fingerprint, "precip:0");
        assert_eq!(rain.text, "Prognoza pogody: dziś opady do 7 mm. Dane: Open-Meteo.");
        assert_eq!(rain.summary, "opady do 7 mm, 22 listopada");
        let spoken = aars_audio::text::normalize(&rain.text);
        assert!(spoken.contains("do siedmiu milimetrów"), "{spoken}");
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
            gust.text.contains("dziś porywy wiatru do 22 m/s."),
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
        assert!(gust.text.contains("dziś porywy wiatru do 22 m/s."), "{}", gust.text);
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
            "Pogoda dla okolic stacji. Obecnie bezchmurnie, temperatura 2 °C. Wiatr 3 m/s, w porywach 5 m/s. W najbliższych godzinach pochmurno, bez opadów. W nocy do 1 °C. Jutro śnieg, od 1 do 3 °C, opady do 2 mm, porywy wiatru do 13 m/s. Dane: Open-Meteo."
        );
    }

    #[test]
    fn the_bulletin_text_before_two_pm() {
        let now = at("2026-11-22T09:00:00Z");
        assert_eq!(
            speech::bulletin(&open_meteo_forecast(), now).unwrap(),
            "Pogoda dla okolic stacji. Obecnie bezchmurnie, temperatura 2 °C. Wiatr 3 m/s, w porywach 5 m/s. W najbliższych godzinach mżawka, słabe opady, porywy wiatru do 11 m/s. Dziś mżawka, do 6 °C, opady do 1 mm, porywy wiatru do 11 m/s. W nocy do 1 °C. Dane: Open-Meteo."
        );
    }

    const STATION: &[u8] =
        include_bytes!("../../tests/fixtures/pogoda/open-meteo-51.38-20.28-2026-11-24.json");
    const STATION_FETCHED: &str = "2026-11-24T22:41:54Z";

    fn station_edited(edit: impl FnOnce(&mut serde_json::Value)) -> Forecast {
        let mut v: serde_json::Value = serde_json::from_slice(STATION).unwrap();
        edit(&mut v);
        open_meteo::parse(&serde_json::to_vec(&v).unwrap(), at(STATION_FETCHED)).unwrap()
    }
    fn station() -> Forecast {
        station_edited(|_| {})
    }

    #[test]
    fn the_full_bulletin_from_the_station_fixture() {
        let f = station();
        let now = at(STATION_FETCHED);
        assert_eq!(
            speech::bulletin(&f, now).unwrap(),
            "Pogoda dla okolic stacji. Obecnie pochmurno, temperatura 1 °C, odczuwalna -3 °C. Wilgotność 93 %. Ciśnienie 1024 hPa, rośnie. Wiatr północno-zachodni 4 m/s, w porywach 9 m/s. W najbliższych godzinach mżawka, słabe opady. W nocy do 0 °C. Jutro śnieg, od -4 do 1 °C, słabe opady, wiatr zachodni do 3 m/s, w porywach do 8 m/s. Wschód słońca jutro o 7:12, zachód o 15:38. Dane: Open-Meteo."
        );
        assert_eq!(
            speech::now_text(&f, now).unwrap(),
            "Obecnie pochmurno, temperatura 1 °C, odczuwalna -3 °C. Wilgotność 93 %. Ciśnienie 1024 hPa, rośnie. Wiatr północno-zachodni 4 m/s, w porywach 9 m/s."
        );
        assert_eq!(
            speech::forecast_text(&f, now).unwrap(),
            "W najbliższych godzinach mżawka, słabe opady. W nocy do 0 °C. Jutro śnieg, od -4 do 1 °C, słabe opady, wiatr zachodni do 3 m/s, w porywach do 8 m/s."
        );
        assert_eq!(
            speech::sun_text(&f, now).unwrap(),
            "Wschód słońca jutro o 7:12, zachód o 15:38."
        );
    }

    #[test]
    fn in_the_morning_today_comes_first_and_the_sun_is_todays() {
        let f = station();
        // 6:00 lokalnie, przed wschodem.
        let dawn = at("2026-11-24T05:00:00Z");
        assert_eq!(
            speech::forecast_text(&f, dawn).unwrap(),
            "W najbliższych godzinach mżawka, słabe opady. Dziś śnieg, do 1 °C, opady do 6 mm, wiatr zachodni do 7 m/s, w porywach do 13 m/s. W nocy do 0 °C."
        );
        assert_eq!(
            speech::sun_text(&f, dawn).unwrap(),
            "Wschód słońca o 7:10, zachód o 15:39."
        );
        // W dzień zostaje sam zachód.
        assert_eq!(
            speech::sun_text(&f, at("2026-11-24T12:00:00Z")).unwrap(),
            "Zachód słońca o 15:39."
        );
        // 14:00 lokalnie to już wieczorny układ.
        assert!(
            speech::forecast_text(&f, at("2026-11-24T13:00:00Z"))
                .unwrap()
                .contains("W nocy do 0 °C. Jutro śnieg")
        );
    }

    #[test]
    fn current_conditions_follow_their_thresholds() {
        let now = at(STATION_FETCHED);
        let text = |edit: fn(&mut serde_json::Value)| {
            speech::now_text(&station_edited(edit), now).unwrap()
        };
        // Odczuwalna różna o mniej niż 2 °C nie jest podawana.
        let t = text(|v| v["current"]["apparent_temperature"] = (-0.9).into());
        assert!(t.starts_with("Obecnie pochmurno, temperatura 1 °C. Wilgotność"), "{t}");
        // Widzialność poniżej 5 km w km, poniżej 1 km w metrach.
        let t = text(|v| v["current"]["visibility"] = 3400.0.into());
        assert!(t.ends_with("w porywach 9 m/s. Widzialność 3,4 km."), "{t}");
        let t = text(|v| v["current"]["visibility"] = 780.0.into());
        assert!(t.ends_with("Widzialność 800 m."), "{t}");
        let t = text(|v| v["current"]["visibility"] = 20.0.into());
        assert!(t.ends_with("Widzialność 100 m."), "{t}");
        // Tendencja: poniżej 1 hPa bez zmian, spadek.
        let t = text(|v| v["hourly"]["pressure_msl"][0] = 1023.5.into());
        assert!(t.contains("Ciśnienie 1024 hPa, bez zmian."), "{t}");
        let t = text(|v| v["hourly"]["pressure_msl"][0] = 1026.0.into());
        assert!(t.contains("Ciśnienie 1024 hPa, spada."), "{t}");
        let t = text(|v| v["hourly"]["pressure_msl"][0] = serde_json::Value::Null);
        assert!(t.contains("Ciśnienie 1024 hPa. Wiatr"), "{t}");
        // Wiatr poniżej 1 m/s to cisza, bez kierunku i porywów.
        let t = text(|v| {
            v["current"]["wind_speed_10m"] = 0.6.into();
            v["current"]["wind_gusts_10m"] = 4.9.into();
        });
        assert!(t.ends_with("Ciśnienie 1024 hPa, rośnie. Cisza."), "{t}");
        // …ale silne porywy przy ciszy nie giną.
        let t = text(|v| {
            v["current"]["wind_speed_10m"] = 0.6.into();
            v["current"]["wind_gusts_10m"] = 5.6.into();
        });
        assert!(t.ends_with("Ciśnienie 1024 hPa, rośnie. Wiatr słaby, w porywach 6 m/s."), "{t}");
        // Porywy nie mocniejsze od wiatru nie są powtarzane.
        let t = text(|v| v["current"]["wind_gusts_10m"] = 4.2.into());
        assert!(t.ends_with("Wiatr północno-zachodni 4 m/s."), "{t}");
        // Samo zachmurzenie dokładniej z procentu; zjawisko z kodu zostaje.
        let t = text(|v| v["current"]["cloud_cover"] = 55.into());
        assert!(t.starts_with("Obecnie umiarkowane zachmurzenie,"), "{t}");
        let t = text(|v| v["current"]["weather_code"] = 45.into());
        assert!(t.starts_with("Obecnie mgła,"), "{t}");
        // Opad bez zjawiska w kodzie: „pada”.
        let t = text(|v| v["current"]["precipitation"] = 0.3.into());
        assert!(t.starts_with("Obecnie pochmurno, pada, temperatura"), "{t}");
        let t = text(|v| {
            v["current"]["precipitation"] = 0.3.into();
            v["current"]["weather_code"] = 63.into();
        });
        assert!(t.starts_with("Obecnie deszcz, temperatura"), "{t}");
    }

    #[test]
    fn the_next_hours_report_rain_and_stronger_gusts() {
        let now = at(STATION_FETCHED);
        // Godziny 0:00–5:00 dnia następnego (indeksy 4–9).
        let f = station_edited(|v| {
            v["hourly"]["weather_code"][5] = 81.into();
            v["hourly"]["precipitation"][5] = 1.4.into();
            v["hourly"]["precipitation"][7] = 0.6.into();
            v["hourly"]["wind_gusts_10m"][8] = 12.4.into();
            // 6:00 to już siódma godzina — nie liczy się.
            v["hourly"]["weather_code"][10] = 95.into();
        });
        assert!(
            speech::forecast_text(&f, now)
                .unwrap()
                .starts_with("W najbliższych godzinach przelotny deszcz, opady do 2 mm, porywy wiatru do 12 m/s. W nocy"),
            "{:?}",
            speech::forecast_text(&f, now)
        );
        // Porywy tylko o 3 m/s mocniejsze niż teraz (8,7 m/s) nie są podawane.
        let f = station_edited(|v| v["hourly"]["wind_gusts_10m"][8] = 11.7.into());
        assert!(!speech::forecast_text(&f, now).unwrap().contains("porywy"));
        // Bez warunków bieżących porywy tylko od 10 m/s.
        let f = station_edited(|v| {
            v.as_object_mut().unwrap().remove("current");
            v["hourly"]["wind_gusts_10m"][8] = 9.9.into();
        });
        assert!(!speech::forecast_text(&f, now).unwrap().contains("porywy"));
        let f = station_edited(|v| {
            v.as_object_mut().unwrap().remove("current");
            v["hourly"]["wind_gusts_10m"][8] = 10.4.into();
        });
        assert!(
            speech::forecast_text(&f, now).unwrap().contains("porywy wiatru do 10 m/s."),
            "{:?}",
            speech::forecast_text(&f, now)
        );
        // Bez kodów i opadów: brak zdania o najbliższych godzinach.
        let f = station_edited(|v| {
            v["hourly"]["weather_code"] = serde_json::json!([]);
            v["hourly"]["precipitation"] = serde_json::json!([]);
        });
        assert!(speech::forecast_text(&f, now).unwrap().starts_with("W nocy"));
    }

    #[test]
    fn the_met_fallback_speaks_mean_wind() {
        let now = at("2026-11-22T19:00:00Z");
        let f = met_no::parse(MET, now).unwrap();
        assert_eq!(
            speech::bulletin(&f, now).unwrap(),
            "Pogoda dla okolic stacji. Obecnie duże zachmurzenie, temperatura -1 °C. Wilgotność 94 %. Ciśnienie 1010 hPa. Wiatr południowo-zachodni 2 m/s. W nocy do -2 °C. Jutro śnieg, od -2 do 2 °C, opady do 5 mm, wiatr do 7 m/s. Dane: MET Norway."
        );
    }

    #[test]
    fn gaps_are_left_out_not_invented() {
        let f = edited(|v| {
            v["daily"]["precipitation_sum"][1] = serde_json::Value::Null;
            v["daily"]["wind_gusts_10m_max"][1] = serde_json::Value::Null;
            v["daily"]["weather_code"][1] = serde_json::Value::Null;
            v["current"]["weather_code"] = serde_json::Value::Null;
            v["current"]["wind_speed_10m"] = serde_json::Value::Null;
            v["current"]["wind_gusts_10m"] = serde_json::Value::Null;
            v.as_object_mut().unwrap().remove("hourly");
        });
        assert_eq!(
            speech::bulletin(&f, at("2026-11-22T18:50:00Z")).unwrap(),
            "Pogoda dla okolic stacji. Obecnie temperatura 2 °C. W nocy do 1 °C. Jutro od 1 do 3 °C. Dane: Open-Meteo."
        );
        let now = at(STATION_FETCHED);
        let f = station_edited(|v| {
            for key in [
                "apparent_temperature",
                "relative_humidity_2m",
                "pressure_msl",
                "wind_direction_10m",
                "cloud_cover",
                "visibility",
                "weather_code",
                "precipitation",
            ] {
                v["current"][key] = serde_json::Value::Null;
            }
            v["daily"]["wind_direction_10m_dominant"][1] = serde_json::Value::Null;
            v["daily"]["sunrise"][1] = serde_json::Value::Null;
        });
        assert_eq!(
            speech::now_text(&f, now).unwrap(),
            "Obecnie temperatura 1 °C. Wiatr 4 m/s, w porywach 9 m/s."
        );
        assert!(speech::forecast_text(&f, now).unwrap().contains("wiatr do 3 m/s, w porywach do 8 m/s."));
        // Brak jutrzejszego wschodu nie gubi zachodu.
        assert_eq!(speech::sun_text(&f, now).unwrap(), "Zachód słońca jutro o 15:38.");
        let f = station_edited(|v| {
            v["daily"]["sunrise"][1] = serde_json::Value::Null;
            v["daily"]["sunset"][1] = serde_json::Value::Null;
        });
        assert_eq!(speech::sun_text(&f, now), None);
        // Przed wschodem bez dzisiejszego zachodu: sam wschód; bez wschodu: sam zachód.
        let dawn = at("2026-11-24T05:00:00Z");
        let f = station_edited(|v| v["daily"]["sunset"][0] = serde_json::Value::Null);
        assert_eq!(speech::sun_text(&f, dawn).unwrap(), "Wschód słońca o 7:10.");
        let f = station_edited(|v| v["daily"]["sunrise"][0] = serde_json::Value::Null);
        assert_eq!(speech::sun_text(&f, dawn).unwrap(), "Zachód słońca o 15:39.");
        let mut empty = open_meteo_forecast();
        empty.current = None;
        empty.days.clear();
        empty.hourly.clear();
        assert_eq!(speech::now_text(&empty, now), None);
        assert_eq!(speech::forecast_text(&empty, now), None);
        assert_eq!(speech::sun_text(&empty, now), None);
        assert_eq!(speech::bulletin(&empty, at("2026-11-22T18:50:00Z")), None);
    }

    #[test]
    fn each_weather_part_fits_its_limit() {
        let src = Arc::new(Pogoda::with_urls("http://127.0.0.1:9/a", "http://127.0.0.1:9/b", Duration::from_secs(1)).unwrap());
        let parts = src.parts();
        let ids: Vec<_> = parts.iter().map(|p| p.id()).collect();
        assert_eq!(ids, ["pogoda_teraz", "prognoza", "slonce"]);
        assert!(parts.iter().all(|p| p.max_chars() == 500));
        let (f, now) = (station(), at(STATION_FETCHED));
        let heading = speech::heading(&f);
        assert_eq!(heading, "Pogoda dla okolic stacji według danych Open-Meteo.");
        for text in [speech::now_text(&f, now), speech::forecast_text(&f, now), speech::sun_text(&f, now)] {
            let n = format!("{heading} {}", text.unwrap()).chars().count();
            assert!(n < 500, "{n}");
        }
    }

    #[test]
    fn the_night_minimum_comes_from_the_coming_night_hours() {
        let now = at(STATION_FETCHED); // 23:41 lokalnie
        let night = |f: &Forecast, now| {
            let text = speech::forecast_text(f, now).unwrap();
            let start = text.find("W nocy").unwrap();
            text[start..text[start..].find('.').unwrap() + start + 1].to_owned()
        };
        // Godziny 23:00–6:00: najzimniej o 6:00 (−0,0 °C), nie −3,6 °C z całej jutrzejszej doby.
        assert_eq!(night(&station(), now), "W nocy do 0 °C.");
        // 7:00 już nie jest nocą, 22:00 przed bieżącą godziną też się nie liczy.
        let f = station_edited(|v| {
            v["hourly"]["temperature_2m"][11] = (-9.0).into();
            v["hourly"]["temperature_2m"][2] = (-9.0).into();
        });
        assert_eq!(night(&f, now), "W nocy do 0 °C.");
        let f = station_edited(|v| v["hourly"]["temperature_2m"][7] = (-2.6).into());
        assert_eq!(night(&f, now), "W nocy do -3 °C.");
        // Rano (6:00 lokalnie) mowa o nadchodzącej nocy 24/25 listopada: 22:00–6:00.
        let f = station_edited(|v| v["hourly"]["temperature_2m"][2] = (-7.2).into());
        assert_eq!(night(&f, at("2026-11-24T05:00:00Z")), "W nocy do -7 °C.");
        // Bez godzin: minimum jutrzejszej doby.
        let f = station_edited(|v| {
            v.as_object_mut().unwrap().remove("hourly");
        });
        assert_eq!(night(&f, now), "W nocy do -4 °C.");
    }

    #[test]
    fn a_gust_rounded_in_m_s_still_trips_its_km_h_threshold() {
        // 80 km/h przychodzi z Open-Meteo jako 22,2 m/s = 79,92 km/h.
        let f = station_edited(|v| v["hourly"]["wind_gusts_10m"][2] = 22.2.into());
        // 20:10 lokalnie: dziś liczy się od 20:00, 22:00 ma 22,2 m/s.
        let out = alerts::to_output(&f, &ctx_at("2026-11-24T19:10:00Z", &[("gust_kmh", "80")]));
        let gust = item(&out, "gust:2026-11-24");
        assert_eq!(gust.fingerprint, "gust:80");
        // Próg i odcisk w km/h, mowa w m/s jak w biuletynie.
        assert_eq!(gust.text, "Prognoza pogody: dziś porywy wiatru do 22 m/s. Dane: Open-Meteo.");
        let out = alerts::to_output(&f, &ctx_at("2026-11-24T19:10:00Z", &[("gust_kmh", "81")]));
        assert!(out.items.iter().all(|i| i.key != "gust:2026-11-24"), "{:?}", out.items);
    }

    #[test]
    fn the_bulletin_texts_are_speakable() {
        let evening = at("2026-11-22T18:50:00Z");
        let morning = at("2026-11-22T09:00:00Z");
        let met_now = at("2026-11-22T19:00:00Z");
        let a = speech::bulletin(&open_meteo_forecast(), evening).unwrap();
        let b = speech::bulletin(&open_meteo_forecast(), morning).unwrap();
        let c = speech::bulletin(&met_no::parse(MET, met_now).unwrap(), met_now).unwrap();
        let station = station();
        let d = speech::bulletin(&station, at(STATION_FETCHED)).unwrap();
        let e = speech::bulletin(&station, at("2026-11-24T05:00:00Z")).unwrap();
        assert_speakable(&[&a, &b, &c, &d, &e]);
        let spoken = aars_audio::text::normalize(&a);
        assert!(spoken.contains("Obecnie bezchmurnie, temperatura dwa stopnie Celsjusza"), "{spoken}");
        assert!(spoken.contains("porywy wiatru do trzynastu metrów na sekundę"), "{spoken}");
        let spoken = aars_audio::text::normalize(&d);
        for phrase in [
            "odczuwalna minus trzy stopnie Celsjusza.",
            "Wilgotność dziewięćdziesiąt trzy procent.",
            "Ciśnienie tysiąc dwadzieścia cztery hektopaskale, rośnie.",
            "Wiatr północno-zachodni cztery metry na sekundę, w porywach dziewięć metrów na sekundę.",
            "W nocy do zera stopni Celsjusza.",
            "od minus czterech do jednego stopnia Celsjusza",
            "Wschód słońca jutro o godzinie siódmej dwanaście, zachód o godzinie piętnastej trzydzieści osiem.",
        ] {
            assert!(spoken.contains(phrase), "{phrase:?} w: {spoken}");
        }
        let spoken = aars_audio::text::normalize(&e);
        assert!(spoken.contains("Dziś śnieg, do jednego stopnia Celsjusza, opady do sześciu milimetrów,"), "{spoken}");
        let spoken = aars_audio::text::normalize(&speech::now_text(&station_edited(|v| v["current"]["visibility"] = 780.0.into()), at(STATION_FETCHED)).unwrap());
        assert!(spoken.ends_with("Widzialność osiemset metrów."), "{spoken}");
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
                sources: BTreeMap::new(),
                active_alerts: Vec::new(),
                alert_sources: Default::default(),
                warnings_max_chars: None,
            }
        }

        async fn text_of(part: &Arc<dyn BulletinPart>, ctx: &PartCtx) -> String {
            match part.compose(ctx).await.unwrap() {
                PartText::Text(t) => t,
                PartText::Empty(why) => panic!("pusta część: {why}"),
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
        async fn the_parts_fall_back_to_met_with_its_user_agent() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/forecast"))
                .respond_with(ResponseTemplate::new(503))
                .expect(1)
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
            let parts = Arc::new(source(&server)).parts();
            let ctx = part_ctx("2026-11-22T19:00:00Z", Some(position(50.04, 21.99)));
            let now = text_of(&parts[0], &ctx).await;
            assert!(now.starts_with("Pogoda dla okolic stacji według danych MET Norway. "), "{now}");
            // Każda część niesie zapowiedź; kompozytor zostawia ją raz.
            let forecast = text_of(&parts[1], &ctx).await;
            let rest = speech::strip_heading(&forecast).unwrap();
            assert!(!rest.is_empty() && !rest.contains("według danych"), "{rest}");
            server.verify().await;
        }

        #[tokio::test]
        async fn three_parts_of_one_bulletin_fetch_the_forecast_once() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/forecast"))
                .respond_with(ok(OPEN_METEO).set_delay(Duration::from_millis(100)))
                .expect(1)
                .mount(&server)
                .await;
            let parts = Arc::new(source(&server)).parts();
            let ctx = part_ctx("2026-11-22T19:00:00Z", Some(position(50.04, 21.99)));
            let (a, b, c) = tokio::join!(
                parts[0].compose(&ctx),
                parts[1].compose(&ctx),
                parts[2].compose(&ctx)
            );
            // Stary zrzut nie ma godzin wschodu i zachodu: słońce puste, ale bez błędu.
            assert!(matches!(a, Ok(PartText::Text(_))), "{a:?}");
            assert!(matches!(b, Ok(PartText::Text(_))), "{b:?}");
            assert!(c.is_ok(), "{c:?}");
            server.verify().await;
        }

        #[tokio::test]
        async fn a_failed_fetch_is_not_repeated_within_one_bulletin() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(503))
                .expect(2)
                .mount(&server)
                .await;
            let parts = Arc::new(source(&server)).parts();
            let ctx = part_ctx("2026-11-22T19:00:00Z", Some(position(50.04, 21.99)));
            let (a, b, c) = tokio::join!(
                parts[0].compose(&ctx),
                parts[1].compose(&ctx),
                parts[2].compose(&ctx)
            );
            for t in [a, b, c] {
                assert!(matches!(t, Err(SourceError::Status(503))), "{t:?}");
            }
            // Jedno żądanie do Open-Meteo i jedno do MET Norway, nie po trzy.
            server.verify().await;

            // Następne złożenie (inna chwila) próbuje od nowa.
            server.reset().await;
            Mock::given(method("GET"))
                .and(path("/v1/forecast"))
                .respond_with(ok(OPEN_METEO))
                .expect(1)
                .mount(&server)
                .await;
            let later = part_ctx("2026-11-22T19:10:00Z", Some(position(50.04, 21.99)));
            assert!(matches!(parts[0].compose(&later).await, Ok(PartText::Text(_))));
            server.verify().await;
        }

        #[tokio::test]
        async fn the_parts_reuse_a_fresh_forecast() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/forecast"))
                .respond_with(ok(OPEN_METEO))
                .expect(2)
                .mount(&server)
                .await;
            let src = Arc::new(source(&server));
            src.fetch(&ctx_at("2026-11-22T18:00:00Z", &[]))
                .await
                .unwrap();
            let parts = src.parts();
            let p = Some(position(50.04, 21.99));
            let t = parts[1]
                .compose(&part_ctx("2026-11-22T18:30:00Z", p.clone()))
                .await
                .unwrap();
            assert!(matches!(t, PartText::Text(_)));
            // 61 min po pobraniu: nowe żądanie (łącznie 2).
            parts[1]
                .compose(&part_ctx("2026-11-22T19:01:00Z", p))
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
            let src = Arc::new(source(&server));
            src.remember(
                &open_meteo_forecast(),
                at("2026-11-22T18:50:00Z"),
                pos_key(50.04, 21.99),
            );
            let t = src.parts()[0]
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
        async fn without_a_position_the_parts_are_empty() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ok(OPEN_METEO))
                .expect(0)
                .mount(&server)
                .await;
            for part in Arc::new(source(&server)).parts() {
                let t = part
                    .compose(&part_ctx("2026-11-22T19:00:00Z", None))
                    .await
                    .unwrap();
                assert_eq!(t, PartText::Empty("brak pozycji stacji"));
            }
            server.verify().await;
        }
    }
}
