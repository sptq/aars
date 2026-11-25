//! Część biuletynu `propagacja`: SFI i Kp z NOAA SWPC, skale NOAA, najbliższa
//! jonosonda KC2G i ocena pasm. Bez źródła w runnerze (burze ogłasza `swpc`).

pub mod parse;
pub mod speech;

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};

use crate::bulletin::{BulletinPart, PartCtx, PartFuture, PartText};
use crate::http;
use crate::source::SourceError;
use crate::speech::{decimal_comma, one_decimal, round_int};
use crate::swpc;

pub const ID: &str = "propagacja";
const F107_MAX_BODY: usize = 256 * 1024;
const KP_MAX_BODY: usize = 64 * 1024;
const KC2G_MAX_BODY: usize = 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const SFI_MAX_AGE: TimeDelta = TimeDelta::hours(36);
const KP_MAX_AGE: TimeDelta = TimeDelta::hours(6);
const SCALES_MAX_AGE: TimeDelta = TimeDelta::hours(6);
/// Dane z przyszłości dalej niż to nie są wiarygodne.
const FUTURE_SLACK: TimeDelta = TimeDelta::hours(1);

pub struct Propagacja {
    client: reqwest::Client,
    f107_url: String,
    kp_url: String,
    scales_url: String,
    kc2g_url: String,
}

fn fresh(stamp: DateTime<Utc>, now: DateTime<Utc>, max_age: TimeDelta) -> bool {
    let age = now - stamp;
    age <= max_age && age >= -FUTURE_SLACK
}

/// Błąd lub dane nie do użycia → zdanie wypada (z ostrzeżeniem w logu).
fn or_drop<T>(what: &str, r: Result<Option<T>, SourceError>) -> Option<T> {
    match r {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(część = ID, dane = what, błąd = %e, "pomijam zdanie propagacji");
            None
        }
    }
}

impl Propagacja {
    pub fn new(cfg: &aars_core::config::SourcesConfig) -> Result<Self, SourceError> {
        Self::with_urls(
            &cfg.swpc_f107_url,
            &cfg.swpc_kp_url,
            &cfg.swpc_url,
            &cfg.kc2g_url,
            DEFAULT_TIMEOUT,
        )
    }

    pub fn with_urls(
        f107: &str,
        kp: &str,
        scales: &str,
        kc2g: &str,
        timeout: Duration,
    ) -> Result<Self, SourceError> {
        Ok(Self {
            client: http::client(timeout)?,
            f107_url: f107.to_owned(),
            kp_url: kp.to_owned(),
            scales_url: scales.to_owned(),
            kc2g_url: kc2g.to_owned(),
        })
    }

    async fn sfi(&self, now: DateTime<Utc>) -> Result<Option<String>, SourceError> {
        let body = http::get_limited(&self.client, &self.f107_url, F107_MAX_BODY).await?;
        let (stamp, sfi) = parse::latest_f107(&body)?;
        Ok(fresh(stamp, now, SFI_MAX_AGE).then(|| {
            format!(
                "Strumień słoneczny {}, {}.",
                round_int(sfi),
                speech::sfi_word(sfi)
            )
        }))
    }

    async fn kp(&self, now: DateTime<Utc>) -> Result<Option<String>, SourceError> {
        let body = http::get_limited(&self.client, &self.kp_url, KP_MAX_BODY).await?;
        let (stamp, kp) = parse::latest_kp(&body)?;
        if !fresh(stamp, now, KP_MAX_AGE) {
            return Ok(None);
        }
        let tenths = round_int(kp * 10.0);
        let r = decimal_comma(tenths as f64 / 10.0);
        Ok(Some(match tenths {
            ..=29 => format!("Indeks K {r}, warunki spokojne."),
            30..=49 => format!("Indeks K {r}, warunki niespokojne."),
            _ => format!("Indeks K {r}, burza geomagnetyczna."),
        }))
    }

    async fn scales(&self, now: DateTime<Utc>) -> Result<Option<String>, SourceError> {
        let body = http::get_limited(&self.client, &self.scales_url, swpc::MAX_BODY).await?;
        let s = swpc::parse::parse_scales(&body)?;
        if !fresh(s.stamp, now, SCALES_MAX_AGE) {
            return Ok(None);
        }
        let labels: Vec<String> = [('G', s.g), ('S', s.s), ('R', s.r)]
            .into_iter()
            .filter(|(_, level)| *level > 0)
            .map(|(scale, level)| swpc::speech::label(scale, level))
            .collect();
        Ok(Some(if labels.is_empty() {
            "Pogoda kosmiczna bez zaburzeń.".to_owned()
        } else {
            format!("Pogoda kosmiczna: {}.", labels.join(", "))
        }))
    }

    async fn ionosonde(
        &self,
        from: (f64, f64),
        now: DateTime<Utc>,
    ) -> Result<Option<String>, SourceError> {
        let body = http::get_limited(&self.client, &self.kc2g_url, KC2G_MAX_BODY).await?;
        let all = parse::ionosondes(&body)?;
        let Some((s, _km)) = parse::pick_ionosonde(&all, from, now) else {
            return Ok(None);
        };
        let (Some(fof2), Some(mufd)) = (s.fof2, s.mufd) else {
            return Ok(None);
        };
        let mut text = format!(
            "Jonosonda {}: częstotliwość krytyczna {} megaherca, MUF {} megaherca.",
            speech::city(&s.name),
            one_decimal(fof2),
            one_decimal(mufd)
        );
        match (speech::nvis_band(fof2), speech::dx_band(mufd)) {
            (Some(n), Some(m)) => text.push_str(&format!(
                " Łączności bliskie do pasma {n} metrów, dalekie do pasma {m} metrów."
            )),
            (Some(n), None) => text.push_str(&format!(" Łączności bliskie do pasma {n} metrów.")),
            (None, Some(m)) => text.push_str(&format!(" Łączności dalekie do pasma {m} metrów.")),
            (None, None) => {}
        }
        Ok(Some(text))
    }
}

impl BulletinPart for Propagacja {
    fn id(&self) -> &'static str {
        ID
    }

    fn max_chars(&self) -> usize {
        500
    }

    fn compose<'a>(&'a self, ctx: &'a PartCtx) -> PartFuture<'a> {
        Box::pin(async move {
            let now = ctx.now;
            let from = ctx.position.as_ref().map(|p| (p.lat, p.lon));
            let iono = async {
                match from {
                    Some(from) => self.ionosonde(from, now).await,
                    None => Ok(None),
                }
            };
            let (sfi, kp, scales, iono) =
                tokio::join!(self.sfi(now), self.kp(now), self.scales(now), iono);
            let noaa: Vec<String> = [
                or_drop("SFI", sfi),
                or_drop("Kp", kp),
                or_drop("skale NOAA", scales),
            ]
            .into_iter()
            .flatten()
            .collect();
            let iono = or_drop("jonosonda KC2G", iono);
            let attribution = match (noaa.is_empty(), iono.is_some()) {
                (true, false) => return Ok(PartText::Empty("brak danych propagacji")),
                (false, true) => "Dane: NOAA, GIRO przez KC2G.",
                (false, false) => "Dane: NOAA.",
                (true, true) => "Dane: GIRO przez KC2G.",
            };
            let mut parts = vec!["Propagacja.".to_owned()];
            parts.extend(noaa);
            parts.extend(iono);
            parts.push(attribution.to_owned());
            Ok(PartText::Text(parts.join(" ")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::parse::*;
    use super::speech::*;
    use super::*;
    use crate::speech::assert_speakable;
    use aars_core::geo::StationPosition;
    use chrono::{DateTime, Utc};
    use std::collections::BTreeMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const F107: &[u8] = include_bytes!("../../tests/fixtures/propagacja/f107-2026-11-22.json");
    const KP: &[u8] = include_bytes!("../../tests/fixtures/propagacja/kp-2026-11-22.json");
    const KC2G: &[u8] =
        include_bytes!("../../tests/fixtures/propagacja/kc2g-stations-2026-11-22.json");
    const SCALES: &[u8] = include_bytes!("../../tests/fixtures/swpc/noaa-scales-2026-11-22.json");
    const NOW: &str = "2026-11-22T19:00:00Z";
    const RZESZOW: (f64, f64) = (50.04, 21.99);

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn json(f: impl FnOnce(&mut serde_json::Value), body: &[u8]) -> Vec<u8> {
        let mut v: serde_json::Value = serde_json::from_slice(body).unwrap();
        f(&mut v);
        serde_json::to_vec(&v).unwrap()
    }

    fn station<'a>(v: &'a mut serde_json::Value, name: &str) -> &'a mut serde_json::Value {
        v.as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["station"]["name"] == name)
            .unwrap()
    }

    #[test]
    fn the_fixtures_give_the_latest_values() {
        assert_eq!(
            latest_f107(F107).unwrap(),
            (at("2026-11-21T22:00:00Z"), 98.0)
        );
        assert_eq!(latest_kp(KP).unwrap(), (at("2026-11-22T15:00:00Z"), 0.33));
        let reversed = json(|v| v.as_array_mut().unwrap().reverse(), F107);
        assert_eq!(
            latest_f107(&reversed).unwrap(),
            (at("2026-11-21T22:00:00Z"), 98.0)
        );
        assert!(matches!(latest_f107(b"[]"), Err(SourceError::Parse(_))));
        assert!(matches!(latest_kp(b"[]"), Err(SourceError::Parse(_))));
        assert!(matches!(
            latest_kp(b"{\"a\":1}"),
            Err(SourceError::Parse(_))
        ));
    }

    #[test]
    fn longitudes_over_180_wrap_and_string_coordinates_parse() {
        let all = ionosondes(KC2G).unwrap();
        let austin = all.iter().find(|s| s.name == "Austin, TX, USA").unwrap();
        assert!((austin.lon - -97.7).abs() < 1e-9, "{}", austin.lon);
        let warsaw = all.iter().find(|s| s.name == "Warsaw, Poland").unwrap();
        assert_eq!((warsaw.lat, warsaw.lon), (52.2, 21.1));
        assert_eq!(warsaw.time, at("2026-11-22T18:15:00Z"));
        assert_eq!(warsaw.cs, -1.0);
        assert_eq!((warsaw.fof2, warsaw.mufd), (Some(3.2), Some(10.56)));
        let kal = all
            .iter()
            .find(|s| s.name.starts_with("Kaliningrad"))
            .unwrap();
        assert_eq!(kal.mufd, None);

        assert_eq!(all.len(), 101);
        let broken = json(
            |v| station(v, "Warsaw, Poland")["station"]["latitude"] = "x".into(),
            KC2G,
        );
        let rest = ionosondes(&broken).unwrap();
        assert_eq!(rest.len(), 100);
        assert!(rest.iter().all(|s| s.name != "Warsaw, Poland"));
        assert!(matches!(
            ionosondes(b"{\"a\":1}"),
            Err(SourceError::Parse(_))
        ));
    }

    #[test]
    fn warsaw_is_picked_for_rzeszow() {
        let all = ionosondes(KC2G).unwrap();
        let (s, km) = pick_ionosonde(&all, RZESZOW, at(NOW)).unwrap();
        assert_eq!(s.name, "Warsaw, Poland");
        assert!((240.0..256.0).contains(&km), "{km}");
    }

    #[test]
    fn the_filter_comes_before_the_distance() {
        let pick = |body: &[u8], now: &str, from| {
            let all = ionosondes(body).unwrap();
            pick_ionosonde(&all, from, at(now)).map(|(s, _)| s.name.clone())
        };
        let bad_cs = json(|v| station(v, "Warsaw, Poland")["cs"] = 50.into(), KC2G);
        assert_eq!(
            pick(&bad_cs, NOW, RZESZOW).as_deref(),
            Some("Pruhonice, Czech Republic")
        );
        assert_eq!(
            pick(KC2G, "2026-11-22T19:20:00Z", RZESZOW).as_deref(),
            Some("Pruhonice, Czech Republic")
        );
        let no_fof2 = json(
            |v| {
                station(v, "Warsaw, Poland")["cs"] = 50.into();
                station(v, "Pruhonice, Czech Republic")["fof2"] = serde_json::Value::Null;
            },
            KC2G,
        );
        assert_eq!(
            pick(&no_fof2, "2026-11-22T19:20:00Z", RZESZOW).as_deref(),
            Some("Rome, Italy RO041")
        );
        assert_eq!(pick(KC2G, NOW, (0.0, 0.0)), None);
    }

    #[test]
    fn band_words_follow_the_fixed_thresholds() {
        assert_eq!(nvis_band(3.2), Some(160));
        assert_eq!(nvis_band(7.5), Some(40));
        assert_eq!(nvis_band(1.5), None);
        assert_eq!(dx_band(10.56), Some(40));
        assert_eq!(dx_band(11.814), Some(40));
        assert_eq!(dx_band(17.0), Some(20));
        assert_eq!(dx_band(4.0), None);
        assert_eq!(sfi_word(89.9), "niski");
        assert_eq!(sfi_word(90.0), "umiarkowany");
        assert_eq!(sfi_word(150.0), "wysoki");
        assert_eq!(city("Rome, Italy RO041"), "Rzym");
        assert_eq!(city("Gibilmanna, Italy"), "Gibilmanna");
        assert_eq!(city("IS141"), "IS141");
        assert_eq!(city("Warsaw, Poland"), "Warszawa");
    }

    struct Bodies {
        f107: Option<Vec<u8>>,
        kp: Option<Vec<u8>>,
        scales: Option<Vec<u8>>,
        kc2g: Option<Vec<u8>>,
    }

    fn all_ok() -> Bodies {
        Bodies {
            f107: Some(F107.to_vec()),
            kp: Some(KP.to_vec()),
            scales: Some(SCALES.to_vec()),
            kc2g: Some(KC2G.to_vec()),
        }
    }

    async fn run(bodies: Bodies, now: &str, pos: Option<(f64, f64)>) -> PartText {
        let server = MockServer::start().await;
        for (p, b) in [
            ("/f107", bodies.f107),
            ("/kp", bodies.kp),
            ("/scales", bodies.scales),
            ("/kc2g", bodies.kc2g),
        ] {
            let resp = match b {
                Some(b) => ResponseTemplate::new(200).set_body_raw(b, "application/json"),
                None => ResponseTemplate::new(503),
            };
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(resp)
                .mount(&server)
                .await;
        }
        let u = |p: &str| format!("{}{p}", server.uri());
        let part = Propagacja::with_urls(
            &u("/f107"),
            &u("/kp"),
            &u("/scales"),
            &u("/kc2g"),
            Duration::from_secs(5),
        )
        .unwrap();
        let ctx = PartCtx {
            now: at(now),
            position: pos.map(|(lat, lon)| StationPosition {
                lat,
                lon,
                locator: "KN00".into(),
            }),
            settings: BTreeMap::new(),
            sources: BTreeMap::new(),
            active_alerts: Vec::new(),
            alert_sources: Default::default(),
            planned: Vec::new(),
        };
        part.compose(&ctx).await.unwrap()
    }

    fn text(t: PartText) -> String {
        match t {
            PartText::Text(s) => s,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn the_full_propagation_text() {
        let t = text(run(all_ok(), NOW, Some(RZESZOW)).await);
        assert_eq!(
            t,
            "Propagacja. Strumień słoneczny 98, umiarkowany. Indeks K 0,3, warunki spokojne. Pogoda kosmiczna bez zaburzeń. Jonosonda Warszawa: częstotliwość krytyczna 3,2 megaherca, MUF 10,6 megaherca. Łączności bliskie do pasma 160 metrów, dalekie do pasma 40 metrów. Dane: NOAA, GIRO przez KC2G."
        );
        assert_speakable(&[&t]);
    }

    #[tokio::test]
    async fn stale_or_failed_data_drops_only_its_sentence() {
        let t = run(all_ok(), "2026-11-23T10:01:00Z", Some(RZESZOW)).await;
        assert_eq!(t, PartText::Empty("brak danych propagacji"));

        let t = text(
            run(
                Bodies {
                    kc2g: None,
                    ..all_ok()
                },
                NOW,
                Some(RZESZOW),
            )
            .await,
        );
        assert!(
            !t.contains("Jonosonda") && t.ends_with("Dane: NOAA."),
            "{t}"
        );

        let t = text(run(all_ok(), NOW, None).await);
        assert!(
            !t.contains("Jonosonda") && t.ends_with("Dane: NOAA."),
            "{t}"
        );

        let none = Bodies {
            f107: None,
            kp: None,
            scales: None,
            ..all_ok()
        };
        let t = text(run(none, NOW, Some(RZESZOW)).await);
        assert!(t.starts_with("Propagacja. Jonosonda Warszawa: "), "{t}");
        assert!(t.ends_with("Dane: GIRO przez KC2G."), "{t}");

        let storm = Bodies {
            kp: Some(json(
                |v| {
                    *v.as_array_mut()
                        .unwrap()
                        .last_mut()
                        .unwrap()
                        .get_mut("Kp")
                        .unwrap() = 5.33.into()
                },
                KP,
            )),
            scales: Some(json(|v| v["0"]["G"]["Scale"] = "3".into(), SCALES)),
            ..all_ok()
        };
        let t = text(run(storm, NOW, None).await);
        assert!(t.contains("Indeks K 5,3, burza geomagnetyczna."), "{t}");
        assert!(
            t.contains("Pogoda kosmiczna: silna burza geomagnetyczna."),
            "{t}"
        );
        assert_speakable(&[&t]);
    }

    #[tokio::test]
    async fn unknown_schemas_never_become_zeros() {
        let bodies = Bodies {
            kc2g: Some(b"{\"a\":1}".to_vec()),
            f107: Some(json(
                |v| {
                    v.as_array_mut().unwrap().iter_mut().for_each(|r| {
                        r.as_object_mut().unwrap().remove("flux");
                    })
                },
                F107,
            )),
            ..all_ok()
        };
        let t = text(run(bodies, NOW, Some(RZESZOW)).await);
        assert!(!t.contains("Jonosonda") && !t.contains("Strumień"), "{t}");
        assert!(!t.contains(" 0 "), "{t}");
    }
}
