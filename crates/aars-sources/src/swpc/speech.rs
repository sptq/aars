//! Teksty ogłoszeń pogody kosmicznej (G/S/R).

const ATTRIBUTION: &str = "Dane: NOAA SWPC.";

fn adjective(level: u8) -> &'static str {
    match level {
        1 => "Słaba",
        2 => "Umiarkowana",
        3 => "Silna",
        4 => "Bardzo silna",
        _ => "Ekstremalna",
    }
}

fn phenomenon(scale: char) -> &'static str {
    match scale {
        'G' => "burza geomagnetyczna",
        'S' => "burza radiacyjna",
        _ => "zaniki łączności radiowej",
    }
}

fn effects(scale: char, level: u8) -> &'static str {
    match (scale, level) {
        ('G', 1 | 2) => {
            "Możliwe pogorszenie propagacji na pasmach KF, zwłaszcza na trasach północnych."
        }
        ('G', _) => {
            "Możliwe pogorszenie propagacji na pasmach KF, zwłaszcza na trasach północnych. Możliwa zorza widoczna w Polsce i łączności przez zorzę na 6 i 2 metrach."
        }
        ('S', _) => "Możliwe zaniki łączności KF w rejonach polarnych.",
        _ => "Zaniki łączności na pasmach KF po dziennej stronie Ziemi.",
    }
}

/// Opis słowny do panelu, np. „silna burza geomagnetyczna”.
pub fn label(scale: char, level: u8) -> String {
    format!("{} {}", adjective(level).to_lowercase(), phenomenon(scale))
}

pub fn announcement(scale: char, level: u8) -> String {
    format!(
        "Uwaga, pogoda kosmiczna. {} {}, poziom {scale}{level} w skali NOAA. {} {ATTRIBUTION}",
        adjective(level),
        phenomenon(scale),
        effects(scale, level)
    )
}

pub fn change(scale: char, level: u8) -> String {
    format!("Wzrost poziomu. {}", announcement(scale, level))
}

pub fn cancellation(scale: char) -> String {
    let head = match scale {
        'G' => "Burza geomagnetyczna wygasła.",
        'S' => "Burza radiacyjna wygasła.",
        _ => "Zaniki łączności radiowej ustały.",
    };
    format!("{head} {ATTRIBUTION}")
}
