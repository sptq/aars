//! Kompozytor biuletynu: trait części, czyste składanie z limitem długości.

pub mod ostrzezenia;
pub mod runner;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aars_core::config::Config;
use aars_core::error::DbError;
use aars_core::geo::{PositionRepo, StationPosition};
use aars_core::module::{BULLETIN, BULLETIN_PARTS};
use aars_core::settings::SettingsRepo;
use aars_core::source_state::{AlertSourceHealth, SourceStateRepo, TrackedItem};
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use crate::source::SourceError;
use crate::speech::{truncate_sentence, warsaw_hhmm};
use crate::{pogoda, propagacja};

pub use ostrzezenia::Ostrzezenia;
pub use runner::{BulletinRunner, BulletinTick, due_fires};

/// Wejście części: chwila nadania, pozycja stacji, ustawienia
/// `module:bulletin:*` (bez prefiksu), żywe alerty i zdrowie źródeł alertów.
pub struct PartCtx {
    pub now: DateTime<Utc>,
    pub position: Option<StationPosition>,
    pub settings: BTreeMap<String, String>,
    pub active_alerts: Vec<TrackedItem>,
    pub alert_sources: AlertSourceHealth,
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

/// Limit długości tekstu: 10 s limitu nadawania na identyfikację i ogony,
/// reszta po około 10 znaków na sekundę, nie więcej niż 1500.
pub fn length_limit(tx_max_duration_s: u64) -> usize {
    usize::try_from(tx_max_duration_s.saturating_sub(10).saturating_mul(10))
        .unwrap_or(usize::MAX)
        .min(1500)
}

const SETTINGS_PREFIX: &str = "module:bulletin:";
const DEFAULT_PART_TIMEOUT: Duration = Duration::from_secs(20);

/// Ustawienia `module:bulletin:*` (bez prefiksu); brakujące klucze z opisu modułu.
pub(crate) async fn module_settings(
    pool: &SqlitePool,
) -> Result<BTreeMap<String, String>, DbError> {
    let mut out: BTreeMap<String, String> = BULLETIN
        .settings
        .iter()
        .map(|s| (s.key.to_owned(), s.default.to_owned()))
        .collect();
    out.extend(
        SettingsRepo::new(pool.clone())
            .kv_prefix(SETTINGS_PREFIX)
            .await?,
    );
    Ok(out)
}

fn order_of(id: &str) -> usize {
    BULLETIN_PARTS
        .iter()
        .position(|(v, _)| *v == id)
        .unwrap_or(BULLETIN_PARTS.len())
}

/// Kompozytor: wspólna ścieżka nadania i podglądu. Wszystko, co zależy od
/// operatora (znak, części, wstęp, limit nadawania), czyta z bazy przy
/// każdym złożeniu.
pub struct Bulletin {
    pool: SqlitePool,
    parts: Vec<Arc<dyn BulletinPart>>,
    part_timeout: Duration,
}

impl Bulletin {
    /// Kolejność części wg `BULLETIN_PARTS`, nie wg wektora.
    pub fn new(pool: SqlitePool, parts: Vec<Arc<dyn BulletinPart>>) -> Self {
        let mut parts = parts;
        parts.sort_by_key(|p| order_of(p.id()));
        Self {
            pool,
            parts,
            part_timeout: DEFAULT_PART_TIMEOUT,
        }
    }

    pub fn with_part_timeout(mut self, t: Duration) -> Self {
        self.part_timeout = t;
        self
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Składa biuletyn na chwilę `now` bez zapisu do kolejki (podgląd).
    pub async fn compose(&self, now: DateTime<Utc>) -> Result<Composed, DbError> {
        self.compose_at(now, now).await
    }

    /// `now` — chwila danych; `spoken` — pora wypowiadana we wstępie.
    pub(crate) async fn compose_at(
        &self,
        now: DateTime<Utc>,
        spoken: DateTime<Utc>,
    ) -> Result<Composed, DbError> {
        let station = SettingsRepo::new(self.pool.clone()).load().await?;
        let settings = module_settings(&self.pool).await?;
        let selected: Vec<String> = settings
            .get("parts")
            .map(|v| v.split(',').map(|p| p.trim().to_owned()).collect())
            .unwrap_or_default();
        let chosen = |id: &str| selected.iter().any(|p| p == id);
        let position = PositionRepo::new(self.pool.clone()).load().await?;
        let (active_alerts, alert_sources) = if chosen("ostrzezenia") {
            let repo = SourceStateRepo::new(self.pool.clone());
            (
                repo.live_alerts(now).await?,
                repo.alert_source_health(now).await?,
            )
        } else {
            (Vec::new(), AlertSourceHealth::default())
        };
        let intro = intro_text(
            settings.get("intro").map(String::as_str).unwrap_or(""),
            &station.callsign,
        );
        let ctx = Arc::new(PartCtx {
            now,
            position,
            settings,
            active_alerts,
            alert_sources,
        });

        let timeout = self.part_timeout;
        let running: Vec<_> = self
            .parts
            .iter()
            .filter(|p| chosen(p.id()))
            .map(|part| {
                let part = Arc::clone(part);
                let ctx = Arc::clone(&ctx);
                let id = part.id();
                let handle = tokio::spawn(async move {
                    let result = tokio::time::timeout(timeout, part.compose(&ctx)).await;
                    outcome(result, part.max_chars(), timeout)
                });
                (id, handle)
            })
            .collect();
        let mut parts = Vec::with_capacity(running.len());
        for (id, handle) in running {
            let outcome = handle
                .await
                .unwrap_or_else(|e| PartOutcome::Failed(format!("część przerwana: {e}")));
            if let PartOutcome::Failed(why) = &outcome {
                tracing::warn!(część = id, błąd = %why, "część biuletynu wypada");
            }
            parts.push((id, outcome));
        }
        Ok(assemble(
            &intro,
            &warsaw_hhmm(spoken),
            parts,
            length_limit(station.tx_max_duration_s),
        ))
    }
}

fn outcome(
    result: Result<Result<PartText, SourceError>, tokio::time::error::Elapsed>,
    max_chars: usize,
    timeout: Duration,
) -> PartOutcome {
    match result {
        Err(_) => PartOutcome::Failed(format!("przekroczony czas {} s", timeout.as_secs())),
        Ok(Err(e)) => PartOutcome::Failed(e.to_string()),
        Ok(Ok(PartText::Empty(why))) => PartOutcome::Empty(why.to_owned()),
        Ok(Ok(PartText::Text(text))) if text.trim().is_empty() => {
            PartOutcome::Empty("pusta treść".to_owned())
        }
        Ok(Ok(PartText::Text(text))) => {
            PartOutcome::Included(truncate_sentence(text.trim(), max_chars).0)
        }
    }
}

/// Biuletyn stacji: jedna `Pogoda` (ta sama instancja trafia do runnera
/// źródeł), `Propagacja`, `Ostrzezenia`.
pub fn build(
    cfg: &Config,
    pool: SqlitePool,
) -> Result<(Arc<Bulletin>, Arc<pogoda::Pogoda>), SourceError> {
    let pogoda = Arc::new(pogoda::Pogoda::new(&cfg.sources)?);
    let parts: Vec<Arc<dyn BulletinPart>> = vec![
        pogoda.clone(),
        Arc::new(propagacja::Propagacja::new(&cfg.sources)?),
        Arc::new(Ostrzezenia),
    ];
    Ok((Arc::new(Bulletin::new(pool, parts)), pogoda))
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
            length_limit(40),
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
        // 10 s na identyfikację i ogony, potem około 10 znaków na sekundę.
        assert_eq!(length_limit(180), 1500);
        assert_eq!(length_limit(160), 1500);
        assert_eq!(length_limit(60), 500);
        assert_eq!(length_limit(10), 0);
        assert_eq!(length_limit(5), 0);
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
