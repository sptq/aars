//! Parsowanie `noaa-scales.json` (SWPC). Dane z sieci: bez `unwrap`, `expect`
//! i indeksowania.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};

use crate::source::SourceError;

/// Bieżące skale NOAA; `null` w odpowiedzi znaczy 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scales {
    pub stamp: DateTime<Utc>,
    pub r: u8,
    pub s: u8,
    pub g: u8,
}

fn err(msg: &str) -> SourceError {
    SourceError::Parse(msg.to_owned())
}

fn scale(current: &serde_json::Value, name: &str) -> Result<u8, SourceError> {
    let entry = current
        .get(name)
        .and_then(|v| v.as_object())
        .ok_or_else(|| SourceError::Parse(format!("brak skali {name}")))?;
    match entry.get("Scale") {
        Some(serde_json::Value::Null) => Ok(0),
        Some(serde_json::Value::String(s)) => match s.trim().parse::<u8>() {
            Ok(n) if n <= 5 => Ok(n),
            _ => Err(SourceError::Parse(format!("zły poziom skali {name}: {s}"))),
        },
        _ => Err(SourceError::Parse(format!("brak poziomu skali {name}"))),
    }
}

pub fn parse_scales(body: &[u8]) -> Result<Scales, SourceError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    let current = value.get("0").ok_or_else(|| err("brak klucza \"0\""))?;
    let date = current
        .get("DateStamp")
        .and_then(|v| v.as_str())
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .ok_or_else(|| err("zła data pomiaru"))?;
    let time = current
        .get("TimeStamp")
        .and_then(|v| v.as_str())
        .and_then(|s| NaiveTime::parse_from_str(s, "%H:%M:%S").ok())
        .ok_or_else(|| err("zła godzina pomiaru"))?;
    Ok(Scales {
        stamp: date.and_time(time).and_utc(),
        r: scale(current, "R")?,
        s: scale(current, "S")?,
        g: scale(current, "G")?,
    })
}
