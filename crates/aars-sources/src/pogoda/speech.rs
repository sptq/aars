//! Tekst części biuletynu „pogoda”; liczby i jednostki jako tokeny dla normalizera (B1, B2).

use chrono::{DateTime, Duration, Timelike, Utc};
use chrono_tz::Europe::Warsaw;

use crate::forecast::{Day, Forecast};
use crate::speech::{decimal_comma, round_int};

/// Od tej godziny (lokalnej) mówimy o jutrze zamiast o dniu dzisiejszym.
const EVENING_FROM_HOUR: u32 = 14;

/// Opady w mm: poniżej 10 z przecinkiem dziesiętnym, od 10 całkowite.
pub fn precip_text(mm: f64) -> String {
    if mm < 10.0 {
        decimal_comma(mm)
    } else {
        round_int(mm).to_string()
    }
}

/// Człon o wietrze: porywy, a bez nich średni wiatr; brak danych → `None`.
fn wind_clause(day: &Day) -> Option<String> {
    if let Some(g) = day.gust_max_kmh {
        Some(format!("porywy wiatru do {} km/h", round_int(g)))
    } else {
        day.wind_max_kmh
            .map(|w| format!("wiatr do {} km/h", round_int(w)))
    }
}

fn precip_clause(day: &Day) -> Option<String> {
    day.precip_mm.map(|mm| {
        if mm < 0.1 {
            "bez opadów".to_owned()
        } else {
            format!("opady do {} mm", precip_text(mm))
        }
    })
}

/// „Dziś…”/„Jutro…”: `label`, opis, `do {t_max} °C`, opady, wiatr.
fn day_sentence(label: &str, day: &Day) -> String {
    let mut s = label.to_owned();
    if let Some(code) = day.code {
        s.push_str(&format!(" {},", code.words()));
    }
    s.push_str(&format!(" do {} °C", round_int(day.t_max)));
    for clause in [precip_clause(day), wind_clause(day)].into_iter().flatten() {
        s.push_str(", ");
        s.push_str(&clause);
    }
    s.push('.');
    s
}

/// Tekst części; `None`, gdy żadne zdanie nie powstało.
pub fn bulletin(forecast: &Forecast, now: DateTime<Utc>) -> Option<String> {
    let local = now.with_timezone(&Warsaw);
    let d0 = local.date_naive();
    let d1 = d0 + Duration::days(1);
    let mut sentences = Vec::new();
    if let Some(cur) = &forecast.current {
        sentences.push(match cur.code {
            Some(code) => format!("Teraz {} °C, {}.", round_int(cur.temp_c), code.words()),
            None => format!("Teraz {} °C.", round_int(cur.temp_c)),
        });
    }
    let night = forecast
        .day(d1)
        .map(|d| format!("W nocy do {} °C.", round_int(d.t_min)));
    if local.hour() < EVENING_FROM_HOUR {
        sentences.extend(forecast.day(d0).map(|d| day_sentence("Dziś", d)));
        sentences.extend(night);
    } else {
        sentences.extend(night);
        sentences.extend(forecast.day(d1).map(|d| day_sentence("Jutro", d)));
    }
    if sentences.is_empty() {
        return None;
    }
    Some(format!(
        "Pogoda dla okolic stacji. {} {}",
        sentences.join(" "),
        forecast.provider.attribution()
    ))
}
