//! Polski tekst komunikatu hydrologicznego. Źródło pisze cyfry i `HH:MM`;
//! słowa robi normalizacja w `aars-audio::text`.

use chrono::{NaiveDateTime, Timelike};

use crate::imgw_meteo::speech::{day_month, level_genitive};
use crate::speech::truncate_sentence;

pub const MAX_TEXT_CHARS: usize = 700;
/// Poniżej tylu znaków budżetu przebieg wypada w całości.
const MIN_COURSE_CHARS: usize = 20;
const SOURCE: &str = "Źródło: IMGW-PIB.";

/// „Ważne do 24 listopada godz. 12:00”; północ to 24:00 poprzedniego dnia.
fn valid_until(until: NaiveDateTime) -> String {
    let (day, time) = if until.hour() == 0 && until.minute() == 0 {
        (until - chrono::Duration::days(1), "24:00".to_owned())
    } else {
        (until, until.format("%H:%M").to_string())
    };
    format!("Ważne do {} godz. {time}", day_month(day))
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn sentence(text: &str) -> String {
    let text = text.trim().trim_end_matches('.').trim_end();
    if text.is_empty() {
        String::new()
    } else {
        format!("{text}.")
    }
}

/// Ogłoszenie: początek, ważność i atrybucja zostają zawsze; skracany jest
/// tylko przebieg (C4).
pub fn announcement(
    level: u8,
    event: &str,
    areas: &str,
    course: &str,
    until: NaiveDateTime,
) -> String {
    let mut head = match level_genitive(level) {
        Some(level) => format!(
            "Ostrzeżenie hydrologiczne IMGW {level} stopnia: {}",
            sentence(event)
        ),
        None => format!("Ostrzeżenie hydrologiczne IMGW: {}", sentence(event)),
    };
    let areas = sentence(&capitalize(areas));
    if !areas.is_empty() {
        head.push(' ');
        head.push_str(&areas);
    }
    let tail = format!(" {}. {SOURCE}", valid_until(until));
    let budget = MAX_TEXT_CHARS.saturating_sub(head.chars().count() + tail.chars().count() + 1);
    let course = course.trim();
    if course.is_empty() || budget < MIN_COURSE_CHARS {
        return format!("{head}{tail}");
    }
    let (mut course, cut) = truncate_sentence(course, budget);
    if cut && !course.ends_with(['.', '!', '?']) {
        course = sentence(&course);
    }
    format!("{head} {course}{tail}")
}

pub fn change(announcement: &str) -> String {
    format!("Zmiana ostrzeżenia hydrologicznego. {announcement}")
}

pub fn cancellation(number: &str, voivodeships: &str, event: &str) -> String {
    format!("Ostrzeżenie hydrologiczne nr {number} ({voivodeships}) „{event}” nie obowiązuje.")
}
