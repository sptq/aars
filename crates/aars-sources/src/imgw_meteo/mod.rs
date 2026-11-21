//! Źródło ostrzeżeń meteorologicznych IMGW (`warningsmeteo`).

pub mod http;
pub mod parse;
pub mod speech;

use std::time::Duration;

use aars_core::module::{ModuleDescriptor, ModuleKind, SettingKind, SettingSpec, Toggle};

use crate::source::{DataSource, FetchFuture, SourceCtx, SourceError, SourceItem, SourceOutput};
use parse::ParsedWarnings;

pub const DEFAULT_URL: &str = "https://danepubliczne.imgw.pl/api/data/warningsmeteo";
pub const MAX_BODY: usize = 2 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// Opis modułu dla panelu (R13): klucze `source:imgw-meteo:*` jak w etapie 8.
pub const DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
    id: "imgw-meteo",
    name: "Ostrzeżenia meteorologiczne IMGW",
    description: "Ostrzeżenia meteorologiczne IMGW dla obszaru stacji, ogłaszane po zmianie.",
    kind: ModuleKind::Source,
    toggle: Toggle::Switchable,
    default_enabled: false,
    settings: &[
        SettingSpec {
            key: "area",
            label: "Obszar",
            help: "Jedna linia (albo wpis po średniku) na obszar: dwucyfrowy kod województwa (np. 10) albo czterocyfrowy kod powiatu z nazwą, którą stacja ma wypowiadać (np. 1061 Łódź). Najwyżej 40 wpisów; puste pole to brak obszaru — źródło nic nie ogłasza.",
            kind: SettingKind::Area,
            default: "",
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

pub struct ImgwMeteo {
    client: reqwest::Client,
    url: String,
}

impl ImgwMeteo {
    pub fn new(url: &str) -> Result<Self, SourceError> {
        Self::with_timeout(url, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(url: &str, timeout: Duration) -> Result<Self, SourceError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("aars/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| SourceError::Http(e.to_string()))?;
        Ok(Self {
            client,
            url: url.to_owned(),
        })
    }
}

impl DataSource for ImgwMeteo {
    fn id(&self) -> &'static str {
        "imgw-meteo"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(300)
    }

    fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a> {
        Box::pin(async move {
            let body = http::get_limited(&self.client, &self.url, MAX_BODY).await?;
            Ok(to_output(parse::parse_warnings(&body)?, ctx))
        })
    }
}

const TIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// Czysta funkcja: odpowiedź IMGW → pozycje dla obszaru ze `ctx`.
pub fn to_output(parsed: ParsedWarnings, ctx: &SourceCtx) -> SourceOutput {
    let mut complete = parsed.skipped == 0;
    let mut items = Vec::new();
    for w in &parsed.warnings {
        let labels: Vec<&str> = ctx
            .area
            .iter()
            .filter(|entry| w.teryt.iter().any(|code| entry.matches(code)))
            .map(|entry| entry.label.as_str())
            .collect();
        if labels.is_empty() {
            continue;
        }
        let (Some(valid_from), Some(valid_until)) = (
            speech::warsaw_to_utc(w.valid_from),
            speech::warsaw_to_utc(w.valid_until),
        ) else {
            complete = false;
            continue;
        };
        let area = labels.join(", ");
        let (priority, interrupting) = speech::level_priority(w.level);
        let level = w.level.map_or_else(|| "-".to_owned(), |n| n.to_string());
        items.push(SourceItem {
            key: w.id.clone(),
            topic: w.event.to_lowercase(),
            fingerprint: format!(
                "{}|{level}|{}|{}|{area}",
                w.event,
                w.valid_from.format(TIME_FORMAT),
                w.valid_until.format(TIME_FORMAT)
            ),
            text: speech::announcement(w, &area),
            change_text: speech::change(w, &area),
            cancel_text: speech::cancellation(&w.event, &area),
            summary: match w.level {
                Some(n) => format!("{}, {n}. stopień", w.event),
                None => w.event.clone(),
            },
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
    use chrono::Utc;

    const JULY: &[u8] = include_bytes!("../../tests/fixtures/imgw/warningsmeteo-2026-07-23.json");

    fn ctx(area: &str) -> SourceCtx {
        SourceCtx {
            now: Utc::now(),
            area: parse_area(area).unwrap(),
            position: None,
            settings: Default::default(),
        }
    }

    fn run(body: &[u8], area: &str) -> SourceOutput {
        to_output(parse::parse_warnings(body).unwrap(), &ctx(area))
    }

    #[test]
    fn the_descriptor_matches_the_stage_8_keys_and_ranges() {
        let d = &DESCRIPTOR;
        assert_eq!((d.id, d.name), ("imgw-meteo", "Ostrzeżenia meteorologiczne IMGW"));
        assert_eq!(d.kind, ModuleKind::Source);
        assert_eq!(d.toggle, Toggle::Switchable);
        assert!(!d.default_enabled);
        assert_eq!(d.setting_key("area"), "source:imgw-meteo:area");
        assert_eq!(d.setting_key("interval_s"), "source:imgw-meteo:interval_s");
        assert_eq!(d.settings[1].kind, SettingKind::Integer { min: 60, max: 3600 });
        assert_eq!(d.settings[1].default, "300");
        assert!(d.settings[0].help.contains("1061 Łódź"));
    }

    #[test]
    fn only_warnings_touching_the_area_are_returned() {
        assert_eq!(run(JULY, "1061 Łódź").items.len(), 1);
        assert_eq!(run(JULY, "10").items.len(), 1);
        assert_eq!(run(JULY, "22").items.len(), 3);
        let none = run(JULY, "3064 Poznań");
        assert_eq!(none.items.len(), 0);
        assert!(none.complete);
        let item = &run(JULY, "1061 Łódź").items[0];
        assert_eq!(item.key, "Sk20260722203344412");
        assert_eq!(item.topic, "burze");
        assert_eq!(item.summary, "Burze, 1. stopień");
        assert_eq!(
            item.fingerprint,
            "Burze|1|2026-07-23 10:00|2026-07-23 22:00|Łódź"
        );
        assert_eq!(
            item.valid_until.map(|d| d.to_rfc3339()).as_deref(),
            Some("2026-07-23T20:00:00+00:00")
        );
        assert!(item.text.contains("Obszar: Łódź."));
        assert!(item.change_text.starts_with("Zmiana ostrzeżenia. "));
        assert!(item.cancel_text.starts_with("Odwołanie ostrzeżenia"));
    }

    #[test]
    fn two_area_entries_hit_by_one_warning_are_read_together() {
        let out = run(JULY, "1061 Łódź\n1020 powiat zgierski");
        assert_eq!(out.items.len(), 1);
        assert!(out.items[0].text.contains("Obszar: Łódź, powiat zgierski."));
        assert!(out.items[0].fingerprint.ends_with("|Łódź, powiat zgierski"));
    }

    #[test]
    fn the_fingerprint_ignores_text_but_not_level_or_validity() {
        let fp = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v: serde_json::Value = serde_json::from_slice(JULY).unwrap();
            let idx = v
                .as_array()
                .unwrap()
                .iter()
                .position(|w| w["id"] == "Sk20260722203344412")
                .unwrap();
            edit(&mut v[idx]);
            run(&serde_json::to_vec(&v).unwrap(), "1061 Łódź").items[0]
                .fingerprint
                .clone()
        };
        let base = fp(&|_| {});
        assert_eq!(
            base,
            fp(&|w| {
                w["tresc"] = "Inny opis.".into();
                w["prawdopodobienstwo"] = "30".into();
            })
        );
        assert_ne!(base, fp(&|w| w["stopien"] = "2".into()));
        assert_ne!(
            base,
            fp(&|w| w["obowiazuje_do"] = "2026-07-23 23:00:00".into())
        );
    }

    #[test]
    fn a_skipped_entry_makes_the_fetch_incomplete() {
        let mut v: serde_json::Value = serde_json::from_slice(JULY).unwrap();
        v[2].as_object_mut().unwrap().remove("teryt");
        let out = run(&serde_json::to_vec(&v).unwrap(), "22");
        assert!(!out.complete);
    }

    #[test]
    fn a_nonexistent_local_time_is_skipped_and_incomplete() {
        let mut v: serde_json::Value = serde_json::from_slice(JULY).unwrap();
        let idx = v
            .as_array()
            .unwrap()
            .iter()
            .position(|w| w["id"] == "Sk20260722203344412")
            .unwrap();
        v[idx]["obowiazuje_od"] = "2026-03-29 02:30:00".into();
        let out = run(&serde_json::to_vec(&v).unwrap(), "1061 Łódź");
        assert!(out.items.is_empty());
        assert!(!out.complete);
    }

    mod http_tests {
        use super::*;
        use wiremock::matchers::{header_regex, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const JULY: &[u8] =
            include_bytes!("../../tests/fixtures/imgw/warningsmeteo-2026-07-23.json");
        const BRAK: &[u8] =
            include_bytes!("../../tests/fixtures/imgw/warningsmeteo-2026-11-20-brak.json");

        async fn server_with(response: ResponseTemplate) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/warningsmeteo"))
                .respond_with(response)
                .mount(&server)
                .await;
            server
        }

        fn source(server: &MockServer) -> ImgwMeteo {
            ImgwMeteo::new(&format!("{}/warningsmeteo", server.uri())).unwrap()
        }

        #[tokio::test]
        async fn the_real_july_response_is_fetched_and_parsed() {
            let server = server_with(
                ResponseTemplate::new(200).set_body_raw(JULY, "application/json; charset=utf-8"),
            )
            .await;
            let out = source(&server).fetch(&ctx("1061 Łódź")).await.unwrap();
            assert_eq!(out.items.len(), 1);
            assert!(out.complete);
            assert_eq!(source(&server).id(), "imgw-meteo");
            assert_eq!(source(&server).default_interval(), Duration::from_secs(300));
        }

        #[tokio::test]
        async fn the_no_warnings_object_is_an_empty_successful_fetch() {
            let server =
                server_with(ResponseTemplate::new(200).set_body_raw(BRAK, "application/json"))
                    .await;
            let out = source(&server).fetch(&ctx("1061 Łódź")).await.unwrap();
            assert!(out.items.is_empty());
            assert!(out.complete);
        }

        #[tokio::test]
        async fn a_server_error_is_a_status_error() {
            let server = server_with(ResponseTemplate::new(503)).await;
            let err = source(&server).fetch(&ctx("10")).await.err().unwrap();
            assert!(matches!(err, SourceError::Status(503)), "{err:?}");
        }

        #[tokio::test]
        async fn an_html_maintenance_page_is_a_parse_error() {
            let server = server_with(
                ResponseTemplate::new(200).set_body_raw("<html>Przerwa</html>", "text/html"),
            )
            .await;
            let err = source(&server).fetch(&ctx("10")).await.err().unwrap();
            assert!(matches!(err, SourceError::Parse(_)), "{err:?}");
        }

        #[tokio::test]
        async fn a_slow_server_times_out() {
            let server = server_with(
                ResponseTemplate::new(200)
                    .set_body_raw(BRAK, "application/json")
                    .set_delay(Duration::from_secs(3)),
            )
            .await;
            let src = ImgwMeteo::with_timeout(
                &format!("{}/warningsmeteo", server.uri()),
                Duration::from_secs(1),
            )
            .unwrap();
            let err = src.fetch(&ctx("10")).await.err().unwrap();
            assert!(matches!(err, SourceError::Http(_)), "{err:?}");
        }

        #[tokio::test]
        async fn an_oversized_body_is_refused() {
            let server = server_with(
                ResponseTemplate::new(200)
                    .set_body_raw(vec![b' '; 3 * 1024 * 1024], "application/json"),
            )
            .await;
            let err = source(&server).fetch(&ctx("10")).await.err().unwrap();
            assert!(matches!(err, SourceError::TooLarge(MAX_BODY)), "{err:?}");
        }

        #[tokio::test]
        async fn a_connection_refused_is_an_http_error() {
            let src = ImgwMeteo::new("http://127.0.0.1:1/warningsmeteo").unwrap();
            let err = src.fetch(&ctx("10")).await.err().unwrap();
            assert!(matches!(err, SourceError::Http(_)), "{err:?}");
        }

        #[tokio::test]
        async fn the_user_agent_names_only_the_program() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(header_regex("user-agent", "^aars/[0-9.]+$"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(BRAK, "application/json"))
                .expect(1)
                .mount(&server)
                .await;
            let src = ImgwMeteo::new(&format!("{}/x", server.uri())).unwrap();
            src.fetch(&ctx("10")).await.unwrap();
        }
    }
}
