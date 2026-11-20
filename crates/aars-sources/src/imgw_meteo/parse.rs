//! Parsowanie odpowiedzi `warningsmeteo` IMGW. Dane z sieci: bez `unwrap`,
//! `expect` i indeksowania.

use chrono::NaiveDateTime;

use crate::source::SourceError;

#[derive(Debug, Clone, PartialEq)]
pub struct MeteoWarning {
    pub id: String,
    pub event: String,
    pub level: Option<u8>,
    pub probability: Option<u8>,
    pub valid_from: NaiveDateTime,
    pub valid_until: NaiveDateTime,
    pub text: String,
    pub teryt: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedWarnings {
    pub warnings: Vec<MeteoWarning>,
    /// Wpisy nieczytelne (brak pola albo zła data) — odpowiedź niekompletna.
    pub skipped: usize,
}

const DATE_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

pub fn parse_warnings(body: &[u8]) -> Result<ParsedWarnings, SourceError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    match value {
        // Brak ostrzeżeń IMGW zgłasza obiektem, nie pustą tablicą.
        serde_json::Value::Object(map) if map.contains_key("message") => {
            Ok(ParsedWarnings::default())
        }
        serde_json::Value::Array(entries) => {
            let mut parsed = ParsedWarnings::default();
            for entry in &entries {
                match parse_entry(entry) {
                    Some(w) => parsed.warnings.push(w),
                    None => parsed.skipped += 1,
                }
            }
            Ok(parsed)
        }
        _ => Err(SourceError::Parse(
            "to nie jest lista ostrzeżeń IMGW".into(),
        )),
    }
}

fn parse_entry(entry: &serde_json::Value) -> Option<MeteoWarning> {
    let text_field = |name: &str| entry.get(name).and_then(|v| v.as_str());
    let date = |name: &str| {
        text_field(name).and_then(|s| NaiveDateTime::parse_from_str(s.trim(), DATE_FORMAT).ok())
    };
    let id = text_field("id")?.trim();
    let event = text_field("nazwa_zdarzenia")?.trim();
    if id.is_empty() || event.is_empty() {
        return None;
    }
    let mut teryt = Vec::new();
    for code in entry.get("teryt")?.as_array()? {
        teryt.push(code.as_str()?.trim().to_owned());
    }
    let level = text_field("stopien")
        .and_then(|s| s.trim().parse::<u8>().ok())
        .filter(|n| (1..=3).contains(n));
    let probability = text_field("prawdopodobienstwo").and_then(|s| s.trim().parse::<u8>().ok());
    Some(MeteoWarning {
        id: id.to_owned(),
        event: event.to_owned(),
        level,
        probability,
        valid_from: date("obowiazuje_od")?,
        valid_until: date("obowiazuje_do")?,
        text: text_field("tresc").unwrap_or_default().trim().to_owned(),
        teryt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    const BRAK: &[u8] =
        include_bytes!("../../tests/fixtures/imgw/warningsmeteo-2026-11-20-brak.json");
    const JULY: &[u8] = include_bytes!("../../tests/fixtures/imgw/warningsmeteo-2026-07-23.json");

    #[test]
    fn the_no_warnings_message_is_an_empty_list() {
        let parsed = parse_warnings(BRAK).unwrap();
        assert!(parsed.warnings.is_empty());
        assert_eq!(parsed.skipped, 0);
    }

    #[test]
    fn the_july_fixture_has_five_warnings_with_county_codes() {
        let parsed = parse_warnings(JULY).unwrap();
        assert_eq!(parsed.warnings.len(), 5);
        assert_eq!(parsed.skipped, 0);
        let w = parsed
            .warnings
            .iter()
            .find(|w| w.id == "Sk20260722203344412")
            .unwrap();
        assert_eq!(w.level, Some(1));
        assert_eq!(w.probability, Some(80));
        assert_eq!(
            w.valid_from,
            NaiveDate::from_ymd_opt(2026, 7, 23)
                .unwrap()
                .and_hms_opt(10, 0, 0)
                .unwrap()
        );
        assert!(w.teryt.iter().any(|t| t == "1061"));
    }

    #[test]
    fn a_broken_entry_is_skipped_and_counted() {
        let mut v: serde_json::Value = serde_json::from_slice(JULY).unwrap();
        v[2].as_object_mut().unwrap().remove("teryt");
        let parsed = parse_warnings(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(parsed.warnings.len(), 4);
        assert_eq!(parsed.skipped, 1);
    }

    #[test]
    fn a_bad_date_or_level_is_handled() {
        let mut v: serde_json::Value = serde_json::from_slice(JULY).unwrap();
        v[0]["obowiazuje_od"] = "wczoraj".into();
        v[1]["stopien"] = "7".into();
        let parsed = parse_warnings(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(parsed.skipped, 1);
        assert_eq!(parsed.warnings.len(), 4);
        assert_eq!(parsed.warnings[0].level, None);
    }

    #[test]
    fn html_and_truncated_json_are_parse_errors() {
        assert!(matches!(
            parse_warnings(b"<html><body>Przerwa techniczna</body></html>"),
            Err(SourceError::Parse(_))
        ));
        assert!(matches!(
            parse_warnings(&JULY[..JULY.len() / 2]),
            Err(SourceError::Parse(_))
        ));
        assert!(matches!(parse_warnings(b""), Err(SourceError::Parse(_))));
        assert!(matches!(
            parse_warnings(b"{\"inne\":1}"),
            Err(SourceError::Parse(_))
        ));
        assert!(matches!(parse_warnings(b"42"), Err(SourceError::Parse(_))));
    }
}
