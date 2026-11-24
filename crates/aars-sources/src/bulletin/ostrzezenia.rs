//! Część `ostrzezenia`: żywe alerty z bazy, bez ponownego filtrowania;
//! mówi też, gdy część źródeł ostrzeżeń jest niedostępna.

use aars_core::source_state::TrackedItem;

use crate::bulletin::{BulletinPart, PartCtx, PartFuture, PartText};
use crate::speech::truncate_sentence;

const MAX_CHARS: usize = 1200;

pub struct Ostrzezenia;

impl BulletinPart for Ostrzezenia {
    fn id(&self) -> &'static str {
        "ostrzezenia"
    }

    fn max_chars(&self) -> usize {
        MAX_CHARS
    }

    fn compose<'a>(&'a self, ctx: &'a PartCtx) -> PartFuture<'a> {
        Box::pin(async move { Ok(compose_alerts(ctx)) })
    }
}

const SOURCES_DOWN: &str = "Część źródeł ostrzeżeń jest niedostępna.";
const HEADER: &str = "Obowiązujące ostrzeżenia.";

/// „1 ostrzeżenie”, „2–4 ostrzeżenia” (poza 12–14), reszta „ostrzeżeń”.
fn warnings_count(n: usize) -> String {
    let form = if n == 1 {
        "ostrzeżenie"
    } else if matches!(n % 10, 2..=4) && !matches!(n % 100, 12..=14) {
        "ostrzeżenia"
    } else {
        "ostrzeżeń"
    };
    format!("{n} {form}")
}

/// Pełna treść alertu (puste → podsumowanie), zakończona kropką.
fn spoken(alert: &TrackedItem) -> Option<String> {
    let raw = if alert.text.trim().is_empty() {
        alert.summary.as_str()
    } else {
        alert.text.as_str()
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.ends_with(['.', '!', '?']) {
        Some(raw.to_owned())
    } else {
        Some(format!(
            "{}.",
            raw.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ';' | ',' | ':'))
        ))
    }
}

fn compose_alerts(ctx: &PartCtx) -> PartText {
    let health = ctx.alert_sources;
    if health.enabled == 0 {
        return PartText::Empty("brak włączonych źródeł ostrzeżeń");
    }
    // Najpilniejsze pierwsze (priorytet malejąco), przy równym starsze.
    let mut alerts: Vec<&TrackedItem> = ctx.active_alerts.iter().collect();
    alerts.sort_by_key(|a| (a.priority as i64, a.first_seen_at, a.id));
    let texts: Vec<String> = alerts.iter().filter_map(|a| spoken(a)).collect();
    let total = alerts.len();
    let some_down = health.unavailable > 0;
    if total == 0 {
        return PartText::Text(if some_down {
            format!("Brak ostrzeżeń z dostępnych źródeł. {SOURCES_DOWN}")
        } else {
            "Brak obowiązujących ostrzeżeń dla okolic stacji.".to_owned()
        });
    }
    let down_tail = if some_down {
        format!(" {SOURCES_DOWN}")
    } else {
        String::new()
    };
    if texts.is_empty() {
        return PartText::Text(format!("Obowiązujące ostrzeżenia: {total}.{down_tail}"));
    }
    // Ogon („Oraz jeszcze…”, „Część źródeł…”) nie może wypaść przy cięciu
    // długości: najpierw ubywa pełnych treści, a gdy zostaje jedna — tnie
    // się ją samą.
    let mut listed_n = texts.len();
    loop {
        let rest = total - listed_n;
        let mut tail = String::new();
        if rest > 0 {
            tail.push_str(&format!(" Oraz jeszcze {}.", warnings_count(rest)));
        }
        tail.push_str(&down_tail);
        let body = texts[..listed_n].join(" ");
        let head = format!("{HEADER} {body}");
        if head.chars().count() + tail.chars().count() <= MAX_CHARS {
            return PartText::Text(format!("{head}{tail}"));
        }
        if listed_n > 1 {
            listed_n -= 1;
            continue;
        }
        // Nagłówek ma własną kropkę, więc tnie się samą treść.
        let budget =
            MAX_CHARS.saturating_sub(HEADER.chars().count() + 1 + tail.chars().count() + 1);
        let (cut, _) = truncate_sentence(&texts[0], budget);
        let cut =
            cut.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '.' | ';' | ',' | ':'));
        return PartText::Text(format!("{HEADER} {cut}.{tail}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bulletin::{BulletinPart, PartCtx, PartText};
    use aars_core::priority::Priority;
    use aars_core::source_state::{AlertSourceHealth, TrackedItem, TrackedState};
    use chrono::Utc;
    use std::collections::BTreeMap;

    fn alert(summary: &str) -> TrackedItem {
        let now = Utc::now();
        TrackedItem {
            id: 1,
            source_id: "swpc".into(),
            item_key: "k".into(),
            topic: "t".into(),
            fingerprint: "f".into(),
            area: String::new(),
            summary: summary.into(),
            text: String::new(),
            cancel_text: String::new(),
            priority: Priority::Normal,
            valid_from: None,
            valid_until: None,
            state: TrackedState::Active,
            misses: 0,
            aired: true,
            first_seen_at: now,
            last_seen_at: now,
            queue_item_id: None,
            superseded: vec![],
            cancel_pending: false,
        }
    }

    fn full(summary: &str, text: &str) -> TrackedItem {
        TrackedItem {
            text: text.into(),
            ..alert(summary)
        }
    }

    const FRESH: AlertSourceHealth = AlertSourceHealth {
        enabled: 3,
        unavailable: 0,
    };
    const PARTLY_DOWN: AlertSourceHealth = AlertSourceHealth {
        enabled: 3,
        unavailable: 1,
    };

    fn ctx(alerts: Vec<TrackedItem>, alert_sources: AlertSourceHealth) -> PartCtx {
        PartCtx {
            now: Utc::now(),
            position: None,
            settings: BTreeMap::new(),
            sources: BTreeMap::new(),
            active_alerts: alerts,
            alert_sources,
        }
    }

    async fn run_with(alerts: Vec<TrackedItem>, health: AlertSourceHealth) -> PartText {
        Ostrzezenia.compose(&ctx(alerts, health)).await.unwrap()
    }

    async fn run(alerts: Vec<TrackedItem>) -> PartText {
        run_with(alerts, FRESH).await
    }

    fn text(t: PartText) -> String {
        match t {
            PartText::Text(t) => t,
            PartText::Empty(why) => panic!("pusta część: {why}"),
        }
    }

    #[tokio::test]
    async fn no_alerts_is_said_out_loud() {
        assert_eq!(
            run(vec![]).await,
            PartText::Text("Brak obowiązujących ostrzeżeń dla okolic stacji.".into())
        );
    }

    #[tokio::test]
    async fn no_enabled_alert_source_is_an_empty_part() {
        let none = AlertSourceHealth::default();
        assert_eq!(
            run_with(vec![], none).await,
            PartText::Empty("brak włączonych źródeł ostrzeżeń")
        );
    }

    #[tokio::test]
    async fn no_alerts_with_a_source_down_does_not_claim_all_clear() {
        assert_eq!(
            text(run_with(vec![], PARTLY_DOWN).await),
            "Brak ostrzeżeń z dostępnych źródeł. Część źródeł ostrzeżeń jest niedostępna."
        );
    }

    #[tokio::test]
    async fn full_texts_are_read_not_summaries() {
        assert_eq!(
            text(
                run(vec![full(
                    "Burze, 1. stopień",
                    "Uwaga, ostrzeżenie IMGW. Burze z gradem, pierwszy stopień, do godziny 20:00."
                )])
                .await
            ),
            "Obowiązujące ostrzeżenia. Uwaga, ostrzeżenie IMGW. Burze z gradem, pierwszy stopień, do godziny 20:00."
        );
    }

    #[tokio::test]
    async fn an_empty_text_falls_back_to_the_summary() {
        assert_eq!(
            text(run_with(vec![alert("Alert RCB")], PARTLY_DOWN).await),
            "Obowiązujące ostrzeżenia. Alert RCB. Część źródeł ostrzeżeń jest niedostępna."
        );
    }

    #[tokio::test]
    async fn alerts_without_any_text_are_counted() {
        assert_eq!(
            text(run(vec![alert(""), alert(" ")]).await),
            "Obowiązujące ostrzeżenia: 2."
        );
        assert_eq!(
            text(run_with(vec![alert("")], PARTLY_DOWN).await),
            "Obowiązujące ostrzeżenia: 1. Część źródeł ostrzeżeń jest niedostępna."
        );
    }

    #[tokio::test]
    async fn alerts_are_ordered_by_priority_then_age() {
        let now = Utc::now();
        let mk = |text: &str, priority: Priority, minutes_ago: i64| {
            let mut a = full("", text);
            a.priority = priority;
            a.first_seen_at = now - chrono::Duration::minutes(minutes_ago);
            a
        };
        let t = text(
            run(vec![
                mk("Niski.", Priority::Low, 90),
                mk("Normalny nowy.", Priority::Normal, 5),
                mk("Pilny.", Priority::High, 1),
                mk("Normalny stary.", Priority::Normal, 60),
            ])
            .await,
        );
        assert_eq!(
            t,
            "Obowiązujące ostrzeżenia. Pilny. Normalny stary. Normalny nowy. Niski."
        );
    }

    #[test]
    fn the_count_is_declined() {
        for (n, expected) in [
            (1, "1 ostrzeżenie"),
            (2, "2 ostrzeżenia"),
            (4, "4 ostrzeżenia"),
            (5, "5 ostrzeżeń"),
            (11, "11 ostrzeżeń"),
            (12, "12 ostrzeżeń"),
            (14, "14 ostrzeżeń"),
            (22, "22 ostrzeżenia"),
            (25, "25 ostrzeżeń"),
        ] {
            assert_eq!(warnings_count(n), expected);
        }
    }

    #[tokio::test]
    async fn what_does_not_fit_becomes_a_declined_remainder() {
        let long = |i: usize| {
            full(
                "",
                &format!("Ostrzeżenie {i} {}.", "bardzo długie ".repeat(10)),
            )
        };
        // Jedna treść ma ok. 150 znaków: 8 mieści się, reszta w ogonie.
        let t = text(run((0..12).map(long).collect()).await);
        assert!(t.chars().count() <= 1200, "{}", t.chars().count());
        let listed = (0..12)
            .filter(|i| t.contains(&format!("Ostrzeżenie {i} ")))
            .count();
        assert!(listed > 1 && listed < 12, "{listed}");
        assert!(
            t.ends_with(&format!("Oraz jeszcze {}.", warnings_count(12 - listed))),
            "{t}"
        );
        // Wypisane są pierwsze, nie dowolne.
        assert!(t.contains("Ostrzeżenie 0 "), "{t}");
        assert!(!t.contains(&format!("Ostrzeżenie {} ", listed + 1)), "{t}");
    }

    #[tokio::test]
    async fn the_remainder_with_a_single_missing_alert_is_singular() {
        let long = |i: usize| {
            full(
                "",
                &format!("Ostrzeżenie {i} {}.", "bardzo długie ".repeat(30)),
            )
        };
        let t = text(run((0..3).map(long).collect()).await);
        assert!(t.ends_with("Oraz jeszcze 1 ostrzeżenie."), "{t}");
    }

    #[tokio::test]
    async fn the_length_cut_keeps_the_source_down_tail() {
        let long = |i: usize| {
            full(
                "",
                &format!("Ostrzeżenie {i} {}.", "bardzo długie ".repeat(30)),
            )
        };
        let t = text(run_with((0..4).map(long).collect(), PARTLY_DOWN).await);
        assert!(t.chars().count() <= 1200, "{}", t.chars().count());
        assert!(
            t.ends_with("Oraz jeszcze 2 ostrzeżenia. Część źródeł ostrzeżeń jest niedostępna."),
            "{t}"
        );

        // Jedno ogromne ostrzeżenie: ucięte, ogon zostaje.
        let huge = full("", &"słowo ".repeat(400));
        let t = text(run_with(vec![huge, alert("Alert RCB")], PARTLY_DOWN).await);
        assert!(t.chars().count() <= 1200, "{}", t.chars().count());
        assert!(
            t.ends_with("Oraz jeszcze 1 ostrzeżenie. Część źródeł ostrzeżeń jest niedostępna."),
            "{t}"
        );
        assert!(t.starts_with("Obowiązujące ostrzeżenia. słowo"), "{t}");
    }

    #[test]
    fn warnings_text_is_speakable() {
        crate::speech::assert_speakable(&[
            "Obowiązujące ostrzeżenia. Uwaga, burze z gradem, pierwszy stopień. Dziś opady do 7 mm. Oraz jeszcze 2 ostrzeżenia.",
            "Brak ostrzeżeń z dostępnych źródeł. Część źródeł ostrzeżeń jest niedostępna.",
            "Obowiązujące ostrzeżenia: 2. Oraz jeszcze 3 ostrzeżenia.",
            "Brak obowiązujących ostrzeżeń dla okolic stacji.",
        ]);
    }
}
