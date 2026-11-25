//! Alerty progowe z prognozy: porywy, mróz, opady (tylko dziś i jutro).
//! Dziś liczy się z godzin od bieżącej pełnej godziny do końca doby
//! lokalnej — wieczorem poranny mróz już nie jest prognozą; jutro z `daily`.

use chrono::{DateTime, Datelike, Duration, DurationRound, NaiveDate, Utc};
use chrono_tz::Europe::Warsaw;

use super::thresholds;
use crate::forecast::{Day, Forecast};
use crate::imgw_meteo::speech::MONTHS_GENITIVE;
use crate::source::{SourceCtx, SourceItem, SourceOutput};
use crate::speech::{round_int, warsaw_to_utc};
use aars_core::priority::Priority;

const ATTRIBUTION: &str = "Dane: Open-Meteo.";

/// Jeden alert: rodzaj, odcisk, tekst (bez przedrostka; `{when}` to dziś/jutro) i podsumowanie.
struct Alert {
    kind: &'static str,
    fingerprint: String,
    text: String,
    summary: String,
}

fn day_month(date: NaiveDate) -> String {
    let month = MONTHS_GENITIVE
        .get(date.month0() as usize)
        .copied()
        .unwrap_or("");
    format!("{} {month}", date.day())
}

/// m/s z prognozy → km/h progów, zaokrąglone do całych km/h. Open-Meteo
/// podaje m/s z jedną cyfrą po przecinku, więc 80 km/h przychodzi jako
/// 22,2 m/s = 79,92 km/h — i tak ma przekroczyć próg 80 i trafić do kubełka 80.
fn kmh(ms: f64) -> f64 {
    (ms * 3.6).round()
}

/// Skrajne wartości okresu, z którego liczymy progi (porywy w km/h, jak
/// progi w ustawieniach); `None` = luka.
struct Extremes {
    date: NaiveDate,
    gust_max_kmh: Option<f64>,
    t_min: Option<f64>,
    precip_mm: Option<f64>,
}

impl Extremes {
    fn of_day(day: &Day) -> Self {
        Self {
            date: day.date,
            gust_max_kmh: day.gust_max_ms.map(kmh),
            t_min: Some(day.t_min),
            precip_mm: day.precip_mm,
        }
    }

    /// Z godzin dnia `date` od pełnej godziny `now` (czas lokalny) do końca
    /// doby: max porywów, min temperatury, suma opadów. Bez godzin → `None`.
    fn rest_of_today(forecast: &Forecast, now: DateTime<Utc>) -> Option<Self> {
        let local_now = now.with_timezone(&Warsaw).naive_local();
        let from = local_now
            .duration_trunc(Duration::hours(1))
            .unwrap_or(local_now);
        let date = local_now.date();
        let hours: Vec<_> = forecast
            .hourly
            .iter()
            .filter(|h| h.local.date() == date && h.local >= from)
            .collect();
        if hours.is_empty() {
            return None;
        }
        let fold = |values: Vec<f64>, f: fn(f64, f64) -> f64| values.into_iter().reduce(f);
        Some(Self {
            date,
            gust_max_kmh: fold(hours.iter().filter_map(|h| h.gust_ms).collect(), f64::max).map(kmh),
            t_min: fold(hours.iter().filter_map(|h| h.temp_c).collect(), f64::min),
            precip_mm: fold(
                hours.iter().filter_map(|h| h.precip_mm).collect(),
                |a, b| a + b,
            ),
        })
    }
}

/// „opady do N mm”; przy zaokrągleniu do 0 (próg poniżej 0,5 mm) — „słabe opady”.
fn rain_clause(mm: i64) -> String {
    if mm < 1 {
        "słabe opady".to_owned()
    } else {
        format!("opady do {mm} mm")
    }
}

/// km/h progu → m/s w mowie (biuletyn i alerty mówią wiatr w m/s).
fn spoken_ms(kmh: f64) -> i64 {
    round_int(kmh / 3.6)
}

fn alerts_for(day: &Extremes, when: &str, (gust, frost, precip): (i64, i64, i64)) -> Vec<Alert> {
    let mut out = Vec::new();
    if let Some(g) = day.gust_max_kmh.filter(|g| *g >= gust as f64) {
        // Próg i odcisk w km/h (kubełek się nie zmienia), mowa w m/s.
        let shown = spoken_ms(g);
        out.push(Alert {
            kind: "gust",
            fingerprint: format!("gust:{}", (g / 10.0).floor() as i64 * 10),
            text: format!("{when} porywy wiatru do {shown} m/s."),
            summary: format!("porywy wiatru do {shown} m/s, {}", day_month(day.date)),
        });
    }
    if let Some(t_min) = day.t_min.filter(|t| *t <= frost as f64) {
        let shown = round_int(t_min);
        out.push(Alert {
            kind: "frost",
            fingerprint: format!("frost:-{}", (t_min.abs() / 5.0).floor() as i64 * 5),
            text: format!("{when} spadek temperatury do {shown} °C."),
            summary: format!("spadek temperatury do {shown} °C, {}", day_month(day.date)),
        });
    }
    if let Some(mm) = day.precip_mm.filter(|mm| *mm >= precip as f64) {
        let clause = rain_clause(round_int(mm));
        out.push(Alert {
            kind: "precip",
            fingerprint: format!("precip:{}", (mm / 10.0).floor() as i64 * 10),
            text: format!("{when} {clause}."),
            summary: format!("{clause}, {}", day_month(day.date)),
        });
    }
    out
}

/// Koniec dnia prognozy: północ lokalna po tej dacie, w UTC.
fn end_of_day(date: NaiveDate) -> Option<DateTime<Utc>> {
    warsaw_to_utc((date + Duration::days(1)).and_hms_opt(0, 0, 0)?)
}

/// Siła odcisku: |liczba| po dwukropku (`gust:80` → 80, `frost:-15` → 15).
fn strength(fingerprint: &str) -> Option<i64> {
    fingerprint
        .rsplit_once(':')
        .and_then(|(_, value)| value.parse::<i64>().ok())
        .map(i64::abs)
}

/// Alert odtworzony z wartości kubełka śledzonego odcisku (dziś, gdy prognoza
/// chwilowo już niczego nie przekracza). Runner tylko dotyka takiej pozycji.
fn held_alert(kind: &'static str, fingerprint: &str, date: NaiveDate) -> Option<Alert> {
    let value = fingerprint.rsplit_once(':')?.1.parse::<i64>().ok()?;
    let when = "dziś";
    let (text, summary) = match kind {
        "gust" => {
            let ms = spoken_ms(value as f64);
            (
                format!("{when} porywy wiatru do {ms} m/s."),
                format!("porywy wiatru do {ms} m/s, {}", day_month(date)),
            )
        }
        "frost" => (
            format!("{when} spadek temperatury do {value} °C."),
            format!("spadek temperatury do {value} °C, {}", day_month(date)),
        ),
        "precip" => (
            format!("{when} {}.", rain_clause(value)),
            format!("{}, {}", rain_clause(value), day_month(date)),
        ),
        _ => return None,
    };
    Some(Alert { kind, fingerprint: fingerprint.to_owned(), text, summary })
}

fn item_of(alert: Alert, date: NaiveDate) -> SourceItem {
    let key = format!("{}:{}", alert.kind, date.format("%Y-%m-%d"));
    SourceItem {
        topic: key.clone(),
        key,
        fingerprint: alert.fingerprint,
        text: format!("Prognoza pogody: {} {ATTRIBUTION}", alert.text),
        change_text: format!("Zmiana prognozy: {} {ATTRIBUTION}", alert.text),
        cancel_text: String::new(),
        summary: alert.summary,
        priority: Priority::Normal,
        interrupting: false,
        repeat: None,
        valid_from: None,
        valid_until: end_of_day(date),
    }
}

pub fn to_output(forecast: &Forecast, ctx: &SourceCtx) -> SourceOutput {
    let limits = thresholds(ctx);
    let today = ctx.now.with_timezone(&Warsaw).date_naive();
    let tomorrow = today + Duration::days(1);
    let periods = [
        (Extremes::rest_of_today(forecast, ctx.now), "dziś"),
        (forecast.day(tomorrow).map(Extremes::of_day), "jutro"),
    ];
    let mut items = Vec::new();
    for (extremes, when) in periods {
        // Dziś prognoza z mijających godzin tylko słabnie: ogłoszona siła
        // zostaje (C10), więc nie ma ponownego ogłoszenia przy spadku ani
        // odwołania, gdy wartość schodzi pod próg.
        let hold_today = when == "dziś";
        let mut alerts = match &extremes {
            Some(day) => alerts_for(day, when, limits),
            None => Vec::new(),
        };
        let date = extremes.as_ref().map_or(today, |day| day.date);
        if hold_today {
            for kind in ["gust", "frost", "precip"] {
                let key = format!("{kind}:{}", date.format("%Y-%m-%d"));
                let Some(held) = ctx.tracked.get(&key) else { continue };
                let Some(held_strength) = strength(held) else { continue };
                match alerts.iter_mut().find(|a| a.kind == kind) {
                    Some(alert) => {
                        if strength(&alert.fingerprint).is_some_and(|new| new < held_strength) {
                            alert.fingerprint = held.clone();
                        }
                    }
                    None => alerts.extend(held_alert(kind, held, date)),
                }
            }
        }
        items.extend(alerts.into_iter().map(|alert| item_of(alert, date)));
    }
    SourceOutput {
        items,
        complete: true,
    }
}
