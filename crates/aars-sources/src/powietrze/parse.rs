//! Parser Open-Meteo Air Quality (`/v1/air-quality`, model CAMS).

use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use serde_json::Value;

use crate::source::SourceError;

/// Pyłki z prognozy CAMS dla Europy (sezonowo; poza sezonem zera albo `null`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pollen {
    Alder,
    Birch,
    Grass,
    Mugwort,
    Olive,
    Ragweed,
}

impl Pollen {
    pub const ALL: [Pollen; 6] = [
        Self::Alder,
        Self::Birch,
        Self::Grass,
        Self::Mugwort,
        Self::Olive,
        Self::Ragweed,
    ];

    /// Nazwa zmiennej w API.
    pub fn key(self) -> &'static str {
        match self {
            Self::Alder => "alder_pollen",
            Self::Birch => "birch_pollen",
            Self::Grass => "grass_pollen",
            Self::Mugwort => "mugwort_pollen",
            Self::Olive => "olive_pollen",
            Self::Ragweed => "ragweed_pollen",
        }
    }

    /// „pyłków …” — dopełniacz.
    pub fn genitive(self) -> &'static str {
        match self {
            Self::Alder => "olchy",
            Self::Birch => "brzozy",
            Self::Grass => "traw",
            Self::Mugwort => "bylicy",
            Self::Olive => "oliwki",
            Self::Ragweed => "ambrozji",
        }
    }

    /// Progi stężenia średniego i wysokiego (ziarna/m³), według typowych
    /// polskich klas stężeń w komunikatach alergologicznych; niżej
    /// stężenie nie jest mówione.
    pub fn thresholds(self) -> (f64, f64) {
        match self {
            Self::Alder => (30.0, 100.0),
            Self::Birch | Self::Olive => (20.0, 75.0),
            Self::Grass => (20.0, 50.0),
            Self::Mugwort | Self::Ragweed => (10.0, 30.0),
        }
    }
}

/// Jedna godzina (albo stan bieżący); brak wartości w odpowiedzi to `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct AirHour {
    pub at: DateTime<Utc>,
    pub aqi: Option<f64>,
    pub pm2_5: Option<f64>,
    pub pm10: Option<f64>,
    /// W kolejności `Pollen::ALL`.
    pub pollen: [Option<f64>; 6],
}

impl AirHour {
    pub fn pollen(&self, p: Pollen) -> Option<f64> {
        let i = Pollen::ALL.iter().position(|x| *x == p)?;
        self.pollen[i]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AirQuality {
    pub current: Option<AirHour>,
    pub hours: Vec<AirHour>,
}

/// Zapytanie: stan bieżący, godzina wstecz i 26 godzin naprzód (doba
/// prognozy niezależnie od pory), czas w UTC.
pub fn url(base: &str, lat: f64, lon: f64, pollen: bool) -> String {
    let mut hourly = "european_aqi,pm10,pm2_5".to_owned();
    if pollen {
        for p in Pollen::ALL {
            hourly.push(',');
            hourly.push_str(p.key());
        }
    }
    format!(
        "{base}?latitude={lat:.4}&longitude={lon:.4}&current=european_aqi,pm10,pm2_5&hourly={hourly}&past_hours=1&forecast_hours=26&timezone=GMT"
    )
}

const TIME: &str = "%Y-%m-%dT%H:%M";

fn bad(what: &str) -> SourceError {
    SourceError::Parse(format!("Open-Meteo Air Quality: {what}"))
}

/// Liczba nieujemna; `null`, tekst albo ujemna to luka.
fn number(v: Option<&Value>) -> Option<f64> {
    v?.as_f64().filter(|x| x.is_finite() && *x >= 0.0)
}

/// Czas odpowiedzi (lokalny według `utc_offset_seconds`) w UTC.
fn time(v: Option<&Value>, offset: TimeDelta) -> Option<DateTime<Utc>> {
    let local = NaiveDateTime::parse_from_str(v?.as_str()?, TIME).ok()?;
    Some((local - offset).and_utc())
}

fn current(root: &Value, offset: TimeDelta) -> Option<AirHour> {
    let c = root.get("current").filter(|c| c.is_object())?;
    Some(AirHour {
        at: time(c.get("time"), offset)?,
        aqi: number(c.get("european_aqi")),
        pm2_5: number(c.get("pm2_5")),
        pm10: number(c.get("pm10")),
        pollen: Pollen::ALL.map(|p| number(c.get(p.key()))),
    })
}

fn hourly(root: &Value, offset: TimeDelta) -> Vec<AirHour> {
    let Some(h) = root.get("hourly").filter(|h| h.is_object()) else {
        return Vec::new();
    };
    let Some(times) = h.get("time").and_then(Value::as_array) else {
        return Vec::new();
    };
    let col = |key: &str| h.get(key).and_then(Value::as_array);
    let (aqi, pm2_5, pm10) = (col("european_aqi"), col("pm2_5"), col("pm10"));
    let pollen = Pollen::ALL.map(|p| col(p.key()));
    let at = |c: Option<&Vec<Value>>, i: usize| number(c.and_then(|c| c.get(i)));
    times
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            Some(AirHour {
                at: time(Some(t), offset)?,
                aqi: at(aqi, i),
                pm2_5: at(pm2_5, i),
                pm10: at(pm10, i),
                pollen: pollen.map(|c| at(c, i)),
            })
        })
        .collect()
}

pub fn parse(body: &[u8]) -> Result<AirQuality, SourceError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    if root.get("error").and_then(Value::as_bool) == Some(true) {
        let reason = root
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("bez opisu");
        return Err(bad(reason));
    }
    let offset = TimeDelta::seconds(
        root.get("utc_offset_seconds")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    );
    let aq = AirQuality {
        current: current(&root, offset),
        hours: hourly(&root, offset),
    };
    if aq.current.is_none() && aq.hours.is_empty() {
        return Err(bad("brak sekcji current i hourly"));
    }
    Ok(aq)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: &[u8] =
        include_bytes!("../../tests/fixtures/powietrze/open-meteo-aq-rzeszow-2026-11-25.json");

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn the_url_asks_for_pollen_only_when_wanted() {
        let u = url("http://x/v1/air-quality", 50.04, 21.99, true);
        assert!(
            u.starts_with("http://x/v1/air-quality?latitude=50.0400&longitude=21.9900&"),
            "{u}"
        );
        assert!(u.contains("current=european_aqi,pm10,pm2_5&"), "{u}");
        assert!(u.contains("hourly=european_aqi,pm10,pm2_5,alder_pollen,birch_pollen,grass_pollen,mugwort_pollen,olive_pollen,ragweed_pollen&"), "{u}");
        assert!(
            u.ends_with("&past_hours=1&forecast_hours=26&timezone=GMT"),
            "{u}"
        );
        let u = url("http://x/v1/air-quality", 50.04, 21.99, false);
        assert!(!u.contains("pollen"), "{u}");
    }

    #[test]
    fn the_fixture_parses_into_utc_hours() {
        let aq = parse(FIX).unwrap();
        let c = aq.current.unwrap();
        assert_eq!(c.at, at("2026-11-25T08:00:00Z"));
        assert_eq!(
            (c.aqi, c.pm2_5, c.pm10),
            (Some(42.0), Some(19.3), Some(21.8))
        );
        assert_eq!(aq.hours.len(), 27);
        assert_eq!(aq.hours[0].at, at("2026-11-25T07:00:00Z"));
        let worst = &aq.hours[11];
        assert_eq!(worst.at, at("2026-11-25T18:00:00Z"));
        assert_eq!(
            (worst.aqi, worst.pm2_5, worst.pm10),
            (Some(69.0), Some(67.7), Some(72.4))
        );
        assert_eq!(worst.pollen(Pollen::Birch), Some(0.0));
    }

    #[test]
    fn a_local_offset_is_taken_back_to_utc() {
        let body = br#"{"utc_offset_seconds":3600,"hourly":{"time":["2026-11-25T09:00"],"european_aqi":[30]}}"#;
        let aq = parse(body).unwrap();
        assert_eq!(aq.hours[0].at, at("2026-11-25T08:00:00Z"));
        assert_eq!(aq.current, None);
    }

    #[test]
    fn nulls_and_short_columns_are_gaps() {
        let body = br#"{"current":{"time":"2026-11-25T08:00","european_aqi":null,"pm10":-1},
            "hourly":{"time":["2026-11-25T08:00","2026-11-25T09:00","zly"],
                      "european_aqi":[null,"x"],"pm2_5":[3.5],"birch_pollen":[null,80.0]}}"#;
        let aq = parse(body).unwrap();
        let c = aq.current.unwrap();
        assert_eq!((c.aqi, c.pm10, c.pm2_5), (None, None, None));
        assert_eq!(aq.hours.len(), 2);
        assert_eq!((aq.hours[0].aqi, aq.hours[0].pm2_5), (None, Some(3.5)));
        assert_eq!((aq.hours[1].aqi, aq.hours[1].pm2_5), (None, None));
        assert_eq!(aq.hours[1].pollen(Pollen::Birch), Some(80.0));
        assert_eq!(aq.hours[1].pollen(Pollen::Grass), None);
    }

    #[test]
    fn errors_and_empty_answers_are_refused() {
        let e = parse(br#"{"error":true,"reason":"Invalid value"}"#).unwrap_err();
        assert!(e.to_string().contains("Invalid value"), "{e}");
        assert!(parse(br#"{"latitude":50.0}"#).is_err());
        assert!(parse(b"nie json").is_err());
    }
}
