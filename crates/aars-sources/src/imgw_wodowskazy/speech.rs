//! Polski tekst o wodowskazach. Źródło pisze cyfry, `cm` i `HH:MM`; słowa
//! robi normalizacja w `aars-audio::text`.

use chrono::{DateTime, Utc};

use super::parse::{Exceedance, Gauge};
use crate::speech::{round_int, warsaw_hhmm};

const SOURCE: &str = "Źródło: IMGW-PIB.";

/// Ogłoszenie przekroczenia (C8: bez rzeki — bez „na rzece …”).
pub fn announcement(g: &Gauge, e: Exceedance, level_cm: f64, measured: DateTime<Utc>) -> String {
    let place = match &g.river {
        Some(river) => format!("Wodowskaz {} na rzece {river}", g.name),
        None => format!("Wodowskaz {}", g.name),
    };
    let threshold = g
        .threshold_cm(e)
        .map(|t| format!(" ({} cm)", round_int(t)))
        .unwrap_or_default();
    format!(
        "{place}: stan wody {} cm, powyżej stanu {}{threshold}. Pomiar z godz. {}. {SOURCE}",
        round_int(level_cm),
        e.adjective_genitive(),
        warsaw_hhmm(measured)
    )
}

pub fn change(announcement: &str) -> String {
    format!("Zmiana stanu wody. {announcement}")
}

/// Odwołanie: poniżej najniższego progu, jaki stacja ma (sam alarmowy →
/// „alarmowego”).
pub fn cancellation(g: &Gauge) -> String {
    let lowest = if g.warning_cm.is_none() && g.alarm_cm.is_some() {
        Exceedance::Alarm
    } else {
        Exceedance::Warning
    };
    format!(
        "Wodowskaz {}: stan wody spadł poniżej stanu {}.",
        g.name,
        lowest.adjective_genitive()
    )
}

/// „{rzeka} w {stacja}” albo sama stacja.
fn label(g: &Gauge) -> String {
    match &g.river {
        Some(river) => format!("{river} w {}", g.name),
        None => g.name.clone(),
    }
}

pub fn summary(g: &Gauge, e: Exceedance) -> String {
    format!("{}: stan {}", label(g), e.noun())
}

/// Część biuletynu `wody` (C9); `None` bez wodowskazów.
pub fn bulletin(gauges: &[(&Gauge, f64)]) -> Option<String> {
    if gauges.is_empty() {
        return None;
    }
    let mut text = "Stany wód.".to_owned();
    for (g, level) in gauges {
        text.push_str(&format!(" {}: {} cm", label(g), round_int(*level)));
        if let Some(e) = g.exceedance() {
            text.push_str(&format!(", stan {}", e.noun()));
        }
        text.push('.');
    }
    text.push(' ');
    text.push_str(SOURCE);
    Some(text)
}
