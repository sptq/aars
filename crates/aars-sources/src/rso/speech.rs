//! Polskie teksty komunikatów RSO.

use super::parse::News;
use crate::speech::{clean_text, truncate_sentence};

const CONTENT_MAX: usize = 600;
const TITLE_MAX: usize = 150;
const SUMMARY_MAX: usize = 80;

fn sentence(s: &str) -> String {
    let s = s.trim_end_matches([',', ';', ':', ' ']);
    if s.ends_with(['.', '!', '?']) { s.to_owned() } else { format!("{s}.") }
}

/// Czy tytuł to goły „Alert RCB” (wtedy zaczynamy od niego zdanie).
pub fn is_plain_alert_rcb(title: &str) -> bool {
    clean_text(title).trim().eq_ignore_ascii_case("Alert RCB")
}

pub fn announcement(n: &News, voivodeship: &str) -> String {
    let title = truncate_sentence(&clean_text(&n.title), TITLE_MAX).0;
    let mut text = if is_plain_alert_rcb(&title) {
        format!("Alert RCB, {voivodeship}.")
    } else if title.is_empty() {
        format!("Komunikat RSO, {voivodeship}.")
    } else {
        format!("Komunikat RSO, {voivodeship}. {}", sentence(&title))
    };
    let (content, cut) = truncate_sentence(&clean_text(&n.content), CONTENT_MAX);
    if !content.is_empty() {
        text.push(' ');
        text.push_str(&sentence(&content));
    }
    if cut {
        text.push_str(" Pełna treść na stronie RSO.");
    }
    text
}

pub fn change(text: &str) -> String {
    format!("Zmiana komunikatu RSO. {text}")
}

pub fn cancel(title: &str) -> String {
    format!("Komunikat RSO „{}” nie obowiązuje.", clean_text(title))
}

pub fn summary(title: &str) -> String {
    clean_text(title).chars().take(SUMMARY_MAX).collect()
}
