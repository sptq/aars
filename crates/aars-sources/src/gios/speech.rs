//! Polskie teksty alertów smogowych.

use super::parse::Warning;
use crate::speech::{clean_text, truncate_sentence};

const PRECAUTION_MAX: usize = 500;

/// „informowania”, „alarmowy”; inny poziom — `standardType` z odpowiedzi.
pub fn level_label(w: &Warning) -> String {
    match w.level.as_str() {
        "PI" => "informowania".to_owned(),
        "PA" => "alarmowy".to_owned(),
        _ => w.standard_type.clone(),
    }
}

pub fn announcement(w: &Warning) -> String {
    let mut text = format!(
        "Alert smogowy GIOŚ, {}. Przekroczony poziom {} dla {}.",
        w.zone_name,
        level_label(w),
        w.param_name
    );
    let (precaution, cut) = truncate_sentence(&clean_text(&w.precaution), PRECAUTION_MAX);
    let precaution = precaution.trim_end_matches([',', ';', ':', ' ']);
    if !precaution.is_empty() {
        text.push(' ');
        text.push_str(precaution);
        if !precaution.ends_with(['.', '!', '?']) {
            text.push('.');
        }
    }
    if cut {
        text.push_str(" Pełne zalecenia na stronie GIOŚ.");
    }
    text.push_str(" Źródło: GIOŚ.");
    text
}

pub fn cancel(zone_name: &str) -> String {
    format!("Alert smogowy dla {zone_name} odwołany. Źródło: GIOŚ.")
}

/// „PM10, poziom informowania, miasto Rzeszów”.
pub fn summary(w: &Warning) -> String {
    let level = match w.level.as_str() {
        "PI" | "PA" => format!("poziom {}", level_label(w)),
        _ => w.standard_type.clone(),
    };
    format!("{}, {level}, {}", w.param_code, w.zone_name)
}
