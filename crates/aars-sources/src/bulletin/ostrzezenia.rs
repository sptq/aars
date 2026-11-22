//! Część `ostrzezenia`: żywe alerty z bazy, bez ponownego filtrowania;
//! mówi też, gdy część źródeł ostrzeżeń jest niedostępna.

use crate::bulletin::{BulletinPart, PartCtx, PartFuture, PartText};
use crate::speech::truncate_sentence;

const MAX_LISTED: usize = 5;
const MAX_CHARS: usize = 500;

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

fn compose_alerts(ctx: &PartCtx) -> PartText {
    let health = ctx.alert_sources;
    if health.enabled == 0 {
        return PartText::Empty("brak włączonych źródeł ostrzeżeń");
    }
    let summaries: Vec<&str> = ctx
        .active_alerts
        .iter()
        .map(|a| {
            a.summary
                .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '.' | ';' | ','))
                .trim()
        })
        .filter(|s| !s.is_empty())
        .collect();
    let total = ctx.active_alerts.len();
    let some_down = health.unavailable > 0;
    if total == 0 {
        return PartText::Text(if some_down {
            format!("Brak ostrzeżeń z dostępnych źródeł. {SOURCES_DOWN}")
        } else {
            "Brak obowiązujących ostrzeżeń.".to_owned()
        });
    }
    let down_tail = if some_down {
        format!(" {SOURCES_DOWN}")
    } else {
        String::new()
    };
    if summaries.is_empty() {
        return PartText::Text(format!("Obowiązujące ostrzeżenia: {total}.{down_tail}"));
    }
    // Ogon („Pozostałych…”, „Część źródeł…”) nie może wypaść przy cięciu
    // długości: najpierw ubywa pozycji listy, a gdy zostaje jedna — tnie
    // się sama lista.
    let mut listed_n = summaries.len().min(MAX_LISTED);
    loop {
        let rest = total - listed_n;
        let mut tail = String::new();
        if rest > 0 {
            tail.push_str(&format!(" Pozostałych ostrzeżeń: {rest}."));
        }
        tail.push_str(&down_tail);
        let listed = summaries
            .iter()
            .take(listed_n)
            .copied()
            .collect::<Vec<_>>()
            .join("; ");
        let head = format!("Obowiązujące ostrzeżenia: {listed}.");
        if head.chars().count() + tail.chars().count() <= MAX_CHARS {
            return PartText::Text(format!("{head}{tail}"));
        }
        if listed_n > 1 {
            listed_n -= 1;
            continue;
        }
        // Jedno ostrzeżenie dłuższe niż limit: miejsce na kropkę zostawione.
        let budget = MAX_CHARS.saturating_sub(tail.chars().count() + 1);
        let (cut, _) = truncate_sentence(&head, budget);
        let cut =
            cut.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '.' | ';' | ',' | ':'));
        return PartText::Text(format!("{cut}.{tail}"));
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
            PartText::Text("Brak obowiązujących ostrzeżeń.".into())
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
    async fn alerts_with_a_source_down_say_so_after_the_list() {
        assert_eq!(
            text(run_with(vec![alert("Alert RCB")], PARTLY_DOWN).await),
            "Obowiązujące ostrzeżenia: Alert RCB. Część źródeł ostrzeżeń jest niedostępna."
        );
    }

    #[tokio::test]
    async fn alerts_without_summaries_are_counted() {
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
    async fn alerts_are_listed_up_to_five() {
        let t = run(vec![
            alert("G3 — silna burza geomagnetyczna"),
            alert("PM10, poziom informowania, miasto Rzeszów"),
        ])
        .await;
        assert_eq!(
            t,
            PartText::Text(
                "Obowiązujące ostrzeżenia: G3 — silna burza geomagnetyczna; PM10, poziom informowania, miasto Rzeszów."
                    .into()
            )
        );
        let many: Vec<_> = (0..7)
            .map(|i| alert(&format!("Alert numer {i};")))
            .collect();
        let t = text(run(many).await);
        assert!(t.ends_with("Pozostałych ostrzeżeń: 2."), "{t}");
        assert!(t.contains("Alert numer 4."), "{t}");
        assert!(!t.contains("numer 5"), "{t}");
        assert!(t.chars().count() <= 500);
    }

    #[tokio::test]
    async fn unnamed_alerts_count_into_the_rest() {
        let t = text(run(vec![alert("Alert RCB"), alert(""), alert("")]).await);
        assert_eq!(
            t,
            "Obowiązujące ostrzeżenia: Alert RCB. Pozostałych ostrzeżeń: 2."
        );
    }

    #[tokio::test]
    async fn the_length_cut_shortens_the_list_not_the_tail() {
        let long = |i: usize| alert(&format!("Ostrzeżenie {i} {}", "bardzo długie ".repeat(8)));
        let t = text(run_with((0..7).map(long).collect(), PARTLY_DOWN).await);
        assert!(t.chars().count() <= 500, "{}", t.chars().count());
        assert!(
            t.ends_with("Część źródeł ostrzeżeń jest niedostępna."),
            "{t}"
        );
        assert!(t.contains("Ostrzeżenie 0 "), "{t}");
        assert!(!t.contains("Ostrzeżenie 4 "), "{t}");
        let listed = (0..7)
            .filter(|i| t.contains(&format!("Ostrzeżenie {i} ")))
            .count();
        assert!(
            t.contains(&format!("Pozostałych ostrzeżeń: {}.", 7 - listed)),
            "{t}"
        );

        // Jedno ogromne ostrzeżenie: ucięte, ogon zostaje.
        let huge = alert(&"słowo ".repeat(120));
        let t = text(run_with(vec![huge, alert("Alert RCB")], PARTLY_DOWN).await);
        assert!(t.chars().count() <= 500, "{}", t.chars().count());
        assert!(
            t.ends_with("Pozostałych ostrzeżeń: 1. Część źródeł ostrzeżeń jest niedostępna."),
            "{t}"
        );
        assert!(t.starts_with("Obowiązujące ostrzeżenia: słowo"), "{t}");
    }

    #[test]
    fn source_summaries_are_speakable() {
        let text = format!(
            "Obowiązujące ostrzeżenia: {}.",
            [
                "Burze, 1. stopień",
                "Alert RCB",
                "G3 — silna burza geomagnetyczna",
                "PM10, poziom informowania, miasto Rzeszów",
                "M2,1, 21 km, Polska"
            ]
            .join("; ")
        );
        crate::speech::assert_speakable(&[
            &text,
            "Brak ostrzeżeń z dostępnych źródeł. Część źródeł ostrzeżeń jest niedostępna.",
            "Obowiązujące ostrzeżenia: 2. Pozostałych ostrzeżeń: 3.",
        ]);
    }
}
