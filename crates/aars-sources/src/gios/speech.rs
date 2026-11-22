//! Polskie teksty alertów smogowych.

use super::parse::Warning;
use crate::speech::{clean_text, truncate_sentence};

const PRECAUTION_MAX: usize = 500;

/// „informowania”, „alarmowy”; inny poziom — `None`.
pub fn level_label(w: &Warning) -> Option<&'static str> {
    match w.level.as_str() {
        "PI" => Some("informowania"),
        "PA" => Some("alarmowy"),
        _ => None,
    }
}

/// „Poziom docelowy” → „poziom docelowy”.
fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Nazwy strefy i zanieczyszczenia stoją w mianowniku („strefa
/// podkarpacka”), więc po dwukropku, nie w dopełniaczu.
fn heading(w: &Warning) -> String {
    format!("Strefa: {}. Zanieczyszczenie: {}.", w.zone_name, w.param_name)
}

pub fn announcement(w: &Warning) -> String {
    let exceeded = match level_label(w) {
        Some(label) => format!("Przekroczony poziom {label}."),
        None => format!("Przekroczony {}.", lower_first(&w.standard_type)),
    };
    let mut text = format!("Alert smogowy GIOŚ. {} {exceeded}", heading(w));
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

/// Komunikat przy zmianie poziomu (PI → PA i z powrotem).
pub fn change(w: &Warning) -> String {
    format!("Zmiana alertu smogowego. {}", announcement(w))
}

pub fn cancel(w: &Warning) -> String {
    format!("Koniec alertu smogowego GIOŚ. {} Źródło: GIOŚ.", heading(w))
}

/// „PM10, poziom informowania, miasto Rzeszów”.
pub fn summary(w: &Warning) -> String {
    let level = match level_label(w) {
        Some(label) => format!("poziom {label}"),
        None => w.standard_type.clone(),
    };
    format!("{}, {level}, {}", w.param_code, w.zone_name)
}
