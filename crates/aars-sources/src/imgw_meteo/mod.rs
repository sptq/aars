//! Źródło ostrzeżeń meteorologicznych IMGW (`warningsmeteo`).

pub mod parse;
pub mod speech;

use crate::source::{SourceCtx, SourceItem, SourceOutput};
use parse::ParsedWarnings;

const TIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// Czysta funkcja: odpowiedź IMGW → pozycje dla obszaru ze `ctx`.
pub fn to_output(parsed: ParsedWarnings, ctx: &SourceCtx) -> SourceOutput {
    let mut complete = parsed.skipped == 0;
    let mut items = Vec::new();
    for w in &parsed.warnings {
        let labels: Vec<&str> = ctx
            .area
            .iter()
            .filter(|entry| w.teryt.iter().any(|code| entry.matches(code)))
            .map(|entry| entry.label.as_str())
            .collect();
        if labels.is_empty() {
            continue;
        }
        let (Some(valid_from), Some(valid_until)) =
            (speech::warsaw_to_utc(w.valid_from), speech::warsaw_to_utc(w.valid_until))
        else {
            complete = false;
            continue;
        };
        let area = labels.join(", ");
        let (priority, interrupting) = speech::level_priority(w.level);
        let level = w.level.map_or_else(|| "-".to_owned(), |n| n.to_string());
        items.push(SourceItem {
            key: w.id.clone(),
            topic: w.event.to_lowercase(),
            fingerprint: format!(
                "{}|{level}|{}|{}|{area}",
                w.event,
                w.valid_from.format(TIME_FORMAT),
                w.valid_until.format(TIME_FORMAT)
            ),
            text: speech::announcement(w, &area),
            change_text: speech::change(w, &area),
            cancel_text: speech::cancellation(&w.event, &area),
            summary: match w.level {
                Some(n) => format!("{}, {n}. stopień", w.event),
                None => w.event.clone(),
            },
            priority,
            interrupting,
            valid_from: Some(valid_from),
            valid_until: Some(valid_until),
        });
    }
    SourceOutput { items, complete }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::area::parse_area;
    use chrono::Utc;

    const JULY: &[u8] = include_bytes!("../../tests/fixtures/imgw/warningsmeteo-2026-07-23.json");

    fn ctx(area: &str) -> SourceCtx {
        SourceCtx { now: Utc::now(), area: parse_area(area).unwrap() }
    }

    fn run(body: &[u8], area: &str) -> SourceOutput {
        to_output(parse::parse_warnings(body).unwrap(), &ctx(area))
    }

    #[test]
    fn only_warnings_touching_the_area_are_returned() {
        assert_eq!(run(JULY, "1061 Łódź").items.len(), 1);
        assert_eq!(run(JULY, "10").items.len(), 1);
        assert_eq!(run(JULY, "22").items.len(), 3);
        let none = run(JULY, "3064 Poznań");
        assert_eq!(none.items.len(), 0);
        assert!(none.complete);
        let item = &run(JULY, "1061 Łódź").items[0];
        assert_eq!(item.key, "Sk20260722203344412");
        assert_eq!(item.topic, "burze");
        assert_eq!(item.summary, "Burze, 1. stopień");
        assert_eq!(item.fingerprint, "Burze|1|2026-07-23 10:00|2026-07-23 22:00|Łódź");
        assert_eq!(item.valid_until.map(|d| d.to_rfc3339()).as_deref(), Some("2026-07-23T20:00:00+00:00"));
        assert!(item.text.contains("Obszar: Łódź."));
        assert!(item.change_text.starts_with("Zmiana ostrzeżenia. "));
        assert!(item.cancel_text.starts_with("Odwołanie ostrzeżenia"));
    }

    #[test]
    fn two_area_entries_hit_by_one_warning_are_read_together() {
        let out = run(JULY, "1061 Łódź\n1020 powiat zgierski");
        assert_eq!(out.items.len(), 1);
        assert!(out.items[0].text.contains("Obszar: Łódź, powiat zgierski."));
        assert!(out.items[0].fingerprint.ends_with("|Łódź, powiat zgierski"));
    }

    #[test]
    fn the_fingerprint_ignores_text_but_not_level_or_validity() {
        let fp = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v: serde_json::Value = serde_json::from_slice(JULY).unwrap();
            let idx = v
                .as_array()
                .unwrap()
                .iter()
                .position(|w| w["id"] == "Sk20260722203344412")
                .unwrap();
            edit(&mut v[idx]);
            run(&serde_json::to_vec(&v).unwrap(), "1061 Łódź").items[0].fingerprint.clone()
        };
        let base = fp(&|_| {});
        assert_eq!(base, fp(&|w| {
            w["tresc"] = "Inny opis.".into();
            w["prawdopodobienstwo"] = "30".into();
        }));
        assert_ne!(base, fp(&|w| w["stopien"] = "2".into()));
        assert_ne!(base, fp(&|w| w["obowiazuje_do"] = "2026-07-23 23:00:00".into()));
    }

    #[test]
    fn a_skipped_entry_makes_the_fetch_incomplete() {
        let mut v: serde_json::Value = serde_json::from_slice(JULY).unwrap();
        v[2].as_object_mut().unwrap().remove("teryt");
        let out = run(&serde_json::to_vec(&v).unwrap(), "22");
        assert!(!out.complete);
    }

    #[test]
    fn a_nonexistent_local_time_is_skipped_and_incomplete() {
        let mut v: serde_json::Value = serde_json::from_slice(JULY).unwrap();
        let idx = v
            .as_array()
            .unwrap()
            .iter()
            .position(|w| w["id"] == "Sk20260722203344412")
            .unwrap();
        v[idx]["obowiazuje_od"] = "2026-03-29 02:30:00".into();
        let out = run(&serde_json::to_vec(&v).unwrap(), "1061 Łódź");
        assert!(out.items.is_empty());
        assert!(!out.complete);
    }
}
