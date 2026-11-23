//! Parsowanie GeoJSON z EMSC FDSN. Dane z sieci: bez `unwrap`, `expect` i indeksowania.

use chrono::{DateTime, Utc};

use crate::source::SourceError;

#[derive(Debug, Clone, PartialEq)]
pub struct Quake {
    pub unid: String,
    pub time: DateTime<Utc>,
    pub lat: f64,
    pub lon: f64,
    pub mag: f64,
    pub region: String,
}

#[derive(Debug, Default)]
pub struct ParsedQuakes {
    pub events: Vec<Quake>,
    /// Zdarzenia bez wymaganego pola — odpowiedź niekompletna.
    pub skipped: usize,
}

fn parse_event(feature: &serde_json::Value) -> Option<Quake> {
    let props = feature.get("properties")?;
    let unid = props.get("unid")?.as_str()?.trim();
    if unid.is_empty() {
        return None;
    }
    let time = DateTime::parse_from_rfc3339(props.get("time")?.as_str()?)
        .ok()?
        .with_timezone(&Utc);
    let finite = |key: &str| props.get(key)?.as_f64().filter(|n| n.is_finite());
    let (lat, lon, mag) = (finite("lat")?, finite("lon")?, finite("mag")?);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let region = props
        .get("flynn_region")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_owned();
    Some(Quake {
        unid: unid.to_owned(),
        time,
        lat,
        lon,
        mag,
        region,
    })
}

pub fn parse_events(body: &[u8]) -> Result<ParsedQuakes, SourceError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    let features = value
        .get("features")
        .and_then(|v| v.as_array())
        .ok_or_else(|| SourceError::Parse("brak listy features".to_owned()))?;
    let mut parsed = ParsedQuakes::default();
    for feature in features {
        match parse_event(feature) {
            Some(q) => parsed.events.push(q),
            None => parsed.skipped += 1,
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/emsc/pl-bbox-2026-11-22.json");

    #[test]
    fn the_fixture_parses_twenty_events() {
        let parsed = parse_events(FIXTURE).unwrap();
        assert_eq!(parsed.events.len(), 20);
        assert_eq!(parsed.skipped, 0);
        let q = &parsed.events[0];
        assert_eq!(q.unid, "20261122_0000158");
        assert_eq!(
            q.time,
            Utc.with_ymd_and_hms(2026, 11, 22, 12, 26, 38).unwrap()
                + chrono::TimeDelta::milliseconds(654)
        );
        assert_eq!((q.lat, q.lon, q.mag), (51.5552, 16.1286, 2.1));
        assert_eq!(q.region, "POLAND");
        assert!(parsed.events.iter().any(|q| q.region == "HUNGARY"));
    }

    #[test]
    fn no_features_key_is_a_parse_error() {
        assert!(matches!(
            parse_events(br#"{"type":"x"}"#),
            Err(SourceError::Parse(_))
        ));
        assert!(matches!(parse_events(b"[]"), Err(SourceError::Parse(_))));
        assert!(matches!(
            parse_events(b"nie json"),
            Err(SourceError::Parse(_))
        ));
    }

    #[test]
    fn an_event_without_a_required_field_is_skipped_and_counted() {
        let mut v: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
        v["features"][1]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("mag");
        v["features"][2]["properties"]["time"] = "wczoraj".into();
        v["features"][3] = serde_json::json!(5);
        let parsed = parse_events(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(parsed.events.len(), 17);
        assert_eq!(parsed.skipped, 3);
    }

    #[test]
    fn a_missing_region_is_an_empty_string() {
        let mut v: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
        v["features"][0]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("flynn_region");
        let parsed = parse_events(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(parsed.events[0].region, "");
    }
}
