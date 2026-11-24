//! Parser Open-Meteo (`/v1/forecast`).

use chrono::{DateTime, Duration, DurationRound, NaiveDate, NaiveDateTime, Utc};
use serde_json::Value;

use super::{Condition, Day, Forecast, Hour, Now, Provider};
use crate::source::SourceError;

pub fn url(base: &str, lat: f64, lon: f64) -> String {
    format!(
        "{base}?latitude={lat:.4}&longitude={lon:.4}&current=temperature_2m,apparent_temperature,relative_humidity_2m,pressure_msl,wind_speed_10m,wind_gusts_10m,wind_direction_10m,cloud_cover,visibility,weather_code,precipitation&hourly=temperature_2m,precipitation,wind_speed_10m,wind_gusts_10m,weather_code,pressure_msl&daily=temperature_2m_max,temperature_2m_min,precipitation_sum,wind_speed_10m_max,wind_gusts_10m_max,weather_code,sunrise,sunset,wind_direction_10m_dominant&timezone=Europe%2FWarsaw&forecast_days=3&past_hours=3&wind_speed_unit=ms"
    )
}

/// Godzina w odpowiedzi (czas lokalny stacji).
const LOCAL_TIME: &str = "%Y-%m-%dT%H:%M";

fn bad(what: &str) -> SourceError {
    SourceError::Parse(format!("Open-Meteo: {what}"))
}

fn code_of(v: Option<&Value>) -> Option<Condition> {
    let v = v?;
    let code = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
    Condition::from_wmo(code)
}

fn local_time(v: Option<&Value>) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(v?.as_str()?, LOCAL_TIME).ok()
}

/// Mnożnik do m/s według `<section>_units.<key>`. Pytamy o m/s, ale
/// odpowiedź mówi, w czym jest naprawdę (starsze odpowiedzi: km/h); brak
/// opisu → m/s, nieznana jednostka → pole jest luką (`None`).
fn to_ms(root: &Value, section: &str, key: &str) -> Option<f64> {
    let unit = root
        .get(format!("{section}_units"))
        .and_then(|u| u.get(key))
        .and_then(Value::as_str);
    match unit {
        None | Some("m/s") => Some(1.0),
        Some("km/h") => Some(1.0 / 3.6),
        Some("kn") => Some(0.514_444),
        Some("mp/h") => Some(0.447_04),
        Some(_) => None,
    }
}

/// Wymagana tablica `daily.<key>`.
fn column<'a>(daily: &'a Value, key: &str) -> Result<&'a Vec<Value>, SourceError> {
    daily
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| bad(&format!("brak daily.{key}")))
}

/// Godziny z `hourly` (czas lokalny); brak sekcji albo `time` → pusto,
/// brak wartości (także krótsza tablica) → luka w tej godzinie.
fn hourly(root: &Value) -> Vec<Hour> {
    let Some(hourly) = root.get("hourly").filter(|h| h.is_object()) else {
        return Vec::new();
    };
    let Some(time) = hourly.get("time").and_then(Value::as_array) else {
        return Vec::new();
    };
    let col = |key: &str| hourly.get(key).and_then(Value::as_array);
    let (temp, precip, wind, gust, codes, pressure) = (
        col("temperature_2m"),
        col("precipitation"),
        col("wind_speed_10m"),
        col("wind_gusts_10m"),
        col("weather_code"),
        col("pressure_msl"),
    );
    let (wind_k, gust_k) = (
        to_ms(root, "hourly", "wind_speed_10m"),
        to_ms(root, "hourly", "wind_gusts_10m"),
    );
    let at = |c: Option<&Vec<Value>>, i: usize| c.and_then(|c| c.get(i)).and_then(Value::as_f64);
    let speed = |c, k: Option<f64>, i| Some(at(c, i)? * k?);
    time.iter()
        .enumerate()
        .filter_map(|(i, t)| {
            Some(Hour {
                local: local_time(Some(t))?,
                temp_c: at(temp, i),
                precip_mm: at(precip, i),
                wind_ms: speed(wind, wind_k, i),
                gust_ms: speed(gust, gust_k, i),
                pressure_hpa: at(pressure, i),
                code: code_of(codes.and_then(|c| c.get(i))),
            })
        })
        .collect()
}

/// Ciśnienie w godzinie `now` (pełnej) minus 3 h wcześniej; luka → `None`.
fn pressure_trend(hours: &[Hour], now: NaiveDateTime) -> Option<f64> {
    let hour = now.duration_trunc(Duration::hours(1)).ok()?;
    let at = |t: NaiveDateTime| {
        hours
            .iter()
            .find(|h| h.local == t)
            .and_then(|h| h.pressure_hpa)
    };
    Some(at(hour)? - at(hour - Duration::hours(3))?)
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
    // Pola dodane później: brak całej tablicy albo wartości to luka.
    let optional = |key: &str| daily.get(key).and_then(Value::as_array);
    let (wind, wind_dir, sunrise, sunset) = (
        optional("wind_speed_10m_max"),
        optional("wind_direction_10m_dominant"),
        optional("sunrise"),
        optional("sunset"),
    );
    let (gust_k, wind_k) = (
        to_ms(&root, "daily", "wind_gusts_10m_max"),
        to_ms(&root, "daily", "wind_speed_10m_max"),
    );

    let mut days = Vec::new();
    for i in 0..n {
        let date = time
            .get(i)
            .and_then(Value::as_str)
            .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            .ok_or_else(|| bad("zła data w daily.time"))?;
        let num = |col: &Vec<Value>| col.get(i).and_then(Value::as_f64);
        let opt = |col: Option<&Vec<Value>>| col.and_then(num);
        let (Some(t_min), Some(t_max)) = (num(t_min), num(t_max)) else {
            continue;
        };
        days.push(Day {
            date,
            t_min,
            t_max,
            precip_mm: num(precip),
            gust_max_ms: num(gust).zip(gust_k).map(|(v, k)| v * k),
            wind_max_ms: opt(wind).zip(wind_k).map(|(v, k)| v * k),
            wind_dir_deg: opt(wind_dir),
            code: code_of(codes.get(i)),
            sunrise: local_time(sunrise.and_then(|c| c.get(i))),
            sunset: local_time(sunset.and_then(|c| c.get(i))),
        });
    }

    let hours = hourly(&root);
    let offset = root
        .get("utc_offset_seconds")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cur = root.get("current").filter(|c| c.is_object());
    let cur_local = local_time(cur.and_then(|c| c.get("time")));
    let issued = cur_local
        .map(|t| t.and_utc() - Duration::seconds(offset))
        .unwrap_or(fetched);
    let current = cur.and_then(|c| {
        let num = |key: &str| c.get(key).and_then(Value::as_f64);
        let speed = |key: &str| Some(num(key)? * to_ms(&root, "current", key)?);
        Some(Now {
            temp_c: num("temperature_2m")?,
            apparent_c: num("apparent_temperature"),
            humidity_pct: num("relative_humidity_2m"),
            pressure_hpa: num("pressure_msl"),
            pressure_trend_hpa_3h: cur_local.and_then(|t| pressure_trend(&hours, t)),
            wind_ms: speed("wind_speed_10m"),
            gust_ms: speed("wind_gusts_10m"),
            wind_dir_deg: num("wind_direction_10m"),
            cloud_pct: num("cloud_cover"),
            visibility_m: num("visibility"),
            precip_mm: num("precipitation"),
            code: code_of(c.get("weather_code")),
        })
    });

    Ok(Forecast {
        issued,
        provider: Provider::OpenMeteo,
        current,
        days,
        hourly: hours,
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
        assert_eq!((now.temp_c, now.code), (2.2, Some(Condition::Bezchmurnie)));
        // Starszy fiks jest w km/h: przeliczone na m/s według `current_units`.
        assert!((now.gust_ms.unwrap() - 17.6 / 3.6).abs() < 1e-9);
        assert_eq!((now.humidity_pct, now.pressure_trend_hpa_3h), (None, None));
        assert_eq!(f.days.len(), 2);
        let d1 = f.day(date(2026, 11, 23)).unwrap();
        assert_eq!(
            (d1.t_min, d1.t_max, d1.precip_mm),
            (0.9, 2.5, Some(2.4))
        );
        assert!((d1.gust_max_ms.unwrap() - 12.5).abs() < 1e-9);
        assert_eq!((d1.wind_max_ms, d1.sunrise, d1.wind_dir_deg), (None, None, None));
        assert_eq!(d1.code, Some(Condition::Snieg));
        assert_eq!(
            f.day(date(2026, 11, 22)).unwrap().code,
            Some(Condition::Mzawka)
        );
    }

    #[test]
    fn hourly_values_are_parsed_as_local_hours_with_gaps() {
        let f = parse(OPEN_METEO, at("2026-11-22T18:50:00Z")).unwrap();
        assert_eq!(f.hourly.len(), 48);
        let h = &f.hourly[12];
        assert_eq!(h.local.to_string(), "2026-11-22 12:00:00");
        assert_eq!((h.temp_c, h.precip_mm), (Some(6.2), Some(0.0)));
        assert!((h.gust_ms.unwrap() - 40.3 / 3.6).abs() < 1e-9);

        let mut v = fixture();
        v["hourly"]["wind_gusts_10m"][3] = Value::Null;
        v["hourly"]["temperature_2m"] = json!([1.0]);
        let f = parse_value(&v).unwrap();
        assert_eq!(f.hourly.len(), 48);
        assert_eq!(f.hourly[3].gust_ms, None);
        assert_eq!(f.hourly[0].temp_c, Some(1.0));
        assert_eq!(f.hourly[1].temp_c, None);

        let mut v = fixture();
        v.as_object_mut().unwrap().remove("hourly");
        assert!(parse_value(&v).unwrap().hourly.is_empty());
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
                .ends_with("&timezone=Europe%2FWarsaw&forecast_days=3&past_hours=3&wind_speed_unit=ms")
        );
    }

    const STATION: &[u8] =
        include_bytes!("../../tests/fixtures/pogoda/open-meteo-51.38-20.28-2026-11-24.json");
    const STATION_FETCHED: &str = "2026-11-24T22:41:54Z";

    fn station() -> Value {
        serde_json::from_slice(STATION).unwrap()
    }
    fn parse_station(v: &Value) -> Forecast {
        parse(&serde_json::to_vec(v).unwrap(), at(STATION_FETCHED)).unwrap()
    }
    fn local(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, LOCAL_TIME).unwrap()
    }

    #[test]
    fn the_url_asks_for_every_field_the_bulletin_speaks() {
        let u = url("http://x/v1/forecast", 51.38, 20.28);
        for field in [
            "current=temperature_2m,apparent_temperature,relative_humidity_2m,pressure_msl,wind_speed_10m,wind_gusts_10m,wind_direction_10m,cloud_cover,visibility,weather_code,precipitation&",
            "hourly=temperature_2m,precipitation,wind_speed_10m,wind_gusts_10m,weather_code,pressure_msl&",
            ",sunrise,sunset,wind_direction_10m_dominant&",
            "&past_hours=3",
            "&wind_speed_unit=ms",
        ] {
            assert!(u.contains(field), "{field} w {u}");
        }
    }

    #[test]
    fn the_station_fixture_parses_every_field() {
        let f = parse(STATION, at(STATION_FETCHED)).unwrap();
        assert_eq!(f.issued, at("2026-11-24T22:30:00Z"));
        let now = f.current.as_ref().unwrap();
        assert_eq!(
            *now,
            Now {
                temp_c: 0.9,
                apparent_c: Some(-3.4),
                humidity_pct: Some(93.0),
                pressure_hpa: Some(1024.2),
                // 23:00 → 1024,3 hPa, 20:00 → 1023,0 hPa.
                pressure_trend_hpa_3h: Some(1024.3 - 1023.0),
                wind_ms: Some(4.0),
                gust_ms: Some(8.7),
                wind_dir_deg: Some(296.0),
                cloud_pct: Some(100.0),
                visibility_m: Some(5080.0),
                precip_mm: Some(0.0),
                code: Some(Condition::Pochmurno),
            }
        );
        assert_eq!(f.days.len(), 3);
        assert_eq!(
            *f.day(date(2026, 11, 25)).unwrap(),
            Day {
                date: date(2026, 11, 25),
                t_min: -3.6,
                t_max: 0.8,
                precip_mm: Some(0.2),
                gust_max_ms: Some(8.2),
                wind_max_ms: Some(3.4),
                wind_dir_deg: Some(270.0),
                code: Some(Condition::Snieg),
                sunrise: Some(local("2026-11-25T07:12")),
                sunset: Some(local("2026-11-25T15:38")),
            }
        );
        // Godziny zaczynają się 3 h przed bieżącą (past_hours=3).
        let h = &f.hourly[0];
        assert_eq!(
            *h,
            Hour {
                local: local("2026-11-24T20:00"),
                temp_c: Some(1.3),
                precip_mm: Some(0.1),
                wind_ms: Some(5.4),
                gust_ms: Some(10.6),
                pressure_hpa: Some(1023.0),
                code: Some(Condition::Mzawka),
            }
        );
    }

    #[test]
    fn null_new_fields_are_gaps() {
        let mut v = station();
        for key in [
            "apparent_temperature",
            "relative_humidity_2m",
            "pressure_msl",
            "wind_speed_10m",
            "wind_gusts_10m",
            "wind_direction_10m",
            "cloud_cover",
            "visibility",
            "precipitation",
        ] {
            v["current"][key] = Value::Null;
        }
        v["daily"]["sunrise"][1] = Value::Null;
        v["daily"]["wind_direction_10m_dominant"][1] = Value::Null;
        v["daily"]["wind_speed_10m_max"][1] = Value::Null;
        v["hourly"]["pressure_msl"][1] = Value::Null;
        v["hourly"]["weather_code"][1] = Value::Null;
        v["hourly"]["wind_speed_10m"][1] = Value::Null;
        let f = parse_station(&v);
        let now = f.current.clone().unwrap();
        assert_eq!(now.temp_c, 0.9);
        assert_eq!(
            (now.apparent_c, now.humidity_pct, now.pressure_hpa, now.wind_ms, now.gust_ms),
            (None, None, None, None, None)
        );
        assert_eq!(
            (now.wind_dir_deg, now.cloud_pct, now.visibility_m, now.precip_mm),
            (None, None, None, None)
        );
        // Tendencja liczy się z godzin, nie z bieżącego ciśnienia.
        assert!(now.pressure_trend_hpa_3h.is_some());
        let d = f.day(date(2026, 11, 25)).unwrap();
        assert_eq!((d.sunrise, d.wind_dir_deg, d.wind_max_ms), (None, None, None));
        assert_eq!(d.sunset, Some(local("2026-11-25T15:38")));
        let h = &f.hourly[1];
        assert_eq!((h.pressure_hpa, h.code, h.wind_ms), (None, None, None));

        // Brak godziny sprzed 3 h albo bieżącej → brak tendencji.
        let mut v = station();
        v["hourly"]["pressure_msl"][0] = Value::Null;
        assert_eq!(parse_station(&v).current.unwrap().pressure_trend_hpa_3h, None);
        let mut v = station();
        v["hourly"]["pressure_msl"][3] = Value::Null;
        assert_eq!(parse_station(&v).current.unwrap().pressure_trend_hpa_3h, None);

        // Brak całych nowych tablic daily to luka, nie błąd schematu.
        let mut v = station();
        let daily = v["daily"].as_object_mut().unwrap();
        for key in ["sunrise", "sunset", "wind_direction_10m_dominant", "wind_speed_10m_max"] {
            daily.remove(key);
        }
        let f = parse_station(&v);
        assert_eq!(f.days.len(), 3);
        assert!(f.days.iter().all(|d| d.sunrise.is_none() && d.wind_max_ms.is_none()));
    }

    #[test]
    fn speeds_follow_the_declared_unit() {
        let mut v = station();
        v["current_units"]["wind_speed_10m"] = "km/h".into();
        v["current"]["wind_speed_10m"] = 36.0.into();
        v["daily_units"]["wind_gusts_10m_max"] = "kn".into();
        v["hourly_units"]["wind_gusts_10m"] = "furlong/fortnight".into();
        let f = parse_station(&v);
        assert!((f.current.unwrap().wind_ms.unwrap() - 10.0).abs() < 1e-9);
        assert!((f.days[0].gust_max_ms.unwrap() - 13.0 * 0.514_444).abs() < 1e-9);
        assert!(f.hourly.iter().all(|h| h.gust_ms.is_none()));
        // Bez opisu jednostek: m/s, o które pytamy.
        let mut v = station();
        v.as_object_mut().unwrap().remove("current_units");
        assert_eq!(parse_station(&v).current.unwrap().gust_ms, Some(8.7));
    }
}
