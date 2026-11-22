//! Alerty progowe z prognozy: porywy, mróz, opady (tylko dziś i jutro).
//! Dziś liczy się z godzin od bieżącej pełnej godziny do końca doby
//! lokalnej — wieczorem poranny mróz już nie jest prognozą; jutro z `daily`.

use chrono::{DateTime, Datelike, Duration, DurationRound, NaiveDate, Utc};
use chrono_tz::Europe::Warsaw;

use super::speech::precip_text;
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

/// Skrajne wartości okresu, z którego liczymy progi; `None` = luka.
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
            gust_max_kmh: day.gust_max_kmh,
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
            gust_max_kmh: fold(hours.iter().filter_map(|h| h.gust_kmh).collect(), f64::max),
            t_min: fold(hours.iter().filter_map(|h| h.temp_c).collect(), f64::min),
            precip_mm: fold(
                hours.iter().filter_map(|h| h.precip_mm).collect(),
                |a, b| a + b,
            ),
        })
    }
}

fn alerts_for(day: &Extremes, when: &str, (gust, frost, precip): (i64, i64, i64)) -> Vec<Alert> {
    let mut out = Vec::new();
    if let Some(g) = day.gust_max_kmh.filter(|g| *g >= gust as f64) {
        let shown = round_int(g);
        out.push(Alert {
            kind: "gust",
            fingerprint: format!("gust:{}", (g / 10.0).floor() as i64 * 10),
            text: format!("{when} porywy wiatru do {shown} km/h."),
            summary: format!("porywy wiatru do {shown} km/h, {}", day_month(day.date)),
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
        let shown = precip_text(mm);
        out.push(Alert {
            kind: "precip",
            fingerprint: format!("precip:{}", (mm / 10.0).floor() as i64 * 10),
            text: format!("{when} opady do {shown} mm."),
            summary: format!("opady do {shown} mm, {}", day_month(day.date)),
        });
    }
    out
}

/// Koniec dnia prognozy: północ lokalna po tej dacie, w UTC.
fn end_of_day(date: NaiveDate) -> Option<DateTime<Utc>> {
    warsaw_to_utc((date + Duration::days(1)).and_hms_opt(0, 0, 0)?)
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
        let Some(day) = extremes else {
            continue;
        };
        let date = day.date;
        for alert in alerts_for(&day, when, limits) {
            let key = format!("{}:{}", alert.kind, date.format("%Y-%m-%d"));
            items.push(SourceItem {
                topic: key.clone(),
                key,
                fingerprint: alert.fingerprint,
                text: format!("Prognoza pogody: {} {ATTRIBUTION}", alert.text),
                change_text: format!("Zmiana prognozy: {} {ATTRIBUTION}", alert.text),
                cancel_text: String::new(),
                summary: alert.summary,
                priority: Priority::Normal,
                interrupting: false,
                valid_from: None,
                valid_until: end_of_day(date),
            });
        }
    }
    SourceOutput {
        items,
        complete: true,
    }
}
