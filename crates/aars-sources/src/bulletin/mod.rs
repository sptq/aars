//! Kompozytor biuletynu: trait części, czyste składanie z limitem długości.

pub mod ostrzezenia;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use aars_core::geo::StationPosition;
use aars_core::source_state::TrackedItem;
use chrono::{DateTime, Utc};

use crate::source::SourceError;

pub use ostrzezenia::Ostrzezenia;

/// Wejście części: chwila nadania, pozycja stacji, ustawienia
/// `module:bulletin:*` (bez prefiksu) i żywe alerty.
pub struct PartCtx {
    pub now: DateTime<Utc>,
    pub position: Option<StationPosition>,
    pub settings: BTreeMap<String, String>,
    pub active_alerts: Vec<TrackedItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PartText {
    Text(String),
    /// Brak treści, z powodem, np. „brak pozycji stacji”.
    Empty(&'static str),
}

pub type PartFuture<'a> = Pin<Box<dyn Future<Output = Result<PartText, SourceError>> + Send + 'a>>;

pub trait BulletinPart: Send + Sync {
    fn id(&self) -> &'static str;
    fn max_chars(&self) -> usize;
    fn compose<'a>(&'a self, ctx: &'a PartCtx) -> PartFuture<'a>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum PartOutcome {
    Included(String),
    Empty(String),
    Failed(String),
    DroppedForLength,
}

impl PartOutcome {
    pub fn describe(&self) -> String {
        match self {
            Self::Included(t) => format!("w biuletynie ({} znaków)", t.chars().count()),
            Self::Empty(p) => format!("pominięta — {p}"),
            Self::Failed(e) => format!("błąd — {e}"),
            Self::DroppedForLength => "pominięta — limit czasu nadawania".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Composed {
    pub text: Option<String>,
    pub parts: Vec<(&'static str, PartOutcome)>,
    pub skipped_reason: Option<&'static str>,
}

const OUTRO: &str = "Koniec biuletynu.";

fn ends_sentence(t: &str) -> bool {
    t.ends_with(['.', '!', '?'])
}

/// Składa biuletyn z części `Included`; dopóki tekst przekracza `limit`
/// znaków, ostatnia dołączona część wypada (`DroppedForLength`).
pub fn assemble(
    intro: &str,
    hhmm: &str,
    parts: Vec<(&'static str, PartOutcome)>,
    limit: usize,
) -> Composed {
    let mut parts = parts;
    let mut dropped = false;
    loop {
        let included: Vec<String> = parts
            .iter()
            .filter_map(|(_, o)| match o {
                PartOutcome::Included(t) => Some(if ends_sentence(t) {
                    t.clone()
                } else {
                    format!("{t}.")
                }),
                _ => None,
            })
            .collect();
        if included.is_empty() {
            let reason = if dropped {
                "limit czasu nadawania"
            } else {
                "brak danych"
            };
            return Composed {
                text: None,
                parts,
                skipped_reason: Some(reason),
            };
        }
        let text = format!("{intro} {hhmm}. {} {OUTRO}", included.join(" "));
        if text.chars().count() <= limit {
            return Composed {
                text: Some(text),
                parts,
                skipped_reason: None,
            };
        }
        if let Some((_, o)) = parts
            .iter_mut()
            .rev()
            .find(|(_, o)| matches!(o, PartOutcome::Included(_)))
        {
            *o = PartOutcome::DroppedForLength;
            dropped = true;
        }
    }
}

/// Wstęp: ustawienie operatora (z kropką na końcu) albo „Biuletyn stacji {znak}.”.
pub fn intro_text(intro_setting: &str, callsign: &str) -> String {
    let intro = intro_setting.trim();
    if intro.is_empty() {
        let callsign = callsign.trim();
        return if callsign.is_empty() {
            "Biuletyn stacji.".to_owned()
        } else {
            format!("Biuletyn stacji {callsign}.")
        };
    }
    if ends_sentence(intro) {
        intro.to_owned()
    } else {
        format!("{intro}.")
    }
}

/// Limit długości tekstu: około 10 znaków na sekundę, nie więcej niż 1500.
pub fn length_limit(tx_max_duration_s: u64) -> usize {
    usize::try_from(tx_max_duration_s.saturating_mul(10))
        .unwrap_or(usize::MAX)
        .min(1500)
}

#[cfg(test)]
mod tests {
    use super::*;
    use PartOutcome::*;

    #[test]
    fn parts_join_in_order_between_intro_and_end() {
        let c = assemble(
            "Biuletyn stacji SP9XYZ.",
            "07:00",
            vec![
                ("pogoda", Included("A b".into())),
                ("propagacja", Empty("brak danych propagacji".into())),
                ("ostrzezenia", Included("C.".into())),
            ],
            1500,
        );
        assert_eq!(
            c.text.as_deref(),
            Some("Biuletyn stacji SP9XYZ. 07:00. A b. C. Koniec biuletynu.")
        );
    }

    fn long(n: usize) -> String {
        format!("{}.", "a".repeat(n - 1))
    }

    #[test]
    fn parts_drop_from_the_end_to_fit_the_limit() {
        let c = assemble(
            "Biuletyn stacji.",
            "07:00",
            vec![
                ("a", Included(long(200))),
                ("b", Included(long(200))),
                ("c", Included(long(200))),
            ],
            length_limit(30),
        );
        assert!(matches!(c.parts[0].1, Included(_)));
        assert_eq!(c.parts[1].1, DroppedForLength);
        assert_eq!(c.parts[2].1, DroppedForLength);
        assert_eq!(c.parts[2].1.describe(), "pominięta — limit czasu nadawania");
        assert!(c.text.is_some());
    }

    #[test]
    fn nothing_to_say_is_no_bulletin() {
        let c = assemble(
            "B.",
            "07:00",
            vec![("a", Empty("x".into())), ("b", Failed("y".into()))],
            1500,
        );
        assert_eq!((c.text, c.skipped_reason), (None, Some("brak danych")));
        let c = assemble("B.", "07:00", vec![("a", Included(long(200)))], 50);
        assert_eq!(
            (c.text, c.skipped_reason),
            (None, Some("limit czasu nadawania"))
        );
    }

    #[test]
    fn the_intro_defaults_to_the_callsign() {
        assert_eq!(intro_text("", "SP9XYZ"), "Biuletyn stacji SP9XYZ.");
        assert_eq!(intro_text("  ", ""), "Biuletyn stacji.");
        assert_eq!(
            intro_text("Tu stacja klubowa", "SP9XYZ"),
            "Tu stacja klubowa."
        );
    }

    #[test]
    fn length_limit_follows_the_tx_limit() {
        assert_eq!(length_limit(180), 1500);
        assert_eq!(length_limit(60), 600);
    }

    #[test]
    fn describe_names_each_outcome() {
        assert_eq!(Included("abc".into()).describe(), "w biuletynie (3 znaków)");
        assert_eq!(
            Empty("brak pozycji stacji".into()).describe(),
            "pominięta — brak pozycji stacji"
        );
        assert_eq!(Failed("zły".into()).describe(), "błąd — zły");
    }
}
