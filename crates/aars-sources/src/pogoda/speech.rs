//! Tekst części biuletynu „pogoda”; liczby i jednostki jako tokeny dla normalizera (B1, B2, B7, B8).
//!
//! Każdy człon pojawia się tylko wtedy, gdy są dla niego dane; wiatr w m/s.

use chrono::{DateTime, Duration, NaiveDateTime, Timelike, Utc};
use chrono_tz::Europe::Warsaw;

use crate::forecast::{Condition, Day, Forecast, Now};
use crate::speech::{
    cloud_words, decimal_comma, pressure_tendency, round_int, wind_direction,
};

/// Od tej godziny (lokalnej) mówimy o jutrze zamiast o dniu dzisiejszym.
const EVENING_FROM_HOUR: u32 = 14;
/// Odczuwalna jest podawana od tej różnicy względem temperatury.
const APPARENT_MIN_DIFF_C: f64 = 2.0;
/// Widzialność jest podawana tylko poniżej tej wartości.
const VISIBILITY_MAX_M: f64 = 5000.0;
/// Wiatr słabszy niż to to cisza.
const CALM_BELOW_MS: f64 = 1.0;
/// Ile godzin prognozy godzinowej to „najbliższe godziny”.
const NEXT_HOURS: usize = 6;
/// Porywy w najbliższych godzinach są podawane, gdy przewyższają bieżące o tyle.
const GUST_RISE_MS: f64 = 3.0;
/// Opad poniżej tej sumy to „bez opadów”.
const DRY_BELOW_MM: f64 = 0.1;
/// Opad poniżej tej sumy to „słabe opady” (po zaokrągleniu byłoby „do 0 mm”).
const LIGHT_BELOW_MM: f64 = 0.5;
/// Przy ciszy porywy od tej wartości są jednak podawane.
const CALM_GUST_FROM_MS: f64 = 5.0;
/// Bez warunków bieżących porywy najbliższych godzin są podawane od tej wartości.
const GUST_NOTABLE_MS: f64 = 10.0;
/// Noc w prognozie: od tej godziny lokalnej…
const NIGHT_FROM_HOUR: u32 = 22;
/// …do tej godziny następnego dnia (włącznie).
const NIGHT_TO_HOUR: u32 = 6;

/// Opad z prognozy: „słabe opady” albo „opady do X mm” z liczbą całkowitą.
/// Ułamek po „do” normalizer czyta nieodmieniony („do pięć przecinek dziewięć
/// milimetra”), całkowita idzie w dopełniaczu („do sześciu milimetrów”), a
/// dziesiąte części milimetra w prognozie i tak nic słuchaczowi nie mówią.
fn wet_clause(mm: f64) -> String {
    if mm < LIGHT_BELOW_MM {
        "słabe opady".to_owned()
    } else {
        format!("opady do {} mm", round_int(mm))
    }
}

/// Godzina lokalna `H:MM`.
fn hhmm(t: NaiveDateTime) -> String {
    t.format("%-H:%M").to_string()
}

/// Opis bieżących warunków: zjawisko z kodu, a dla samego zachmurzenia
/// dokładniej z procentu pokrycia nieba.
fn now_description(cur: &Now) -> Option<String> {
    let base = match (cur.code, cur.cloud_pct) {
        (Some(code), Some(pct)) if code <= Condition::Pochmurno => cloud_words(pct),
        (Some(code), _) => code.words(),
        (None, Some(pct)) => cloud_words(pct),
        (None, None) => return None,
    };
    let wet = cur.precip_mm.is_some_and(|mm| mm >= DRY_BELOW_MM);
    let described_wet = cur.code.is_some_and(Condition::is_precipitation);
    Some(if wet && !described_wet {
        format!("{base}, pada")
    } else {
        base.to_owned()
    })
}

fn wind_now(cur: &Now) -> Option<String> {
    let gust = cur.gust_ms.map(round_int);
    let Some(wind) = cur.wind_ms else {
        return gust.map(|g| format!("Porywy wiatru do {g} m/s."));
    };
    if wind < CALM_BELOW_MS {
        return Some(match cur.gust_ms.filter(|g| *g >= CALM_GUST_FROM_MS) {
            Some(g) => format!("Wiatr słaby, w porywach {} m/s.", round_int(g)),
            None => "Cisza.".to_owned(),
        });
    }
    let mut s = "Wiatr".to_owned();
    if let Some(deg) = cur.wind_dir_deg {
        s.push(' ');
        s.push_str(wind_direction(deg));
    }
    let wind = round_int(wind);
    s.push_str(&format!(" {wind} m/s"));
    if let Some(g) = gust.filter(|g| *g > wind) {
        s.push_str(&format!(", w porywach {g} m/s"));
    }
    s.push('.');
    Some(s)
}

fn visibility(m: f64) -> Option<String> {
    if m >= VISIBILITY_MAX_M {
        return None;
    }
    Some(if m < 1000.0 {
        let hundreds = ((m / 100.0).round() * 100.0).max(100.0);
        format!("Widzialność {} m.", hundreds as i64)
    } else {
        format!("Widzialność {} km.", decimal_comma(m / 1000.0))
    })
}

/// „Obecnie… Wilgotność… Ciśnienie… Wiatr… Widzialność…”; `None` bez warunków bieżących.
pub fn now_text(forecast: &Forecast, _now: DateTime<Utc>) -> Option<String> {
    let cur = forecast.current.as_ref()?;
    let mut sentences = Vec::new();
    let mut first = "Obecnie".to_owned();
    if let Some(desc) = now_description(cur) {
        first.push_str(&format!(" {desc},"));
    }
    first.push_str(&format!(" temperatura {} °C", round_int(cur.temp_c)));
    if let Some(a) = cur
        .apparent_c
        .filter(|a| (a - cur.temp_c).abs() >= APPARENT_MIN_DIFF_C)
    {
        first.push_str(&format!(", odczuwalna {} °C", round_int(a)));
    }
    first.push('.');
    sentences.push(first);
    if let Some(h) = cur.humidity_pct {
        sentences.push(format!("Wilgotność {} %.", round_int(h)));
    }
    if let Some(p) = cur.pressure_hpa {
        sentences.push(match cur.pressure_trend_hpa_3h {
            Some(d) => format!("Ciśnienie {} hPa, {}.", round_int(p), pressure_tendency(d)),
            None => format!("Ciśnienie {} hPa.", round_int(p)),
        });
    }
    sentences.extend(wind_now(cur));
    sentences.extend(cur.visibility_m.and_then(visibility));
    Some(sentences.join(" "))
}

/// Minimum temperatury godzinowej najbliższej nocy (22:00–6:00 lokalnie,
/// od bieżącej pełnej godziny); bez godzin z tego okresu → `None`.
fn night_min(forecast: &Forecast, now: DateTime<Utc>) -> Option<f64> {
    let local = now.with_timezone(&Warsaw);
    let local_now = local.naive_local();
    // Przed 6:00 trwa noc, która zaczęła się wczoraj.
    let evening = if local.hour() < NIGHT_TO_HOUR {
        local.date_naive() - Duration::days(1)
    } else {
        local.date_naive()
    };
    let from = evening.and_hms_opt(NIGHT_FROM_HOUR, 0, 0)?;
    let to = (evening + Duration::days(1)).and_hms_opt(NIGHT_TO_HOUR, 0, 0)?;
    let current_hour = local_now.date().and_hms_opt(local_now.hour(), 0, 0)?;
    forecast
        .hourly
        .iter()
        .filter(|h| h.local >= from && h.local <= to && h.local >= current_hour)
        .filter_map(|h| h.temp_c)
        .reduce(f64::min)
}

/// „W najbliższych godzinach…”: najdotkliwszy opis, suma opadu i porywy,
/// gdy wyraźnie mocniejsze niż teraz.
fn next_hours_sentence(forecast: &Forecast, now: DateTime<Utc>) -> Option<String> {
    let local_now = now.with_timezone(&Warsaw).naive_local();
    let hours: Vec<_> = forecast
        .hourly
        .iter()
        .filter(|h| h.local > local_now)
        .take(NEXT_HOURS)
        .collect();
    let worst = hours.iter().filter_map(|h| h.code).max();
    let precip = hours
        .iter()
        .filter_map(|h| h.precip_mm)
        .reduce(|a, b| a + b);
    let gust = hours.iter().filter_map(|h| h.gust_ms).reduce(f64::max);
    let mut clauses: Vec<String> = Vec::new();
    clauses.extend(worst.map(|c| c.words().to_owned()));
    match precip {
        Some(mm) if mm >= DRY_BELOW_MM => clauses.push(wet_clause(mm)),
        Some(_) if !worst.is_some_and(Condition::is_precipitation) => {
            clauses.push("bez opadów".to_owned());
        }
        _ => {}
    }
    let gust_now = forecast
        .current
        .as_ref()
        .and_then(|c| c.gust_ms.or(c.wind_ms));
    let notable = |g: f64| match gust_now {
        Some(now) => g - now > GUST_RISE_MS,
        None => g >= GUST_NOTABLE_MS,
    };
    if let Some(g) = gust.filter(|g| notable(*g)) {
        clauses.push(format!("porywy wiatru do {} m/s", round_int(g)));
    }
    if clauses.is_empty() {
        return None;
    }
    Some(format!("W najbliższych godzinach {}.", clauses.join(", ")))
}

/// Człon o wietrze dnia: kierunek, średni wiatr i porywy; bez średniego
/// same porywy; brak danych → `None`.
fn wind_clause(day: &Day) -> Option<String> {
    let gust = day.gust_max_ms.map(round_int);
    match day.wind_max_ms.filter(|w| *w >= CALM_BELOW_MS) {
        Some(w) => {
            let w = round_int(w);
            let mut s = "wiatr".to_owned();
            if let Some(deg) = day.wind_dir_deg {
                s.push(' ');
                s.push_str(wind_direction(deg));
            }
            s.push_str(&format!(" do {w} m/s"));
            if let Some(g) = gust.filter(|g| *g > w) {
                s.push_str(&format!(", w porywach do {g} m/s"));
            }
            Some(s)
        }
        None => gust.map(|g| format!("porywy wiatru do {g} m/s")),
    }
}

fn precip_clause(day: &Day) -> Option<String> {
    day.precip_mm.map(|mm| {
        if mm < DRY_BELOW_MM {
            "bez opadów".to_owned()
        } else {
            wet_clause(mm)
        }
    })
}

/// „Dziś…”/„Jutro…”: `label`, opis, temperatury, opady, wiatr. Dziś tylko
/// maksimum (poranne minimum już minęło), jutro zakres.
fn day_sentence(label: &str, day: &Day, with_min: bool) -> String {
    let mut s = label.to_owned();
    if let Some(code) = day.code {
        s.push_str(&format!(" {},", code.words()));
    }
    let (lo, hi) = (round_int(day.t_min), round_int(day.t_max));
    if with_min && lo < hi {
        s.push_str(&format!(" od {lo} do {hi} °C"));
    } else {
        s.push_str(&format!(" do {hi} °C"));
    }
    for clause in [precip_clause(day), wind_clause(day)].into_iter().flatten() {
        s.push_str(", ");
        s.push_str(&clause);
    }
    s.push('.');
    s
}

/// „W najbliższych godzinach… Dziś…/W nocy…/Jutro…”; `None` bez prognozy.
pub fn forecast_text(forecast: &Forecast, now: DateTime<Utc>) -> Option<String> {
    let local = now.with_timezone(&Warsaw);
    let d0 = local.date_naive();
    let d1 = d0 + Duration::days(1);
    let mut sentences = Vec::new();
    sentences.extend(next_hours_sentence(forecast, now));
    let night = night_min(forecast, now)
        .or_else(|| forecast.day(d1).map(|d| d.t_min))
        .map(|t| format!("W nocy do {} °C.", round_int(t)));
    if local.hour() < EVENING_FROM_HOUR {
        sentences.extend(forecast.day(d0).map(|d| day_sentence("Dziś", d, false)));
        sentences.extend(night);
    } else {
        sentences.extend(night);
        sentences.extend(forecast.day(d1).map(|d| day_sentence("Jutro", d, true)));
    }
    (!sentences.is_empty()).then(|| sentences.join(" "))
}

/// Wschód i zachód słońca w czasie Warszawy: przed wschodem dzisiejsze,
/// w dzień sam zachód, po zachodzie jutrzejsze; brak jednej z godzin nie
/// gubi drugiej; `None` bez danych.
pub fn sun_text(forecast: &Forecast, now: DateTime<Utc>) -> Option<String> {
    let local = now.with_timezone(&Warsaw);
    let local_now = local.naive_local();
    let today = forecast.day(local.date_naive());
    let tomorrow = forecast.day(local.date_naive() + Duration::days(1));
    let sentence = |when: &str, rise: Option<NaiveDateTime>, set: Option<NaiveDateTime>| {
        Some(match (rise, set) {
            (Some(r), Some(s)) => format!("Wschód słońca{when} o {}, zachód o {}.", hhmm(r), hhmm(s)),
            (Some(r), None) => format!("Wschód słońca{when} o {}.", hhmm(r)),
            (None, Some(s)) => format!("Zachód słońca{when} o {}.", hhmm(s)),
            (None, None) => return None,
        })
    };
    if let Some(day) = today {
        if day.sunrise.is_some_and(|r| local_now < r) {
            return sentence("", day.sunrise, day.sunset.filter(|s| local_now < *s));
        }
        if let Some(s) = day.sunset.filter(|s| local_now < *s) {
            return sentence("", None, Some(s));
        }
    }
    let day = tomorrow?;
    sentence(" jutro", day.sunrise, day.sunset)
}

/// Zapowiedź pogody z atrybucją, przed pierwszą częścią pogodową biuletynu.
pub fn heading(forecast: &Forecast) -> String {
    format!("{HEADING_START}{}.", forecast.provider.name())
}

const HEADING_START: &str = "Pogoda dla okolic stacji według danych ";

/// Tekst bez zapowiedzi z `heading`; `None`, gdy jej nie ma.
pub fn strip_heading(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(HEADING_START)?;
    let end = rest.find(". ")?;
    Some(&rest[end + 2..])
}

/// Cała pogoda jednym tekstem (dawna część `pogoda`); `None`, gdy żadne
/// zdanie nie powstało.
pub fn bulletin(forecast: &Forecast, now: DateTime<Utc>) -> Option<String> {
    let parts: Vec<String> = [
        now_text(forecast, now),
        forecast_text(forecast, now),
        sun_text(forecast, now),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "Pogoda dla okolic stacji. {} {}",
        parts.join(" "),
        forecast.provider.attribution()
    ))
}
