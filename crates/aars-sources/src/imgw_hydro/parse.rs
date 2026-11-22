//! Parsowanie odpowiedzi `warningshydro` IMGW. Dane z sieci: bez `unwrap`,
//! `expect` i indeksowania.

use chrono::NaiveDateTime;

use crate::source::SourceError;
use crate::speech::clean_text;

#[derive(Debug, Clone, PartialEq)]
pub struct HydroArea {
    /// Małymi literami, jak w odpowiedzi („pomorskie”).
    pub voivodeship: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HydroWarning {
    pub number: String,
    pub office: String,
    /// −1 to susza hydrologiczna (nie ogłaszamy), 1–3 to stopnie ostrzeżenia.
    pub level: i8,
    pub valid_from: NaiveDateTime,
    pub valid_until: NaiveDateTime,
    pub event: String,
    pub course: String,
    pub areas: Vec<HydroArea>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedHydro {
    pub warnings: Vec<HydroWarning>,
    /// Wpisy nieczytelne (brak pola, nieznany stopień, zła data).
    pub skipped: usize,
}

const DATE_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

pub fn parse_warnings(body: &[u8]) -> Result<ParsedHydro, SourceError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    match value {
        // Brak ostrzeżeń IMGW zgłasza obiektem; inny obiekt (np. błąd serwera
        // z kodem 200) to błąd, nie brak ostrzeżeń.
        serde_json::Value::Object(map)
            if map
                .get("message")
                .and_then(|m| m.as_str())
                .is_some_and(|m| m.trim().starts_with("Brak ostrzeżeń")) =>
        {
            Ok(ParsedHydro::default())
        }
        serde_json::Value::Array(entries) => {
            let mut parsed = ParsedHydro::default();
            for entry in &entries {
                match parse_entry(entry) {
                    Some(w) => parsed.warnings.push(w),
                    None => parsed.skipped += 1,
                }
            }
            Ok(parsed)
        }
        _ => Err(SourceError::Parse(
            "to nie jest lista ostrzeżeń hydrologicznych IMGW".into(),
        )),
    }
}

fn parse_entry(entry: &serde_json::Value) -> Option<HydroWarning> {
    let text_field = |name: &str| entry.get(name).and_then(|v| v.as_str());
    let date = |name: &str| {
        text_field(name).and_then(|s| NaiveDateTime::parse_from_str(s.trim(), DATE_FORMAT).ok())
    };
    let number = text_field("numer")?.trim();
    let office = text_field("biuro")?.trim();
    let event = clean_text(text_field("zdarzenie")?);
    if number.is_empty() || office.is_empty() || event.is_empty() {
        return None;
    }
    let level: i8 = text_field("stopień")
        .or_else(|| text_field("stopien"))?
        .trim()
        .parse()
        .ok()
        .filter(|n| [-1, 1, 2, 3].contains(n))?;
    let mut areas = Vec::new();
    for area in entry.get("obszary")?.as_array()? {
        let voivodeship = area.get("wojewodztwo")?.as_str()?.trim().to_lowercase();
        let description = clean_text(
            area.get("opis")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
        );
        areas.push(HydroArea {
            voivodeship,
            description,
        });
    }
    Some(HydroWarning {
        number: number.to_owned(),
        office: office.to_owned(),
        level,
        valid_from: date("data_od")?,
        valid_until: date("data_do")?,
        event,
        course: clean_text(text_field("przebieg").unwrap_or_default()),
        areas,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: &[u8] = include_bytes!("../../tests/fixtures/imgw/warningshydro-2026-11-22.json");

    #[test]
    fn the_fixture_has_34_warnings_and_30_droughts() {
        let p = parse_warnings(FIX).unwrap();
        assert_eq!((p.warnings.len(), p.skipped), (34, 0));
        assert_eq!(p.warnings.iter().filter(|w| w.level == -1).count(), 30);
        let w140 = p.warnings.iter().find(|w| w.number == "140").unwrap();
        assert_eq!(
            w140.areas
                .iter()
                .map(|a| a.voivodeship.as_str())
                .collect::<Vec<_>>(),
            ["pomorskie", "warmińsko-mazurskie"]
        );
        assert_eq!(w140.level, 2);
        assert!(
            w140.office
                .starts_with("Biuro Prognoz Hydrologicznych w Gdyni")
        );
    }

    #[test]
    fn the_no_warnings_object_is_empty_and_another_object_is_a_parse_error() {
        let p =
            parse_warnings(r#"{"message":"Brak ostrzeżeń hydrologicznych"}"#.as_bytes()).unwrap();
        assert!(p.warnings.is_empty() && p.skipped == 0);
        for bad in [r#"{"error":1}"#, "<html>Przerwa</html>", "42"] {
            let err = parse_warnings(bad.as_bytes()).unwrap_err();
            assert!(matches!(err, SourceError::Parse(_)), "{bad}: {err:?}");
        }
    }

    #[test]
    fn unreadable_entries_are_counted() {
        let mut v: serde_json::Value = serde_json::from_slice(FIX).unwrap();
        v[0].as_object_mut().unwrap().remove("numer");
        v[1]["stopień"] = "7".into();
        v[2]["data_od"] = "wczoraj".into();
        v[3].as_object_mut().unwrap().remove("obszary");
        let p = parse_warnings(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!((p.warnings.len(), p.skipped), (30, 4));
    }

    #[test]
    fn the_ascii_level_key_is_accepted_as_a_fallback() {
        let mut v: serde_json::Value = serde_json::from_slice(FIX).unwrap();
        let first = v[0].as_object_mut().unwrap();
        let level = first.remove("stopień").unwrap();
        first.insert("stopien".into(), level);
        let p = parse_warnings(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!((p.warnings.len(), p.skipped), (34, 0));
    }
}
