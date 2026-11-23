//! Parser MET Norway (`locationforecast/2.0/compact`).

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Europe::Warsaw;
use serde_json::Value;

use super::{Condition, Day, Forecast, Now, Provider};
use crate::source::SourceError;

pub fn url(base: &str, lat: f64, lon: f64) -> String {
    format!("{base}?lat={lat:.4}&lon={lon:.4}")
}

fn bad(what: &str) -> SourceError {
    SourceError::Parse(format!("MET Norway: {what}"))
}

struct Entry {
    time: DateTime<Utc>,
    temp: f64,
    wind_ms: Option<f64>,
    /// `next_1_hours` (albo `next_6_hours`): opad.
    precip: Option<f64>,
    /// Symbol z `next_1_hours` (albo `next_6_hours`).
    code: Option<Condition>,
}

fn block<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    data.get(key).filter(|b| b.is_object())
}

fn entry(v: &Value) -> Option<Entry> {
    let time = v.get("time")?.as_str()?.parse::<DateTime<Utc>>().ok()?;
    let data = v.get("data")?;
    let instant = data.get("instant")?.get("details")?;
    let temp = instant.get("air_temperature")?.as_f64()?;
    let wind_ms = instant.get("wind_speed").and_then(Value::as_f64);
    let next = block(data, "next_1_hours").or_else(|| block(data, "next_6_hours"));
    let precip = next
        .and_then(|b| b.get("details"))
        .and_then(|d| d.get("precipitation_amount"))
        .and_then(Value::as_f64);
    let code = next
        .and_then(|b| b.get("summary"))
        .and_then(|s| s.get("symbol_code"))
        .and_then(Value::as_str)
        .and_then(Condition::from_met_symbol);
    Some(Entry {
        time,
        temp,
        wind_ms,
        precip,
        code,
    })
}

#[derive(Default)]
struct DayAcc {
    t_min: f64,
    t_max: f64,
    precip: Option<f64>,
    wind_ms: Option<f64>,
    code: Option<Condition>,
}

pub fn parse(body: &[u8], now: DateTime<Utc>) -> Result<Forecast, SourceError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    let props = root
        .get("properties")
        .ok_or_else(|| bad("brak properties"))?;
    let series = props
        .get("timeseries")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("brak timeseries"))?;
    let issued = props
        .get("meta")
        .and_then(|m| m.get("updated_at"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .ok_or_else(|| bad("brak meta.updated_at"))?;

    let mut entries: Vec<Entry> = series.iter().filter_map(entry).collect();
    entries.sort_by_key(|e| e.time);

    let mut acc: BTreeMap<NaiveDate, DayAcc> = BTreeMap::new();
    for e in &entries {
        let date = e.time.with_timezone(&Warsaw).date_naive();
        let d = acc.entry(date).or_insert_with(|| DayAcc {
            t_min: e.temp,
            t_max: e.temp,
            ..DayAcc::default()
        });
        d.t_min = d.t_min.min(e.temp);
        d.t_max = d.t_max.max(e.temp);
        if let Some(p) = e.precip {
            *d.precip.get_or_insert(0.0) += p;
        }
        if let Some(w) = e.wind_ms {
            d.wind_ms = Some(d.wind_ms.map_or(w, |m| m.max(w)));
        }
        if let Some(c) = e.code {
            d.code = Some(d.code.map_or(c, |m| m.max(c)));
        }
    }
    let days = acc
        .into_iter()
        .map(|(date, d)| Day {
            date,
            t_min: d.t_min,
            t_max: d.t_max,
            precip_mm: d.precip,
            gust_max_kmh: None,
            wind_max_kmh: d.wind_ms.map(|w| w * 3.6),
            code: d.code,
        })
        .collect();

    let current = entries
        .iter()
        .rev()
        .find(|e| e.time <= now)
        .or_else(|| entries.first())
        .map(|e| Now {
            temp_c: e.temp,
            code: e.code,
            wind_kmh: e.wind_ms.map(|w| w * 3.6),
            gust_kmh: None,
        });

    Ok(Forecast {
        issued,
        provider: Provider::MetNorway,
        current,
        days,
        hourly: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{at, date};
    use super::super::{Condition, Provider};
    use super::*;
    use serde_json::{Value, json};

    const MET: &[u8] = include_bytes!("../../tests/fixtures/pogoda/met-no-rzeszow-2026-11-22.json");

    #[test]
    fn the_met_fixture_aggregates_local_days() {
        let f = parse(MET, at("2026-11-22T19:00:00Z")).unwrap();
        assert_eq!(f.provider, Provider::MetNorway);
        assert_eq!(f.issued, at("2026-11-22T17:17:09Z"));
        let now = f.current.as_ref().unwrap();
        assert_eq!(
            (now.temp_c, now.code),
            (-0.7, Some(Condition::MaleZachmurzenie))
        );
        let d = f.day(date(2026, 11, 23)).unwrap();
        assert_eq!((d.t_min, d.t_max, d.gust_max_kmh), (-1.9, 1.5, None));
        assert!((d.precip_mm.unwrap() - 4.6).abs() < 1e-9);
        assert!((d.wind_max_kmh.unwrap() - 25.56).abs() < 1e-6);
        assert_eq!(d.code, Some(Condition::Snieg));
    }

    #[test]
    fn current_falls_back_to_the_first_entry_and_schema_errors_fail() {
        let f = parse(MET, at("2026-11-22T10:00:00Z")).unwrap();
        assert_eq!(f.current.unwrap().temp_c, -1.0);
        let mut v: Value = serde_json::from_slice(MET).unwrap();
        v["properties"]
            .as_object_mut()
            .unwrap()
            .remove("timeseries");
        let body = serde_json::to_vec(&v).unwrap();
        assert!(matches!(
            parse(&body, at("2026-11-22T19:00:00Z")),
            Err(SourceError::Parse(_))
        ));
        assert!(matches!(
            parse(
                &serde_json::to_vec(&json!({})).unwrap(),
                at("2026-11-22T19:00:00Z")
            ),
            Err(SourceError::Parse(_))
        ));
    }

    #[test]
    fn url_rounds_coordinates_to_four_places() {
        assert!(url("http://x/compact", 50.040049, 21.99).ends_with("?lat=50.0400&lon=21.9900"));
    }
}
