//! Słowa części `propagacja`: progi, pasma, polskie nazwy miast.

/// < 90 niski, 90–149 umiarkowany, ≥ 150 wysoki.
pub fn sfi_word(sfi: f64) -> &'static str {
    if sfi < 90.0 {
        "niski"
    } else if sfi < 150.0 {
        "umiarkowany"
    } else {
        "wysoki"
    }
}

/// Pasma (metry, dolna krawędź w MHz), od najniższego do najwyższego.
const NVIS: [(u16, f64); 5] = [(160, 1.8), (80, 3.5), (60, 5.3), (40, 7.0), (30, 10.1)];
const DX: [(u16, f64); 8] = [
    (80, 3.5),
    (40, 7.0),
    (30, 10.1),
    (20, 14.0),
    (17, 18.068),
    (15, 21.0),
    (12, 24.89),
    (10, 28.0),
];

fn highest(table: &[(u16, f64)], limit: f64) -> Option<u16> {
    table
        .iter()
        .rev()
        .find(|(_, edge)| *edge < limit)
        .map(|(m, _)| *m)
}

/// Najwyższe pasmo bliskie o dolnej krawędzi poniżej foF2 (bez marginesu).
pub fn nvis_band(fof2: f64) -> Option<u16> {
    highest(&NVIS, fof2)
}

/// Najwyższe pasmo dalekie o dolnej krawędzi poniżej MUF × 0,85.
pub fn dx_band(mufd: f64) -> Option<u16> {
    highest(&DX, mufd * 0.85)
}

const CITIES: [(&str, &str); 10] = [
    ("Warsaw", "Warszawa"),
    ("Pruhonice", "Pruhonice"),
    ("Juliusruh", "Juliusruh"),
    ("Sopron", "Sopron"),
    ("Chilton", "Chilton"),
    ("Dourbes", "Dourbes"),
    ("Rome", "Rzym"),
    ("Athens", "Ateny"),
    ("Moscow", "Moskwa"),
    ("Nicosia", "Nikozja"),
];

/// Polska nazwa miasta jonosondy; nieznana → część nazwy przed przecinkiem.
pub fn city(name: &str) -> String {
    let head = name.split(',').next().unwrap_or(name).trim();
    CITIES
        .iter()
        .find(|(en, _)| *en == head)
        .map_or(head, |(_, pl)| *pl)
        .to_owned()
}
