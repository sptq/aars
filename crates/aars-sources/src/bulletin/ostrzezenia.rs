//! Część `ostrzezenia`: żywe alerty z bazy, bez ponownego filtrowania.

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

fn compose_alerts(ctx: &PartCtx) -> PartText {
    let summaries: Vec<&str> = ctx
        .active_alerts
        .iter()
        .map(|a| {
            a.summary
                .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '.' | ';' | ','))
        })
        .filter(|s| !s.is_empty())
        .collect();
    if summaries.is_empty() {
        return PartText::Text("Brak obowiązujących ostrzeżeń.".to_owned());
    }
    let listed = summaries
        .iter()
        .take(MAX_LISTED)
        .copied()
        .collect::<Vec<_>>()
        .join("; ");
    let mut text = format!("Obowiązujące ostrzeżenia: {listed}.");
    if summaries.len() > MAX_LISTED {
        text.push_str(&format!(
            " Pozostałych ostrzeżeń: {}.",
            summaries.len() - MAX_LISTED
        ));
    }
    PartText::Text(truncate_sentence(&text, MAX_CHARS).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bulletin::{BulletinPart, PartCtx, PartText};
    use aars_core::priority::Priority;
    use aars_core::source_state::{TrackedItem, TrackedState};
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

    fn ctx(alerts: Vec<TrackedItem>) -> PartCtx {
        PartCtx {
            now: Utc::now(),
            position: None,
            settings: BTreeMap::new(),
            active_alerts: alerts,
        }
    }

    async fn run(alerts: Vec<TrackedItem>) -> PartText {
        Ostrzezenia.compose(&ctx(alerts)).await.unwrap()
    }

    #[tokio::test]
    async fn no_alerts_is_said_out_loud() {
        assert_eq!(
            run(vec![]).await,
            PartText::Text("Brak obowiązujących ostrzeżeń.".into())
        );
    }

    #[tokio::test]
    async fn alerts_are_listed_up_to_five() {
        let t = run(vec![
            alert("G3 — silna burza geomagnetyczna"),
            alert("PM10, poziom informowania, miasto Rzeszów"),
            alert(""),
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
        let PartText::Text(t) = run(many).await else {
            panic!()
        };
        assert!(t.ends_with("Pozostałych ostrzeżeń: 2."), "{t}");
        assert!(t.contains("Alert numer 4."), "{t}");
        assert!(!t.contains("numer 5"), "{t}");
        assert!(t.chars().count() <= 500);
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
        crate::speech::assert_speakable(&[&text]);
    }
}
