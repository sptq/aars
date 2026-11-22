//! Parsowanie odpowiedzi GIOŚ. Dane z sieci: bez `unwrap`, `expect` i indeksowania.

use crate::source::SourceError;

#[derive(Debug, Clone, PartialEq)]
pub struct Station {
    pub id: i64,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    pub zone_code: String,
    pub zone_name: String,
    /// `standardTypeCode`: `PI`, `PA` albo inny.
    pub level: String,
    /// `standardType` z odpowiedzi („Poziom informowania”).
    pub standard_type: String,
    pub param_code: String,
    pub param_name: String,
    pub station_id: i64,
    pub precaution: String,
}

/// Współrzędna w GIOŚ to napis (`"50.060381"`); liczba też przechodzi.
fn coordinate(v: &serde_json::Value, min: f64, max: f64) -> Option<f64> {
    let n = match v {
        serde_json::Value::String(s) => s.trim().replace(',', ".").parse::<f64>().ok()?,
        other => other.as_f64()?,
    };
    (n.is_finite() && (min..=max).contains(&n)).then_some(n)
}

fn parse_station(v: &serde_json::Value) -> Option<Station> {
    Some(Station {
        id: v.get("Identyfikator stacji")?.as_i64()?,
        name: v
            .get("Nazwa stacji")
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .trim()
            .to_owned(),
        lat: coordinate(v.get("WGS84 φ N")?, -90.0, 90.0)?,
        lon: coordinate(v.get("WGS84 λ E")?, -180.0, 180.0)?,
    })
}

/// Jedna strona listy stacji i `totalPages` (brak pola = 1). Stacje bez
/// identyfikatora albo współrzędnych są pomijane: lista bywa niepełna.
pub fn parse_stations(body: &[u8]) -> Result<(Vec<Station>, u32), SourceError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    let list = value
        .get("Lista stacji pomiarowych")
        .and_then(|v| v.as_array())
        .ok_or_else(|| SourceError::Parse("brak listy stacji pomiarowych".to_owned()))?;
    let pages = value
        .get("totalPages")
        .and_then(|v| v.as_u64())
        .map_or(1, |n| u32::try_from(n).unwrap_or(u32::MAX));
    Ok((list.iter().filter_map(parse_station).collect(), pages))
}

fn text<'a>(v: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    Some(cur.as_str()?.trim()).filter(|s| !s.is_empty())
}

fn parse_warning(v: &serde_json::Value) -> Option<Warning> {
    let level = text(v, &["standard", "standardTypeCode"])?.to_owned();
    Some(Warning {
        zone_code: text(v, &["zone", "zoneCode"])?.to_owned(),
        zone_name: text(v, &["zone", "zoneName"])?.to_owned(),
        standard_type: text(v, &["standard", "standardType"])
            .unwrap_or(&level)
            .to_owned(),
        level,
        param_code: text(v, &["param", "paramCode"])?.to_owned(),
        param_name: text(v, &["param", "paramName"])?.to_owned(),
        station_id: v.get("sensor")?.get("stationId")?.as_i64()?,
        precaution: v
            .get("recommendedPrecaution")
            .and_then(|p| p.as_str())
            .unwrap_or_default()
            .to_owned(),
    })
}

/// Ostrzeżenia (nieaktywne pomijane po cichu) i liczba nieczytelnych.
pub fn parse_warnings(body: &[u8]) -> Result<(Vec<Warning>, usize), SourceError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    let list = value
        .as_array()
        .ok_or_else(|| SourceError::Parse("brak listy ostrzeżeń".to_owned()))?;
    let mut warnings = Vec::new();
    let mut skipped = 0;
    for item in list {
        if item.get("active").and_then(|a| a.as_bool()) == Some(false) {
            continue;
        }
        match parse_warning(item) {
            Some(w) => warnings.push(w),
            None => skipped += 1,
        }
    }
    Ok((warnings, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATIONS: &[u8] =
        include_bytes!("../../tests/fixtures/gios/findAll-size500-2026-11-22.json");
    const WARNINGS: &[u8] = include_bytes!("../../tests/fixtures/gios/getWarnings-2026-11-22.json");

    #[test]
    fn the_station_list_parses_with_string_coordinates() {
        let (stations, pages) = parse_stations(STATIONS).unwrap();
        assert_eq!((stations.len(), pages), (288, 1));
        let s = stations.iter().find(|s| s.id == 17179).unwrap();
        assert_eq!((s.lat, s.lon), (50.060381, 21.980511));
        assert!(s.name.contains("Starzyńskiego"), "{}", s.name);
    }

    #[test]
    fn a_station_without_coordinates_is_dropped_not_fatal() {
        let body = r#"{"Lista stacji pomiarowych":[
            {"Identyfikator stacji":1,"Nazwa stacji":"A","WGS84 φ N":"50.1","WGS84 λ E":"20.2"},
            {"Identyfikator stacji":2,"Nazwa stacji":"B","WGS84 φ N":"x","WGS84 λ E":"20.2"},
            {"Identyfikator stacji":3,"Nazwa stacji":"C"}],"totalPages":3}"#
            .as_bytes();
        let (stations, pages) = parse_stations(body).unwrap();
        assert_eq!((stations.len(), pages), (1, 3));
        assert!(parse_stations(b"{}").is_err());
        assert!(parse_stations(b"nie json").is_err());
    }

    #[test]
    fn the_warnings_fixture_parses_eighteen_items() {
        let (w, skipped) = parse_warnings(WARNINGS).unwrap();
        assert_eq!((w.len(), skipped), (18, 0));
        let rzeszow = w.iter().find(|w| w.zone_code == "PL1801").unwrap();
        assert_eq!(
            (
                rzeszow.zone_name.as_str(),
                rzeszow.level.as_str(),
                rzeszow.standard_type.as_str(),
                rzeszow.param_code.as_str(),
                rzeszow.station_id
            ),
            ("miasto Rzeszów", "PI", "Poziom informowania", "PM10", 17179)
        );
        assert!(rzeszow.precaution.starts_with("Ogół ludności:"));
    }

    #[test]
    fn a_warning_missing_a_required_field_is_skipped() {
        let mut v: serde_json::Value = serde_json::from_slice(WARNINGS).unwrap();
        v[0]["zone"]["zoneCode"] = serde_json::Value::Null;
        v[1]["recommendedPrecaution"] = serde_json::Value::Null;
        let (w, skipped) = parse_warnings(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!((w.len(), skipped), (17, 1));
        assert!(w.iter().any(|w| w.precaution.is_empty()));
        assert!(parse_warnings(b"{}").is_err());
    }

    #[test]
    fn an_inactive_warning_is_ignored() {
        let mut v: serde_json::Value = serde_json::from_slice(WARNINGS).unwrap();
        v[0]["active"] = false.into();
        let (w, skipped) = parse_warnings(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!((w.len(), skipped), (17, 0));
    }
}
