//! Polskie teksty komunikatów o wstrząsach.

use super::parse::Quake;

/// Polska nazwa regionu Flinna-Engdahla (A13); nieznany → `None`.
pub fn region_name(flynn: &str) -> Option<&'static str> {
    Some(match flynn.trim().to_uppercase().as_str() {
        "POLAND" => "Polska",
        "CZECH REPUBLIC" | "CZECHIA" => "Czechy",
        "SLOVAKIA" => "Słowacja",
        "GERMANY" | "SOUTHERN GERMANY" | "NORTHERN GERMANY" => "Niemcy",
        "UKRAINE" => "Ukraina",
        "BELARUS" => "Białoruś",
        "LITHUANIA" => "Litwa",
        "KALININGRAD REGION, RUSSIA" => "obwód kaliningradzki",
        "BALTIC SEA" => "Morze Bałtyckie",
        "HUNGARY" => "Węgry",
        _ => return None,
    })
}

/// Magnituda z przecinkiem dziesiętnym, jedno miejsce: `2,1`.
pub fn mag_text(mag: f64) -> String {
    format!("{mag:.1}").replace('.', ",")
}

/// `direction` to wynik `compass8` („na zachód”).
pub fn announcement(q: &Quake, km: u32, direction: &str) -> String {
    let region = region_name(&q.region)
        .map(|r| format!(", {r}"))
        .unwrap_or_default();
    format!(
        "Wstrząs sejsmiczny o magnitudzie {}, {} {direction} od stacji{region}, o godzinie {}. Dane: EMSC.",
        mag_text(q.mag),
        crate::speech::km_phrase(km),
        crate::speech::warsaw_hhmm(q.time),
    )
}

pub fn change(q: &Quake, km: u32, direction: &str) -> String {
    format!(
        "Poprawka danych wstrząsu. {}",
        announcement(q, km, direction)
    )
}

/// `M3,5, 120 km, Polska` (bez regionu: `M3,5, 120 km`).
pub fn summary(q: &Quake, km: u32) -> String {
    let mut s = format!("M{}, {km} km", mag_text(q.mag));
    if let Some(r) = region_name(&q.region) {
        s.push_str(", ");
        s.push_str(r);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_are_translated() {
        assert_eq!(region_name("POLAND"), Some("Polska"));
        assert_eq!(region_name("CZECH REPUBLIC"), Some("Czechy"));
        assert_eq!(region_name("CZECHIA"), Some("Czechy"));
        assert_eq!(region_name("SLOVAKIA"), Some("Słowacja"));
        assert_eq!(region_name("SOUTHERN GERMANY"), Some("Niemcy"));
        assert_eq!(region_name("NORTHERN GERMANY"), Some("Niemcy"));
        assert_eq!(region_name("GERMANY"), Some("Niemcy"));
        assert_eq!(region_name("UKRAINE"), Some("Ukraina"));
        assert_eq!(region_name("BELARUS"), Some("Białoruś"));
        assert_eq!(region_name("LITHUANIA"), Some("Litwa"));
        assert_eq!(
            region_name("KALININGRAD REGION, RUSSIA"),
            Some("obwód kaliningradzki")
        );
        assert_eq!(region_name("BALTIC SEA"), Some("Morze Bałtyckie"));
        assert_eq!(region_name("HUNGARY"), Some("Węgry"));
        assert_eq!(region_name("ROMANIA"), None);
        assert_eq!(region_name(""), None);
    }

    #[test]
    fn the_magnitude_uses_a_decimal_comma() {
        assert_eq!(mag_text(2.1), "2,1");
        assert_eq!(mag_text(3.0), "3,0");
        assert_eq!(mag_text(4.56), "4,6");
    }
}
