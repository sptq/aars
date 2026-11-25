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
use aars_core::bulletin_layout::{self, Block, BlockKind, MAX_PAUSE_MS};
use aars_core::module::BULLETIN;
use aars_core::settings::SettingsRepo;
use aars_core::source_state::{AlertSourceHealth, SourceStateRepo, TrackedItem};
use chrono::{DateTime, NaiveTime, Timelike, Utc};
use chrono_tz::Europe::Warsaw;
use sqlx::SqlitePool;

use crate::source::SourceError;
use crate::speech::truncate_sentence;
use crate::{imgw_wodowskazy, pogoda, propagacja};

pub use ostrzezenia::Ostrzezenia;
pub use runner::{BulletinRunner, BulletinTick, cycle_fires, due_fires};

/// Wejście części: chwila nadania, pozycja stacji, ustawienia
/// `module:bulletin:*` (bez prefiksu), żywe alerty i zdrowie źródeł alertów.
pub struct PartCtx {
    pub now: DateTime<Utc>,
    pub position: Option<StationPosition>,
    pub settings: BTreeMap<String, String>,
    /// Ustawienia źródeł `source:*` bez prefiksu `source:`
    /// (`"imgw-wodowskazy:radius_km"` → `"25"`).
    pub sources: BTreeMap<String, String>,
    pub active_alerts: Vec<TrackedItem>,
    pub alert_sources: AlertSourceHealth,
    /// Identyfikatory części składanych w tym biuletynie, w kolejności układu.
    pub planned: Vec<&'static str>,
}

impl PartCtx {
    /// Ustawienia jednego źródła, klucze bez `"{id}:"`.
    pub fn source_settings(&self, source_id: &str) -> BTreeMap<String, String> {
        let prefix = format!("{source_id}:");
        self.sources
            .iter()
            .filter_map(|(k, v)| Some((k.strip_prefix(&prefix)?.to_owned(), v.clone())))
            .collect()
    }
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
    /// Każdy klocek układu: polska nazwa i wynik.
    pub parts: Vec<(&'static str, PartOutcome)>,
    pub skipped_reason: Option<&'static str>,
}

fn ends_sentence(t: &str) -> bool {
    t.ends_with(['.', '!', '?'])
}

/// Klocek po złożeniu: wynik i pauzy wokół niego.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub kind: BlockKind,
    pub outcome: PartOutcome,
    pub pause_before_ms: u32,
    pub pause_after_ms: u32,
}

impl Piece {
    pub fn new(kind: BlockKind, outcome: PartOutcome) -> Self {
        Self {
            kind,
            outcome,
            pause_before_ms: 0,
            pause_after_ms: 0,
        }
    }

    fn from_block(block: &Block, outcome: PartOutcome) -> Self {
        Self {
            kind: block.kind,
            outcome,
            pause_before_ms: block.pause_before_ms,
            pause_after_ms: block.pause_after_ms,
        }
    }
}

/// Tekst z klocków `Included`; między sąsiednimi pauza po poprzednim plus
/// przed następnym (najwyżej 10 s) jako `[[pauza:MS]]`. Pauza przed
/// pierwszym i po ostatnim klocku nic nie wnosi, więc jej nie ma.
/// Zwraca tekst i sumę pauz w ms.
fn join(pieces: &[Piece]) -> (String, u64) {
    let mut text = String::new();
    let mut total = 0u64;
    let mut prev_after: Option<u32> = None;
    for piece in pieces {
        let PartOutcome::Included(t) = &piece.outcome else {
            continue;
        };
        if let Some(after) = prev_after {
            let pause = (after + piece.pause_before_ms).min(MAX_PAUSE_MS);
            if pause > 0 {
                text.push_str(&format!(" [[pauza:{pause}]] "));
                total += u64::from(pause);
            } else {
                text.push(' ');
            }
        }
        text.push_str(t);
        if !ends_sentence(t) {
            text.push('.');
        }
        prev_after = Some(piece.pause_after_ms);
    }
    (text, total)
}

/// „Koszt” tekstu w znakach: mowa plus pauzy po 10 znaków na sekundę.
fn cost(text: &str, pause_ms: u64) -> usize {
    let pause_chars = usize::try_from(pause_ms / 100).unwrap_or(usize::MAX);
    aars_core::pause::strip_markers(text)
        .chars()
        .count()
        .saturating_add(pause_chars)
}

/// Składa biuletyn z klocków. Dopóki tekst przekracza `limit`, ostatni
/// dołączony klocek, który może wypaść (nie wstęp, ostrzeżenia ani
/// zakończenie), dostaje `DroppedForLength`. Gdy zostały same chronione,
/// tekst idzie mimo limitu — ostrzeżeń nie wolno zgubić (i same skracają się
/// do swojego limitu). Bez żadnego klocka z treścią (poza wstępem
/// i zakończeniem) biuletynu nie ma.
pub fn assemble(pieces: Vec<Piece>, limit: usize) -> Composed {
    let mut pieces = pieces;
    let mut dropped = false;
    loop {
        let has_content = pieces.iter().any(|p| {
            matches!(p.outcome, PartOutcome::Included(_))
                && !matches!(p.kind, BlockKind::Wstep | BlockKind::Zakonczenie)
        });
        if !has_content {
            let reason = if dropped {
                "limit czasu nadawania"
            } else {
                "brak danych"
            };
            return composed(pieces, None, Some(reason));
        }
        let (text, pauses) = join(&pieces);
        if cost(&text, pauses) <= limit {
            return composed(pieces, Some(text), None);
        }
        let Some(last) = pieces
            .iter_mut()
            .rev()
            .find(|p| matches!(p.outcome, PartOutcome::Included(_)) && !p.kind.is_protected())
        else {
            tracing::warn!(
                znaków = cost(&text, pauses),
                limit,
                "biuletyn dłuższy niż limit mimo usunięcia klocków; idzie w całości"
            );
            return composed(pieces, Some(text), None);
        };
        last.outcome = PartOutcome::DroppedForLength;
        dropped = true;
    }
}

fn composed(pieces: Vec<Piece>, text: Option<String>, skipped_reason: Option<&'static str>) -> Composed {
    Composed {
        text,
        parts: pieces
            .into_iter()
            .map(|p| (p.kind.label(), p.outcome))
            .collect(),
        skipped_reason,
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
pub(crate) const DEFAULT_INTERVAL_MIN: u32 = 10;

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

/// Tryb nadawania z ustawień: `Some(interwał w minutach)` w trybie
/// cyklicznym, `None` w trybie stałych pór.
pub(crate) fn cycle_interval(settings: &BTreeMap<String, String>) -> Option<u32> {
    if settings.get("mode").map(|m| m.trim()) == Some("pory") {
        return None;
    }
    Some(
        settings
            .get("interval_min")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|n| (5..=60).contains(n))
            .unwrap_or(DEFAULT_INTERVAL_MIN),
    )
}

/// Numer cyklu: minuty od północy (czas Warszawy) / interwał.
pub fn cycle_index(local: NaiveTime, interval_min: u32) -> u32 {
    (local.hour() * 60 + local.minute()) / interval_min.max(1)
}

/// Kompozytor: wspólna ścieżka nadania i podglądu. Wszystko, co zależy od
/// operatora (znak, układ, tryb, limit nadawania), czyta z bazy przy
/// każdym złożeniu.
pub struct Bulletin {
    pool: SqlitePool,
    parts: Vec<Arc<dyn BulletinPart>>,
    part_timeout: Duration,
}

/// Co zrobić z klockiem układu w tym złożeniu.
enum Plan {
    Done(PartOutcome),
    Part(Arc<dyn BulletinPart>),
}

impl Bulletin {
    /// Części wybierane po `id` z klocków układu; kolejność wektora bez znaczenia.
    pub fn new(pool: SqlitePool, parts: Vec<Arc<dyn BulletinPart>>) -> Self {
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

    /// `now` — chwila danych; `spoken` — pora wypowiadana we wstępie,
    /// od niej zależy też numer cyklu i „w godzinach”.
    pub(crate) async fn compose_at(
        &self,
        now: DateTime<Utc>,
        spoken: DateTime<Utc>,
    ) -> Result<Composed, DbError> {
        let station = SettingsRepo::new(self.pool.clone()).load().await?;
        let settings = module_settings(&self.pool).await?;
        let layout = bulletin_layout::load(&self.pool).await?;
        let local = spoken.with_timezone(&Warsaw).naive_local();
        let cycle = cycle_interval(&settings).map(|n| cycle_index(local.time(), n));

        let wants_alerts = layout.blocks.iter().any(|b| {
            b.kind == BlockKind::Ostrzezenia && b.enabled && b.when.matches(cycle, local.time())
        });
        let (active_alerts, alert_sources) = if wants_alerts {
            let repo = SourceStateRepo::new(self.pool.clone());
            (
                repo.live_alerts(now).await?,
                repo.alert_source_health(now).await?,
            )
        } else {
            (Vec::new(), AlertSourceHealth::default())
        };

        let plans: Vec<Plan> = layout
            .blocks
            .iter()
            .map(|block| {
                if !block.enabled {
                    return Plan::Done(PartOutcome::Empty("klocek wyłączony".to_owned()));
                }
                if !block.when.matches(cycle, local.time()) {
                    return Plan::Done(PartOutcome::Empty(format!(
                        "nadawany {}",
                        block.when.describe()
                    )));
                }
                let Some(id) = block.kind.part_id() else {
                    let text = bulletin_layout::render_template(&block.text, &station.callsign, local);
                    return Plan::Done(if text.trim().is_empty() {
                        PartOutcome::Empty("pusty tekst (bez znaku stacji)".to_owned())
                    } else {
                        PartOutcome::Included(text)
                    });
                };
                if block.kind == BlockKind::Ostrzezenia
                    && !block.say_when_empty
                    && active_alerts.is_empty()
                {
                    return Plan::Done(PartOutcome::Empty(
                        "brak obowiązujących ostrzeżeń".to_owned(),
                    ));
                }
                match self.parts.iter().find(|p| p.id() == id) {
                    Some(part) => Plan::Part(Arc::clone(part)),
                    None => Plan::Done(PartOutcome::Failed("część niedostępna".to_owned())),
                }
            })
            .collect();

        let position = PositionRepo::new(self.pool.clone()).load().await?;
        let sources = SettingsRepo::new(self.pool.clone())
            .kv_prefix("source:")
            .await?;
        let ctx = Arc::new(PartCtx {
            now,
            position,
            settings,
            sources,
            active_alerts,
            alert_sources,
            planned: plans
                .iter()
                .filter_map(|p| match p {
                    Plan::Part(part) => Some(part.id()),
                    Plan::Done(_) => None,
                })
                .collect(),
        });

        let timeout = self.part_timeout;
        let running: Vec<_> = plans
            .into_iter()
            .map(|plan| match plan {
                Plan::Done(outcome) => Err(outcome),
                Plan::Part(part) => {
                    let ctx = Arc::clone(&ctx);
                    let id = part.id();
                    let handle = tokio::spawn(async move {
                        let result = tokio::time::timeout(timeout, part.compose(&ctx)).await;
                        outcome(result, part.max_chars(), timeout)
                    });
                    Ok((id, handle))
                }
            })
            .collect();
        let mut pieces = Vec::with_capacity(running.len());
        for (block, run) in layout.blocks.iter().zip(running) {
            let outcome = match run {
                Err(done) => done,
                Ok((id, handle)) => {
                    let outcome = handle
                        .await
                        .unwrap_or_else(|e| PartOutcome::Failed(format!("część przerwana: {e}")));
                    if let PartOutcome::Failed(why) = &outcome {
                        tracing::warn!(część = id, błąd = %why, "część biuletynu wypada");
                    }
                    outcome
                }
            };
            pieces.push(Piece::from_block(block, outcome));
        }
        Ok(assemble(pieces, length_limit(station.tx_max_duration_s)))
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
        // Pauzy wstawia tylko składanie biuletynu, nie dane z sieci.
        Ok(Ok(PartText::Text(text))) if aars_core::pause::strip_markers(&text).trim().is_empty() => {
            PartOutcome::Empty("pusta treść".to_owned())
        }
        Ok(Ok(PartText::Text(text))) => {
            let text = aars_core::pause::strip_markers(&text);
            PartOutcome::Included(truncate_sentence(text.trim(), max_chars).0)
        }
    }
}

/// Złożony biuletyn i instancje, które dzieli z runnerem źródeł.
pub struct BuiltBulletin {
    pub bulletin: Arc<Bulletin>,
    pub pogoda: Arc<pogoda::Pogoda>,
    pub wodowskazy: Arc<imgw_wodowskazy::ImgwWodowskazy>,
}

/// Biuletyn stacji: jedna `Pogoda` (trzy części z jednej prognozy) i jedna
/// `ImgwWodowskazy` (te same instancje trafiają do runnera źródeł),
/// `Propagacja`, `Ostrzezenia`.
pub fn build(cfg: &Config, pool: SqlitePool) -> Result<BuiltBulletin, SourceError> {
    let pogoda = Arc::new(pogoda::Pogoda::new(&cfg.sources)?);
    let wodowskazy = Arc::new(imgw_wodowskazy::ImgwWodowskazy::new(
        &cfg.sources.imgw_hydro_stations_url,
    )?);
    let mut parts: Vec<Arc<dyn BulletinPart>> = pogoda.parts();
    parts.extend([
        wodowskazy.clone() as Arc<dyn BulletinPart>,
        Arc::new(propagacja::Propagacja::new(&cfg.sources)?),
        Arc::new(Ostrzezenia),
    ]);
    Ok(BuiltBulletin {
        bulletin: Arc::new(Bulletin::new(pool, parts)),
        pogoda,
        wodowskazy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use BlockKind::*;
    const WARN: BlockKind = BlockKind::Ostrzezenia;
    use PartOutcome::*;

    fn inc(kind: BlockKind, t: &str) -> Piece {
        Piece::new(kind, Included(t.to_owned()))
    }

    fn paused(kind: BlockKind, t: &str, before: u32, after: u32) -> Piece {
        Piece {
            pause_before_ms: before,
            pause_after_ms: after,
            ..inc(kind, t)
        }
    }

    #[test]
    fn blocks_join_in_layout_order_with_pauses_between() {
        let c = assemble(
            vec![
                paused(Wstep, "Tu SP9XYZ.", 300, 700),
                paused(WARN, "Brak ostrzeżeń", 200, 0),
                Piece::new(Propagacja, Empty("brak danych propagacji".into())),
                inc(PogodaTeraz, "Ciepło."),
                paused(Zakonczenie, "Koniec.", 0, 900),
            ],
            1500,
        );
        // Pauza przed pierwszym i po ostatnim nie trafia do tekstu; sąsiednie się sumują.
        assert_eq!(
            c.text.as_deref(),
            Some("Tu SP9XYZ. [[pauza:900]] Brak ostrzeżeń. Ciepło. Koniec.")
        );
        let labels: Vec<_> = c.parts.iter().map(|(l, _)| *l).collect();
        assert_eq!(
            labels,
            ["Wstęp", "Obowiązujące ostrzeżenia", "Warunki propagacji KF", "Pogoda bieżąca", "Zakończenie"]
        );
    }

    #[test]
    fn adjacent_pauses_are_capped_at_ten_seconds() {
        let c = assemble(
            vec![paused(Tekst, "A.", 0, 8000), paused(Wody, "B.", 7000, 0)],
            1500,
        );
        assert_eq!(c.text.as_deref(), Some("A. [[pauza:10000]] B."));
        let segments = aars_core::pause::split(c.text.as_deref().unwrap());
        assert_eq!(segments.len(), 3, "{segments:?}");
    }

    #[test]
    fn pauses_skip_over_left_out_blocks() {
        let c = assemble(
            vec![
                paused(Wstep, "A.", 0, 700),
                Piece { pause_before_ms: 5000, ..Piece::new(Wody, Empty("x".into())) },
                inc(Prognoza, "B."),
            ],
            1500,
        );
        assert_eq!(c.text.as_deref(), Some("A. [[pauza:700]] B."));
    }

    #[test]
    fn a_part_with_a_pause_marker_is_included_without_it() {
        let got = outcome(
            Ok(Ok(PartText::Text("Wiatr[[pauza:9000]] słaby.".into()))),
            1000,
            Duration::from_secs(1),
        );
        assert_eq!(got, Included("Wiatr słaby.".into()));
        let c = assemble(vec![Piece::new(Wody, got)], 1500);
        assert!(!c.text.unwrap().contains("pauza"));
    }

    fn long(n: usize) -> String {
        format!("{}.", "a".repeat(n - 1))
    }

    #[test]
    fn blocks_drop_from_the_end_but_warnings_intro_and_end_stay() {
        let c = assemble(
            vec![
                inc(Wstep, &long(50)),
                inc(WARN, &long(150)),
                inc(PogodaTeraz, &long(60)),
                inc(Propagacja, &long(200)),
                inc(Tekst, &long(100)),
                inc(Zakonczenie, &long(20)),
            ],
            length_limit(40),
        );
        assert!(matches!(c.parts[0].1, Included(_)));
        assert!(matches!(c.parts[1].1, Included(_)));
        assert!(matches!(c.parts[2].1, Included(_)));
        assert_eq!(c.parts[3].1, DroppedForLength);
        assert_eq!(c.parts[4].1, DroppedForLength);
        assert!(matches!(c.parts[5].1, Included(_)));
        assert_eq!(c.parts[4].1.describe(), "pominięta — limit czasu nadawania");
        let text = c.text.unwrap();
        assert!(text.chars().count() <= 300, "{}", text.chars().count());

        // Same chronione ponad limit: idą w całości, ostrzeżeń się nie gubi.
        let c = assemble(
            vec![inc(Wstep, &long(50)), inc(WARN, &long(400)), inc(Wody, &long(10))],
            length_limit(40),
        );
        assert_eq!(c.parts[2].1, DroppedForLength);
        assert!(c.text.unwrap().contains(&long(400)));
    }

    #[test]
    fn pauses_count_towards_the_length_limit() {
        // 2 × 99 znaków + spacja = 199; pauza 10 s to 100 znaków → 299 ≤ 300.
        let pieces = |ms| vec![paused(WARN, &long(99), 0, ms), inc(Wody, &long(99))];
        assert!(matches!(assemble(pieces(10_000), 300).parts[1].1, Included(_)));
        assert_eq!(assemble(pieces(10_000), 298).parts[1].1, DroppedForLength);
        assert!(matches!(assemble(pieces(0), 199).parts[1].1, Included(_)));
    }

    #[test]
    fn nothing_but_intro_and_end_is_no_bulletin() {
        let c = assemble(
            vec![
                inc(Wstep, "Tu X."),
                Piece::new(Wody, Empty("x".into())),
                Piece::new(Propagacja, Failed("y".into())),
                inc(Zakonczenie, "Koniec."),
            ],
            1500,
        );
        assert_eq!((c.text, c.skipped_reason), (None, Some("brak danych")));
        let c = assemble(vec![inc(Wstep, "A."), inc(Wody, &long(200))], 50);
        assert_eq!(
            (c.text, c.skipped_reason),
            (None, Some("limit czasu nadawania"))
        );
    }

    #[test]
    fn cycle_numbers_count_from_midnight() {
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert_eq!(cycle_index(t(0, 0), 10), 0);
        assert_eq!(cycle_index(t(0, 9), 10), 0);
        assert_eq!(cycle_index(t(0, 30), 10), 3);
        assert_eq!(cycle_index(t(23, 50), 10), 143);
        assert_eq!(cycle_index(t(1, 0), 15), 4);
        let s = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
        };
        assert_eq!(cycle_interval(&s(&[])), Some(10));
        assert_eq!(cycle_interval(&s(&[("mode", "cykl"), ("interval_min", "15")])), Some(15));
        assert_eq!(cycle_interval(&s(&[("mode", "cykl"), ("interval_min", "3")])), Some(10));
        assert_eq!(cycle_interval(&s(&[("mode", "pory"), ("interval_min", "15")])), None);
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
    fn source_settings_strip_the_source_id() {
        let ctx = PartCtx {
            now: Utc::now(),
            position: None,
            settings: BTreeMap::new(),
            sources: [
                ("imgw-wodowskazy:radius_km", "30"),
                ("imgw-wodowskazy:stations", "1"),
                ("imgw-wodowskazy-x:radius_km", "9"),
                ("imgw-meteo:area", "18"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
            active_alerts: Vec::new(),
            alert_sources: AlertSourceHealth::default(),
            planned: Vec::new(),
        };
        let s = ctx.source_settings("imgw-wodowskazy");
        assert_eq!(s.len(), 2);
        assert_eq!(s["radius_km"], "30");
        assert_eq!(s["stations"], "1");
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
