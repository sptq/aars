//! Parsery odpowiedzi SWPC (F10.7, Kp) i KC2G (jonosondy). Dane z sieci:
//! bez `unwrap`, `expect` i indeksowania.

use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use serde_json::Value;

use crate::source::SourceError;
use crate::speech::haversine_km;

#[derive(Debug, Clone, PartialEq)]
pub struct Ionosonde {
    pub name: String,
    pub lat: f64,
    /// −180..180.
    pub lon: f64,
    pub time: DateTime<Utc>,
    /// Pewność skalowania; `-1` = skalowanie ręczne, brak = NaN.
    pub cs: f64,
    pub fof2: Option<f64>,
    pub mufd: Option<f64>,
}

fn err(msg: &str) -> SourceError {
    SourceError::Parse(msg.to_owned())
}

fn records(body: &[u8]) -> Result<Vec<Value>, SourceError> {
    match serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))? {
        Value::Array(a) => Ok(a),
        _ => Err(err("oczekiwano tablicy rekordów")),
    }
}

/// Liczba skończona z liczby JSON albo z napisu.
fn number(v: Option<&Value>) -> Option<f64> {
    let n = match v? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

fn time(v: Option<&Value>) -> Option<DateTime<Utc>> {
    let s = v?.as_str()?;
    NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|t| t.and_utc())
}

fn latest(
    body: &[u8],
    value_key: &str,
    only_2800: bool,
) -> Result<(DateTime<Utc>, f64), SourceError> {
    records(body)?
        .iter()
        .filter(|r| !only_2800 || number(r.get("frequency")) == Some(2800.0))
        .filter_map(|r| Some((time(r.get("time_tag"))?, number(r.get(value_key))?)))
        .max_by_key(|(t, _)| *t)
        .ok_or_else(|| err("brak użytecznego rekordu"))
}

/// Najnowszy strumień F10.7 (`frequency == 2800`).
pub fn latest_f107(body: &[u8]) -> Result<(DateTime<Utc>, f64), SourceError> {
    latest(body, "flux", true)
}

/// Najnowszy planetarny Kp (`time_tag` = początek okna 3 h).
pub fn latest_kp(body: &[u8]) -> Result<(DateTime<Utc>, f64), SourceError> {
    latest(body, "Kp", false)
}

fn ionosonde(r: &Value) -> Option<Ionosonde> {
    let st = r.get("station")?;
    let lat = number(st.get("latitude")).filter(|l| (-90.0..=90.0).contains(l))?;
    let lon = number(st.get("longitude")).filter(|l| (-180.0..=360.0).contains(l))?;
    Some(Ionosonde {
        name: st.get("name")?.as_str()?.trim().to_owned(),
        lat,
        lon: if lon > 180.0 { lon - 360.0 } else { lon },
        time: time(r.get("time"))?,
        cs: number(r.get("cs")).unwrap_or(f64::NAN),
        fof2: number(r.get("fof2")),
        mufd: number(r.get("mufd")),
    })
}

/// Jonosondy z KC2G; rekord bez czytelnych współrzędnych lub czasu pomijamy.
pub fn ionosondes(body: &[u8]) -> Result<Vec<Ionosonde>, SourceError> {
    Ok(records(body)?.iter().filter_map(ionosonde).collect())
}

const MAX_AGE: TimeDelta = TimeDelta::minutes(60);
const MAX_KM: f64 = 1200.0;

/// Najbliższa wiarygodna, świeża jonosonda w promieniu 1200 km.
pub fn pick_ionosonde(
    all: &[Ionosonde],
    from: (f64, f64),
    now: DateTime<Utc>,
) -> Option<(&Ionosonde, f64)> {
    all.iter()
        .filter(|s| (TimeDelta::zero()..=MAX_AGE).contains(&(now - s.time)))
        .filter(|s| s.cs == -1.0 || s.cs >= 70.0)
        .filter(|s| s.fof2.is_some() && s.mufd.is_some())
        .map(|s| (s, haversine_km(from, (s.lat, s.lon))))
        .filter(|(_, km)| *km <= MAX_KM)
        .min_by(|a, b| a.1.total_cmp(&b.1))
}
