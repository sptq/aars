//! Źródło `imgw-wodowskazy`: przekroczenia stanów ostrzegawczych i
//! alarmowych na wodowskazach w pobliżu stacji, oraz część biuletynu `wody`
//! — jedna instancja, jedna pamięć ostatniej listy stacji.

pub mod parse;
pub mod speech;

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aars_core::geo::StationPosition;
use aars_core::module::{ModuleDescriptor, ModuleKind, Needs, SettingKind, SettingSpec, Toggle};
use aars_core::priority::Priority;
use chrono::{DateTime, TimeDelta, Utc};

use crate::bulletin::{BulletinPart, PartCtx, PartFuture, PartText};
use crate::http;
use crate::source::{DataSource, FetchFuture, SourceCtx, SourceError, SourceItem, SourceOutput};
use crate::speech::{haversine_km, warsaw_to_utc};
pub use parse::{Exceedance, Gauge};

pub const ID: &str = "imgw-wodowskazy";
pub const PART_ID: &str = "wody";
pub const DEFAULT_URL: &str = "https://danepubliczne.imgw.pl/api/data/hydro";
pub const MAX_BODY: usize = 2 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_RADIUS_KM: u32 = 25;
/// Pomiar starszy niż to nie jest „świeży” (C6).
const FRESH_READING: TimeDelta = TimeDelta::hours(3);
/// Lista starsza niż to nie jest używana przez część biuletynu (C9).
const FRESH_FETCH: TimeDelta = TimeDelta::minutes(20);
const VALID_FOR: TimeDelta = TimeDelta::hours(24);
const PART_GAUGES: usize = 3;
/// Śledzony wodowskaz jest ogłaszany dalej, dopóki stan ≥ próg − tyle cm.
const HYSTERESIS_CM: f64 = 5.0;

pub const DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
    id: ID,
    name: "Stany wodowskazów IMGW",
    description: "Komunikat, gdy wodowskaz w pobliżu stacji przekroczy stan ostrzegawczy lub alarmowy według IMGW-PIB. Część biuletynu „Stany wód w okolicy” korzysta z tych samych danych.",
    kind: ModuleKind::Source,
    toggle: Toggle::Switchable,
    default_enabled: false,
    needs: Needs::Position,
    auto_area: None,
    settings: &[
        SettingSpec {
            key: "radius_km",
            label: "Promień od stacji (km)",
            help: "Wodowskazy bliżej niż tyle kilometrów od stacji są śledzone.",
            kind: SettingKind::Integer { min: 5, max: 100 },
            default: "25",
        },
        SettingSpec {
            key: "stations",
            label: "Wodowskazy (id IMGW)",
            help: "Opcjonalnie identyfikatory stacji IMGW (id_stacji) po przecinku, np. 150220010. Niepuste pole zastępuje promień.",
            kind: SettingKind::Text { max_len: 200 },
            default: "",
        },
        SettingSpec {
            key: "interval_s",
            label: "Interwał odpytywania (s)",
            help: "Jak często odpytywać źródło.",
            kind: SettingKind::Integer {
                min: 600,
                max: 3600,
            },
            default: "900",
        },
    ],
    see_also: None,
};

/// Promień z ustawień; poza 5–100 albo nieczytelny → 25.
pub(crate) fn radius_km(settings: &BTreeMap<String, String>) -> u32 {
    settings
        .get("radius_km")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|r| (5..=100).contains(r))
        .unwrap_or(DEFAULT_RADIUS_KM)
}

/// Lista `stations`: po przecinku, przycięte, tylko cyfry, bez duplikatów,
/// posortowane.
pub(crate) fn station_ids(settings: &BTreeMap<String, String>) -> Vec<String> {
    let mut ids: Vec<String> = settings
        .get("stations")
        .map(String::as_str)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_owned)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Wodowskazy do śledzenia z odległością w km: niepuste `ids` = tylko te
/// (promień ignorowany), inaczej w promieniu; rosnąco po km, remis po `id`.
pub(crate) fn selected<'a>(
    gauges: &'a [Gauge],
    pos: (f64, f64),
    radius_km: u32,
    ids: &[String],
) -> Vec<(&'a Gauge, f64)> {
    let mut out: Vec<(&Gauge, f64)> = gauges
        .iter()
        .filter(|g| ids.is_empty() || ids.contains(&g.id))
        .map(|g| (g, haversine_km(pos, (g.lat, g.lon))))
        .filter(|(_, km)| !ids.is_empty() || *km <= f64::from(radius_km))
        .collect();
    out.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.id.cmp(&b.0.id)));
    out
}

/// Stan i czas pomiaru (UTC), gdy pomiar jest świeży: `now − pomiar ≤ 3 h`,
/// pomiar „z przyszłości” też; czas nieistniejący w Warszawie = brak (C6).
fn fresh_reading(g: &Gauge, now: DateTime<Utc>) -> Option<(f64, DateTime<Utc>)> {
    let level = g.level_cm?;
    let measured = warsaw_to_utc(g.measured_at?)?;
    (now - measured <= FRESH_READING).then_some((level, measured))
}

fn latlon(pos: Option<&StationPosition>) -> Option<(f64, f64)> {
    pos.map(|p| (p.lat, p.lon))
}

/// Przekroczenie do ogłoszenia z uwzględnieniem śledzonego odcisku: śledzona
/// pozycja nie obniża stanu (raz `alarmowy` zostaje `alarmowy`) i trzyma się,
/// dopóki stan nie spadnie o więcej niż `HYSTERESIS_CM` poniżej najniższego
/// progu stacji — wodowskaz chwiejący się przy progu nie migocze na antenie.
fn held_exceedance(g: &Gauge, level: f64, held: Option<&String>) -> Option<Exceedance> {
    let now = g.exceedance();
    let held = match held.map(String::as_str) {
        Some("ostrzegawczy") => Exceedance::Warning,
        Some("alarmowy") => Exceedance::Alarm,
        _ => return now,
    };
    match now {
        Some(Exceedance::Alarm) => Some(Exceedance::Alarm),
        Some(Exceedance::Warning) => Some(held),
        None => {
            let lowest = g.warning_cm.or(g.alarm_cm)?;
            (level >= lowest - HYSTERESIS_CM).then_some(held)
        }
    }
}

/// Czysta funkcja: lista IMGW → pozycje przekroczeń wodowskazów z `ctx` (C6–C8).
pub fn to_output(gauges: &[Gauge], ctx: &SourceCtx) -> SourceOutput {
    let chosen = match latlon(ctx.position.as_ref()) {
        Some(pos) => selected(
            gauges,
            pos,
            radius_km(&ctx.settings),
            &station_ids(&ctx.settings),
        ),
        None => Vec::new(),
    };
    let mut fresh: HashSet<&str> = HashSet::new();
    let mut items = Vec::new();
    for (g, _) in chosen {
        let Some((level, measured)) = fresh_reading(g, ctx.now) else {
            continue;
        };
        // Bez progów nie wiadomo, czy stan spadł; taka stacja nie jest
        // „świeża” dla kompletności, więc nie da fałszywego odwołania.
        if g.warning_cm.or(g.alarm_cm).is_some() {
            fresh.insert(&g.id);
        }
        let Some(e) = held_exceedance(g, level, ctx.tracked.get(&g.id)) else {
            continue;
        };
        let text = speech::announcement(g, e, level, measured);
        items.push(SourceItem {
            key: g.id.clone(),
            topic: g.id.clone(),
            fingerprint: e.noun().to_owned(),
            change_text: speech::change(&text),
            cancel_text: speech::cancellation(g),
            text,
            summary: speech::summary(g, e),
            priority: match e {
                Exceedance::Warning => Priority::Normal,
                Exceedance::Alarm => Priority::High,
            },
            interrupting: false,
            repeat: None,
            valid_from: Some(measured),
            valid_until: Some(measured + VALID_FOR),
        });
    }
    // Rozstrzygnięcie 1 / C7: brak świeżego pomiaru śledzonej stacji nie
    // może dać odwołania.
    let complete = ctx.tracked_keys.iter().all(|k| fresh.contains(k.as_str()));
    SourceOutput { items, complete }
}

type Memory = Option<(DateTime<Utc>, Arc<Vec<Gauge>>)>;

pub struct ImgwWodowskazy {
    client: reqwest::Client,
    url: String,
    memory: Mutex<Memory>,
}

impl ImgwWodowskazy {
    pub fn new(url: &str) -> Result<Self, SourceError> {
        Self::with_timeout(url, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(url: &str, timeout: Duration) -> Result<Self, SourceError> {
        Ok(Self {
            client: http::client(timeout)?,
            url: url.to_owned(),
            memory: Mutex::new(None),
        })
    }

    async fn download(&self, now: DateTime<Utc>) -> Result<Arc<Vec<Gauge>>, SourceError> {
        let body = http::get_limited(&self.client, &self.url, MAX_BODY).await?;
        let gauges = Arc::new(parse::parse_gauges(&body)?);
        // Blokada tylko wokół zapisu, nigdy przez await; zatruty Mutex odzyskujemy.
        let mut memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        *memory = Some((now, gauges.clone()));
        Ok(gauges)
    }

    fn recall(&self, now: DateTime<Utc>) -> Option<Arc<Vec<Gauge>>> {
        let memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        memory
            .as_ref()
            .filter(|(at, _)| now - *at <= FRESH_FETCH)
            .map(|(_, g)| g.clone())
    }
}

impl DataSource for ImgwWodowskazy {
    fn id(&self) -> &'static str {
        ID
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(900)
    }

    fn needs(&self) -> Needs {
        Needs::Position
    }

    fn scope(&self, ctx: &SourceCtx) -> String {
        let r = radius_km(&ctx.settings);
        let ids = station_ids(&ctx.settings).join(",");
        match &ctx.position {
            Some(pos) => format!("{:.2},{:.2}|r={r}|s={ids}", pos.lat, pos.lon),
            None => format!("brak pozycji|r={r}|s={ids}"),
        }
    }

    fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a> {
        Box::pin(async move {
            let gauges = self.download(ctx.now).await?;
            Ok(to_output(&gauges, ctx))
        })
    }
}

impl BulletinPart for ImgwWodowskazy {
    fn id(&self) -> &'static str {
        PART_ID
    }

    fn max_chars(&self) -> usize {
        400
    }

    fn compose<'a>(&'a self, ctx: &'a PartCtx) -> PartFuture<'a> {
        Box::pin(async move {
            let Some(pos) = latlon(ctx.position.as_ref()) else {
                return Ok(PartText::Empty("brak pozycji stacji"));
            };
            let gauges = match self.recall(ctx.now) {
                Some(g) => g,
                None => self.download(ctx.now).await?,
            };
            let settings = ctx.source_settings(ID);
            let nearest: Vec<(&Gauge, f64)> =
                selected(&gauges, pos, radius_km(&settings), &station_ids(&settings))
                    .into_iter()
                    .filter_map(|(g, _)| fresh_reading(g, ctx.now).map(|(level, _)| (g, level)))
                    .take(PART_GAUGES)
                    .collect();
            Ok(match speech::bulletin(&nearest) {
                Some(text) => PartText::Text(text),
                None => PartText::Empty("brak świeżych pomiarów wodowskazów"),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: &[u8] = include_bytes!("../../tests/fixtures/imgw/hydro-2026-11-22.json");

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn map(s: &[(&str, &str)]) -> BTreeMap<String, String> {
        s.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn position() -> StationPosition {
        StationPosition {
            lat: 50.04,
            lon: 21.99,
            locator: "KN00".to_owned(),
        }
    }

    fn ctx_full(now: &str, settings: &[(&str, &str)], tracked: &[&str]) -> SourceCtx {
        SourceCtx {
            now: at(now),
            area: Vec::new(),
            area_text: String::new(),
            position: Some(position()),
            settings: map(settings),
            tracked_keys: tracked.iter().map(|k| (*k).to_owned()).collect(),
            tracked: Default::default(),
        }
    }

    fn ctx(settings: &[(&str, &str)]) -> SourceCtx {
        ctx_full("2026-11-22T20:00:00Z", settings, &[])
    }

    fn gauges() -> Vec<Gauge> {
        parse::parse_gauges(FIX).unwrap()
    }

    /// Fiks z poprawką wpisu stacji `id`.
    fn edited(id: &str, edit: &dyn Fn(&mut serde_json::Value)) -> Vec<Gauge> {
        let mut v: serde_json::Value = serde_json::from_slice(FIX).unwrap();
        let arr = v.as_array_mut().unwrap();
        let idx = arr.iter().position(|g| g["id_stacji"] == id).unwrap();
        edit(&mut arr[idx]);
        parse::parse_gauges(&serde_json::to_vec(&v).unwrap()).unwrap()
    }

    fn item<'a>(out: &'a SourceOutput, key: &str) -> &'a SourceItem {
        out.items
            .iter()
            .find(|i| i.key == key)
            .unwrap_or_else(|| panic!("brak {key}"))
    }

    #[test]
    fn the_descriptor_has_the_agreed_keys_and_copy() {
        let d = &DESCRIPTOR;
        assert_eq!(
            (d.id, d.name),
            ("imgw-wodowskazy", "Stany wodowskazów IMGW")
        );
        assert_eq!((d.kind, d.toggle), (ModuleKind::Source, Toggle::Switchable));
        assert!(!d.default_enabled);
        assert_eq!(d.needs, Needs::Position);
        let keys: Vec<_> = d.settings.iter().map(|s| (s.key, s.default)).collect();
        assert_eq!(
            keys,
            [("radius_km", "25"), ("stations", ""), ("interval_s", "900")]
        );
        assert_eq!(
            d.settings[0].kind,
            SettingKind::Integer { min: 5, max: 100 }
        );
        assert_eq!(d.settings[1].kind, SettingKind::Text { max_len: 200 });
        assert_eq!(
            d.settings[2].kind,
            SettingKind::Integer {
                min: 600,
                max: 3600
            }
        );
    }

    #[test]
    fn the_fixture_parses_913_gauges_and_17_exceed() {
        let g = gauges();
        assert_eq!(g.len(), 913);
        assert_eq!(g.iter().filter(|g| g.exceedance().is_some()).count(), 17);
        assert_eq!(
            g.iter()
                .filter(|g| g.exceedance() == Some(Exceedance::Alarm))
                .count(),
            2
        );
        let m = g.iter().find(|g| g.id == "149229999").unwrap();
        assert_eq!(
            (m.warning_cm, m.alarm_cm, m.exceedance()),
            (None, None, None)
        );
        let gdansk = g.iter().find(|g| g.id == "154180140").unwrap();
        assert_eq!(gdansk.river, None);
    }

    #[test]
    fn empty_zero_or_junk_thresholds_are_no_threshold() {
        for junk in [
            serde_json::json!(""),
            serde_json::json!("0"),
            serde_json::json!("abc"),
            serde_json::Value::Null,
        ] {
            let g = edited("150220010", &|w| {
                w["stan_wody"] = "500".into();
                w["stan_ostrzegawczy"] = junk.clone();
                w["stan_alarmowy"] = serde_json::Value::Null;
            });
            let r = g.iter().find(|g| g.id == "150220010").unwrap();
            assert_eq!((r.warning_cm, r.alarm_cm), (None, None), "{junk}");
            assert_eq!(r.exceedance(), None, "{junk}");
        }
        // Sam próg alarmowy bez ostrzegawczego też działa.
        let g = edited("150220010", &|w| {
            w["stan_wody"] = "500".into();
            w["stan_ostrzegawczy"] = serde_json::Value::Null;
        });
        let r = g.iter().find(|g| g.id == "150220010").unwrap();
        assert_eq!(r.exceedance(), Some(Exceedance::Alarm));
    }

    #[test]
    fn nearby_gauges_by_radius_or_list() {
        let g = gauges();
        let ids = |s: &[(&str, &str)]| {
            selected(
                &g,
                (50.04, 21.99),
                radius_km(&map(s)),
                &station_ids(&map(s)),
            )
            .iter()
            .map(|(g, _)| g.id.clone())
            .collect::<Vec<_>>()
        };
        assert_eq!(ids(&[]), ["150220010", "149210130", "149210120"]);
        assert_eq!(ids(&[("radius_km", "30")]).len(), 5);
        assert_eq!(ids(&[("radius_km", "3")]), ids(&[]));
        assert_eq!(ids(&[("radius_km", "abc")]), ids(&[]));
        assert_eq!(
            ids(&[("stations", " 149210120, 150220010,x")]),
            ["150220010", "149210120"]
        );
        assert_eq!(station_ids(&map(&[("stations", "2, 1,1,,x1")])), ["1", "2"]);
        let near = selected(&g, (50.04, 21.99), 25, &[]);
        assert!((near[0].1 - 1.8).abs() < 0.1, "{}", near[0].1);
    }

    #[test]
    fn quiet_rzeszow_has_no_items_and_is_complete() {
        let o = to_output(&gauges(), &ctx(&[]));
        assert!(o.items.is_empty() && o.complete);
    }

    #[test]
    fn hel_and_wladyslawowo_exceed() {
        let o = to_output(&gauges(), &ctx(&[("stations", "153180120,154180100")]));
        assert_eq!(o.items.len(), 2);
        assert!(o.complete);
        let hel = item(&o, "153180120");
        assert_eq!(hel.topic, "153180120");
        assert_eq!(
            (hel.fingerprint.as_str(), hel.priority, hel.interrupting),
            ("ostrzegawczy", Priority::Normal, false)
        );
        assert_eq!(
            hel.text,
            "Wodowskaz Hel (Bałtyk): stan wody 566 cm, powyżej stanu ostrzegawczego (550 cm). Pomiar z godz. 20:20. Źródło: IMGW-PIB."
        );
        assert_eq!(hel.summary, "Hel, Bałtyk: stan ostrzegawczy");
        assert_eq!(
            hel.cancel_text,
            "Wodowskaz Hel: stan wody spadł poniżej stanu ostrzegawczego."
        );
        assert_eq!(
            hel.valid_from.unwrap().to_rfc3339(),
            "2026-11-22T19:20:00+00:00"
        );
        assert_eq!(
            hel.valid_until.unwrap().to_rfc3339(),
            "2026-11-23T19:20:00+00:00"
        );
        let wl = item(&o, "154180100");
        assert_eq!(
            (wl.fingerprint.as_str(), wl.priority, wl.interrupting),
            ("alarmowy", Priority::High, false)
        );
        assert!(
            wl.text.contains("powyżej stanu alarmowego (570 cm)"),
            "{}",
            wl.text
        );
        assert!(
            wl.change_text
                .starts_with("Zmiana stanu wody. Wodowskaz Władysławowo"),
            "{}",
            wl.change_text
        );
        assert_eq!(wl.change_text, format!("Zmiana stanu wody. {}", wl.text));
    }

    #[test]
    fn a_gauge_without_a_river_is_named_alone() {
        let o = to_output(&gauges(), &ctx(&[("stations", "154180140")]));
        let g = item(&o, "154180140");
        assert!(
            g.text
                .starts_with("Wodowskaz Gdańsk: stan wody 570 cm, powyżej stanu alarmowego"),
            "{}",
            g.text
        );
        assert_eq!(g.summary, "Gdańsk: stan alarmowy");
    }

    #[test]
    fn a_reading_older_than_three_hours_is_skipped() {
        let o = to_output(
            &gauges(),
            &ctx_full("2026-11-22T22:30:00Z", &[("stations", "153180120")], &[]),
        );
        assert!(o.items.is_empty());
        assert!(o.complete);
        // Pomiar „z przyszłości” też jest świeży.
        let o = to_output(
            &gauges(),
            &ctx_full("2026-11-22T18:00:00Z", &[("stations", "153180120")], &[]),
        );
        assert_eq!(o.items.len(), 1);
    }

    #[test]
    fn a_tracked_gauge_without_a_fresh_reading_makes_the_fetch_incomplete() {
        let settings = [("stations", "153180120,154180100")];
        let run = |g: &[Gauge]| {
            to_output(
                g,
                &ctx_full("2026-11-22T20:00:00Z", &settings, &["153180120"]),
            )
        };
        let old = edited("153180120", &|w| {
            w["stan_wody_data_pomiaru"] = "2026-11-22 17:00:00".into()
        });
        let null = edited("153180120", &|w| w["stan_wody"] = serde_json::Value::Null);
        let mut gone = gauges();
        gone.retain(|g| g.id != "153180120");
        for g in [old, null, gone] {
            let o = run(&g);
            assert!(!o.complete);
            assert!(o.items.iter().all(|i| i.key != "153180120"));
            assert!(o.items.iter().any(|i| i.key == "154180100"));
        }
        // Progi zniknęły z odpowiedzi ("", null, "0"): to nie jest „poniżej
        // progu”, więc odpowiedź niekompletna, bez fałszywego odwołania.
        for junk in [
            serde_json::json!(""),
            serde_json::Value::Null,
            serde_json::json!("0"),
        ] {
            let no_thresholds = edited("153180120", &|w| {
                w["stan_ostrzegawczy"] = junk.clone();
                w["stan_alarmowy"] = junk.clone();
            });
            let o = run(&no_thresholds);
            assert!(!o.complete, "{junk}");
            assert!(o.items.iter().all(|i| i.key != "153180120"));
        }
        // Świeży pomiar poniżej progu: odwołanie możliwe, odpowiedź kompletna.
        let below = edited("153180120", &|w| w["stan_wody"] = "500".into());
        let o = run(&below);
        assert!(o.complete);
        assert!(o.items.iter().all(|i| i.key != "153180120"));
        // Nieśledzona stara stacja nie psuje kompletności.
        let old = edited("154180100", &|w| {
            w["stan_wody_data_pomiaru"] = "2026-11-22 17:00:00".into()
        });
        assert!(run(&old).complete);
    }

    #[test]
    fn an_alarm_only_gauge_cancels_below_the_alarm_level() {
        let g = edited("153180120", &|w| {
            w["stan_ostrzegawczy"] = serde_json::Value::Null;
            w["stan_wody"] = "575".into();
        });
        let o = to_output(&g, &ctx(&[("stations", "153180120")]));
        assert_eq!(
            item(&o, "153180120").cancel_text,
            "Wodowskaz Hel: stan wody spadł poniżej stanu alarmowego."
        );
        let o = to_output(&gauges(), &ctx(&[("stations", "154180100")]));
        assert_eq!(
            item(&o, "154180100").cancel_text,
            "Wodowskaz Władysławowo: stan wody spadł poniżej stanu ostrzegawczego."
        );
    }

    #[test]
    fn a_nonexistent_warsaw_time_is_no_reading() {
        let g = edited("153180120", &|w| {
            w["stan_wody_data_pomiaru"] = "2026-03-29 02:30:00".into()
        });
        let o = to_output(
            &g,
            &ctx_full(
                "2026-03-29T02:00:00Z",
                &[("stations", "153180120")],
                &["153180120"],
            ),
        );
        assert!(o.items.is_empty() && !o.complete);
    }

    #[test]
    fn an_array_without_any_gauge_is_a_parse_error() {
        for bad in [
            r#"[{"x":1}]"#,
            r#"{"message":"Brak danych"}"#,
            "<html>",
            "42",
        ] {
            let err = parse::parse_gauges(bad.as_bytes()).unwrap_err();
            assert!(matches!(err, SourceError::Parse(_)), "{bad}: {err:?}");
        }
        assert_eq!(parse::parse_gauges(b"[]").unwrap(), Vec::<Gauge>::new());
    }

    #[test]
    fn the_scope_names_position_radius_and_list() {
        let src = ImgwWodowskazy::new(DEFAULT_URL).unwrap();
        assert_eq!(
            src.scope(&ctx(&[("stations", "150220010, 149210120")])),
            "50.04,21.99|r=25|s=149210120,150220010"
        );
        assert_eq!(
            src.scope(&ctx(&[("radius_km", "40")])),
            "50.04,21.99|r=40|s="
        );
    }

    #[test]
    fn texts_are_speakable() {
        let g = gauges();
        let ids: Vec<&str> = g
            .iter()
            .filter(|g| g.exceedance().is_some())
            .map(|g| g.id.as_str())
            .collect();
        let o = to_output(&g, &ctx(&[("stations", &ids.join(","))]));
        assert_eq!(o.items.len(), 17);
        for i in &o.items {
            crate::speech::assert_speakable(&[&i.text, &i.change_text, &i.cancel_text]);
        }
        let part = speech::bulletin(
            &selected(&g, (50.04, 21.99), 25, &[])
                .into_iter()
                .map(|(g, _)| (g, g.level_cm.unwrap()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        crate::speech::assert_speakable(&[&part]);
    }

    #[test]
    fn a_tracked_gauge_keeps_its_level_within_the_hysteresis() {
        // Gdańsk: ostrzegawczy 550, alarmowy 570.
        let run = |level: &str, held: Option<&str>, edit: &dyn Fn(&mut serde_json::Value)| {
            let g = edited("154180140", &|w| {
                w["stan_wody"] = level.into();
                edit(w);
            });
            let mut c = ctx_full("2026-11-22T20:00:00Z", &[("stations", "154180140")], &[]);
            if let Some(held) = held {
                c.tracked_keys.insert("154180140".to_owned());
                c.tracked.insert("154180140".to_owned(), held.to_owned());
            }
            let o = to_output(&g, &c);
            assert!(o.complete);
            o.items.first().map(|i| i.fingerprint.clone())
        };
        let none = &|_: &mut serde_json::Value| {};
        // Bez śledzenia: zwykłe progi.
        assert_eq!(run("569", None, none).as_deref(), Some("ostrzegawczy"));
        assert_eq!(run("549", None, none), None);
        // Śledzony alarm nie spada do ostrzegawczego i trzyma się do 545 cm.
        assert_eq!(
            run("569", Some("alarmowy"), none).as_deref(),
            Some("alarmowy")
        );
        assert_eq!(
            run("545", Some("alarmowy"), none).as_deref(),
            Some("alarmowy")
        );
        assert_eq!(run("544", Some("alarmowy"), none), None);
        // Śledzony ostrzegawczy rośnie do alarmowego.
        assert_eq!(
            run("571", Some("ostrzegawczy"), none).as_deref(),
            Some("alarmowy")
        );
        assert_eq!(
            run("547", Some("ostrzegawczy"), none).as_deref(),
            Some("ostrzegawczy")
        );
        // Stacja z samym alarmowym: histereza od alarmowego.
        let alarm_only =
            &|w: &mut serde_json::Value| w["stan_ostrzegawczy"] = serde_json::Value::Null;
        assert_eq!(
            run("565", Some("alarmowy"), alarm_only).as_deref(),
            Some("alarmowy")
        );
        assert_eq!(run("564", Some("alarmowy"), alarm_only), None);
        // Nieznany odcisk w bazie: zwykłe progi.
        assert_eq!(run("546", Some("coś"), none), None);
    }

    #[test]
    fn water_body_names_are_spelled_out() {
        let g = gauges();
        let river = |id: &str| {
            g.iter()
                .find(|g| g.id == id)
                .unwrap()
                .river
                .clone()
                .unwrap()
        };
        assert_eq!(river("154210060"), "Jezioro Mamry");
        assert_eq!(river("152210150"), "Zbiornik Dębe");
        assert_eq!(river("152230110"), "Zbiornik Siemianówka");
        assert_eq!(river("153220190"), "Jezioro Rajgrodzkie");
        assert_eq!(river("153160040"), "Jezioro Drawsko");
        assert_eq!(river("154170140"), "Jezioro Jasień Południowe");
        assert_eq!(river("153220140"), "Ełk (Kanał Rudzki)");
        assert_eq!(river("154180100"), "Bałtyk");
        assert_eq!(
            parse::water_name("Jez.  Pn. Kanał. X"),
            "Jezioro Północne Kanał X"
        );
    }

    #[test]
    fn lakes_reservoirs_and_the_sea_are_not_rivers() {
        let forced = |id: &str, level: &str| {
            let g = edited(id, &|w| w["stan_wody"] = level.into());
            to_output(&g, &ctx(&[("stations", id)]))
        };
        let mamry = forced("154210060", "170");
        let mamry = item(&mamry, "154210060");
        assert!(
            mamry
                .text
                .starts_with("Wodowskaz Przystań (Jezioro Mamry): stan wody 170 cm, powyżej stanu ostrzegawczego (160 cm)."),
            "{}",
            mamry.text
        );
        assert_eq!(mamry.summary, "Przystań, Jezioro Mamry: stan ostrzegawczy");
        let debe = forced("152210150", "380");
        assert!(
            item(&debe, "152210150").text.starts_with(
                "Wodowskaz Popowo (Zbiornik Dębe): stan wody 380 cm, powyżej stanu alarmowego"
            ),
            "{}",
            item(&debe, "152210150").text
        );
        let o = to_output(&gauges(), &ctx(&[("stations", "153180120")]));
        let hel = item(&o, "153180120");
        assert!(
            hel.text
                .starts_with("Wodowskaz Hel (Bałtyk): stan wody 566 cm"),
            "{}",
            hel.text
        );
        // Zwykła rzeka dalej „na rzece”.
        let rzeszow = forced("150220010", "310");
        assert!(
            item(&rzeszow, "150220010")
                .text
                .starts_with("Wodowskaz Rzeszów na rzece Wisłok: stan wody 310 cm"),
            "{}",
            item(&rzeszow, "150220010").text
        );
        assert_eq!(
            item(&rzeszow, "150220010").summary,
            "Rzeszów, Wisłok: stan ostrzegawczy"
        );
    }

    #[test]
    fn texts_for_every_gauge_in_the_fixture_are_speakable() {
        let g = gauges();
        assert_eq!(g.len(), 913);
        for gauge in &g {
            // Wymuszone przekroczenie: stan nad progiem, próg zawsze jest.
            let mut forced = gauge.clone();
            let warning = forced.warning_cm.unwrap_or(300.0);
            forced.warning_cm = Some(warning);
            forced.level_cm = Some(warning + 1.0);
            let e = forced.exceedance().unwrap();
            let text = speech::announcement(&forced, e, warning + 1.0, at("2026-11-22T19:20:00Z"));
            let part = speech::bulletin(&[(&forced, warning + 1.0)]).unwrap();
            crate::speech::assert_speakable(&[
                &text,
                &speech::change(&text),
                &speech::cancellation(&forced),
                &speech::summary(&forced, e),
                &part,
            ]);
        }
    }

    mod part_tests {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn server(expect: u64) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/hydro"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(FIX, "application/json"))
                .expect(expect)
                .mount(&server)
                .await;
            server
        }

        fn source(server: &MockServer) -> ImgwWodowskazy {
            ImgwWodowskazy::with_timeout(&format!("{}/hydro", server.uri()), Duration::from_secs(5))
                .unwrap()
        }

        fn part_ctx(now: &str, pos: Option<StationPosition>, sources: &[(&str, &str)]) -> PartCtx {
            PartCtx {
                now: at(now),
                position: pos,
                settings: BTreeMap::new(),
                sources: map(sources),
                active_alerts: Vec::new(),
                alert_sources: Default::default(),
                warnings_max_chars: None,
            }
        }

        async fn text(src: &ImgwWodowskazy, ctx: &PartCtx) -> String {
            match src.compose(ctx).await.unwrap() {
                PartText::Text(t) => t,
                PartText::Empty(why) => panic!("pusta część: {why}"),
            }
        }

        #[tokio::test]
        async fn the_source_fetches_and_reads_the_list() {
            let server = server(1).await;
            let src = source(&server);
            let out = src.fetch(&ctx(&[("stations", "153180120")])).await.unwrap();
            assert_eq!(out.items.len(), 1);
            assert_eq!(DataSource::id(&src), "imgw-wodowskazy");
            assert_eq!(BulletinPart::id(&src), "wody");
            assert_eq!(src.max_chars(), 400);
            assert_eq!(src.needs(), Needs::Position);
            assert_eq!(src.default_interval(), Duration::from_secs(900));
        }

        #[tokio::test]
        async fn the_part_names_the_three_nearest_gauges() {
            let server = server(1).await;
            let t = text(
                &source(&server),
                &part_ctx("2026-11-22T20:00:00Z", Some(position()), &[]),
            )
            .await;
            assert_eq!(
                t,
                "Stany wód. Rzeszów, Wisłok: 72 cm. Żarnowa, Wisłok: 164 cm. Godowa, Stobnica: 609 cm. Źródło: IMGW-PIB."
            );
        }

        #[tokio::test]
        async fn the_part_marks_exceedances_and_reads_source_settings() {
            let server = server(1).await;
            let t = text(
                &source(&server),
                &part_ctx(
                    "2026-11-22T20:00:00Z",
                    Some(position()),
                    &[
                        ("imgw-wodowskazy:stations", "153180120,154180100"),
                        ("imgw-meteo:area", "18"),
                    ],
                ),
            )
            .await;
            assert!(t.contains("Hel, Bałtyk: 566 cm, stan ostrzegawczy."), "{t}");
            assert!(
                t.contains("Władysławowo, Bałtyk: 572 cm, stan alarmowy."),
                "{t}"
            );
        }

        #[tokio::test]
        async fn the_part_reuses_a_recent_fetch_of_the_source() {
            let server = server(1).await;
            let src = source(&server);
            src.fetch(&ctx(&[])).await.unwrap();
            let t = text(
                &src,
                &part_ctx("2026-11-22T20:10:00Z", Some(position()), &[]),
            )
            .await;
            assert!(t.starts_with("Stany wód. Rzeszów, Wisłok: 72 cm."), "{t}");
        }

        #[tokio::test]
        async fn an_old_fetch_is_refreshed_by_the_part() {
            let server = server(2).await;
            let src = source(&server);
            src.fetch(&ctx(&[])).await.unwrap();
            text(
                &src,
                &part_ctx("2026-11-22T20:30:00Z", Some(position()), &[]),
            )
            .await;
        }

        #[tokio::test]
        async fn without_position_or_fresh_gauges_the_part_is_empty() {
            let server = server(1).await;
            let src = source(&server);
            assert_eq!(
                src.compose(&part_ctx("2026-11-22T20:00:00Z", None, &[]))
                    .await
                    .unwrap(),
                PartText::Empty("brak pozycji stacji")
            );
            assert_eq!(
                src.compose(&part_ctx("2026-11-23T12:00:00Z", Some(position()), &[]))
                    .await
                    .unwrap(),
                PartText::Empty("brak świeżych pomiarów wodowskazów")
            );
        }
    }
}
