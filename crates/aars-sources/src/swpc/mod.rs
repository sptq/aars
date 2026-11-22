//! Źródło pogody kosmicznej NOAA SWPC (skale G, S, R).

pub mod parse;
pub mod speech;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use aars_core::module::{ModuleDescriptor, ModuleKind, Needs, SettingKind, SettingSpec, Toggle};
use aars_core::priority::Priority;
use chrono::{DateTime, TimeDelta, Utc};

use crate::http;
use crate::source::{DataSource, FetchFuture, SourceCtx, SourceError, SourceItem, SourceOutput};
use parse::Scales;

pub const MAX_BODY: usize = 64 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
/// Po tylu godzinach poniżej progu epizod się kończy (A9).
const LINGER: TimeDelta = TimeDelta::hours(3);
/// Starsza ramka danych to nieaktualne dane.
const MAX_AGE: TimeDelta = TimeDelta::hours(3);
const DEFAULTS: (u8, u8, u8) = (3, 2, 3);

pub const DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
    id: "swpc",
    name: "Pogoda kosmiczna NOAA",
    description: "Burze geomagnetyczne (G), radiacyjne (S) i zaniki łączności KF (R) według skal NOAA SWPC.",
    kind: ModuleKind::Source,
    toggle: Toggle::Switchable,
    default_enabled: false,
    needs: Needs::Nothing,
    settings: &[
        SettingSpec {
            key: "g_min",
            label: "Najniższy poziom G do ogłoszenia",
            help: "Burze geomagnetyczne od tego poziomu (1–5) są ogłaszane.",
            kind: SettingKind::Integer { min: 1, max: 5 },
            default: "3",
        },
        SettingSpec {
            key: "s_min",
            label: "Najniższy poziom S do ogłoszenia",
            help: "Burze radiacyjne od tego poziomu (1–5) są ogłaszane.",
            kind: SettingKind::Integer { min: 1, max: 5 },
            default: "2",
        },
        SettingSpec {
            key: "r_min",
            label: "Najniższy poziom R do ogłoszenia",
            help: "Zaniki łączności radiowej od tego poziomu (1–5) są ogłaszane.",
            kind: SettingKind::Integer { min: 1, max: 5 },
            default: "3",
        },
        SettingSpec {
            key: "interval_s",
            label: "Interwał odpytywania (s)",
            help: "Jak często odpytywać źródło.",
            kind: SettingKind::Integer { min: 60, max: 3600 },
            default: "300",
        },
    ],
    see_also: None,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    pub peak: u8,
    pub below_since: Option<DateTime<Utc>>,
}

/// Epizody według skali (`'G'`, `'S'`, `'R'`) — pamięć między cyklami (A9).
pub type Episodes = BTreeMap<char, Episode>;

pub struct Swpc {
    client: reqwest::Client,
    url: String,
    episodes: Mutex<Episodes>,
}

impl Swpc {
    pub fn new(url: &str) -> Result<Self, SourceError> {
        Self::with_timeout(url, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(url: &str, timeout: Duration) -> Result<Self, SourceError> {
        Ok(Self {
            client: http::client(timeout)?,
            url: url.to_owned(),
            episodes: Mutex::new(Episodes::new()),
        })
    }
}

fn threshold(ctx: &SourceCtx, key: &str, default: u8) -> u8 {
    ctx.settings
        .get(key)
        .and_then(|v| v.trim().parse::<u8>().ok())
        .filter(|n| (1..=5).contains(n))
        .unwrap_or(default)
}

fn thresholds(ctx: &SourceCtx) -> (u8, u8, u8) {
    (
        threshold(ctx, "g_min", DEFAULTS.0),
        threshold(ctx, "s_min", DEFAULTS.1),
        threshold(ctx, "r_min", DEFAULTS.2),
    )
}

impl DataSource for Swpc {
    fn id(&self) -> &'static str {
        "swpc"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(300)
    }

    fn needs(&self) -> Needs {
        Needs::Nothing
    }

    fn scope(&self, ctx: &SourceCtx) -> String {
        let (g, s, r) = thresholds(ctx);
        format!("g={g}|s={s}|r={r}")
    }

    fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a> {
        Box::pin(async move {
            let body = http::get_limited(&self.client, &self.url, MAX_BODY).await?;
            let scales = parse::parse_scales(&body)?;
            // Blokada tylko wokół czystej funkcji, nigdy przez await.
            let mut episodes = self
                .episodes
                .lock()
                // Zatruty Mutex: stan epizodów jest prosty, więc go odzyskujemy.
                .unwrap_or_else(|e| e.into_inner());
            if ctx.now - scales.stamp > MAX_AGE {
                return Err(SourceError::Parse("nieaktualne dane SWPC".to_owned()));
            }
            Ok(to_output(&scales, ctx, &mut episodes))
        })
    }
}

/// Czysta funkcja: skale → pozycje, z epizodami z poprzednich cykli (A9).
pub fn to_output(scales: &Scales, ctx: &SourceCtx, episodes: &mut Episodes) -> SourceOutput {
    let (g_min, s_min, r_min) = thresholds(ctx);
    let mut items = Vec::new();
    for (scale, level, min) in [
        ('G', scales.g, g_min),
        ('S', scales.s, s_min),
        ('R', scales.r, r_min),
    ] {
        if level >= min {
            let ep = episodes.entry(scale).or_insert(Episode {
                peak: level,
                below_since: None,
            });
            ep.peak = ep.peak.max(level);
            ep.below_since = None;
        } else if let Some(ep) = episodes.get_mut(&scale) {
            let since = *ep.below_since.get_or_insert(ctx.now);
            if ctx.now - since >= LINGER {
                episodes.remove(&scale);
            }
        }
        // Próg podniesiony ponad szczyt epizodu: epizod się kończy (inaczej
        // wróciłby jako nowe ogłoszenie przy nowym zakresie).
        if episodes.get(&scale).is_some_and(|ep| ep.peak < min) {
            episodes.remove(&scale);
        }
        let Some(ep) = episodes.get(&scale) else {
            continue;
        };
        let peak = ep.peak;
        items.push(SourceItem {
            key: scale.to_string(),
            topic: scale.to_string(),
            fingerprint: format!("{scale}{peak}"),
            text: speech::announcement(scale, peak),
            change_text: speech::change(scale, peak),
            cancel_text: speech::cancellation(scale),
            summary: format!("{scale}{peak} — {}", speech::label(scale, peak)),
            priority: Priority::High,
            interrupting: false,
            valid_from: None,
            valid_until: None,
        });
    }
    SourceOutput {
        items,
        complete: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::Value;

    const QUIET: &[u8] = include_bytes!("../../tests/fixtures/swpc/noaa-scales-2026-11-22.json");

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 22, h, m, 0).unwrap()
    }

    fn ctx_at(now: DateTime<Utc>, settings: &[(&str, &str)]) -> SourceCtx {
        SourceCtx {
            now,
            area: Vec::new(),
            area_text: String::new(),
            position: None,
            settings: settings
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            tracked_keys: Default::default(),
            tracked: Default::default(),
        }
    }

    fn ctx() -> SourceCtx {
        ctx_at(at(15, 0), &[])
    }

    fn body(edit: impl FnOnce(&mut Value)) -> Vec<u8> {
        let mut v: Value = serde_json::from_slice(QUIET).unwrap();
        edit(&mut v);
        serde_json::to_vec(&v).unwrap()
    }

    fn with_g(g: &str) -> Vec<u8> {
        body(|v| v["0"]["G"]["Scale"] = g.into())
    }

    fn run(body: &[u8], ctx: &SourceCtx, ep: &mut Episodes) -> SourceOutput {
        to_output(&parse::parse_scales(body).unwrap(), ctx, ep)
    }

    #[test]
    fn the_descriptor_has_the_documented_settings() {
        let d = &DESCRIPTOR;
        assert_eq!((d.id, d.name), ("swpc", "Pogoda kosmiczna NOAA"));
        assert_eq!(d.needs, Needs::Nothing);
        assert!(!d.default_enabled);
        let keys: Vec<_> = d.settings.iter().map(|s| (s.key, s.default)).collect();
        assert_eq!(
            keys,
            [
                ("g_min", "3"),
                ("s_min", "2"),
                ("r_min", "3"),
                ("interval_s", "300")
            ]
        );
        assert_eq!(d.settings[0].kind, SettingKind::Integer { min: 1, max: 5 });
        assert_eq!(
            d.settings[3].kind,
            SettingKind::Integer { min: 60, max: 3600 }
        );
    }

    #[test]
    fn the_defaults_match_the_descriptor() {
        let d = |k: &str| {
            DESCRIPTOR
                .settings
                .iter()
                .find(|s| s.key == k)
                .map(|s| s.default)
        };
        assert_eq!(d("g_min"), Some(DEFAULTS.0.to_string().as_str()));
        assert_eq!(d("s_min"), Some(DEFAULTS.1.to_string().as_str()));
        assert_eq!(d("r_min"), Some(DEFAULTS.2.to_string().as_str()));
    }

    #[test]
    fn the_quiet_fixture_has_no_items() {
        let out = run(QUIET, &ctx(), &mut Episodes::new());
        assert!(out.items.is_empty());
        assert!(out.complete);
    }

    #[test]
    fn a_g3_above_threshold_is_one_high_item() {
        let out = run(&with_g("3"), &ctx(), &mut Episodes::new());
        assert_eq!(out.items.len(), 1);
        let item = &out.items[0];
        assert_eq!(
            (
                item.key.as_str(),
                item.topic.as_str(),
                item.fingerprint.as_str()
            ),
            ("G", "G", "G3")
        );
        assert_eq!(item.priority, Priority::High);
        assert!(!item.interrupting);
        assert!(item.valid_until.is_none());
        assert!(item.text.contains("Silna burza geomagnetyczna, poziom G3"));
        assert!(item.text.contains("zorza"));
        assert!(item.text.ends_with("Dane: NOAA SWPC."));
        assert!(
            item.change_text
                .starts_with("Wzrost poziomu. Uwaga, pogoda kosmiczna.")
        );
        assert_eq!(
            item.cancel_text,
            "Burza geomagnetyczna wygasła. Dane: NOAA SWPC."
        );
        assert_eq!(item.summary, "G3 — silna burza geomagnetyczna");
    }

    #[test]
    fn s_and_r_have_their_own_texts() {
        let b = body(|v| {
            v["0"]["S"]["Scale"] = "2".into();
            v["0"]["R"]["Scale"] = "3".into();
        });
        let out = run(&b, &ctx(), &mut Episodes::new());
        let keys: Vec<_> = out.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["S", "R"]);
        assert!(out.items[0].text.contains("burza radiacyjna, poziom S2 w skali NOAA. Możliwe zaniki łączności KF w rejonach polarnych."));
        assert!(out.items[1].text.contains("Silne zaniki łączności radiowej, poziom R3 w skali NOAA. Zaniki łączności na pasmach KF po dziennej stronie Ziemi."));
        assert_eq!(
            out.items[1].cancel_text,
            "Zaniki łączności radiowej ustały. Dane: NOAA SWPC."
        );
    }

    #[test]
    fn below_threshold_is_ignored() {
        assert!(
            run(&with_g("2"), &ctx(), &mut Episodes::new())
                .items
                .is_empty()
        );
    }

    #[test]
    fn the_settings_change_the_threshold_and_the_scope() {
        let c = ctx_at(at(15, 0), &[("g_min", "2")]);
        let out = run(&with_g("2"), &c, &mut Episodes::new());
        assert_eq!(out.items.len(), 1);
        let src = Swpc::new("http://127.0.0.1:1/x").unwrap();
        assert_eq!(src.scope(&c), "g=2|s=2|r=3");
        assert_eq!(src.scope(&ctx()), "g=3|s=2|r=3");
        let bad = ctx_at(at(15, 0), &[("g_min", "x"), ("s_min", "9")]);
        assert_eq!(src.scope(&bad), "g=3|s=2|r=3");
    }

    #[test]
    fn an_episode_keeps_its_peak_and_lingers_three_hours() {
        let mut ep = Episodes::new();
        let step = |h, m, g: &str, ep: &mut Episodes| run(&with_g(g), &ctx_at(at(h, m), &[]), ep);
        let fp = |o: &SourceOutput| o.items.first().map(|i| i.fingerprint.clone());
        assert_eq!(fp(&step(15, 0, "3", &mut ep)).as_deref(), Some("G3"));
        assert_eq!(fp(&step(15, 10, "2", &mut ep)).as_deref(), Some("G3"));
        assert_eq!(fp(&step(15, 20, "4", &mut ep)).as_deref(), Some("G4"));
        assert_eq!(fp(&step(15, 30, "2", &mut ep)).as_deref(), Some("G4"));
        assert_eq!(fp(&step(18, 29, "2", &mut ep)).as_deref(), Some("G4"));
        assert_eq!(fp(&step(18, 31, "2", &mut ep)), None);
        assert!(ep.is_empty());
    }

    #[test]
    fn raising_the_threshold_ends_an_episode_below_it() {
        let mut ep = Episodes::new();
        assert_eq!(run(&with_g("3"), &ctx(), &mut ep).items.len(), 1);
        // Próg podniesiony do G4, a burza słabnie: epizod G3 jest poniżej
        // nowego progu, więc nie wraca jako nowe ogłoszenie przy nowym zakresie.
        let raised = ctx_at(at(15, 10), &[("g_min", "4")]);
        assert!(run(&with_g("2"), &raised, &mut ep).items.is_empty());
        assert!(ep.is_empty());
        // Także gdy bieżący poziom wciąż jest G3.
        assert_eq!(run(&with_g("3"), &ctx(), &mut ep).items.len(), 1);
        assert!(run(&with_g("3"), &raised, &mut ep).items.is_empty());
        assert!(ep.is_empty());
    }

    #[test]
    fn r1_and_r5_read_like_this() {
        let r = |level: &str| {
            let c = ctx_at(at(15, 0), &[("r_min", "1")]);
            run(&body(|v| v["0"]["R"]["Scale"] = level.into()), &c, &mut Episodes::new()).items[0]
                .text
                .clone()
        };
        assert_eq!(
            r("1"),
            "Uwaga, pogoda kosmiczna. Słabe zaniki łączności radiowej, poziom R1 w skali NOAA. \
             Zaniki łączności na pasmach KF po dziennej stronie Ziemi. Dane: NOAA SWPC."
        );
        assert_eq!(
            r("5"),
            "Uwaga, pogoda kosmiczna. Ekstremalne zaniki łączności radiowej, poziom R5 w skali NOAA. \
             Zaniki łączności na pasmach KF po dziennej stronie Ziemi. Dane: NOAA SWPC."
        );
    }

    #[test]
    fn a_strong_geomagnetic_storm_mentions_vhf_bands_in_words() {
        let text = &run(&with_g("3"), &ctx(), &mut Episodes::new()).items[0].text;
        assert!(text.contains("łączności przez zorzę na pasmach sześciu i dwóch metrów."), "{text}");
    }

    #[test]
    fn swpc_texts_survive_normalization() {
        let b = body(|v| {
            v["0"]["G"]["Scale"] = "4".into();
            v["0"]["S"]["Scale"] = "2".into();
            v["0"]["R"]["Scale"] = "3".into();
        });
        let out = run(&b, &ctx(), &mut Episodes::new());
        assert_eq!(out.items.len(), 3);
        for i in &out.items {
            crate::speech::assert_speakable(&[&i.text, &i.change_text, &i.cancel_text]);
        }
    }

    #[test]
    fn a_missing_scale_is_a_parse_error() {
        let b = body(|v| {
            v["0"].as_object_mut().unwrap().remove("G");
        });
        assert!(matches!(
            parse::parse_scales(&b),
            Err(SourceError::Parse(_))
        ));
        let b = body(|v| {
            v["0"]["G"].as_object_mut().unwrap().remove("Scale");
        });
        assert!(matches!(
            parse::parse_scales(&b),
            Err(SourceError::Parse(_))
        ));
        assert!(matches!(
            parse::parse_scales(&with_g("x")),
            Err(SourceError::Parse(_))
        ));
    }

    #[test]
    fn a_missing_current_key_is_a_parse_error() {
        let b = body(|v| {
            v.as_object_mut().unwrap().remove("0");
        });
        assert!(matches!(
            parse::parse_scales(&b),
            Err(SourceError::Parse(_))
        ));
        assert!(matches!(
            parse::parse_scales(b"[]"),
            Err(SourceError::Parse(_))
        ));
    }

    #[test]
    fn null_scales_are_zero() {
        let s = parse::parse_scales(&body(|v| v["0"]["R"]["Scale"] = Value::Null)).unwrap();
        assert_eq!((s.r, s.s, s.g), (0, 0, 0));
        assert_eq!(
            s.stamp,
            Utc.with_ymd_and_hms(2026, 11, 22, 14, 59, 0).unwrap()
        );
    }

    mod http_tests {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn server_with(response: ResponseTemplate) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/s.json"))
                .respond_with(response)
                .mount(&server)
                .await;
            server
        }

        fn source(server: &MockServer) -> Swpc {
            Swpc::new(&format!("{}/s.json", server.uri())).unwrap()
        }

        fn ok(b: Vec<u8>) -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_raw(b, "application/json")
        }

        #[tokio::test]
        async fn the_real_fixture_is_fetched_and_parsed() {
            let server = server_with(ok(QUIET.to_vec())).await;
            let src = source(&server);
            let out = src.fetch(&ctx()).await.unwrap();
            assert!(out.items.is_empty() && out.complete);
            assert_eq!(src.id(), "swpc");
            assert_eq!(src.needs(), Needs::Nothing);
            assert_eq!(src.default_interval(), Duration::from_secs(300));
        }

        #[tokio::test]
        async fn episodes_persist_between_fetches() {
            let server = server_with(ok(with_g("3"))).await;
            let src = source(&server);
            assert_eq!(src.fetch(&ctx()).await.unwrap().items.len(), 1);
            server.reset().await;
            Mock::given(method("GET"))
                .and(path("/s.json"))
                .respond_with(ok(with_g("0")))
                .mount(&server)
                .await;
            let out = src.fetch(&ctx_at(at(15, 10), &[])).await.unwrap();
            assert_eq!(out.items.len(), 1);
        }

        #[tokio::test]
        async fn an_old_stamp_is_a_parse_error() {
            let server = server_with(ok(QUIET.to_vec())).await;
            let src = source(&server);
            let err = src.fetch(&ctx_at(at(18, 0), &[])).await.err().unwrap();
            assert!(matches!(err, SourceError::Parse(_)), "{err:?}");
            assert!(src.fetch(&ctx_at(at(17, 20), &[])).await.is_ok());
        }

        #[tokio::test]
        async fn a_server_error_is_a_status_error() {
            let server = server_with(ResponseTemplate::new(503)).await;
            let err = source(&server).fetch(&ctx()).await.err().unwrap();
            assert!(matches!(err, SourceError::Status(503)), "{err:?}");
        }

        #[tokio::test]
        async fn an_oversized_body_is_refused() {
            let server = server_with(ok(vec![b' '; 100 * 1024])).await;
            let err = source(&server).fetch(&ctx()).await.err().unwrap();
            assert!(matches!(err, SourceError::TooLarge(MAX_BODY)), "{err:?}");
        }
    }
}
