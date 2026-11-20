//! Polski tekst komunikatu. Źródło pisze cyfry i `HH:MM`; słowa robi
//! normalizacja w `aars-audio::text`.

use aars_core::priority::Priority;
use chrono::{DateTime, Datelike, LocalResult, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Europe::Warsaw;

use super::parse::MeteoWarning;

pub const MONTHS_GENITIVE: [&str; 12] = [
    "stycznia",
    "lutego",
    "marca",
    "kwietnia",
    "maja",
    "czerwca",
    "lipca",
    "sierpnia",
    "września",
    "października",
    "listopada",
    "grudnia",
];

/// Czas IMGW to `Europe/Warsaw` (R11): niejednoznaczny → wcześniejszy,
/// nieistniejący → `None`.
pub fn warsaw_to_utc(t: NaiveDateTime) -> Option<DateTime<Utc>> {
    match Warsaw.from_local_datetime(&t) {
        LocalResult::Single(d) | LocalResult::Ambiguous(d, _) => Some(d.with_timezone(&Utc)),
        LocalResult::None => None,
    }
}

fn day_month(t: NaiveDateTime) -> String {
    let month = MONTHS_GENITIVE
        .get(t.month0() as usize)
        .copied()
        .unwrap_or("");
    format!("{} {month}", t.day())
}

pub fn validity(from: NaiveDateTime, until: NaiveDateTime) -> String {
    let (end_day, end_time) = if until.hour() == 0 && until.minute() == 0 {
        // Północ kończy poprzedni dzień jako 24:00.
        (until - chrono::Duration::days(1), "24:00".to_owned())
    } else {
        (until, until.format("%H:%M").to_string())
    };
    let start_time = from.format("%H:%M");
    if from.date() == end_day.date() {
        format!("{} od {start_time} do {end_time}", day_month(from))
    } else {
        format!(
            "od {start_time} {} do {end_time} {}",
            day_month(from),
            day_month(end_day)
        )
    }
}

fn level_genitive(level: u8) -> Option<&'static str> {
    match level {
        1 => Some("pierwszego"),
        2 => Some("drugiego"),
        3 => Some("trzeciego"),
        _ => None,
    }
}

const MAX_TEXT_CHARS: usize = 400;

/// Przycina opis do 400 znaków: na ostatniej kropce, a bez niej na granicy
/// słowa (z kropką na końcu).
fn trim_text(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_TEXT_CHARS {
        return text.to_owned();
    }
    let cut = text
        .char_indices()
        .nth(MAX_TEXT_CHARS)
        .map_or(text.len(), |(i, _)| i);
    let head = text.get(..cut).unwrap_or(text);
    if let Some(dot) = head.rfind('.') {
        return head.get(..=dot).unwrap_or(head).to_owned();
    }
    let words = match head.rfind(char::is_whitespace) {
        Some(space) => head.get(..space).unwrap_or(head),
        None => head,
    };
    let words = words.trim_end_matches(|c: char| c.is_whitespace() || ",;:-".contains(c));
    format!("{words}.")
}

pub fn announcement(w: &MeteoWarning, area: &str) -> String {
    let mut out = match w.level.and_then(level_genitive) {
        Some(level) => format!("Ostrzeżenie meteorologiczne {level} stopnia: {}.", w.event),
        None => format!("Ostrzeżenie meteorologiczne: {}.", w.event),
    };
    out.push_str(&format!(
        " Obszar: {area}. Obowiązuje {}.",
        validity(w.valid_from, w.valid_until)
    ));
    if let Some(p) = w.probability {
        out.push_str(&format!(" Prawdopodobieństwo {p} %."));
    }
    let text = trim_text(&w.text);
    if !text.is_empty() {
        out.push(' ');
        out.push_str(&text);
    }
    out.push_str(" Źródło: IMGW.");
    out
}

pub fn change(w: &MeteoWarning, area: &str) -> String {
    format!("Zmiana ostrzeżenia. {}", announcement(w, area))
}

pub fn cancellation(event: &str, area: &str) -> String {
    format!("Odwołanie ostrzeżenia meteorologicznego: {event}. Obszar: {area}. Źródło: IMGW.")
}

/// R6: 3 → alarmowy i przerywa, 2 → wysoki, reszta → zwykły.
pub fn level_priority(level: Option<u8>) -> (Priority, bool) {
    match level {
        Some(3) => (Priority::Emergency, true),
        Some(2) => (Priority::High, false),
        _ => (Priority::Normal, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imgw_meteo::parse::parse_warnings;
    use chrono::NaiveDate;

    fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn july_lodz() -> MeteoWarning {
        let body = include_bytes!("../../tests/fixtures/imgw/warningsmeteo-2026-07-23.json");
        let parsed = parse_warnings(body).unwrap();
        parsed
            .warnings
            .into_iter()
            .find(|w| w.id == "Sk20260722203344412")
            .unwrap()
    }

    #[test]
    fn july_storm_for_lodz_reads_like_this() {
        let w = july_lodz();
        let text = announcement(&w, "Łódź");
        assert!(
            text.starts_with(
                "Ostrzeżenie meteorologiczne pierwszego stopnia: Burze. Obszar: Łódź. \
             Obowiązuje 23 lipca od 10:00 do 22:00. Prawdopodobieństwo 80 %. Prognozowane"
            ),
            "{text}"
        );
        assert!(text.ends_with(" Źródło: IMGW."), "{text}");
        assert!(text.contains(w.text.trim()), "{text}");
        let spoken = aars_audio::text::normalize(&text);
        assert!(
            spoken.contains(
                "dwudziestego trzeciego lipca od godziny dziesiątej do godziny dwudziestej drugiej"
            ),
            "{spoken}"
        );
        assert!(spoken.contains("osiemdziesiąt procent"), "{spoken}");
        for part in [
            "od piętnastu milimetrów do dwudziestu pięciu milimetrów",
            "punktowo do trzydziestu milimetrów",
            "porywy wiatru do siedemdziesięciu pięciu kilometrów na godzinę",
        ] {
            assert!(spoken.contains(part), "{spoken}");
        }
        assert_eq!(change(&w, "Łódź"), format!("Zmiana ostrzeżenia. {text}"));
    }

    #[test]
    fn missing_level_probability_and_text_drop_their_sentences() {
        let mut w = july_lodz();
        w.level = None;
        w.probability = None;
        w.text = "  ".into();
        assert_eq!(
            announcement(&w, "Łódź"),
            "Ostrzeżenie meteorologiczne: Burze. Obszar: Łódź. Obowiązuje 23 lipca od 10:00 do 22:00. Źródło: IMGW."
        );
    }

    #[test]
    fn long_text_is_cut_at_the_last_full_stop_within_400_chars() {
        let mut w = july_lodz();
        w.text = format!("{} Koniec.", "Zdanie testowe. ".repeat(30));
        let text = announcement(&w, "Łódź");
        let body = text.split("Prawdopodobieństwo 80 %. ").nth(1).unwrap();
        let body = body.strip_suffix(" Źródło: IMGW.").unwrap();
        assert!(body.chars().count() <= 400);
        assert!(body.ends_with("Zdanie testowe."), "{body}");
        // bez kropki: granica słowa + kropka
        w.text = "słowo ".repeat(100);
        let text = announcement(&w, "Łódź");
        let body = text.split("Prawdopodobieństwo 80 %. ").nth(1).unwrap();
        let body = body.strip_suffix(" Źródło: IMGW.").unwrap();
        assert!(body.chars().count() <= 400);
        assert!(body.ends_with("słowo."), "{body}");
    }

    #[test]
    fn cancellation_text() {
        assert_eq!(
            cancellation("Burze", "Łódź"),
            "Odwołanie ostrzeżenia meteorologicznego: Burze. Obszar: Łódź. Źródło: IMGW."
        );
    }

    #[test]
    fn a_warning_until_midnight_ends_at_24_of_the_same_day() {
        assert_eq!(
            validity(dt(2026, 7, 23, 12, 0), dt(2026, 7, 24, 0, 0)),
            "23 lipca od 12:00 do 24:00"
        );
    }

    #[test]
    fn a_warning_over_two_days_names_both_dates() {
        assert_eq!(
            validity(dt(2026, 7, 23, 22, 0), dt(2026, 7, 24, 6, 0)),
            "od 22:00 23 lipca do 06:00 24 lipca"
        );
    }

    #[test]
    fn levels_map_to_priorities() {
        assert_eq!(level_priority(Some(3)), (Priority::Emergency, true));
        assert_eq!(level_priority(Some(2)), (Priority::High, false));
        assert_eq!(level_priority(Some(1)), (Priority::Normal, false));
        assert_eq!(level_priority(None), (Priority::Normal, false));
    }

    #[test]
    fn warsaw_time_is_converted_with_daylight_saving() {
        let utc = |t| warsaw_to_utc(t).map(|d| d.format("%Y-%m-%d %H:%MZ").to_string());
        assert_eq!(
            utc(dt(2026, 7, 23, 10, 0)).as_deref(),
            Some("2026-07-23 08:00Z")
        );
        assert_eq!(
            utc(dt(2024, 12, 30, 2, 0)).as_deref(),
            Some("2024-12-30 01:00Z")
        );
        assert_eq!(utc(dt(2026, 3, 29, 2, 30)), None);
        // niejednoznaczna chwila: wcześniejsza (czas letni)
        assert_eq!(
            utc(dt(2026, 10, 25, 2, 30)).as_deref(),
            Some("2026-10-25 00:30Z")
        );
    }

    #[test]
    fn month_names_match_the_normalizer() {
        assert_eq!(
            MONTHS_GENITIVE,
            aars_audio::text::normalize::MONTHS_GENITIVE
        );
    }
}
