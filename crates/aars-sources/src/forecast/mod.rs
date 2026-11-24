//! Wspólny model prognozy pogody (B5, B6) dla Open-Meteo i MET Norway.

pub mod met_no;
pub mod open_meteo;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

/// Dostawca prognozy; decyduje o atrybucji w treści.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenMeteo,
    MetNorway,
}

impl Provider {
    pub fn attribution(self) -> &'static str {
        match self {
            Provider::OpenMeteo => "Dane: Open-Meteo.",
            Provider::MetNorway => "Dane: MET Norway.",
        }
    }
}

/// Opis pogody; kolejność wariantów = rosnąca dotkliwość.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Condition {
    Bezchmurnie,
    MaleZachmurzenie,
    Pochmurno,
    Mgla,
    Mzawka,
    PrzelotnyDeszcz,
    Deszcz,
    DeszczZeSniegiem,
    Snieg,
    Burza,
}

impl Condition {
    pub fn words(self) -> &'static str {
        match self {
            Condition::Bezchmurnie => "bezchmurnie",
            Condition::MaleZachmurzenie => "małe zachmurzenie",
            Condition::Pochmurno => "pochmurno",
            Condition::Mgla => "mgła",
            Condition::Mzawka => "mżawka",
            Condition::PrzelotnyDeszcz => "przelotny deszcz",
            Condition::Deszcz => "deszcz",
            Condition::DeszczZeSniegiem => "deszcz ze śniegiem",
            Condition::Snieg => "śnieg",
            Condition::Burza => "burza",
        }
    }

    /// Opad (mżawka, deszcz, śnieg, burza), nie samo zachmurzenie czy mgła.
    pub fn is_precipitation(self) -> bool {
        self >= Condition::Mzawka
    }

    /// Kod WMO (Open-Meteo); nieznany kod → `None`.
    pub fn from_wmo(code: i64) -> Option<Condition> {
        Some(match code {
            0 => Condition::Bezchmurnie,
            1 | 2 => Condition::MaleZachmurzenie,
            3 => Condition::Pochmurno,
            45 | 48 => Condition::Mgla,
            51..=57 => Condition::Mzawka,
            61..=67 => Condition::Deszcz,
            80..=82 => Condition::PrzelotnyDeszcz,
            71..=77 | 85 | 86 => Condition::Snieg,
            95..=99 => Condition::Burza,
            _ => return None,
        })
    }

    /// `symbol_code` MET Norway, np. `lightrainshowers_day`.
    pub fn from_met_symbol(symbol: &str) -> Option<Condition> {
        let base = symbol.split('_').next().unwrap_or("");
        if base.contains("thunder") {
            return Some(Condition::Burza);
        }
        let base = base
            .strip_prefix("light")
            .or_else(|| base.strip_prefix("heavy"))
            .unwrap_or(base);
        Some(match base {
            "clearsky" => Condition::Bezchmurnie,
            "fair" | "partlycloudy" => Condition::MaleZachmurzenie,
            "cloudy" => Condition::Pochmurno,
            "fog" => Condition::Mgla,
            "rain" => Condition::Deszcz,
            "rainshowers" => Condition::PrzelotnyDeszcz,
            "sleet" | "sleetshowers" => Condition::DeszczZeSniegiem,
            "snow" | "snowshowers" => Condition::Snieg,
            _ => return None,
        })
    }
}

/// Warunki bieżące. Prędkości wiatru w m/s, kierunek skąd wieje w stopniach,
/// ciśnienie zredukowane do poziomu morza; każde pole poza temperaturą może być luką.
#[derive(Debug, Clone, PartialEq)]
pub struct Now {
    pub temp_c: f64,
    pub apparent_c: Option<f64>,
    pub humidity_pct: Option<f64>,
    pub pressure_hpa: Option<f64>,
    /// Ciśnienie teraz minus 3 h temu (z prognozy godzinowej).
    pub pressure_trend_hpa_3h: Option<f64>,
    pub wind_ms: Option<f64>,
    pub gust_ms: Option<f64>,
    pub wind_dir_deg: Option<f64>,
    pub cloud_pct: Option<f64>,
    pub visibility_m: Option<f64>,
    pub precip_mm: Option<f64>,
    pub code: Option<Condition>,
}

/// Jeden dzień prognozy (data lokalna); wiatr w m/s.
#[derive(Debug, Clone, PartialEq)]
pub struct Day {
    pub date: NaiveDate,
    pub t_min: f64,
    pub t_max: f64,
    pub precip_mm: Option<f64>,
    pub gust_max_ms: Option<f64>,
    pub wind_max_ms: Option<f64>,
    /// Dominujący kierunek wiatru (skąd wieje), stopnie.
    pub wind_dir_deg: Option<f64>,
    pub code: Option<Condition>,
    /// Wschód i zachód słońca w czasie lokalnym stacji.
    pub sunrise: Option<NaiveDateTime>,
    pub sunset: Option<NaiveDateTime>,
}

/// Jedna godzina prognozy (tylko Open-Meteo); `local` to czas lokalny
/// stacji, jak w odpowiedzi, a każda wartość może być luką. Wiatr w m/s.
#[derive(Debug, Clone, PartialEq)]
pub struct Hour {
    pub local: NaiveDateTime,
    pub temp_c: Option<f64>,
    pub precip_mm: Option<f64>,
    pub wind_ms: Option<f64>,
    pub gust_ms: Option<f64>,
    pub pressure_hpa: Option<f64>,
    pub code: Option<Condition>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    pub issued: DateTime<Utc>,
    pub provider: Provider,
    pub current: Option<Now>,
    pub days: Vec<Day>,
    /// Pusty, gdy dostawca nie dał prognozy godzinowej.
    pub hourly: Vec<Hour>,
}

impl Forecast {
    pub fn day(&self, date: NaiveDate) -> Option<&Day> {
        self.days.iter().find(|d| d.date == date)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDate, Utc};

    pub(crate) fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }
    pub(crate) fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn wmo_codes_map_to_polish_words() {
        let w = |c| Condition::from_wmo(c).map(Condition::words);
        assert_eq!(w(0), Some("bezchmurnie"));
        assert_eq!(w(2), Some("małe zachmurzenie"));
        assert_eq!(w(3), Some("pochmurno"));
        assert_eq!(w(45), Some("mgła"));
        assert_eq!(w(53), Some("mżawka"));
        assert_eq!(w(63), Some("deszcz"));
        assert_eq!(w(81), Some("przelotny deszcz"));
        assert_eq!(w(73), Some("śnieg"));
        assert_eq!(w(95), Some("burza"));
        assert_eq!(w(4), None);
    }

    #[test]
    fn met_symbols_map_through_suffixes_and_intensity() {
        let m = Condition::from_met_symbol;
        assert_eq!(m("lightsleet"), Some(Condition::DeszczZeSniegiem));
        assert_eq!(m("heavyrainshowers_day"), Some(Condition::PrzelotnyDeszcz));
        assert_eq!(
            m("lightssnowshowersandthunder_night"),
            Some(Condition::Burza)
        );
        assert_eq!(
            m("partlycloudy_polartwilight"),
            Some(Condition::MaleZachmurzenie)
        );
        assert_eq!(m("zzz"), None);
    }

    #[test]
    fn attribution_names_the_provider() {
        assert_eq!(Provider::OpenMeteo.attribution(), "Dane: Open-Meteo.");
        assert_eq!(Provider::MetNorway.attribution(), "Dane: MET Norway.");
    }
}
