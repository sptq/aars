//! Parsowanie odpowiedzi `hydro` IMGW (stany wodowskazów). Dane z sieci: bez
//! `unwrap`, `expect` i indeksowania.

use chrono::NaiveDateTime;

use crate::source::SourceError;
use crate::speech::clean_text;

/// Przekroczony stan na wodowskazie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exceedance {
    Warning,
    Alarm,
}

impl Exceedance {
    /// „powyżej stanu ostrzegawczego”.
    pub fn adjective_genitive(self) -> &'static str {
        match self {
            Self::Warning => "ostrzegawczego",
            Self::Alarm => "alarmowego",
        }
    }

    /// „stan ostrzegawczy”; to też odcisk pozycji.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Warning => "ostrzegawczy",
            Self::Alarm => "alarmowy",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gauge {
    pub id: String,
    pub name: String,
    /// `None` dla pustej, `null` albo `"-"` (morze w Gdańsku); skróty
    /// akwenów rozwinięte (`water_name`).
    pub river: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub level_cm: Option<f64>,
    /// Czas Warszawy, jak w odpowiedzi.
    pub measured_at: Option<NaiveDateTime>,
    pub warning_cm: Option<f64>,
    pub alarm_cm: Option<f64>,
}

impl Gauge {
    /// Alarm, gdy jest próg alarmowy i stan go osiąga; inaczej ostrzegawczy
    /// tak samo (C6). Bez stanu — nic.
    pub fn exceedance(&self) -> Option<Exceedance> {
        let level = self.level_cm?;
        if self.alarm_cm.is_some_and(|a| level >= a) {
            Some(Exceedance::Alarm)
        } else if self.warning_cm.is_some_and(|w| level >= w) {
            Some(Exceedance::Warning)
        } else {
            None
        }
    }

    /// Próg, którego dotyczy przekroczenie.
    pub fn threshold_cm(&self, e: Exceedance) -> Option<f64> {
        match e {
            Exceedance::Warning => self.warning_cm,
            Exceedance::Alarm => self.alarm_cm,
        }
    }
}

const DATE_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// Lista wodowskazów; wpisy bez id, nazwy albo współrzędnych pomijane po
/// cichu, ale niepusta tablica bez żadnej stacji to zmiana schematu (C7).
pub fn parse_gauges(body: &[u8]) -> Result<Vec<Gauge>, SourceError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    let serde_json::Value::Array(entries) = value else {
        return Err(SourceError::Parse(
            "to nie jest lista wodowskazów IMGW".into(),
        ));
    };
    let gauges: Vec<Gauge> = entries.iter().filter_map(parse_entry).collect();
    if gauges.is_empty() && !entries.is_empty() {
        return Err(SourceError::Parse(
            "lista IMGW bez żadnego czytelnego wodowskazu".into(),
        ));
    }
    Ok(gauges)
}

fn number(v: Option<&serde_json::Value>) -> Option<f64> {
    let n = match v? {
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok()?,
        serde_json::Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

/// Próg ≤ 0, pusty, `null` albo nie-liczba to brak progu (C6).
fn threshold(v: Option<&serde_json::Value>) -> Option<f64> {
    number(v).filter(|n| *n > 0.0)
}

fn parse_entry(entry: &serde_json::Value) -> Option<Gauge> {
    let text = |name: &str| entry.get(name).and_then(|v| v.as_str());
    let id = text("id_stacji")?.trim();
    let name = clean_text(text("stacja")?);
    if id.is_empty() || name.is_empty() {
        return None;
    }
    let river = text("rzeka")
        .map(|r| water_name(&clean_text(r)))
        .filter(|r| !r.is_empty() && r != "-");
    Some(Gauge {
        id: id.to_owned(),
        name,
        river,
        lat: number(entry.get("lat"))?,
        lon: number(entry.get("lon"))?,
        level_cm: number(entry.get("stan_wody")),
        measured_at: text("stan_wody_data_pomiaru")
            .and_then(|s| NaiveDateTime::parse_from_str(s.trim(), DATE_FORMAT).ok()),
        warning_cm: threshold(entry.get("stan_ostrzegawczy")),
        alarm_cm: threshold(entry.get("stan_alarmowy")),
    })
}

/// Skróty w nazwach akwenów IMGW (z kropką i bez).
const WATER_WORDS: &[(&str, &str)] = &[
    ("Jez", "Jezioro"),
    ("jez", "Jezioro"),
    ("Zb", "Zbiornik"),
    ("Pd", "Południowe"),
    ("Pn", "Północne"),
    ("Kanał", "Kanał"),
];

/// Nazwa rzeki albo akwenu do mowy: kropka między literami → spacja
/// („Jez.Miedwie”), skróty rozwinięte („Jez. Mamry” → „Jezioro Mamry”,
/// „Zb. Dębe” → „Zbiornik Dębe”, „Pd.” → „Południowe”), spacje zwinięte.
pub fn water_name(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut spaced = String::with_capacity(raw.len());
    for (i, c) in chars.iter().enumerate() {
        let between_letters = *c == '.'
            && i.checked_sub(1)
                .and_then(|p| chars.get(p))
                .is_some_and(|p| p.is_alphabetic())
            && chars.get(i + 1).is_some_and(|n| n.is_alphabetic());
        if between_letters {
            spaced.push(' ');
        } else {
            spaced.push(*c);
        }
    }
    spaced
        .split_whitespace()
        .map(|word| {
            let bare = word.strip_suffix('.').unwrap_or(word);
            match WATER_WORDS.iter().find(|(abbr, _)| *abbr == bare) {
                Some((_, full)) => (*full).to_owned(),
                None => word.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
