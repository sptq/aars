//! Źródło ostrzeżeń hydrologicznych IMGW (`warningshydro`).

pub mod parse;
pub mod speech;

use std::time::Duration;

use aars_core::area::voivodeship_name;
use aars_core::module::{ModuleDescriptor, ModuleKind, Needs, SettingKind, SettingSpec, Toggle};

use sha2::{Digest, Sha256};

use crate::http;
use crate::imgw_meteo::speech::level_priority;
use crate::source::{DataSource, FetchFuture, SourceCtx, SourceError, SourceItem, SourceOutput};
use parse::{HydroArea, ParsedHydro};

pub const ID: &str = "imgw-hydro";
pub const DEFAULT_URL: &str = "https://danepubliczne.imgw.pl/api/data/warningshydro";
pub const MAX_BODY: usize = 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

pub const DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
    id: ID,
    name: "Ostrzeżenia hydrologiczne IMGW",
    description: "Ostrzeżenia hydrologiczne IMGW-PIB dla województw obszaru stacji, ogłaszane po zmianie. Susza hydrologiczna nie jest ogłaszana.",
    kind: ModuleKind::Source,
    toggle: Toggle::Switchable,
    default_enabled: false,
    needs: Needs::Area,
    settings: &[
        SettingSpec {
            key: "area",
            label: "Obszar",
            help: "Kody jak w ostrzeżeniach meteo (np. 18 albo 1863 Rzeszów). Ostrzeżenia hydrologiczne IMGW dotyczą województw, więc powiat oznacza całe swoje województwo. Puste pole — źródło nic nie ogłasza.",
            kind: SettingKind::Area,
            default: "",
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

pub struct ImgwHydro {
    client: reqwest::Client,
    url: String,
}

impl ImgwHydro {
    pub fn new(url: &str) -> Result<Self, SourceError> {
        Self::with_timeout(url, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(url: &str, timeout: Duration) -> Result<Self, SourceError> {
        let client = http::client(timeout)?;
        Ok(Self {
            client,
            url: url.to_owned(),
        })
    }
}

impl DataSource for ImgwHydro {
    fn id(&self) -> &'static str {
        ID
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(600)
    }

    fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a> {
        Box::pin(async move {
            let body = http::get_limited(&self.client, &self.url, MAX_BODY).await?;
            Ok(to_output(&parse::parse_warnings(&body)?, ctx))
        })
    }
}

const TIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// Czysta funkcja: odpowiedź IMGW → pozycje dla województw obszaru ze `ctx`.
pub fn to_output(parsed: &ParsedHydro, ctx: &SourceCtx) -> SourceOutput {
    let mut stations: Vec<&'static str> = Vec::new();
    for entry in &ctx.area {
        if let Some(name) = entry.code.get(..2).and_then(voivodeship_name)
            && !stations.contains(&name)
        {
            stations.push(name);
        }
    }
    let mut complete = parsed.skipped == 0;
    let mut items = Vec::new();
    for w in &parsed.warnings {
        let matching: Vec<&HydroArea> = w
            .areas
            .iter()
            .filter(|a| stations.contains(&a.voivodeship.as_str()))
            .collect();
        if matching.is_empty() {
            continue;
        }
        let (Some(valid_from), Some(valid_until)) = (
            crate::speech::warsaw_to_utc(w.valid_from),
            crate::speech::warsaw_to_utc(w.valid_until),
        ) else {
            complete = false;
            continue;
        };
        let mut voivodeships: Vec<&str> = Vec::new();
        for a in &matching {
            if !voivodeships.contains(&a.voivodeship.as_str()) {
                voivodeships.push(&a.voivodeship);
            }
        }
        let descriptions: Vec<&str> = matching
            .iter()
            .map(|a| a.description.as_str())
            .filter(|d| !d.is_empty())
            .collect();
        let level = u8::try_from(w.level).ok();
        let (priority, interrupting) = level_priority(level);
        let text = speech::announcement(
            level.unwrap_or(0),
            &w.event,
            &descriptions.join("; "),
            &w.course,
            w.valid_until,
        );
        let key = format!("{}|{}", w.number, w.office);
        let digest = Sha256::digest(w.course.as_bytes());
        let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        items.push(SourceItem {
            topic: key.clone(),
            key,
            fingerprint: format!("{}|{}|{hex}", w.level, w.valid_until.format(TIME_FORMAT)),
            change_text: speech::change(&text),
            cancel_text: speech::cancellation(&w.number, &voivodeships.join(", "), &w.event),
            text,
            summary: format!("{}, {}. stopień", w.event, w.level),
            priority,
            interrupting,
            valid_from: Some(valid_from),
            valid_until: Some(valid_until),
        });
    }
    SourceOutput { items, complete }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::area::parse_area;
    use aars_core::priority::Priority;
    use chrono::Utc;

    const FIX: &[u8] = include_bytes!("../../tests/fixtures/imgw/warningshydro-2026-11-22.json");
    const GDYNIA: &str = "Biuro Prognoz Hydrologicznych w Gdyni, Wydział Prognoz i Opracowań Hydrologicznych w Gdyni";

    fn ctx(area: &str) -> SourceCtx {
        let entries = parse_area(area).unwrap();
        SourceCtx {
            now: Utc::now(),
            area_text: aars_core::area::area_text(&entries),
            area: entries,
            position: None,
            settings: Default::default(),
            tracked_keys: Default::default(),
            tracked: Default::default(),
        }
    }

    fn run(body: &[u8], area: &str) -> SourceOutput {
        to_output(&parse::parse_warnings(body).unwrap(), &ctx(area))
    }

    /// Fiks z poprawką wpisu o numerze 137.
    fn edited(edit: &dyn Fn(&mut serde_json::Value)) -> Vec<u8> {
        let mut v: serde_json::Value = serde_json::from_slice(FIX).unwrap();
        let idx = v
            .as_array()
            .unwrap()
            .iter()
            .position(|w| w["numer"] == "137")
            .unwrap();
        edit(&mut v[idx]);
        serde_json::to_vec(&v).unwrap()
    }

    fn item137(body: &[u8]) -> SourceItem {
        run(body, "22")
            .items
            .into_iter()
            .find(|i| i.key.starts_with("137|"))
            .unwrap()
    }

    #[test]
    fn the_descriptor_has_the_agreed_keys_and_copy() {
        let d = &DESCRIPTOR;
        assert_eq!(
            (d.id, d.name),
            ("imgw-hydro", "Ostrzeżenia hydrologiczne IMGW")
        );
        assert_eq!(d.kind, ModuleKind::Source);
        assert_eq!(d.toggle, Toggle::Switchable);
        assert!(!d.default_enabled);
        assert_eq!(d.needs, Needs::Area);
        assert_eq!(d.setting_key("area"), "source:imgw-hydro:area");
        assert_eq!(d.settings[0].kind, SettingKind::Area);
        assert_eq!(
            d.settings[1].kind,
            SettingKind::Integer {
                min: 300,
                max: 3600
            }
        );
        assert_eq!(d.settings[1].default, "600");
        assert!(
            d.description
                .ends_with("Susza hydrologiczna nie jest ogłaszana.")
        );
    }

    #[test]
    fn droughts_are_never_items() {
        // 30 = wielkopolskie, 24 = śląskie, 12 = małopolskie mają suszę w fiksie.
        let out = run(
            FIX,
            "30\n24\n12\n10\n02\n04\n06\n08\n14\n16\n18\n20\n26\n28\n32\n22",
        );
        assert!(out.items.iter().all(|i| !i.summary.contains("Susza")
            && i.key.chars().next().is_some_and(|c| c.is_ascii_digit())));
        assert!(
            out.items
                .iter()
                .all(|i| ["137", "138", "139", "140"].contains(&i.key.split('|').next().unwrap()))
        );
        assert_eq!(out.items.len(), 4);
        assert!(out.complete);
    }

    #[test]
    fn a_drought_without_an_office_does_not_make_the_fetch_incomplete() {
        let mut v: serde_json::Value = serde_json::from_slice(FIX).unwrap();
        v[0].as_object_mut().unwrap().remove("biuro");
        v[1]["biuro"] = serde_json::Value::Null;
        v[2].as_object_mut().unwrap().remove("obszary");
        let out = run(&serde_json::to_vec(&v).unwrap(), "22");
        assert!(out.complete);
        assert_eq!(out.items.len(), 2);
    }

    #[test]
    fn warnings_match_by_voivodeship() {
        let keys = |area: &str| {
            run(FIX, area)
                .items
                .iter()
                .map(|i| i.key.split('|').next().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(keys("22"), ["137", "140"]);
        assert_eq!(keys("3262 Szczecin"), ["138", "139"]);
        assert_eq!(keys("28"), ["140"]);
        let none = run(FIX, "18");
        assert!(none.items.is_empty() && none.complete);
    }

    #[test]
    fn the_item_for_137() {
        let out = run(FIX, "22");
        let i = out
            .items
            .iter()
            .find(|i| i.key == format!("137|{GDYNIA}"))
            .unwrap();
        assert_eq!(i.topic, i.key);
        assert_eq!((i.priority, i.interrupting), (Priority::High, false));
        assert_eq!(
            i.summary,
            "Wezbranie z przekroczeniem stanów ostrzegawczych, 2. stopień"
        );
        assert_eq!(
            i.valid_until.unwrap().to_rfc3339(),
            "2026-11-24T11:00:00+00:00"
        );
        assert!(i.valid_from.is_some());
        assert!(i.text.starts_with("Ostrzeżenie hydrologiczne IMGW drugiego stopnia: Wezbranie z przekroczeniem stanów ostrzegawczych. Pomorskie, Wybrzeże Wschodnie"), "{}", i.text);
        assert!(
            i.text
                .ends_with("Ważne do godz. 12:00 24 listopada. Źródło: IMGW-PIB."),
            "{}",
            i.text
        );
        assert!(
            i.text
                .contains("ostrzegawczych. W związku z prognozowaną sytuacją"),
            "{}",
            i.text
        );
        assert!(
            i.change_text
                .starts_with("Zmiana ostrzeżenia hydrologicznego. Ostrzeżenie hydrologiczne IMGW")
        );
        assert!(
            i.fingerprint.starts_with("2|2026-11-24 12:00|"),
            "{}",
            i.fingerprint
        );
        let hex = i.fingerprint.rsplit('|').next().unwrap();
        assert_eq!(hex.len(), 16);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_cancellation_names_number_and_station_voivodeships() {
        let c = |area: &str| {
            run(FIX, area)
                .items
                .into_iter()
                .find(|i| i.key.starts_with("140|"))
                .unwrap()
                .cancel_text
        };
        assert_eq!(
            c("22"),
            "Ostrzeżenie hydrologiczne nr 140 (pomorskie) „Wezbranie z przekroczeniem stanów ostrzegawczych” nie obowiązuje."
        );
        assert_eq!(
            c("22\n28"),
            "Ostrzeżenie hydrologiczne nr 140 (pomorskie, warmińsko-mazurskie) „Wezbranie z przekroczeniem stanów ostrzegawczych” nie obowiązuje."
        );
    }

    #[test]
    fn a_county_means_its_whole_voivodeship() {
        assert_eq!(run(FIX, "2262 Słupsk").items.len(), 2);
    }

    #[test]
    fn level_three_interrupts() {
        let i = item137(&edited(&|w| w["stopień"] = "3".into()));
        assert_eq!((i.priority, i.interrupting), (Priority::Emergency, true));
        assert!(
            i.text
                .starts_with("Ostrzeżenie hydrologiczne IMGW trzeciego stopnia:")
        );
        assert!(i.fingerprint.starts_with("3|"));
    }

    #[test]
    fn the_fingerprint_follows_level_end_and_course_only() {
        let fp = |edit: &dyn Fn(&mut serde_json::Value)| item137(&edited(edit)).fingerprint;
        let base = fp(&|_| {});
        assert_eq!(
            base,
            fp(&|w| {
                w["komentarz"] = "Inny komentarz.".into();
                w["opublikowano"] = "2026-11-22 23:00:00".into();
                w["prawdopodobienstwo"] = "10".into();
            })
        );
        assert_ne!(base, fp(&|w| w["stopień"] = "3".into()));
        assert_ne!(base, fp(&|w| w["data_do"] = "2026-11-24 13:00:00".into()));
        assert_ne!(base, fp(&|w| w["przebieg"] = "Nowy przebieg.".into()));
    }

    #[test]
    fn a_long_course_is_cut_but_validity_and_source_stay() {
        let i = item137(&edited(&|w| {
            w["przebieg"] = "Zdanie numer jeden. ".repeat(100).into()
        }));
        assert!(i.text.chars().count() <= 700, "{}", i.text.chars().count());
        assert!(i.text.ends_with("Źródło: IMGW-PIB."));
        assert!(i.text.contains("Ważne do godz. 12:00 24 listopada."));
        assert!(i.text.contains("Zdanie numer jeden."));
    }

    #[test]
    fn the_validity_is_read_as_until_an_hour() {
        let i = item137(FIX);
        assert!(
            i.text
                .ends_with("Ważne do godz. 12:00 24 listopada. Źródło: IMGW-PIB."),
            "{}",
            i.text
        );
        let spoken = aars_audio::text::normalize(&i.text);
        assert!(spoken.contains("Ważne do godziny dwunastej"), "{spoken}");
    }

    #[test]
    fn midnight_is_24_00_of_the_previous_day() {
        let i = item137(&edited(&|w| w["data_do"] = "2026-11-25 00:00:00".into()));
        assert!(
            i.text
                .ends_with("Ważne do godz. 24:00 24 listopada. Źródło: IMGW-PIB."),
            "{}",
            i.text
        );
    }

    #[test]
    fn an_unknown_level_or_bad_date_is_skipped_and_incomplete() {
        for body in [
            edited(&|w| w["stopień"] = "4".into()),
            edited(&|w| w["data_do"] = "2026-03-29 02:30:00".into()),
        ] {
            let out = run(&body, "22");
            assert!(out.items.iter().all(|i| !i.key.starts_with("137|")));
            assert!(!out.complete);
            assert!(out.items.iter().any(|i| i.key.starts_with("140|")));
        }
    }

    #[test]
    fn texts_are_speakable() {
        let out = run(FIX, "22\n32\n28");
        assert_eq!(out.items.len(), 4);
        for i in &out.items {
            crate::speech::assert_speakable(&[&i.text, &i.change_text, &i.cancel_text]);
        }
    }

    mod http_tests {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn server_with(response: ResponseTemplate) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/warningshydro"))
                .respond_with(response)
                .mount(&server)
                .await;
            server
        }

        fn source(server: &MockServer) -> ImgwHydro {
            ImgwHydro::new(&format!("{}/warningshydro", server.uri())).unwrap()
        }

        #[tokio::test]
        async fn the_real_response_is_fetched_and_parsed() {
            let server =
                server_with(ResponseTemplate::new(200).set_body_raw(FIX, "application/json")).await;
            let src = source(&server);
            let out = src.fetch(&ctx("22")).await.unwrap();
            assert_eq!(out.items.len(), 2);
            assert!(out.complete);
            assert_eq!(src.id(), "imgw-hydro");
            assert_eq!(src.needs(), Needs::Area);
            assert_eq!(src.default_interval(), Duration::from_secs(600));
        }

        #[tokio::test]
        async fn a_server_error_is_a_status_error() {
            let server = server_with(ResponseTemplate::new(503)).await;
            let err = source(&server).fetch(&ctx("22")).await.err().unwrap();
            assert!(matches!(err, SourceError::Status(503)), "{err:?}");
        }

        #[tokio::test]
        async fn an_html_maintenance_page_is_a_parse_error() {
            let server = server_with(
                ResponseTemplate::new(200).set_body_raw("<html>Przerwa</html>", "text/html"),
            )
            .await;
            let err = source(&server).fetch(&ctx("22")).await.err().unwrap();
            assert!(matches!(err, SourceError::Parse(_)), "{err:?}");
        }

        #[tokio::test]
        async fn an_oversized_body_is_refused() {
            let server = server_with(
                ResponseTemplate::new(200)
                    .set_body_raw(vec![b' '; MAX_BODY + 1024], "application/json"),
            )
            .await;
            let err = source(&server).fetch(&ctx("22")).await.err().unwrap();
            assert!(matches!(err, SourceError::TooLarge(MAX_BODY)), "{err:?}");
        }
    }
}
