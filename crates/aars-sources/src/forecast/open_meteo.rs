//! Parser Open-Meteo (`/v1/forecast`).

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use serde_json::Value;

use super::{Condition, Day, Forecast, Now, Provider};
use crate::source::SourceError;

pub fn url(base: &str, lat: f64, lon: f64) -> String {
    format!(
        "{base}?latitude={lat:.4}&longitude={lon:.4}&current=temperature_2m,wind_speed_10m,wind_gusts_10m,weather_code,precipitation&hourly=temperature_2m,precipitation,wind_gusts_10m,weather_code&daily=temperature_2m_max,temperature_2m_min,precipitation_sum,wind_gusts_10m_max,weather_code&timezone=Europe%2FWarsaw&forecast_days=3"
    )
}

fn bad(what: &str) -> SourceError {
    SourceError::Parse(format!("Open-Meteo: {what}"))
}

fn code_of(v: Option<&Value>) -> Option<Condition> {
    let v = v?;
    let code = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
    Condition::from_wmo(code)
}

/// Wymagana tablica `daily.<key>`.
fn column<'a>(daily: &'a Value, key: &str) -> Result<&'a Vec<Value>, SourceError> {
    daily
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| bad(&format!("brak daily.{key}")))
}

pub fn parse(body: &[u8], fetched: DateTime<Utc>) -> Result<Forecast, SourceError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    let daily = root
        .get("daily")
        .filter(|d| d.is_object())
        .ok_or_else(|| bad("brak daily"))?;
    let time = column(daily, "time")?;
    let t_max = column(daily, "temperature_2m_max")?;
    let t_min = column(daily, "temperature_2m_min")?;
    let precip = column(daily, "precipitation_sum")?;
    let gust = column(daily, "wind_gusts_10m_max")?;
    let codes = column(daily, "weather_code")?;
    let n = time.len();
    if [t_max, t_min, precip, gust, codes]
        .iter()
        .any(|c| c.len() != n)
    {
        return Err(bad("tablice daily różnej długości"));
    }

    let mut days = Vec::new();
    for i in 0..n {
        let date = time
            .get(i)
            .and_then(Value::as_str)
            .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            .ok_or_else(|| bad("zła data w daily.time"))?;
        let num = |col: &Vec<Value>| col.get(i).and_then(Value::as_f64);
        let (Some(t_min), Some(t_max)) = (num(t_min), num(t_max)) else {
            continue;
        };
        days.push(Day {
            date,
            t_min,
            t_max,
            precip_mm: num(precip),
            gust_max_kmh: num(gust),
            wind_max_kmh: None,
            code: code_of(codes.get(i)),
        });
    }

    let offset = root
        .get("utc_offset_seconds")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cur = root.get("current").filter(|c| c.is_object());
    let issued = cur
        .and_then(|c| c.get("time"))
        .and_then(Value::as_str)
        .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").ok())
        .map(|t| t.and_utc() - Duration::seconds(offset))
        .unwrap_or(fetched);
    let current = cur.and_then(|c| {
        Some(Now {
            temp_c: c.get("temperature_2m")?.as_f64()?,
            code: code_of(c.get("weather_code")),
            wind_kmh: c.get("wind_speed_10m").and_then(Value::as_f64),
            gust_kmh: c.get("wind_gusts_10m").and_then(Value::as_f64),
        })
    });

    Ok(Forecast {
        issued,
        provider: Provider::OpenMeteo,
        current,
        days,
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{at, date};
    use super::super::{Condition, Provider};
    use super::*;
    use serde_json::{Value, json};

    const OPEN_METEO: &[u8] =
        include_bytes!("../../tests/fixtures/pogoda/open-meteo-rzeszow-2026-11-22.json");

    #[test]
    fn the_open_meteo_fixture_parses() {
        let f = parse(OPEN_METEO, at("2026-11-22T18:50:00Z")).unwrap();
        assert_eq!(f.provider, Provider::OpenMeteo);
        assert_eq!(f.issued, at("2026-11-22T18:30:00Z"));
        let now = f.current.as_ref().unwrap();
        assert_eq!(
            (now.temp_c, now.code, now.gust_kmh),
            (2.2, Some(Condition::Bezchmurnie), Some(17.6))
        );
        assert_eq!(f.days.len(), 2);
        let d1 = f.day(date(2026, 11, 23)).unwrap();
        assert_eq!(
            (d1.t_min, d1.t_max, d1.precip_mm, d1.gust_max_kmh),
            (0.9, 2.5, Some(2.4), Some(45.0))
        );
        assert_eq!(d1.code, Some(Condition::Snieg));
        assert_eq!(
            f.day(date(2026, 11, 22)).unwrap().code,
            Some(Condition::Mzawka)
        );
    }

    fn fixture() -> Value {
        serde_json::from_slice(OPEN_METEO).unwrap()
    }
    fn parse_value(v: &Value) -> Result<Forecast, SourceError> {
        parse(&serde_json::to_vec(v).unwrap(), at("2026-11-22T18:50:00Z"))
    }

    #[test]
    fn nulls_in_daily_arrays_are_gaps_not_zeros() {
        let mut v = fixture();
        v["daily"]["precipitation_sum"][1] = Value::Null;
        v["daily"]["weather_code"][0] = json!(999);
        v["daily"]["temperature_2m_max"][0] = Value::Null;
        v["current"]["weather_code"] = Value::Null;
        let f = parse_value(&v).unwrap();
        assert_eq!(f.days.len(), 1);
        assert!(f.day(date(2026, 11, 22)).is_none());
        assert_eq!(f.day(date(2026, 11, 23)).unwrap().precip_mm, None);
        assert_eq!(f.current.unwrap().code, None);
    }

    #[test]
    fn unknown_day_code_and_missing_current_are_gaps() {
        let mut v = fixture();
        v["daily"]["weather_code"][0] = json!(999);
        v["current"]["temperature_2m"] = Value::Null;
        let f = parse_value(&v).unwrap();
        assert_eq!(f.day(date(2026, 11, 22)).unwrap().code, None);
        assert!(f.current.is_none());
        let mut v = fixture();
        v.as_object_mut().unwrap().remove("current");
        assert_eq!(parse_value(&v).unwrap().issued, at("2026-11-22T18:50:00Z"));
    }

    #[test]
    fn a_broken_open_meteo_schema_is_a_parse_error() {
        let mut v = fixture();
        v.as_object_mut().unwrap().remove("daily");
        assert!(matches!(parse_value(&v), Err(SourceError::Parse(_))));
        let mut v = fixture();
        v["daily"]["temperature_2m_min"] = json!([1.0]);
        assert!(matches!(parse_value(&v), Err(SourceError::Parse(_))));
        let mut v = fixture();
        v["daily"] = json!([]);
        assert!(matches!(parse_value(&v), Err(SourceError::Parse(_))));
        assert!(matches!(
            parse(b"not json", at("2026-11-22T18:50:00Z")),
            Err(SourceError::Parse(_))
        ));
    }

    #[test]
    fn url_rounds_coordinates_to_four_places() {
        assert!(
            url("http://x/v1/forecast", 50.04, 21.99)
                .contains("latitude=50.0400&longitude=21.9900")
        );
        assert!(
            url("http://x/v1/forecast", 50.04, 21.99)
                .ends_with("&timezone=Europe%2FWarsaw&forecast_days=3")
        );
    }
}
