//! Wspólne kawałki polskiego tekstu komunikatów. Źródło pisze cyfry i
//! `HH:MM`; słowa robi normalizacja w `aars-audio::text`.

use chrono::{DateTime, LocalResult, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Europe::Warsaw;

/// Czas polski (`Europe/Warsaw`, R11): niejednoznaczny → wcześniejszy,
/// nieistniejący → `None`.
pub fn warsaw_to_utc(t: NaiveDateTime) -> Option<DateTime<Utc>> {
    match Warsaw.from_local_datetime(&t) {
        LocalResult::Single(d) | LocalResult::Ambiguous(d, _) => Some(d.with_timezone(&Utc)),
        LocalResult::None => None,
    }
}

/// `HH:MM` czasu polskiego.
pub fn warsaw_hhmm(t: DateTime<Utc>) -> String {
    t.with_timezone(&Warsaw).format("%H:%M").to_string()
}

/// „1 kilometr”, „2 kilometry”, „5 kilometrów”.
pub fn km_phrase(km: u32) -> String {
    let word = if km == 1 {
        "kilometr"
    } else if (2..=4).contains(&(km % 10)) && !(12..=14).contains(&(km % 100)) {
        "kilometry"
    } else {
        "kilometrów"
    };
    format!("{km} {word}")
}

/// Tekst z sieci do mowy: bez znaczników HTML i encji, jedna linia; wiersze
/// listy (`- `) łączy średnik.
pub fn clean_text(raw: &str) -> String {
    let decoded = decode_entities(&strip_tags(raw));
    let mut out = String::new();
    for line in decoded.lines() {
        let line = collapse_spaces(line);
        if line.is_empty() {
            continue;
        }
        let (item, separator) = match line.strip_prefix("- ") {
            Some(rest) => (rest, "; "),
            None => (line.as_str(), " "),
        };
        if !out.is_empty() {
            out.push_str(separator);
        }
        out.push_str(item);
    }
    out
}

fn collapse_spaces(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Usuwa `<…>`; `<` bez zamykającego `>` zostaje dosłownie.
fn strip_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        match rest[start..].find('>') {
            Some(end) => rest = &rest[start + end + 1..],
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "#39" => Some('\''),
        "nbsp" => Some(' '),
        _ => name
            .strip_prefix('#')
            .filter(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
            .and_then(|digits| digits.parse::<u32>().ok())
            .and_then(char::from_u32),
    }
}

/// Nieznana encja zostaje dosłownie.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let decoded = tail
            .find(';')
            .filter(|&end| end <= 8)
            .and_then(|end| entity(&tail[..end]).map(|c| (c, end)));
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Skraca do `max_chars` znaków: na ostatnim końcu zdania (`. `, `! `, `? `)
/// przed limitem, inaczej na ostatniej spacji. `true`, gdy ucięto.
pub fn truncate_sentence(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_owned(), false);
    }
    // Znak tuż za limitem też się liczy: spacja tam pozwala uciąć dokładnie na limicie.
    let window: String = text.chars().take(max_chars.saturating_add(1)).collect();
    let sentence_end = ["! ", ". ", "? "]
        .iter()
        .filter_map(|end| window.rfind(end).map(|i| i + 1))
        .max();
    let cut = match sentence_end.or_else(|| window.rfind(' ')) {
        Some(at) => window[..at].to_owned(),
        None => text.chars().take(max_chars).collect(),
    };
    (cut.trim_end().to_owned(), true)
}

const EARTH_RADIUS_KM: f64 = 6371.0;

/// Odległość po kole wielkim; punkty jako (szerokość, długość) w stopniach.
pub fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lat2) = (a.0.to_radians(), b.0.to_radians());
    let dlat = lat2 - lat1;
    let dlon = (b.1 - a.1).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().min(1.0).asin()
}

const COMPASS8: [&str; 8] = [
    "na północ",
    "na północny wschód",
    "na wschód",
    "na południowy wschód",
    "na południe",
    "na południowy zachód",
    "na zachód",
    "na północny zachód",
];

/// Kierunek z `from` do `to` (azymut początkowy), osiem stron po 45°;
/// północ to 337,5°..22,5°.
pub fn compass8(from: (f64, f64), to: (f64, f64)) -> &'static str {
    let (lat1, lat2) = (from.0.to_radians(), to.0.to_radians());
    let dlon = (to.1 - from.1).to_radians();
    let y = dlon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
    let bearing = y.atan2(x).to_degrees().rem_euclid(360.0);
    let sector = ((bearing + 22.5) / 45.0).floor() as usize % 8;
    COMPASS8.get(sector).copied().unwrap_or(COMPASS8[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn km_phrase_agrees_with_the_number() {
        assert_eq!(km_phrase(1), "1 kilometr");
        assert_eq!(km_phrase(2), "2 kilometry");
        assert_eq!(km_phrase(5), "5 kilometrów");
        assert_eq!(km_phrase(12), "12 kilometrów");
        assert_eq!(km_phrase(22), "22 kilometry");
        assert_eq!(km_phrase(112), "112 kilometrów");
    }

    #[test]
    fn clean_text_strips_tags_entities_newlines_and_list_dashes() {
        assert_eq!(clean_text("<p>A&amp;B</p>\r\n- x\n- y"), "A&B; x; y");
        assert_eq!(clean_text("a &lt;b&gt; &quot;c&quot; &#39;d&#39;&nbsp;e &#261;"), "a <b> \"c\" 'd' e ą");
        assert_eq!(clean_text("  dużo    spacji  "), "dużo spacji");
    }

    #[test]
    fn truncate_sentence_cuts_at_the_last_sentence_end_before_the_limit() {
        let text = "Pierwsze zdanie. Drugie zdanie! Trzecie zdanie?";
        assert_eq!(truncate_sentence(text, 100), (text.to_owned(), false));
        assert_eq!(truncate_sentence(text, 40), ("Pierwsze zdanie. Drugie zdanie!".to_owned(), true));
        assert_eq!(truncate_sentence(text, 20), ("Pierwsze zdanie.".to_owned(), true));
        // Bez końca zdania przed limitem: na ostatniej spacji.
        assert_eq!(truncate_sentence("jeden dwa trzy cztery", 12), ("jeden dwa".to_owned(), true));
    }

    #[test]
    fn warsaw_hhmm_uses_local_time() {
        let winter = chrono::DateTime::parse_from_rfc3339("2026-11-22T13:05:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(warsaw_hhmm(winter), "14:05");
        let summer = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:05:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(warsaw_hhmm(summer), "14:05");
    }

    #[test]
    fn warsaw_to_utc_handles_daylight_saving() {
        let t = NaiveDate::from_ymd_opt(2026, 7, 23).unwrap().and_hms_opt(14, 0, 0).unwrap();
        assert_eq!(warsaw_to_utc(t).unwrap().format("%H:%M").to_string(), "12:00");
        let gap = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap().and_hms_opt(2, 30, 0).unwrap();
        assert!(warsaw_to_utc(gap).is_none());
    }

    #[test]
    fn haversine_lodz_warsaw_is_about_120_km() {
        // Łódź (centrum) – Warszawa.
        let km = haversine_km((51.7592, 19.4560), (52.2297, 21.0122));
        assert!((118.0..=122.0).contains(&km), "{km}");
    }

    #[test]
    fn compass8_names_the_direction() {
        let lodz = (51.77, 19.46);
        assert_eq!(compass8(lodz, (51.5552, 16.1286)), "na zachód");
        assert_eq!(compass8(lodz, (53.0, 19.46)), "na północ");
        assert_eq!(compass8(lodz, (50.0, 19.46)), "na południe");
        assert_eq!(compass8(lodz, (51.77, 22.0)), "na wschód");
        assert_eq!(compass8(lodz, (52.5, 20.6)), "na północny wschód");
        assert_eq!(compass8(lodz, (52.5, 18.3)), "na północny zachód");
        assert_eq!(compass8(lodz, (51.0, 20.7)), "na południowy wschód");
        assert_eq!(compass8(lodz, (51.0, 18.2)), "na południowy zachód");
    }
}
