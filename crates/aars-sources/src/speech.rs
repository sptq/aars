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
/// listy (`- `) łączy średnik, a wiersz po liście zaczyna nowe zdanie.
pub fn clean_text(raw: &str) -> String {
    let decoded = decode_entities(&strip_tags(raw));
    let mut out = String::new();
    let mut after_item = false;
    for line in decoded.lines() {
        let line = collapse_spaces(line);
        if line.is_empty() {
            continue;
        }
        let (item, is_item) = match line.strip_prefix("- ") {
            Some(rest) => (rest, true),
            None => (line.as_str(), false),
        };
        if !out.is_empty() {
            if is_item {
                // „gardła,” + „; ” dałoby „gardła,;”.
                while out.ends_with([',', ';']) {
                    out.pop();
                }
            }
            let separator = if is_item && !out.ends_with(':') {
                "; "
            } else if after_item && !out.ends_with(['.', '!', '?']) {
                ". "
            } else {
                " "
            };
            out.push_str(separator);
        }
        out.push_str(item);
        after_item = is_item;
    }
    out
}

fn collapse_spaces(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Zamienia znaczniki `<…>` na spację. Znacznik zaczyna `<` z literą
/// ASCII, `/` albo `!` zaraz za nim; inne `<` (np. `< 5`) i `<` bez
/// zamykającego `>` zostają dosłownie.
fn strip_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let opens_tag = tail.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!');
        match tail.find('>').filter(|_| opens_tag) {
            Some(end) => {
                out.push(' ');
                rest = &tail[end + 1..];
            }
            None => {
                out.push('<');
                rest = tail;
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
        _ => {
            let number = name.strip_prefix('#')?;
            let (digits, radix) = match number.strip_prefix(['x', 'X']) {
                Some(hex) => (hex, 16),
                None => (number, 10),
            };
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            u32::from_str_radix(digits, radix).ok().and_then(char::from_u32)
        }
    }
}

/// Encje nazwane i liczbowe (dziesiętne `&#NN;`, szesnastkowe `&#xNN;`);
/// nieznana encja zostaje dosłownie.
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
                // Znaki sterujące (np. `&#0;`) nie trafiają do syntezy; białe
                // sterujące stają się spacją, żeby nie skleić słów.
                if c.is_whitespace() {
                    out.push(' ');
                } else if !c.is_control() {
                    out.push(c);
                }
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

/// Wspólne sprawdzenie testów źródeł: tekst po normalizacji z `aars-audio`
/// nie zawiera znanych potknięć syntezy.
#[cfg(test)]
pub(crate) fn assert_speakable(texts: &[&str]) {
    const RAW: &[&str] = &["RCB", "NOAA", "SWPC", "EMSC", "RSO", "GIOŚ", "PM10", "PM2,5", "PM2.5", "MUF", "KC2G"];
    for text in texts.iter().filter(|t| !t.is_empty()) {
        let spoken = aars_audio::text::normalize(text);
        assert!(!spoken.contains("godzinie godzina"), "{spoken}");
        assert!(!spoken.contains(",;"), "{spoken}");
        for word in spoken.split_whitespace() {
            let core = word.trim_matches(|c: char| !c.is_alphanumeric());
            assert!(!RAW.contains(&core), "surowy skrót {core:?} w: {spoken}");
        }
    }
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
    fn a_line_after_a_list_starts_a_new_sentence() {
        // Fragment z fiksa rso/wszystkie-informacje-drogowe-2026-11-22.json.
        let raw = "od 31.10.2026: Dojazd do miejscowości \r\n\r\n- Gierłoż\r\n\r\n- Kruszewiec\r\n\r\n- Cegielnia\r\n\r\n- Kwiedzina\r\n\r\n- Pożarki\r\n\r\n- Martiany\r\n\r\nOdbywał się będzie drogą wojewódzką numer 592 od strony miejscowości Kętrzyn.\r\n\r\n \r\n\r\n ";
        assert_eq!(
            clean_text(raw),
            "od 31.10.2026: Dojazd do miejscowości; Gierłoż; Kruszewiec; Cegielnia; Kwiedzina; Pożarki; Martiany. Odbywał się będzie drogą wojewódzką numer 592 od strony miejscowości Kętrzyn."
        );
        // Kropka na końcu pozycji listy nie jest podwajana.
        assert_eq!(clean_text("Lista:\n- a.\nDalej"), "Lista: a. Dalej");
    }

    #[test]
    fn the_first_list_item_after_a_colon_is_joined_with_a_space() {
        // Kształt zaleceń GIOŚ.
        assert_eq!(
            clean_text("Ogół ludności:\n- rozważ\n- ogranicz"),
            "Ogół ludności: rozważ; ogranicz"
        );
        assert_eq!(
            clean_text("Ogół ludności:\n- rozważ, \n-  ogranicz.\nWrażliwe grupy:\n- unikaj"),
            "Ogół ludności: rozważ; ogranicz. Wrażliwe grupy: unikaj"
        );
    }

    #[test]
    fn a_list_item_ending_in_a_comma_or_semicolon_is_not_doubled() {
        // Kształt z prawdziwego zalecenia GIOŚ: „…gardła,\n- ogranicz”.
        assert_eq!(clean_text("Objawy:\n- ból gardła,\n- ogranicz;\n- unikaj"), "Objawy: ból gardła; ogranicz; unikaj");
    }

    #[test]
    fn control_characters_from_entities_are_dropped_and_hex_entities_decoded() {
        assert_eq!(clean_text("a&#0;b&#7;c"), "abc");
        assert_eq!(clean_text("x&#10;y"), "x y");
        assert_eq!(clean_text("&#x105;&#X119; &#x41;"), "ąę A");
        assert_eq!(clean_text("&#xZZ; &#x;"), "&#xZZ; &#x;");
    }

    #[test]
    fn tags_become_spaces_and_plain_angle_brackets_survive() {
        assert_eq!(clean_text("a<br>b"), "a b");
        assert_eq!(clean_text("<p>Zdanie.</p><p>Drugie</p>"), "Zdanie. Drugie");
        assert_eq!(clean_text("temperatura < 5 i > 0"), "temperatura < 5 i > 0");
        assert_eq!(clean_text("x <!-- uwaga --> y </div>z"), "x y z");
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
