use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use aars_core::error::DbError;
use aars_core::geo::PositionRepo;
use aars_core::module::Needs;
use aars_core::settings::SettingsRepo;
use aars_core::priority::Priority;
use aars_core::queue::{EnqueueRequest, QueueRepo, QueueState};
use aars_core::sink::CancelToken;
use aars_core::source_state::{NewTracked, SourceStateRepo, TrackedItem, TrackedState};
use chrono::{DateTime, Utc};
use tokio::sync::Notify;

use crate::source::{DataSource, SourceCtx, SourceItem};

/// Tyle kolejnych udanych odpytań bez pozycji odwołuje ją (R5).
const MISSES_TO_CANCEL: i64 = 2;
const BACKOFF_CEILING: Duration = Duration::from_secs(1800);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CycleReport {
    /// `"wyłączone"`, `"brak obszaru"` albo `"brak pozycji stacji"`.
    pub skipped: Option<&'static str>,
    pub fetched: bool,
    pub announced: usize,
    pub changed: usize,
    /// Pozycje oznaczone jako odwołane (także te, które nie wymagały
    /// komunikatu, bo poprzedni jeszcze czekał w kolejce).
    pub cancelled: usize,
    pub error: Option<String>,
}

/// R13: `min(interval · 2^(n−1), max(interval, 1800 s))` po `n` porażkach.
pub fn next_delay(interval: Duration, consecutive_failures: u32) -> Duration {
    if consecutive_failures < 2 {
        return interval;
    }
    let ceiling = interval.max(BACKOFF_CEILING);
    let factor = 1u32 << (consecutive_failures - 1).min(20);
    interval.saturating_mul(factor).min(ceiling)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    Aired,
    NotAired,
    /// Element jest wzięty do nadania albo jeszcze czeka.
    Unresolved,
}

pub struct SourceRunner {
    source: Arc<dyn DataSource>,
    state: SourceStateRepo,
    queue: Arc<QueueRepo>,
    refresh: Arc<Notify>,
}

impl SourceRunner {
    pub fn new(source: Arc<dyn DataSource>, state: SourceStateRepo, queue: Arc<QueueRepo>) -> Self {
        Self { source, state, queue, refresh: Arc::new(Notify::new()) }
    }

    /// `notify_one` na tym uchwycie budzi runner natychmiast („Odśwież teraz”).
    pub fn refresh_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.refresh)
    }

    /// Jeden przebieg. Nie zwraca błędu ani nie panikuje: błędy idą do
    /// `source_state` i do logu.
    pub async fn cycle(&self, now: DateTime<Utc>) -> CycleReport {
        let mut report = CycleReport::default();
        if let Err(error) = self.run_cycle(now, &mut report).await {
            tracing::error!(source = self.source.id(), %error, "cykl źródła przerwany błędem bazy");
            report.error = Some(error.to_string());
        }
        report
    }

    pub async fn run(self, shutdown: CancelToken) {
        let id = self.source.id();
        while !shutdown.is_cancelled() {
            self.cycle(Utc::now()).await;

            let interval = match self.state.config(id).await {
                Ok(Some(cfg)) => Duration::from_secs(cfg.interval_s),
                _ => self.source.default_interval(),
            };
            let failures = match self.state.statuses().await {
                Ok(all) => all
                    .iter()
                    .find(|s| s.source_id == id)
                    .map_or(0, |s| u32::try_from(s.consecutive_failures).unwrap_or(u32::MAX)),
                Err(_) => 0,
            };
            self.wait(next_delay(interval, failures), &shutdown).await;
        }
    }

    /// Czeka w kawałkach do 1 s, żeby szybko zauważyć zamknięcie; `notify`
    /// przerywa czekanie.
    async fn wait(&self, delay: Duration, shutdown: &CancelToken) {
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            tokio::select! {
                _ = self.refresh.notified() => return,
                _ = tokio::time::sleep(remaining.min(Duration::from_secs(1))) => {}
            }
        }
    }

    /// Pozycja i ustawienia modułu na ten cykl (R20). Błąd odczytu nie
    /// przerywa cyklu: źródło dostaje `None` albo puste ustawienia.
    async fn context(
        &self,
        now: DateTime<Utc>,
        id: &str,
        cfg: &aars_core::source_state::SourceConfig,
    ) -> SourceCtx {
        let pool = self.state.pool().clone();
        let position = match PositionRepo::new(pool.clone()).load().await {
            Ok(p) => p,
            Err(error) => {
                tracing::warn!(source = id, %error, "nie można odczytać pozycji stacji");
                None
            }
        };
        let settings = match SettingsRepo::new(pool).kv_prefix(&format!("source:{id}:")).await {
            Ok(s) => s,
            Err(error) => {
                tracing::warn!(source = id, %error, "nie można odczytać ustawień modułu");
                Default::default()
            }
        };
        SourceCtx {
            now,
            area: cfg.area.clone(),
            area_text: cfg.area_text.clone(),
            position,
            settings,
            tracked_keys: HashSet::new(),
            tracked: Default::default(),
        }
    }

    async fn run_cycle(&self, now: DateTime<Utc>, report: &mut CycleReport) -> Result<(), DbError> {
        let id = self.source.id();
        let Some(mut cfg) = self.state.config(id).await? else {
            report.skipped = Some("wyłączone");
            return Ok(());
        };
        if !cfg.enabled {
            report.skipped = Some("wyłączone");
            return Ok(());
        }
        let needs = self.source.needs();
        if needs == Needs::Area && cfg.area.is_empty() {
            // Ręczny obszar pusty: źródło, które to umie, liczy go z pozycji.
            let Some(granularity) = self.source.auto_area() else {
                report.skipped = Some("brak obszaru");
                return Ok(());
            };
            let position = match PositionRepo::new(self.state.pool().clone()).load().await {
                Ok(p) => p,
                Err(error) => {
                    tracing::warn!(source = id, %error, "nie można odczytać pozycji stacji");
                    None
                }
            };
            if position.is_none() {
                report.skipped = Some("brak pozycji stacji");
                return Ok(());
            }
            let Some(entry) = aars_core::area_auto::auto_area(position.as_ref(), granularity) else {
                report.skipped = Some("pozycja poza obszarem Polski");
                return Ok(());
            };
            cfg.area = vec![entry];
            cfg.area_text = aars_core::area::area_text(&cfg.area);
        }
        // Kontekst przed porównaniem R9. Zakres nie zależy od
        // `tracked_keys`, więc najpierw zakres, potem klucze aktywnych pozycji
        // śledzonych przy tym zakresie (inny zakres R9 zaraz porzuci).
        let mut ctx = self.context(now, id, &cfg).await;
        if needs == Needs::Position && ctx.position.is_none() {
            report.skipped = Some("brak pozycji stacji");
            return Ok(());
        }
        let scope = self.source.scope(&ctx);
        let active = self.state.active_items(id).await?;
        ctx.tracked_keys = active.iter().filter(|a| a.area == scope).map(|a| a.item_key.clone()).collect();
        ctx.tracked = active
            .iter()
            .filter(|a| a.area == scope)
            .map(|a| (a.item_key.clone(), a.fingerprint.clone()))
            .collect();

        self.settle_pending_cancellations(id, now).await?;

        report.fetched = true;
        let output = match self.source.fetch(&ctx).await {
            Ok(mut output) => {
                // Pauzy może wstawiać tylko operator: tekst z sieci nie niesie
                // znaczników (inaczej zewnętrzny feed trzymałby PTT na ciszy).
                for item in &mut output.items {
                    item.text = aars_core::pause::strip_markers(&item.text);
                    item.change_text = aars_core::pause::strip_markers(&item.change_text);
                    item.cancel_text = aars_core::pause::strip_markers(&item.cancel_text);
                }
                output
            }
            Err(error) => {
                let message = error.to_string();
                tracing::warn!(source = id, error = %message, "odpytanie źródła nie powiodło się");
                self.state.record_failure(id, now, &message).await?;
                report.error = Some(message);
                return Ok(());
            }
        };

        // R9: pozycje śledzone przy innym zakresie (A2) znikają po cichu, a
        // ich czekające komunikaty z kolejki też (inaczej poprawka literówki
        // w obszarze nadałaby to samo ostrzeżenie drugi raz).
        let mut kept: Vec<TrackedItem> = Vec::new();
        for item in active {
            if item.area == scope {
                kept.push(item);
            } else {
                self.cancel_waiting(item.queue_item_id).await;
                self.state.set_state(item.id, TrackedState::Dropped).await?;
            }
        }
        // Lepka flaga „poszło na antenę”: obecny albo zastąpiony element
        // kolejki skończył jako `done`. Samo `transmitting` nie wystarcza:
        // to też „wzięty i zaraz odrzucony przez interlock” (I1).
        for old in kept.iter_mut().filter(|a| !a.aired) {
            if self.fate(old.queue_ids()).await? == Fate::Aired {
                self.state.set_aired(old.id).await?;
                old.aired = true;
            }
        }

        let out_keys: HashSet<&str> = output.items.iter().map(|i| i.key.as_str()).collect();
        let live = |a: &TrackedItem| a.valid_until.is_none_or(|until| until > now);
        let mut consumed: HashSet<i64> = HashSet::new();
        // Wiersze, które w tym cyklu zmieniły odcisk: ich stary odcisk już nie obowiązuje.
        let mut refingerprinted: HashSet<i64> = HashSet::new();
        let mut seen: HashSet<&str> = HashSet::new();
        // Odcisk -> (element kolejki, czy poszło na antenę, zastąpione
        // elementy) dla pozycji ogłoszonych albo przejętych w tym cyklu.
        let mut announced: HashMap<&str, (Option<i64>, bool, Vec<i64>)> = HashMap::new();

        for item in &output.items {
            if !seen.insert(item.key.as_str()) {
                continue;
            }
            let existing = kept.iter().find(|a| a.item_key == item.key);
            if item.valid_until.is_some_and(|until| until <= now) {
                if let Some(old) = existing {
                    self.state.set_state(old.id, TrackedState::Expired).await?;
                    consumed.insert(old.id);
                }
                continue;
            }

            if let Some(old) = existing {
                consumed.insert(old.id);
                if old.fingerprint == item.fingerprint {
                    self.state.touch(old.id, now, item.valid_until).await?;
                    announced
                        .entry(item.fingerprint.as_str())
                        .or_insert((old.queue_item_id, old.aired, old.superseded.clone()));
                } else {
                    let queued = self.announce(item, &item.change_text).await?;
                    let (aired, superseded) = self.supersede(old).await?;
                    self.track(item, &scope, Some(queued), aired, superseded.clone(), now).await?;
                    refingerprinted.insert(old.id);
                    announced.insert(item.fingerprint.as_str(), (Some(queued), aired, superseded));
                    report.changed += 1;
                }
                continue;
            }

            // Nowy klucz (R4): ten sam odcisk co jakakolwiek aktywna albo
            // ogłoszona w tym cyklu pozycja to to samo ostrzeżenie.
            if let Some((queued, aired, superseded)) = announced.get(item.fingerprint.as_str()) {
                self.track(item, &scope, *queued, *aired, superseded.clone(), now).await?;
                continue;
            }
            if let Some(old) = kept.iter().find(|a| live(a) && !refingerprinted.contains(&a.id) && a.fingerprint == item.fingerprint) {
                // Stare id zniknęło: przejmujemy po cichu, stare kończy się jako Replaced.
                if !out_keys.contains(old.item_key.as_str()) && !consumed.contains(&old.id) {
                    self.state.set_state(old.id, TrackedState::Replaced).await?;
                    consumed.insert(old.id);
                }
                self.track(item, &scope, old.queue_item_id, old.aired, old.superseded.clone(), now).await?;
                announced.insert(item.fingerprint.as_str(), (old.queue_item_id, old.aired, old.superseded.clone()));
                continue;
            }
            let replaced = kept.iter().find(|a| {
                live(a) && !consumed.contains(&a.id) && a.topic == item.topic && !out_keys.contains(a.item_key.as_str())
            });
            if let Some(old) = replaced {
                let queued = self.announce(item, &item.change_text).await?;
                let (aired, superseded) = self.supersede(old).await?;
                self.state.set_state(old.id, TrackedState::Replaced).await?;
                consumed.insert(old.id);
                self.track(item, &scope, Some(queued), aired, superseded.clone(), now).await?;
                announced.insert(item.fingerprint.as_str(), (Some(queued), aired, superseded));
                report.changed += 1;
            } else {
                let queued = self.announce(item, &item.text).await?;
                self.track(item, &scope, Some(queued), false, Vec::new(), now).await?;
                announced.insert(item.fingerprint.as_str(), (Some(queued), false, Vec::new()));
                report.announced += 1;
            }
        }

        // Pozycje, których nie ma w odpowiedzi.
        for old in kept.iter().filter(|a| !consumed.contains(&a.id) && !out_keys.contains(a.item_key.as_str())) {
            if !live(old) {
                self.state.set_state(old.id, TrackedState::Expired).await?;
            } else if !output.complete {
                continue;
            } else if output.items.iter().any(|i| {
                i.key != old.item_key
                    && i.fingerprint == old.fingerprint
                    && i.valid_until.is_none_or(|until| until > now)
            }) {
                // To samo ostrzeżenie żyje dalej pod innym id.
                self.state.set_state(old.id, TrackedState::Replaced).await?;
            } else if old.misses + 1 < MISSES_TO_CANCEL {
                self.state.set_misses(old.id, old.misses + 1).await?;
            } else {
                // R16: najpierw komunikat, potem stan. Odwołanie nadajemy
                // tylko po tym, co poszło na antenę; element kolejki zawsze
                // wycofujemy. Gdy oryginał jest właśnie wzięty, rozstrzygnie
                // to późniejszy cykl (`settle_pending_cancellations`).
                self.cancel_waiting(old.queue_item_id).await;
                let fate = if old.aired { Fate::Aired } else { self.fate(old.queue_ids()).await? };
                match fate {
                    // A3: pusty tekst odwołania to brak komunikatu.
                    Fate::Aired if old.cancel_text.is_empty() => {}
                    Fate::Aired => {
                        self.enqueue(old.cancel_text.clone(), Priority::Normal, false, old.valid_until).await?;
                    }
                    Fate::Unresolved if old.cancel_text.is_empty() => {}
                    Fate::Unresolved => self.state.set_cancel_pending(old.id, true).await?,
                    Fate::NotAired => {}
                }
                self.state.set_state(old.id, TrackedState::Cancelled).await?;
                report.cancelled += 1;
            }
        }

        self.state.record_success(id, now).await?;
        Ok(())
    }

    async fn enqueue(
        &self,
        text: String,
        priority: Priority,
        interrupting: bool,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<i64, DbError> {
        let ids = self
            .queue
            .enqueue(EnqueueRequest {
                message_id: None,
                rendered_text: text,
                priority,
                interrupting,
                not_before: None,
                expires_at,
                repeat_count: 1,
                repeat_interval: Duration::ZERO,
                created_by: None,
            })
            .await?;
        ids.first().copied().ok_or_else(|| DbError::Invalid("kolejka nie zwróciła identyfikatora".to_owned()))
    }

    async fn announce(&self, item: &SourceItem, text: &str) -> Result<i64, DbError> {
        self.enqueue(text.to_owned(), item.priority, item.interrupting, item.valid_until).await
    }

    /// Co wiadomo o nadaniu pozycji z jej elementów kolejki: `done`
    /// któregokolwiek to nadanie; wzięty albo czekający jeszcze się waży.
    async fn fate(&self, ids: impl Iterator<Item = i64>) -> Result<Fate, DbError> {
        let mut fate = Fate::NotAired;
        for queue_item_id in ids {
            match self.queue.state_of(queue_item_id).await? {
                Some(QueueState::Done) => return Ok(Fate::Aired),
                Some(QueueState::Transmitting | QueueState::Pending) => fate = Fate::Unresolved,
                _ => {}
            }
        }
        Ok(fate)
    }

    /// Wycofuje element kolejki pozycji zastępowanej zmianą. Zwraca, czy
    /// pozycja poszła na antenę, i zastąpione elementy do dalszej obserwacji:
    /// wzięty element może jeszcze skończyć jako `done` (N1) albo, gdy
    /// interlock go odrzuci, jako `cancelled` (I1).
    async fn supersede(&self, old: &TrackedItem) -> Result<(bool, Vec<i64>), DbError> {
        let mut superseded = old.superseded.clone();
        let mut aired = old.aired;
        if let Some(queue_item_id) = old.queue_item_id {
            self.cancel_waiting(Some(queue_item_id)).await;
            match self.queue.state_of(queue_item_id).await? {
                Some(QueueState::Done) => aired = true,
                Some(QueueState::Transmitting) => superseded.push(queue_item_id),
                _ => {}
            }
        }
        Ok((aired, superseded))
    }

    /// Odwołania odłożone, bo oryginał był wzięty do nadania: gdy poszedł
    /// na antenę, nadajemy odwołanie; gdy skończył inaczej, nic.
    async fn settle_pending_cancellations(&self, id: &str, now: DateTime<Utc>) -> Result<(), DbError> {
        for waiting in self.state.awaiting_cancellation(id).await? {
            match self.fate(waiting.queue_ids()).await? {
                Fate::Aired => {
                    if !waiting.cancel_text.is_empty() && waiting.valid_until.is_none_or(|until| until > now) {
                        self.enqueue(waiting.cancel_text.clone(), Priority::Normal, false, waiting.valid_until)
                            .await?;
                    }
                    self.state.set_aired(waiting.id).await?;
                    self.state.set_cancel_pending(waiting.id, false).await?;
                }
                Fate::NotAired => self.state.set_cancel_pending(waiting.id, false).await?,
                Fate::Unresolved => {
                    for queue_item_id in waiting.queue_ids() {
                        self.cancel_waiting(Some(queue_item_id)).await;
                    }
                }
            }
        }
        Ok(())
    }

    /// R7: anuluje element kolejki, który jeszcze czeka. `true`, gdy czekał.
    async fn cancel_waiting(&self, queue_item_id: Option<i64>) -> bool {
        let Some(queue_item_id) = queue_item_id else {
            return false;
        };
        match self.queue.cancel(queue_item_id).await {
            Ok(rows) => rows == 1,
            Err(error) => {
                tracing::warn!(%error, queue_item_id, "nie udało się anulować elementu kolejki");
                false
            }
        }
    }

    /// R16: wywoływane po `enqueue`; gdy zapis zawiedzie, następny cykl
    /// nada ponownie.
    async fn track(
        &self,
        item: &SourceItem,
        scope: &str,
        queue_item_id: Option<i64>,
        aired: bool,
        superseded: Vec<i64>,
        now: DateTime<Utc>,
    ) -> Result<(), DbError> {
        let new = NewTracked {
            source_id: self.source.id().to_owned(),
            item_key: item.key.clone(),
            topic: item.topic.clone(),
            fingerprint: item.fingerprint.clone(),
            area: scope.to_owned(),
            summary: item.summary.clone(),
            text: item.text.clone(),
            cancel_text: item.cancel_text.clone(),
            priority: item.priority,
            valid_from: item.valid_from,
            valid_until: item.valid_until,
            aired,
            first_seen_at: now,
            last_seen_at: now,
            queue_item_id,
            superseded,
        };
        self.state.upsert_active(&new, now).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::TimeDelta;

    use aars_core::db::test_pool;
    use aars_core::queue::QueueItem;

    use super::*;
    use crate::source::{FetchFuture, SourceError, SourceOutput};

    type SeenCtx = (Option<aars_core::geo::StationPosition>, std::collections::BTreeMap<String, String>);

    struct FakeSource {
        script: Mutex<VecDeque<Result<SourceOutput, SourceError>>>,
        calls: AtomicUsize,
        last_ctx: Mutex<Option<SeenCtx>>,
        last_keys: Mutex<Option<HashSet<String>>>,
        last_tracked: Mutex<Option<HashMap<String, String>>>,
        needs: Needs,
        /// `None`: domyślny zakres (tekst obszaru).
        scope: Mutex<Option<String>>,
    }

    impl FakeSource {
        fn new(script: Vec<Result<SourceOutput, SourceError>>) -> Arc<Self> {
            Self::needing(Needs::Area, script)
        }
        fn needing(needs: Needs, script: Vec<Result<SourceOutput, SourceError>>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into()),
                calls: AtomicUsize::new(0),
                last_ctx: Mutex::new(None),
                last_keys: Mutex::new(None),
                last_tracked: Mutex::new(None),
                needs,
                scope: Mutex::new(None),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl DataSource for FakeSource {
        fn id(&self) -> &'static str {
            "imgw-meteo"
        }
        fn default_interval(&self) -> Duration {
            Duration::from_secs(300)
        }
        fn needs(&self) -> Needs {
            self.needs
        }
        fn scope(&self, ctx: &SourceCtx) -> String {
            self.scope.lock().unwrap().clone().unwrap_or_else(|| ctx.area_text.clone())
        }
        fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_ctx.lock().unwrap() = Some((ctx.position.clone(), ctx.settings.clone()));
            *self.last_keys.lock().unwrap() = Some(ctx.tracked_keys.clone());
            *self.last_tracked.lock().unwrap() = Some(ctx.tracked.clone());
            let next = self.script.lock().ok().and_then(|mut s| s.pop_front());
            Box::pin(async move { next.unwrap_or(Err(SourceError::Http("skrypt wyczerpany".into()))) })
        }
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        at("2026-07-23T10:00:00Z")
    }

    fn warning(key: &str, fingerprint: &str) -> SourceItem {
        SourceItem {
            key: key.into(),
            topic: "Burze".into(),
            fingerprint: fingerprint.into(),
            text: format!("nowe {key} {fingerprint}"),
            change_text: format!("zmiana {key} {fingerprint}"),
            cancel_text: format!("odwołanie {key}"),
            summary: format!("Burze {key}"),
            priority: Priority::Normal,
            interrupting: false,
            valid_from: None,
            valid_until: Some(at("2099-01-01T00:00:00Z")),
        }
    }

    fn ok(items: Vec<SourceItem>) -> Result<SourceOutput, SourceError> {
        Ok(SourceOutput { items, complete: true })
    }

    fn incomplete(items: Vec<SourceItem>) -> Result<SourceOutput, SourceError> {
        Ok(SourceOutput { items, complete: false })
    }

    struct Rig {
        runner: SourceRunner,
        source: Arc<FakeSource>,
        queue: Arc<QueueRepo>,
        state: SourceStateRepo,
    }

    async fn rig(script: Vec<Result<SourceOutput, SourceError>>) -> Rig {
        rig_with(FakeSource::new(script)).await
    }

    async fn rig_with(source: Arc<FakeSource>) -> Rig {
        let pool = test_pool().await;
        let state = SourceStateRepo::new(pool.clone());
        state.save_config("imgw-meteo", "10", 300).await.unwrap();
        state.set_enabled("imgw-meteo", true).await.unwrap();
        let queue = Arc::new(QueueRepo::new(pool));
        let runner = SourceRunner::new(source.clone(), state.clone(), queue.clone());
        Rig { runner, source, queue, state }
    }

    async fn texts(queue: &QueueRepo) -> Vec<String> {
        let mut items: Vec<QueueItem> = queue.pending(100).await.unwrap();
        items.sort_by_key(|i| i.id);
        items.into_iter().map(|i| i.rendered_text).collect()
    }

    #[tokio::test]
    async fn pause_markers_in_source_text_never_reach_the_queue_or_storage() {
        let mut item = warning("a", "f1");
        item.text = "Burza[[pauza:9000]] nad miastem".into();
        item.cancel_text = "[[pauza:5]]koniec".into();
        let r = rig(vec![ok(vec![item])]).await;
        r.runner.cycle(now()).await;
        assert_eq!(texts(&r.queue).await, vec!["Burza nad miastem"]);
        let stored = r.state.active_items("imgw-meteo").await.unwrap();
        assert_eq!(stored[0].text, "Burza nad miastem");
        assert_eq!(stored[0].cancel_text, "koniec");
    }

    #[tokio::test]
    async fn a_new_item_is_queued_once_and_a_repeat_fetch_queues_nothing() {
        let mut item = warning("a", "f1");
        item.priority = Priority::High;
        let r = rig(vec![ok(vec![item.clone()]), ok(vec![item.clone()])]).await;
        let first = r.runner.cycle(now()).await;
        assert_eq!(first.announced, 1);
        let second = r.runner.cycle(now()).await;
        assert_eq!(second.announced, 0);
        let pending = r.queue.pending(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].rendered_text, "nowe a f1");
        assert_eq!(pending[0].priority, Priority::High);
        assert_eq!(pending[0].expires_at, item.valid_until);
    }

    #[tokio::test]
    async fn a_changed_fingerprint_is_announced_with_the_change_text() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![warning("a", "f2")])]).await;
        r.runner.cycle(now()).await;
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.changed, 1);
        assert_eq!(report.announced, 0);
        // Stare ogłoszenie jeszcze czekało, więc zostaje tylko zmiana (R7).
        assert_eq!(texts(&r.queue).await, vec!["zmiana a f2"]);
        // Śledzona pozycja trzyma pełną treść, nie tekst zmiany.
        let active = r.state.active_items("imgw-meteo").await.unwrap();
        assert_eq!(active[0].text, "nowe a f2");
    }

    #[tokio::test]
    async fn the_same_warning_under_a_new_key_is_adopted_silently() {
        let r = rig(vec![
            ok(vec![warning("a", "f1")]),
            ok(vec![warning("b", "f1")]),
            ok(vec![warning("b", "f1")]),
            ok(vec![warning("b", "f1")]),
        ])
        .await;
        for _ in 0..4 {
            r.runner.cycle(now()).await;
        }
        assert_eq!(texts(&r.queue).await, vec!["nowe a f1"]);
        let active = r.state.active_items("imgw-meteo").await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].item_key, "b");
    }

    #[tokio::test]
    async fn a_new_key_with_a_raised_level_is_one_change_not_new_plus_cancel() {
        let mut raised = warning("b", "f2");
        raised.priority = Priority::High;
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![raised]), ok(vec![warning("b", "f2")])]).await;
        r.runner.cycle(now()).await;
        let report = r.runner.cycle(now()).await;
        assert_eq!((report.changed, report.announced, report.cancelled), (1, 0, 0));
        r.runner.cycle(now()).await;
        assert_eq!(texts(&r.queue).await, vec!["zmiana b f2"]);
    }

    #[tokio::test]
    async fn a_missing_item_is_cancelled_after_two_successful_fetches_only() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        // Ogłoszenie poszło na antenę.
        let claimed = r.queue.claim_next().await.unwrap().unwrap();
        r.queue.mark_done(claimed.id).await.unwrap();

        r.runner.cycle(now()).await;
        assert!(texts(&r.queue).await.is_empty());
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.cancelled, 1);
        let pending = r.queue.pending(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].rendered_text, "odwołanie a");
        assert_eq!(pending[0].priority, Priority::Normal);
        assert!(!pending[0].interrupting);
        r.runner.cycle(now()).await;
        assert_eq!(r.queue.pending(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_or_incomplete_fetch_never_cancels() {
        let mut script = vec![ok(vec![warning("a", "f1")])];
        for _ in 0..3 {
            script.push(Err(SourceError::Parse("html".into())));
        }
        for _ in 0..3 {
            script.push(incomplete(vec![]));
        }
        let r = rig(script).await;
        r.runner.cycle(now()).await;
        let claimed = r.queue.claim_next().await.unwrap().unwrap();
        r.queue.mark_done(claimed.id).await.unwrap();

        for _ in 0..3 {
            let report = r.runner.cycle(now()).await;
            assert!(report.error.is_some());
        }
        let status = &r.state.statuses().await.unwrap()[0];
        assert_eq!(status.consecutive_failures, 3);
        assert!(status.last_error.as_deref().unwrap().contains("html"));
        for _ in 0..3 {
            r.runner.cycle(now()).await;
        }
        assert!(r.queue.pending(10).await.unwrap().is_empty());
        assert_eq!(r.state.active_items("imgw-meteo").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_new_item_in_an_incomplete_fetch_is_still_announced() {
        let r = rig(vec![incomplete(vec![warning("a", "f1")])]).await;
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.announced, 1);
        assert_eq!(texts(&r.queue).await, vec!["nowe a f1"]);
    }

    #[tokio::test]
    async fn an_expired_item_disappears_silently() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        let claimed = r.queue.claim_next().await.unwrap().unwrap();
        r.queue.mark_done(claimed.id).await.unwrap();
        let after = at("2100-01-01T00:00:00Z");
        r.runner.cycle(after).await;
        assert!(r.state.active_items("imgw-meteo").await.unwrap().is_empty());
        r.runner.cycle(after).await;
        assert!(r.queue.pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_item_already_expired_is_not_announced() {
        let mut stale = warning("a", "f1");
        stale.valid_until = Some(at("2026-07-23T09:00:00Z"));
        let r = rig(vec![ok(vec![stale])]).await;
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.announced, 0);
        assert!(r.queue.pending(10).await.unwrap().is_empty());
        assert!(r.state.active_items("imgw-meteo").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn changing_the_area_drops_tracked_items_without_cancelling() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        let claimed = r.queue.claim_next().await.unwrap().unwrap();
        r.queue.mark_done(claimed.id).await.unwrap();
        r.state.save_config("imgw-meteo", "14", 300).await.unwrap();
        for _ in 0..2 {
            r.runner.cycle(now()).await;
        }
        assert!(r.queue.pending(10).await.unwrap().is_empty());
        assert!(r.state.active_items("imgw-meteo").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cancellation_of_a_never_aired_warning_cancels_the_queued_one_instead() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        r.runner.cycle(now()).await;
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.cancelled, 1);
        assert!(r.queue.pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_source_disabled_between_cycles_does_not_fetch_again() {
        let r = rig(vec![ok(vec![]), ok(vec![])]).await;
        assert!(r.runner.cycle(now()).await.fetched);
        assert_eq!(r.source.calls(), 1);
        r.state.set_enabled("imgw-meteo", false).await.unwrap();
        let report = r.runner.cycle(now()).await;
        assert_eq!(r.source.calls(), 1);
        assert_eq!(report.skipped, Some("wyłączone"));
    }

    #[tokio::test]
    async fn the_context_carries_position_and_module_settings() {
        let r = rig(vec![ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        let (pos, settings) = r.source.last_ctx.lock().unwrap().clone().unwrap();
        assert!(pos.is_none());
        assert_eq!(settings["interval_s"], "300");

        let pool = r.state.pool().clone();
        let p = aars_core::geo::parse_position("", "", "JO91RS").unwrap();
        PositionRepo::new(pool).save(p.as_ref()).await.unwrap();
        r.runner.cycle(now()).await;
        let (pos, _) = r.source.last_ctx.lock().unwrap().clone().unwrap();
        assert_eq!(pos.unwrap().locator, "JO91RS");
    }

    #[tokio::test]
    async fn a_disabled_source_or_an_empty_area_does_not_fetch() {
        let r = rig(vec![ok(vec![])]).await;
        r.state.set_enabled("imgw-meteo", false).await.unwrap();
        assert_eq!(r.runner.cycle(now()).await.skipped, Some("wyłączone"));
        r.state.set_enabled("imgw-meteo", true).await.unwrap();
        r.state.save_config("imgw-meteo", "", 300).await.unwrap();
        assert_eq!(r.runner.cycle(now()).await.skipped, Some("brak obszaru"));
        assert_eq!(r.source.calls(), 0);
    }

    #[test]
    fn backoff_doubles_up_to_the_ceiling() {
        let s = Duration::from_secs;
        assert_eq!(next_delay(s(300), 0), s(300));
        assert_eq!(next_delay(s(300), 1), s(300));
        assert_eq!(next_delay(s(300), 2), s(600));
        assert_eq!(next_delay(s(300), 3), s(1200));
        assert_eq!(next_delay(s(300), 5), s(1800));
        assert_eq!(next_delay(s(300), u32::MAX), s(1800));
        assert_eq!(next_delay(s(3600), 4), s(3600));
    }

    #[tokio::test]
    async fn run_stops_on_shutdown_and_wakes_on_refresh() {
        let r = rig((0..5).map(|_| ok(vec![])).collect()).await;
        let refresh = r.runner.refresh_handle();
        let source = r.source.clone();
        let shutdown = CancelToken::new();
        let handle = tokio::spawn(r.runner.run(shutdown.clone()));

        let wait_for = |n: usize| {
            let source = source.clone();
            async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                while source.calls() < n && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                source.calls() >= n
            }
        };
        assert!(wait_for(1).await);
        refresh.notify_one();
        assert!(wait_for(2).await);

        shutdown.cancel();
        let finished = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(finished.is_ok(), "run nie zakończył się po zamknięciu");
    }

    async fn air(r: &Rig) {
        let claimed = r.queue.claim_next().await.unwrap().unwrap();
        r.queue.mark_done(claimed.id).await.unwrap();
    }

    #[tokio::test]
    async fn one_warning_under_two_ids_at_once_is_announced_once_in_either_order() {
        for (order, label) in [(["a", "b"], "old first"), (["b", "a"], "new first")] {
            let both = || ok(order.iter().map(|k| warning(k, "f1")).collect());
            let r = rig(vec![ok(vec![warning("a", "f1")]), both(), ok(vec![warning("b", "f1")]), ok(vec![warning("b", "f1")]), ok(vec![warning("b", "f1")])]).await;
            for _ in 0..5 {
                r.runner.cycle(now()).await;
            }
            assert_eq!(texts(&r.queue).await, vec!["nowe a f1"], "{label}");
            assert_eq!(r.state.active_items("imgw-meteo").await.unwrap().len(), 1, "{label}");
        }
    }

    #[tokio::test]
    async fn the_old_id_vanishing_while_the_same_warning_lives_on_is_not_a_cancellation() {
        let r = rig(vec![
            ok(vec![warning("a", "f1"), warning("b", "f1")]),
            ok(vec![warning("b", "f1")]),
            ok(vec![warning("b", "f1")]),
        ])
        .await;
        r.runner.cycle(now()).await;
        air(&r).await;
        r.runner.cycle(now()).await;
        r.runner.cycle(now()).await;
        assert!(texts(&r.queue).await.is_empty());
    }

    #[tokio::test]
    async fn a_failed_cancellation_enqueue_leaves_the_item_active_for_a_retry() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        air(&r).await;
        r.runner.cycle(now()).await;
        sqlx::query("CREATE TRIGGER no_insert BEFORE INSERT ON queue_items BEGIN SELECT RAISE(ABORT, 'zablokowane'); END")
            .execute(r.queue.pool())
            .await
            .unwrap();
        let report = r.runner.cycle(now()).await;
        assert!(report.error.is_some());
        assert_eq!(r.state.active_items("imgw-meteo").await.unwrap().len(), 1);
        sqlx::query("DROP TRIGGER no_insert").execute(r.queue.pool()).await.unwrap();
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.cancelled, 1);
        assert_eq!(texts(&r.queue).await, vec!["odwołanie a"]);
    }

    #[tokio::test]
    async fn a_pending_change_is_cancelled_and_the_cancellation_is_broadcast_when_the_original_aired() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![warning("a", "f2")]), ok(vec![]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        air(&r).await;
        r.runner.cycle(now()).await;
        for _ in 0..3 {
            r.runner.cycle(now()).await;
        }
        assert_eq!(texts(&r.queue).await, vec!["odwołanie a"]);
    }

    #[tokio::test]
    async fn the_aired_flag_survives_a_second_change() {
        let r = rig(vec![
            ok(vec![warning("a", "f1")]),
            ok(vec![warning("a", "f2")]),
            ok(vec![warning("a", "f3")]),
            ok(vec![]),
            ok(vec![]),
        ])
        .await;
        r.runner.cycle(now()).await;
        air(&r).await;
        for _ in 0..4 {
            r.runner.cycle(now()).await;
        }
        assert_eq!(texts(&r.queue).await, vec!["odwołanie a"]);
    }

    #[tokio::test]
    async fn an_original_cancelled_by_the_operator_is_not_followed_by_a_cancellation() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        let queued = r.queue.pending(10).await.unwrap()[0].id;
        r.queue.cancel(queued).await.unwrap();
        r.runner.cycle(now()).await;
        r.runner.cycle(now()).await;
        assert!(texts(&r.queue).await.is_empty());
        assert!(r.state.active_items("imgw-meteo").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_new_warning_after_the_old_one_expired_is_new_not_a_change() {
        let mut old = warning("a", "f1");
        old.valid_until = Some(at("2026-07-23T10:30:00Z"));
        let r = rig(vec![ok(vec![old]), ok(vec![warning("b", "f2")])]).await;
        r.runner.cycle(now()).await;
        let report = r.runner.cycle(at("2026-07-23T11:00:00Z")).await;
        assert_eq!((report.announced, report.changed), (1, 0));
        assert_eq!(texts(&r.queue).await.last().map(String::as_str), Some("nowe b f2"));
        let active = r.state.active_items("imgw-meteo").await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].item_key, "b");
    }

    #[tokio::test]
    async fn a_change_during_transmission_keeps_the_aired_bit() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![warning("a", "f2")]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        // Oryginał jest właśnie nadawany.
        let claimed = r.queue.claim_next().await.unwrap().unwrap();
        r.runner.cycle(now()).await;
        r.queue.mark_done(claimed.id).await.unwrap();
        r.runner.cycle(now()).await;
        r.runner.cycle(now()).await;
        assert_eq!(texts(&r.queue).await, vec!["odwołanie a"]);
    }

    async fn queue_state(r: &Rig, id: i64) -> QueueState {
        r.queue.state_of(id).await.unwrap().expect("element kolejki istnieje")
    }

    #[tokio::test]
    async fn an_original_claimed_and_held_by_the_kill_switch_is_not_aired_and_its_withdrawal_is_silent() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![warning("a", "f1")]), ok(vec![]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        // Wyłącznik: dyspozytor bierze element, sekwencer odmawia, hold.
        let original = r.queue.claim_next().await.unwrap().unwrap().id;
        r.runner.cycle(now()).await;
        r.queue.hold(original, "wstrzymane: wyłącznik", Utc::now()).await.unwrap();
        assert!(!r.state.active_items("imgw-meteo").await.unwrap()[0].aired, "wzięty i odrzucony to nie nadany");

        r.runner.cycle(now()).await;
        // Odwołanie przychodzi, gdy element znów jest wzięty, a potem wstrzymany.
        r.queue.claim_next().await.unwrap().unwrap();
        r.runner.cycle(now()).await;
        r.queue.hold(original, "wstrzymane: wyłącznik", Utc::now()).await.unwrap();
        r.runner.cycle(now()).await;

        assert!(texts(&r.queue).await.is_empty(), "żadnego odwołania niesłyszanego ostrzeżenia");
        assert_eq!(queue_state(&r, original).await, QueueState::Cancelled);
        assert!(r.queue.claim_next().await.unwrap().is_none(), "wycofany oryginał nie wychodzi po wstrzymaniu");
        assert!(r.state.active_items("imgw-meteo").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_change_while_the_original_is_claimed_and_held_airs_only_the_change_once() {
        let r = rig(vec![
            ok(vec![warning("a", "f1")]),
            ok(vec![warning("a", "f2")]),
            ok(vec![warning("a", "f2")]),
            ok(vec![warning("a", "f2")]),
        ])
        .await;
        r.runner.cycle(now()).await;
        let original = r.queue.claim_next().await.unwrap().unwrap().id;
        r.runner.cycle(now()).await;
        r.queue.hold(original, "wstrzymane: cisza nocna", Utc::now()).await.unwrap();
        r.runner.cycle(now()).await;

        assert_eq!(queue_state(&r, original).await, QueueState::Cancelled, "stary tekst nie wraca do kolejki");
        assert_eq!(texts(&r.queue).await, vec!["zmiana a f2"]);
        let change = r.queue.claim_next().await.unwrap().unwrap();
        assert_eq!(change.rendered_text, "zmiana a f2");
        r.queue.mark_done(change.id).await.unwrap();
        r.runner.cycle(now()).await;
        assert!(r.queue.claim_next().await.unwrap().is_none(), "zmiana idzie raz");
    }

    #[tokio::test]
    async fn a_withdrawal_while_the_original_is_really_on_air_is_broadcast_once_it_finishes() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![]), ok(vec![]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        r.runner.cycle(now()).await;
        let original = r.queue.claim_next().await.unwrap().unwrap().id;
        r.runner.cycle(now()).await;
        r.queue.mark_done(original).await.unwrap();
        r.runner.cycle(now()).await;
        r.runner.cycle(now()).await;
        assert_eq!(texts(&r.queue).await, vec!["odwołanie a"]);
        assert!(r.state.active_items("imgw-meteo").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fixing_a_typo_in_an_area_label_does_not_air_a_waiting_warning_twice() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![warning("a", "f1")])]).await;
        r.state.save_config("imgw-meteo", "1061 Lodz", 300).await.unwrap();
        r.runner.cycle(now()).await;
        r.state.save_config("imgw-meteo", "1061 Łódź", 300).await.unwrap();
        r.runner.cycle(now()).await;
        assert_eq!(texts(&r.queue).await, vec!["nowe a f1"], "czekające ostrzeżenie nie może pójść dwa razy");
    }

    fn silent(key: &str, fingerprint: &str) -> SourceItem {
        SourceItem { cancel_text: String::new(), ..warning(key, fingerprint) }
    }

    async fn item_state(r: &Rig, key: &str) -> String {
        sqlx::query_scalar("SELECT state FROM source_items WHERE item_key = ?")
            .bind(key)
            .fetch_one(r.state.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_position_source_without_a_position_is_skipped() {
        let r = rig_with(FakeSource::needing(Needs::Position, vec![ok(vec![warning("a", "f1")])])).await;
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.skipped, Some("brak pozycji stacji"));
        assert!(!report.fetched);
        assert_eq!(r.source.calls(), 0);
    }

    #[tokio::test]
    async fn a_source_that_needs_nothing_runs_without_an_area() {
        let r = rig_with(FakeSource::needing(Needs::Nothing, vec![ok(vec![warning("a", "f1")])])).await;
        r.state.save_config("imgw-meteo", "", 300).await.unwrap();
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.skipped, None);
        assert!(report.fetched);
        assert_eq!(report.announced, 1);
        assert_eq!(texts(&r.queue).await, vec!["nowe a f1"]);
    }

    #[tokio::test]
    async fn an_item_without_cancel_text_disappears_silently() {
        let r = rig(vec![ok(vec![silent("a", "f1")]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        air(&r).await;
        r.runner.cycle(now()).await;
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.cancelled, 1);
        assert!(texts(&r.queue).await.is_empty(), "pusty tekst odwołania to brak komunikatu");
        assert_eq!(item_state(&r, "a").await, "cancelled");
    }

    #[tokio::test]
    async fn an_unaired_item_without_cancel_text_does_not_leave_a_pending_cancellation() {
        let r = rig(vec![ok(vec![silent("a", "f1")]), ok(vec![]), ok(vec![]), ok(vec![])]).await;
        r.runner.cycle(now()).await;
        let original = r.queue.claim_next().await.unwrap().unwrap().id;
        r.runner.cycle(now()).await;
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.cancelled, 1);
        assert!(r.state.awaiting_cancellation("imgw-meteo").await.unwrap().is_empty(), "cancel_pending == false");
        r.queue.mark_done(original).await.unwrap();
        r.runner.cycle(now()).await;
        assert!(texts(&r.queue).await.is_empty());
    }

    #[tokio::test]
    async fn a_scope_change_drops_items_silently() {
        let r = rig(vec![ok(vec![warning("a", "f1")]), ok(vec![]), ok(vec![])]).await;
        *r.source.scope.lock().unwrap() = Some("x|m=3.0".to_owned());
        r.runner.cycle(now()).await;
        air(&r).await;
        assert_eq!(r.state.active_items("imgw-meteo").await.unwrap()[0].area, "x|m=3.0");
        *r.source.scope.lock().unwrap() = Some("x|m=4.0".to_owned());
        let first = r.runner.cycle(now()).await;
        let second = r.runner.cycle(now()).await;
        assert_eq!((first.cancelled, second.cancelled), (0, 0));
        assert_eq!(item_state(&r, "a").await, "dropped");
        assert!(texts(&r.queue).await.is_empty());
    }

    #[tokio::test]
    async fn two_keys_with_distinct_fingerprints_are_two_announcements() {
        let r = rig(vec![ok(vec![warning("a", "a|3.0"), warning("b", "b|3.0")])]).await;
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.announced, 2);
        assert_eq!(texts(&r.queue).await.len(), 2);
    }

    #[tokio::test]
    async fn a_position_source_with_a_position_runs_without_an_area() {
        let r = rig_with(FakeSource::needing(Needs::Position, vec![ok(vec![warning("a", "f1")])])).await;
        r.state.save_config("imgw-meteo", "", 300).await.unwrap();
        let p = aars_core::geo::parse_position("", "", "JO91RS").unwrap();
        PositionRepo::new(r.state.pool().clone()).save(p.as_ref()).await.unwrap();
        let report = r.runner.cycle(now()).await;
        assert_eq!(report.skipped, None);
        assert!(report.fetched);
        assert_eq!(report.announced, 1);
    }

    #[tokio::test]
    async fn keys_tracked_under_another_scope_are_not_tracked_keys() {
        let r = rig(vec![ok(vec![warning("k1", "f1")]), ok(vec![]), ok(vec![])]).await;
        *r.source.scope.lock().unwrap() = Some("A".to_owned());
        r.runner.cycle(now()).await;
        *r.source.scope.lock().unwrap() = Some("B".to_owned());
        r.runner.cycle(now()).await;
        assert_eq!(r.source.last_keys.lock().unwrap().clone().unwrap(), HashSet::new());
    }

    #[tokio::test]
    async fn tracked_keys_reach_the_source() {
        let r = rig(vec![ok(vec![warning("k1", "f1")]), ok(vec![warning("k1", "f1")])]).await;
        r.runner.cycle(now()).await;
        assert_eq!(r.source.last_keys.lock().unwrap().clone().unwrap(), HashSet::new());
        r.runner.cycle(now()).await;
        let keys = r.source.last_keys.lock().unwrap().clone().unwrap();
        assert_eq!(keys, HashSet::from(["k1".to_owned()]));
    }

    #[tokio::test]
    async fn tracked_fingerprints_reach_the_source() {
        let r = rig(vec![ok(vec![warning("k1", "f1")]), ok(vec![warning("k1", "f1")])]).await;
        r.runner.cycle(now()).await;
        r.runner.cycle(now()).await;
        let tracked = r.source.last_tracked.lock().unwrap().clone().unwrap();
        assert_eq!(tracked, HashMap::from([("k1".to_owned(), "f1".to_owned())]));
    }

    #[tokio::test]
    async fn an_unchanged_item_gets_its_new_valid_until() {
        let mut first = warning("k1", "f1");
        first.valid_until = Some(now() + TimeDelta::hours(1));
        let mut second = warning("k1", "f1");
        second.valid_until = Some(now() + TimeDelta::hours(3));
        let r = rig(vec![ok(vec![first]), ok(vec![second])]).await;
        r.runner.cycle(now()).await;
        let report = r.runner.cycle(now()).await;
        assert_eq!((report.announced, report.changed), (0, 0));
        let items = r.state.active_items("imgw-meteo").await.unwrap();
        assert_eq!(items[0].valid_until, Some(now() + TimeDelta::hours(3)));
    }

    /// Wynik prawdziwego `gios::to_output` dla stacji przy Rzeszowie (promień 5 km):
    /// fiks (PI) albo fiks z dodanym PA dla tej samej strefy i parametru.
    fn gios_rzeszow(pa: bool, keep_pi: bool) -> SourceOutput {
        const STATIONS: &[u8] = include_bytes!("../tests/fixtures/gios/findAll-size500-2026-11-22.json");
        const WARNINGS: &[u8] = include_bytes!("../tests/fixtures/gios/getWarnings-2026-11-22.json");
        let mut v: serde_json::Value = serde_json::from_slice(WARNINGS).unwrap();
        if pa {
            let list = v.as_array_mut().unwrap();
            let i = list.iter().position(|w| w["zone"]["zoneCode"] == "PL1801").unwrap();
            let mut alarm = list[i].clone();
            alarm["standard"]["standardTypeCode"] = "PA".into();
            if keep_pi {
                list.push(alarm);
            } else {
                list[i] = alarm;
            }
        }
        let (stations, _) = crate::gios::parse::parse_stations(STATIONS).unwrap();
        let (warnings, skipped) = crate::gios::parse::parse_warnings(&serde_json::to_vec(&v).unwrap()).unwrap();
        let ctx = SourceCtx {
            now: now(),
            area: Vec::new(),
            area_text: String::new(),
            position: Some(aars_core::geo::StationPosition { lat: 50.04, lon: 22.00, locator: "KN00".into() }),
            settings: [("radius_km".to_owned(), "5".to_owned())].into(),
            tracked_keys: Default::default(),
            tracked: Default::default(),
        };
        crate::gios::to_output(&warnings, skipped, &stations, &ctx)
    }

    #[tokio::test]
    async fn a_gios_escalation_from_pi_to_pa_is_one_change_and_no_cancellation() {
        let pi = gios_rzeszow(false, false);
        assert_eq!(pi.items.len(), 1);
        let r = rig(vec![
            Ok(pi),
            Ok(gios_rzeszow(true, true)),
            Ok(gios_rzeszow(true, false)),
            Ok(gios_rzeszow(true, false)),
        ])
        .await;
        let first = r.runner.cycle(now()).await;
        assert_eq!(first.announced, 1);
        air(&r).await;
        let mut changed = 0;
        let mut cancelled = 0;
        for _ in 0..3 {
            let report = r.runner.cycle(now()).await;
            changed += report.changed;
            cancelled += report.cancelled;
        }
        assert_eq!((changed, cancelled), (1, 0));
        let pending = texts(&r.queue).await;
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert!(pending[0].starts_with("Zmiana alertu smogowego. Alert smogowy GIOŚ."), "{}", pending[0]);
        assert!(pending[0].contains("Przekroczony poziom alarmowy."), "{}", pending[0]);
        assert!(!pending.iter().any(|t| t.contains("Koniec alertu")));
        let active = r.state.active_items("imgw-meteo").await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].item_key, "PL1801|PA|PM10");
    }
}
