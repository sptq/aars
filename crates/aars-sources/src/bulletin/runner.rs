//! `BulletinRunner`: cykl co N minut albo stałe pory lokalne, spóźnienia,
//! cisza nocna, idempotencja przez `bulletin_fires`. Tylko wstawia do
//! kolejki — nigdy nie dotyka sekwencera ani PTT.

use std::sync::Arc;
use std::time::Duration;

use aars_core::bulletin::BulletinFireRepo;
use aars_core::error::DbError;
use aars_core::interlock::quiet_refuses;
use aars_core::module::{BULLETIN, ModuleRepo};
use aars_core::priority::Priority;
use aars_core::queue::{EnqueueRequest, QueueRepo};
use aars_core::settings::SettingsRepo;
use aars_core::sink::CancelToken;
use chrono::offset::LocalResult;
use chrono::{DateTime, Local, NaiveTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Europe::Warsaw;

use aars_core::bulletin_layout::{cycle_interval, cycle_minutes};

use crate::bulletin::{Bulletin, module_settings};
use crate::speech::warsaw_to_utc;

const TICK: Duration = Duration::from_secs(10);
/// Element kolejki żyje tyle po porze, więc większa tolerancja nie ma sensu (B3).
/// W trybie cyklicznym krócej, gdy cykl jest krótszy: biuletyn nie zalega
/// do następnego.
const EXPIRY: TimeDelta = TimeDelta::minutes(15);
/// Wpisy `bulletin_fires` starsze niż to są usuwane.
const KEEP_FIRES: TimeDelta = TimeDelta::days(7);
const MAX_GRACE_S: u64 = 900;
/// Tik co 10 s i składanie trwają; mniejsza tolerancja zabiłaby każdą porę.
const MIN_GRACE_S: u64 = 60;
const DEFAULT_TIMES: &str = "07:00,19:00";

const LATE: &str = "spóźnione ponad misfire_grace_s";
const SUPERSEDED: &str = "zastąpione nowszą porą";
const QUIET: &str = "cisza nocna";

#[derive(Debug, Default, PartialEq)]
pub struct BulletinTick {
    pub disabled: bool,
    pub queued: Option<i64>,
    pub skipped: Vec<(DateTime<Utc>, String)>,
}

/// Chwile UTC pór dla wczoraj i dziś (data lokalna `now`), tylko te, które
/// już minęły, rosnąco. Pora nieistniejąca przy zmianie czasu wypada,
/// podwójna liczy się raz (pierwsza).
pub fn due_fires(times: &[NaiveTime], now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let today = now.with_timezone(&Warsaw).date_naive();
    let days = [today.pred_opt(), Some(today)];
    let mut out: Vec<DateTime<Utc>> = days
        .into_iter()
        .flatten()
        .flat_map(|day| times.iter().map(move |t| day.and_time(*t)))
        .filter_map(warsaw_to_utc)
        .filter(|at| *at <= now)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Chwile cyklu: wielokrotności `interval_min` minut od północy czasu
/// Warszawy (wczoraj i dziś), z ostatnich 24 h, tylko minione, rosnąco.
/// Chwila nieistniejąca przy zmianie czasu wypada; godzina powtórzona
/// jesienią daje obie chwile (cykl biegnie dalej co N minut).
pub fn cycle_fires(interval_min: u32, now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let today = now.with_timezone(&Warsaw).date_naive();
    let from = now - TimeDelta::hours(24);
    let mut out = Vec::new();
    for day in [today.pred_opt(), Some(today)].into_iter().flatten() {
        for minute in cycle_minutes(interval_min) {
            let Some(naive) = day.and_hms_opt(minute / 60, minute % 60, 0) else {
                break;
            };
            match Warsaw.from_local_datetime(&naive) {
                LocalResult::Single(d) => out.push(d.with_timezone(&Utc)),
                LocalResult::Ambiguous(a, b) => {
                    out.push(a.with_timezone(&Utc));
                    out.push(b.with_timezone(&Utc));
                }
                LocalResult::None => {}
            }
        }
    }
    out.retain(|at| *at > from && *at <= now);
    out.sort_unstable();
    out.dedup();
    out
}

fn parse_times(text: &str) -> Option<Vec<NaiveTime>> {
    let times: Option<Vec<NaiveTime>> = text
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| NaiveTime::parse_from_str(v, "%H:%M").ok())
        .collect();
    times.filter(|t| !t.is_empty())
}

/// Najlepsza próba zapisania powodu przy porze, która i tak już przepadła;
/// błąd zapisu idzie tylko do dziennika.
async fn mark_skipped(fires: &BulletinFireRepo, fire_at: DateTime<Utc>, reason: &str) {
    if let Err(e) = fires.set_skipped(fire_at, reason).await {
        tracing::warn!(pora = %fire_at, powód = reason, błąd = %e, "nie zapisano powodu pominięcia biuletynu");
    }
}

pub struct BulletinRunner {
    bulletin: Arc<Bulletin>,
    queue: Arc<QueueRepo>,
}

impl BulletinRunner {
    pub fn new(bulletin: Arc<Bulletin>, queue: Arc<QueueRepo>) -> Self {
        Self { bulletin, queue }
    }

    /// Jeden przebieg; wszystko czytane z bazy przy każdym wywołaniu.
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<BulletinTick, DbError> {
        let pool = self.bulletin.pool().clone();
        if !ModuleRepo::new(pool.clone()).is_enabled(&BULLETIN).await? {
            return Ok(BulletinTick {
                disabled: true,
                ..BulletinTick::default()
            });
        }
        let settings = module_settings(&pool).await?;
        let interval = cycle_interval(&settings);
        let (due, expiry) = match interval {
            Some(n) => (
                cycle_fires(n, now),
                EXPIRY.min(TimeDelta::minutes(i64::from(n))),
            ),
            None => {
                let times = settings
                    .get("times")
                    .and_then(|t| parse_times(t))
                    .or_else(|| parse_times(DEFAULT_TIMES))
                    .unwrap_or_default();
                (due_fires(&times, now), EXPIRY)
            }
        };
        let station = SettingsRepo::new(pool.clone()).load().await?;
        let grace_s = station.misfire_grace_s.clamp(MIN_GRACE_S, MAX_GRACE_S);
        let grace = TimeDelta::seconds(i64::try_from(grace_s).unwrap_or(0));
        let fires = BulletinFireRepo::new(pool);

        let mut tick = BulletinTick::default();
        // Zajęte pory pomijamy bez zapisu: w trybie cyklicznym to do 144
        // chwil na dobę, sprawdzanych co 10 s.
        let claimed = match due.first() {
            Some(&first) => fires.claimed_since(first).await?,
            None => Vec::new(),
        };
        let (late, fitting): (Vec<_>, Vec<_>) = due.into_iter().partition(|at| now - *at > grace);
        for at in late.into_iter().filter(|at| !claimed.contains(at)) {
            if fires.claim(at, Some(LATE)).await? {
                tick.skipped.push((at, LATE.to_owned()));
            }
        }
        let Some((&fire_at, older)) = fitting.split_last() else {
            return Ok(tick);
        };
        for &at in older.iter().filter(|at| !claimed.contains(at)) {
            if fires.claim(at, Some(SUPERSEDED)).await? {
                tick.skipped.push((at, SUPERSEDED.to_owned()));
            }
        }
        if !fires.claim(fire_at, None).await? {
            return Ok(tick);
        }
        // Raz na nową porę, nadaną czy pominiętą (cisza całą dobę też ma granicę).
        if let Err(e) = fires.prune_before(now - KEEP_FIRES).await {
            tracing::warn!(błąd = %e, "nie usunięto starych wpisów biuletynu");
        }

        let local = now.with_timezone(&Local).time();
        if quiet_refuses(
            station.quiet_now(local),
            station.quiet_min_priority,
            Priority::Normal,
        ) {
            fires.set_skipped(fire_at, QUIET).await?;
            tick.skipped.push((fire_at, QUIET.to_owned()));
            tracing::info!(pora = %fire_at, "biuletyn pominięty — cisza nocna");
            return Ok(tick);
        }

        let composed = match self.bulletin.compose_at(now, fire_at).await {
            Ok(c) => c,
            Err(e) => {
                // Pora jest już zajęta; zostawiamy ślad, dlaczego nic nie poszło.
                mark_skipped(&fires, fire_at, &format!("błąd składania: {e}")).await;
                return Err(e);
            }
        };
        let Some(text) = composed.text else {
            let reason = composed.skipped_reason.unwrap_or("brak danych");
            fires.set_skipped(fire_at, reason).await?;
            tick.skipped.push((fire_at, reason.to_owned()));
            tracing::info!(pora = %fire_at, powód = reason, "biuletyn pominięty");
            return Ok(tick);
        };
        let enqueued = self
            .queue
            .enqueue(EnqueueRequest {
                message_id: None,
                rendered_text: text,
                priority: Priority::Normal,
                interrupting: false,
                not_before: Some(now),
                expires_at: Some(fire_at + expiry),
                repeat_count: 1,
                repeat_interval: Duration::ZERO,
                created_by: None,
            })
            .await;
        let ids = match enqueued {
            Ok(ids) => ids,
            Err(e) => {
                mark_skipped(&fires, fire_at, &format!("błąd kolejki: {e}")).await;
                return Err(e);
            }
        };
        let Some(&id) = ids.first() else {
            const NO_ITEM: &str = "kolejka nie zwróciła elementu";
            mark_skipped(&fires, fire_at, NO_ITEM).await;
            tick.skipped.push((fire_at, NO_ITEM.to_owned()));
            return Ok(tick);
        };
        fires.set_queued(fire_at, id).await?;
        tick.queued = Some(id);
        tracing::info!(pora = %fire_at, element = id, "biuletyn w kolejce");
        Ok(tick)
    }

    /// Tik co 10 s; błąd tiku idzie do logu, pętla trwa.
    pub async fn run(self, shutdown: CancelToken) {
        while !shutdown.is_cancelled() {
            if let Err(e) = self.tick(Utc::now()).await {
                tracing::error!(błąd = %e, "tik biuletynu nieudany");
            }
            let deadline = tokio::time::Instant::now() + TICK;
            loop {
                if shutdown.is_cancelled() {
                    return;
                }
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                tokio::time::sleep(remaining.min(Duration::from_secs(1))).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bulletin::{BulletinPart, PartCtx, PartFuture, PartOutcome, PartText};
    use crate::source::SourceError;
    use aars_core::db::test_pool;
    use aars_core::module::ModuleRegistry;
    use aars_core::queue::QueueItem;
    use chrono::NaiveDateTime;
    use sqlx::SqlitePool;

    struct Fixed(&'static str, PartText);
    struct Failing(&'static str);
    struct Slow(&'static str, Duration);

    impl BulletinPart for Fixed {
        fn id(&self) -> &'static str {
            self.0
        }
        fn max_chars(&self) -> usize {
            500
        }
        fn compose<'a>(&'a self, _ctx: &'a PartCtx) -> PartFuture<'a> {
            Box::pin(async move { Ok(self.1.clone()) })
        }
    }

    impl BulletinPart for Failing {
        fn id(&self) -> &'static str {
            self.0
        }
        fn max_chars(&self) -> usize {
            500
        }
        fn compose<'a>(&'a self, _ctx: &'a PartCtx) -> PartFuture<'a> {
            Box::pin(async move { Err(SourceError::Parse("zepsute".into())) })
        }
    }

    impl BulletinPart for Slow {
        fn id(&self) -> &'static str {
            self.0
        }
        fn max_chars(&self) -> usize {
            500
        }
        fn compose<'a>(&'a self, _ctx: &'a PartCtx) -> PartFuture<'a> {
            Box::pin(async move {
                tokio::time::sleep(self.1).await;
                Ok(PartText::Text("Za późno.".into()))
            })
        }
    }

    fn text(id: &'static str, t: &str) -> Arc<dyn BulletinPart> {
        Arc::new(Fixed(id, PartText::Text(t.to_owned())))
    }

    fn default_parts() -> Vec<Arc<dyn BulletinPart>> {
        vec![
            text("ostrzezenia", "Brak obowiązujących ostrzeżeń."),
            text("pogoda_teraz", "Pogoda A."),
            text("propagacja", "Propagacja B."),
        ]
    }

    /// Wstęp domyślnego układu dla SP9XYZ o danej porze (23.11.2026 to poniedziałek).
    fn intro(hm: &str) -> String {
        format!(
            "Tu automatyczna stacja informacyjna SP9XYZ. Poniedziałek, 23 listopada, godzina {hm}. [[pauza:700]] "
        )
    }

    const END: &str = "Tu SP9XYZ. Koniec komunikatu.";

    /// Czas lokalny Warszawy → UTC.
    fn t(local: &str) -> DateTime<Utc> {
        let naive = NaiveDateTime::parse_from_str(local, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(local, "%Y-%m-%d %H:%M"))
            .unwrap();
        warsaw_to_utc(naive).unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn hm(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, "%H:%M").unwrap()
    }

    struct Station {
        pool: SqlitePool,
        bulletin: Arc<Bulletin>,
        queue: Arc<QueueRepo>,
    }

    impl Station {
        fn runner(&self) -> BulletinRunner {
            BulletinRunner::new(self.bulletin.clone(), self.queue.clone())
        }

        async fn put(&self, key: &str, value: &str) {
            SettingsRepo::new(self.pool.clone())
                .kv_put(key, value)
                .await
                .unwrap();
        }

        async fn queued(&self) -> Vec<QueueItem> {
            self.queue.pending(100).await.unwrap()
        }

        async fn reason(&self, at: DateTime<Utc>) -> Option<String> {
            BulletinFireRepo::new(self.pool.clone())
                .get(at)
                .await
                .unwrap()
                .and_then(|f| f.skipped_reason)
        }
    }

    async fn station_with(parts: Vec<Arc<dyn BulletinPart>>, timeout: Option<Duration>) -> Station {
        let pool = test_pool().await;
        let modules = ModuleRepo::new(pool.clone());
        modules
            .register(&ModuleRegistry::new(vec![BULLETIN]))
            .await
            .unwrap();
        modules.set_enabled(&BULLETIN, true).await.unwrap();
        let mut bulletin = Bulletin::new(pool.clone(), parts);
        if let Some(t) = timeout {
            bulletin = bulletin.with_part_timeout(t);
        }
        let s = Station {
            bulletin: Arc::new(bulletin),
            queue: Arc::new(QueueRepo::new(pool.clone())),
            pool,
        };
        s.put("callsign", "SP9XYZ").await;
        s.put("module:bulletin:mode", "pory").await;
        s.put("module:bulletin:times", "07:00,19:00").await;
        s.put("misfire_grace_s", "600").await;
        s
    }

    async fn station() -> Station {
        station_with(default_parts(), None).await
    }

    #[test]
    fn due_fires_cover_yesterday_and_dst() {
        assert_eq!(
            due_fires(&[hm("23:59")], t("2026-11-24 00:00:30")),
            [utc("2026-11-23T22:59:00Z")]
        );
        assert_eq!(
            due_fires(&[hm("02:30")], t("2026-03-29 12:00")),
            [utc("2026-03-28T01:30:00Z")]
        );
        let autumn = due_fires(&[hm("02:30")], t("2026-10-25 12:00"));
        assert!(autumn.contains(&utc("2026-10-25T00:30:00Z")), "{autumn:?}");
        assert!(!autumn.contains(&utc("2026-10-25T01:30:00Z")), "{autumn:?}");
        // Tylko minione, rosnąco.
        assert_eq!(
            due_fires(&[hm("19:00"), hm("07:00")], t("2026-11-23 07:00")),
            [
                t("2026-11-22 07:00"),
                t("2026-11-22 19:00"),
                t("2026-11-23 07:00")
            ]
        );
    }

    #[tokio::test]
    async fn a_due_time_fires_once_even_after_a_restart() {
        let s = station().await;
        let tick = s.runner().tick(t("2026-11-23 07:00:05")).await.unwrap();
        let id = tick.queued.expect("biuletyn w kolejce");
        let items = s.queued().await;
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.id, id);
        assert!(
            item.rendered_text.starts_with(&intro("7:00")),
            "{}",
            item.rendered_text
        );
        assert!(item.rendered_text.ends_with(END), "{}", item.rendered_text);
        assert_eq!(item.priority, Priority::Normal);
        assert!(!item.interrupting);
        assert_eq!(item.expires_at, Some(utc("2026-11-23T06:15:00Z")));
        assert_eq!(
            BulletinFireRepo::new(s.pool.clone())
                .get(t("2026-11-23 07:00"))
                .await
                .unwrap()
                .unwrap()
                .queue_item_id,
            Some(id)
        );

        let again = s.runner().tick(t("2026-11-23 07:00:15")).await.unwrap();
        assert_eq!(again.queued, None);
        let restarted = BulletinRunner::new(s.bulletin.clone(), s.queue.clone());
        assert_eq!(
            restarted
                .tick(t("2026-11-23 07:00:25"))
                .await
                .unwrap()
                .queued,
            None
        );
        assert_eq!(s.queued().await.len(), 1);
    }

    #[tokio::test]
    async fn a_disabled_bulletin_records_nothing() {
        let s = station().await;
        ModuleRepo::new(s.pool.clone())
            .set_enabled(&BULLETIN, false)
            .await
            .unwrap();
        let tick = s.runner().tick(t("2026-11-23 07:00:05")).await.unwrap();
        assert_eq!(
            tick,
            BulletinTick {
                disabled: true,
                ..BulletinTick::default()
            }
        );
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bulletin_fires")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(s.queued().await.is_empty());
    }

    #[tokio::test]
    async fn a_late_time_is_skipped_not_aired() {
        let s = station().await;
        let tick = s.runner().tick(t("2026-11-23 07:11")).await.unwrap();
        assert_eq!(tick.queued, None);
        assert!(tick.skipped.contains(&(
            t("2026-11-23 07:00"),
            "spóźnione ponad misfire_grace_s".to_owned()
        )));
        assert!(s.queued().await.is_empty());

        // B3: tolerancja nie przekracza 900 s, choćby operator dał więcej.
        let s = station().await;
        s.put("misfire_grace_s", "3600").await;
        let tick = s.runner().tick(t("2026-11-23 07:16")).await.unwrap();
        assert_eq!(tick.queued, None);
        assert_eq!(
            s.reason(t("2026-11-23 07:00")).await.as_deref(),
            Some("spóźnione ponad misfire_grace_s")
        );
        assert!(s.queued().await.is_empty());
    }

    #[tokio::test]
    async fn a_tiny_grace_still_airs_a_bulletin_a_few_seconds_late() {
        // Tolerancja poniżej 60 s zabiłaby biuletyn przy tiku co 10 s.
        let s = station().await;
        s.put("misfire_grace_s", "0").await;
        let tick = s.runner().tick(t("2026-11-23 07:00:15")).await.unwrap();
        assert!(tick.queued.is_some(), "{tick:?}");
        assert_eq!(s.queued().await.len(), 1);
    }

    #[tokio::test]
    async fn a_queue_error_leaves_a_reason_on_the_fire() {
        let s = station().await;
        sqlx::query(
            "CREATE TRIGGER zepsuta_kolejka BEFORE INSERT ON queue_items
             BEGIN SELECT RAISE(ABORT, 'zepsuta kolejka'); END",
        )
        .execute(&s.pool)
        .await
        .unwrap();
        let fire = t("2026-11-23 07:00");
        assert!(s.runner().tick(t("2026-11-23 07:00:05")).await.is_err());
        let reason = s.reason(fire).await.expect("powód przy porze");
        assert!(reason.starts_with("błąd kolejki: "), "{reason}");
        assert!(reason.contains("zepsuta kolejka"), "{reason}");
        // Pora zajęta: kolejny tik nie próbuje drugi raz.
        sqlx::query("DROP TRIGGER zepsuta_kolejka")
            .execute(&s.pool)
            .await
            .unwrap();
        let again = s.runner().tick(t("2026-11-23 07:00:15")).await.unwrap();
        assert_eq!(again.queued, None);
        assert!(s.queued().await.is_empty());
    }

    #[tokio::test]
    async fn no_avalanche_after_downtime() {
        let s = station().await;
        let tick = s.runner().tick(t("2026-11-23 20:30")).await.unwrap();
        assert_eq!(tick.queued, None);
        let late = "spóźnione ponad misfire_grace_s".to_owned();
        assert_eq!(
            tick.skipped,
            [
                (t("2026-11-22 07:00"), late.clone()),
                (t("2026-11-22 19:00"), late.clone()),
                (t("2026-11-23 07:00"), late.clone()),
                (t("2026-11-23 19:00"), late),
            ]
        );
        assert!(s.queued().await.is_empty());
    }

    #[tokio::test]
    async fn adding_a_time_just_after_it_fires_once() {
        let s = station().await;
        s.put("module:bulletin:times", "19:00").await;
        let tick = s.runner().tick(t("2026-11-23 07:02")).await.unwrap();
        assert_eq!(tick.queued, None);
        assert!(s.queued().await.is_empty());

        s.put("module:bulletin:times", "07:00,19:00").await;
        let tick = s.runner().tick(t("2026-11-23 07:03")).await.unwrap();
        assert!(tick.queued.is_some());

        assert_eq!(
            s.runner().tick(t("2026-11-23 07:04")).await.unwrap().queued,
            None
        );
        let restarted = BulletinRunner::new(s.bulletin.clone(), s.queue.clone());
        assert_eq!(
            restarted.tick(t("2026-11-23 07:05")).await.unwrap().queued,
            None
        );
        assert_eq!(s.queued().await.len(), 1);
    }

    #[tokio::test]
    async fn only_the_newest_due_time_is_composed() {
        let s = station().await;
        s.put("module:bulletin:times", "07:00,07:05").await;
        let tick = s.runner().tick(t("2026-11-23 07:06")).await.unwrap();
        assert!(tick.queued.is_some());
        let items = s.queued().await;
        assert_eq!(items.len(), 1);
        assert!(
            items[0].rendered_text.contains("godzina 7:05."),
            "{}",
            items[0].rendered_text
        );
        assert_eq!(
            s.reason(t("2026-11-23 07:00")).await.as_deref(),
            Some("zastąpione nowszą porą")
        );
    }

    fn local_hhmm(at: DateTime<Utc>) -> String {
        at.with_timezone(&Local).format("%H:%M").to_string()
    }

    #[tokio::test]
    async fn quiet_hours_skip_a_normal_bulletin() {
        let now = t("2026-11-23 07:00:05");
        let window = format!(
            "{}-{}",
            local_hhmm(now - TimeDelta::hours(1)),
            local_hhmm(now + TimeDelta::hours(1))
        );
        let s = station().await;
        s.put("quiet_hours", &window).await;
        s.put("quiet_min_priority", &(Priority::High as i64).to_string())
            .await;
        let tick = s.runner().tick(now).await.unwrap();
        assert_eq!(tick.queued, None);
        // Wcześniejsze wpisy to wczorajsze pory spóźnione przy pierwszym tiku.
        assert_eq!(
            tick.skipped.last(),
            Some(&(t("2026-11-23 07:00"), "cisza nocna".to_owned()))
        );
        assert!(s.queued().await.is_empty());

        let s = station().await;
        s.put("quiet_hours", &window).await;
        s.put("quiet_min_priority", &(Priority::Normal as i64).to_string())
            .await;
        assert!(s.runner().tick(now).await.unwrap().queued.is_some());
    }

    #[tokio::test]
    async fn a_quiet_window_ending_at_the_fire_time_does_not_block_it() {
        let fire = t("2026-11-23 07:00");
        let window = format!(
            "{}-{}",
            local_hhmm(fire - TimeDelta::hours(9)),
            local_hhmm(fire)
        );
        let s = station().await;
        s.put("quiet_hours", &window).await;
        s.put("quiet_min_priority", &(Priority::High as i64).to_string())
            .await;
        let tick = s.runner().tick(fire + TimeDelta::seconds(1)).await.unwrap();
        assert!(tick.queued.is_some(), "{tick:?}");
    }

    #[tokio::test]
    async fn an_empty_bulletin_is_not_aired() {
        let empty = |id| -> Arc<dyn BulletinPart> { Arc::new(Fixed(id, PartText::Empty("nic"))) };
        let s = station_with(
            vec![empty("pogoda_teraz"), empty("propagacja"), empty("ostrzezenia")],
            None,
        )
        .await;
        let tick = s.runner().tick(t("2026-11-23 07:00:05")).await.unwrap();
        // Wcześniejsze wpisy to wczorajsze pory spóźnione przy pierwszym tiku.
        assert_eq!(
            tick.skipped.last(),
            Some(&(t("2026-11-23 07:00"), "brak danych".to_owned()))
        );
        assert!(s.queued().await.is_empty());

        let s = station_with(
            vec![
                empty("pogoda_teraz"),
                empty("propagacja"),
                text("ostrzezenia", "Brak obowiązujących ostrzeżeń."),
            ],
            None,
        )
        .await;
        let tick = s.runner().tick(t("2026-11-23 07:00:05")).await.unwrap();
        assert!(tick.queued.is_some());
    }

    #[tokio::test]
    async fn a_failing_part_drops_out() {
        let s = station_with(
            vec![
                Arc::new(Failing("pogoda_teraz")),
                text("propagacja", "Propagacja B."),
                text("ostrzezenia", "Brak obowiązujących ostrzeżeń."),
            ],
            None,
        )
        .await;
        let c = s.bulletin.compose(t("2026-11-23 07:00")).await.unwrap();
        let by_label = |l: &str| &c.parts.iter().find(|(x, _)| *x == l).unwrap().1;
        assert!(matches!(by_label("Pogoda bieżąca"), PartOutcome::Failed(_)), "{c:?}");
        assert!(matches!(by_label("Warunki propagacji KF"), PartOutcome::Included(_)));
        assert!(matches!(by_label("Obowiązujące ostrzeżenia"), PartOutcome::Included(_)));
        assert_eq!(
            by_label("Stany wód w okolicy"),
            &PartOutcome::Failed("część niedostępna".to_owned())
        );
        assert_eq!(
            c.text,
            Some(format!(
                "{}Brak obowiązujących ostrzeżeń. [[pauza:700]] Propagacja B. {END}",
                intro("7:00")
            ))
        );
    }

    #[tokio::test]
    async fn slow_parts_time_out_in_parallel() {
        let slow = |id| -> Arc<dyn BulletinPart> { Arc::new(Slow(id, Duration::from_secs(5))) };
        let s = station_with(
            vec![slow("pogoda_teraz"), slow("propagacja"), slow("ostrzezenia")],
            Some(Duration::from_millis(200)),
        )
        .await;
        let started = std::time::Instant::now();
        let c = s.bulletin.compose(t("2026-11-23 07:00")).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        for label in ["Pogoda bieżąca", "Warunki propagacji KF", "Obowiązujące ostrzeżenia"] {
            let o = &c.parts.iter().find(|(l, _)| *l == label).unwrap().1;
            assert!(
                matches!(o, PartOutcome::Failed(e) if e.starts_with("przekroczony czas")),
                "{label}: {o:?}"
            );
        }
        assert_eq!(c.text, None);
    }

    #[tokio::test]
    async fn settings_are_read_at_each_fire() {
        let s = station().await;
        s.runner().tick(t("2026-11-23 07:00:05")).await.unwrap();
        s.put("callsign", "SP9ABC").await;
        s.runner().tick(t("2026-11-23 19:00:05")).await.unwrap();
        let items = s.queued().await;
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|i| {
            i.rendered_text
                .starts_with("Tu automatyczna stacja informacyjna SP9ABC. Poniedziałek, 23 listopada, godzina 19:00.")
        }));

        s.put(
            "module:bulletin:layout",
            r#"{"bloki":[{"rodzaj":"wstep","tekst":"Tu {znak}, {godzina}."},{"rodzaj":"ostrzezenia","pauza_przed_ms":400,"mow_gdy_brak":true}]}"#,
        )
        .await;
        let c = s.bulletin.compose(t("2026-11-23 19:00")).await.unwrap();
        assert_eq!(
            c.text.as_deref(),
            Some("Tu SP9ABC, 19:00. [[pauza:400]] Brak obowiązujących ostrzeżeń.")
        );
        let labels: Vec<_> = c.parts.iter().map(|(l, _)| *l).collect();
        assert_eq!(labels, ["Wstęp", "Obowiązujące ostrzeżenia"]);
    }

    #[tokio::test]
    async fn old_parts_and_intro_are_migrated_on_first_compose() {
        let s = station().await;
        s.put("module:bulletin:parts", "ostrzezenia").await;
        s.put("module:bulletin:intro", "Tu stacja klubowa").await;
        let c = s.bulletin.compose(t("2026-11-23 19:00")).await.unwrap();
        assert_eq!(
            c.text,
            Some(format!(
                "Tu stacja klubowa. Godzina 19:00. [[pauza:700]] Brak obowiązujących ostrzeżeń. [[pauza:700]] {END}"
            ))
        );
        let off = |l: &str| c.parts.iter().find(|(x, _)| *x == l).unwrap().1.clone();
        assert_eq!(off("Pogoda bieżąca"), PartOutcome::Empty("klocek wyłączony".into()));
        assert_eq!(off("Warunki propagacji KF"), PartOutcome::Empty("klocek wyłączony".into()));
    }

    #[tokio::test]
    async fn without_a_callsign_the_intro_and_end_say_no_empty_tu() {
        let s = station().await;
        s.put("callsign", "").await;
        let c = s.bulletin.compose(t("2026-11-24 07:00")).await.unwrap();
        let text = c.text.unwrap();
        assert!(
            text.starts_with("Wtorek, 24 listopada, godzina 7:00. [[pauza:700]] "),
            "{text}"
        );
        assert!(text.ends_with(" Koniec komunikatu."), "{text}");
        assert!(!text.contains("Tu "), "{text}");
    }

    #[tokio::test]
    async fn blocks_follow_the_cycle_number_and_hours() {
        let s = station_with(
            vec![
                text("ostrzezenia", "Brak obowiązujących ostrzeżeń."),
                text("propagacja", "Propagacja B."),
            ],
            None,
        )
        .await;
        s.put("module:bulletin:mode", "cykl").await;
        s.put(
            "module:bulletin:layout",
            r#"{"bloki":[
                {"rodzaj":"ostrzezenia","mow_gdy_brak":true},
                {"rodzaj":"propagacja","kiedy":{"typ":"co_n_cykli","n":3}},
                {"rodzaj":"tekst","tekst":"Dobranoc.","kiedy":{"typ":"w_godzinach","od":"22:00","do":"06:00"}}
            ]}"#,
        )
        .await;
        let text_at = async |at: &str| s.bulletin.compose(t(at)).await.unwrap().text.unwrap();
        // 0:30 → cykl 3: propagacja tak, noc tak.
        assert_eq!(
            text_at("2026-11-23 00:30").await,
            "Brak obowiązujących ostrzeżeń. Propagacja B. Dobranoc."
        );
        // 0:40 → cykl 4: bez propagacji.
        assert_eq!(text_at("2026-11-23 00:40").await, "Brak obowiązujących ostrzeżeń. Dobranoc.");
        // 12:00 → cykl 72: propagacja, dzień — bez dobranocy.
        let c = s.bulletin.compose(t("2026-11-23 12:00")).await.unwrap();
        assert_eq!(c.text.as_deref(), Some("Brak obowiązujących ostrzeżeń. Propagacja B."));
        assert_eq!(
            c.parts[2].1,
            PartOutcome::Empty("nadawany w godzinach 22:00–06:00".into())
        );
        assert_eq!(
            s.bulletin.compose(t("2026-11-23 12:10")).await.unwrap().parts[1].1.describe(),
            "pominięta — nadawany co 3. cykl"
        );
        // Tryb pór: „co N-ty cykl” zawsze pasuje.
        s.put("module:bulletin:mode", "pory").await;
        assert!(text_at("2026-11-23 00:40").await.contains("Propagacja B."));
    }

    #[tokio::test]
    async fn warnings_without_say_when_empty_drop_out_when_nothing_is_active() {
        let s = station().await;
        s.put(
            "module:bulletin:layout",
            r#"{"bloki":[{"rodzaj":"ostrzezenia"},{"rodzaj":"pogoda_teraz"}]}"#,
        )
        .await;
        let c = s.bulletin.compose(t("2026-11-23 07:00")).await.unwrap();
        assert_eq!(c.text.as_deref(), Some("Pogoda A."));
        assert_eq!(
            c.parts[0].1,
            PartOutcome::Empty("brak obowiązujących ostrzeżeń".into())
        );
    }

    #[tokio::test]
    async fn the_tx_limit_drops_blocks_from_the_end_but_keeps_warnings() {
        let long = |c: &str| format!("{}.", c.repeat(199));
        let s = station_with(
            vec![
                text("ostrzezenia", &long("c")),
                text("pogoda_teraz", &long("a")),
                text("propagacja", &long("b")),
            ],
            None,
        )
        .await;
        // (70 − 10) × 10 = 600 znaków: wstęp, ostrzeżenia, zakończenie i jedna część.
        s.put("tx_max_duration_s", "70").await;
        let c = s.bulletin.compose(t("2026-11-23 07:00")).await.unwrap();
        let text = c.text.expect("tekst");
        assert!(text.contains(&long("c")), "{text}");
        assert!(text.contains(&long("a")), "{text}");
        assert!(!text.contains(&long("b")));
        assert!(text.ends_with(END));
        let of = |l: &str| c.parts.iter().find(|(x, _)| *x == l).unwrap().1.clone();
        assert_eq!(of("Warunki propagacji KF"), PartOutcome::DroppedForLength);
        assert!(matches!(of("Pogoda bieżąca"), PartOutcome::Included(_)));

        // Za mało nawet na jedną część: ostrzeżenia i tak idą.
        s.put("tx_max_duration_s", "20").await;
        let c = s.bulletin.compose(t("2026-11-23 07:00")).await.unwrap();
        let text = c.text.expect("ostrzeżenia nie wypadają");
        assert!(text.contains(&long("c")));
        assert!(!text.contains(&long("a")));
    }

    // --- Tryb cykliczny ---

    #[test]
    fn cycle_fires_are_aligned_to_the_interval_from_midnight() {
        let fires = cycle_fires(10, t("2026-11-23 07:03"));
        assert_eq!(fires.last(), Some(&t("2026-11-23 07:00")));
        assert_eq!(fires.first(), Some(&t("2026-11-22 07:10")));
        assert_eq!(fires.len(), 144);
        assert!(fires.windows(2).all(|w| w[1] - w[0] == TimeDelta::minutes(10)));

        let fires = cycle_fires(15, t("2026-11-23 00:14:59"));
        assert_eq!(fires.last(), Some(&t("2026-11-23 00:00")));
        assert_eq!(cycle_fires(15, t("2026-11-23 00:15")).last(), Some(&t("2026-11-23 00:15")));
        assert_eq!(fires.len(), 96);
        // 7 min nie dzieli doby: cykl zaczyna się od nowa o północy.
        let fires = cycle_fires(7, t("2026-11-23 00:01"));
        assert!(fires.contains(&t("2026-11-22 23:55")));
        assert!(fires.contains(&t("2026-11-23 00:00")));
    }

    #[test]
    fn cycle_fires_across_the_autumn_and_spring_clock_change() {
        // 25.10.2026: 3:00 CEST → 2:00 CET; 2:00–2:59 dwa razy.
        let fires = cycle_fires(10, t("2026-10-25 04:00"));
        let around: Vec<_> = fires
            .iter()
            .filter(|f| **f >= utc("2026-10-24T23:50:00Z") && **f <= utc("2026-10-25T02:10:00Z"))
            .collect();
        // Co 10 minut bez dziury i bez powtórki, także przez powtórzoną godzinę.
        assert_eq!(around.len(), 15, "{around:?}");
        assert!(around.windows(2).all(|w| *w[1] - *w[0] == TimeDelta::minutes(10)));
        assert!(fires.contains(&utc("2026-10-25T00:30:00Z")));
        assert!(fires.contains(&utc("2026-10-25T01:30:00Z")));
        // 29.03.2026: 2:00 CET → 3:00 CEST; 2:xx nie istnieje.
        let fires = cycle_fires(10, t("2026-03-29 03:30"));
        let i = fires.iter().position(|f| *f == t("2026-03-29 01:50")).unwrap();
        assert_eq!(fires[i + 1], t("2026-03-29 03:00"));
    }

    async fn cycle_station() -> Station {
        let s = station().await;
        s.put("module:bulletin:mode", "cykl").await;
        s.put("module:bulletin:interval_min", "10").await;
        s
    }

    #[tokio::test]
    async fn the_cycle_fires_every_ten_minutes_once_even_after_a_restart() {
        let s = cycle_station().await;
        let first = s.runner().tick(t("2026-11-23 07:00:05")).await.unwrap();
        assert!(first.queued.is_some());
        // Wczorajsze i wcześniejsze chwile — spóźnione, bez nadania.
        assert!(first.skipped.iter().all(|(_, r)| r == LATE), "{:?}", first.skipped);
        assert_eq!(s.queued().await.len(), 1);
        let item = &s.queued().await[0];
        // Cykl 10 min: element wygasa po długości cyklu.
        assert_eq!(item.expires_at, Some(t("2026-11-23 07:10")));
        assert!(item.rendered_text.starts_with(&intro("7:00")), "{}", item.rendered_text);

        // Ten sam cykl: drugi tik i restart nic nie dokładają.
        assert_eq!(s.runner().tick(t("2026-11-23 07:00:15")).await.unwrap().queued, None);
        let restarted = BulletinRunner::new(s.bulletin.clone(), s.queue.clone());
        let again = restarted.tick(t("2026-11-23 07:05")).await.unwrap();
        assert_eq!(again.queued, None);
        assert!(again.skipped.is_empty(), "{:?}", again.skipped);
        assert_eq!(s.queued().await.len(), 1);

        // Następny cykl.
        assert!(s.runner().tick(t("2026-11-23 07:10:05")).await.unwrap().queued.is_some());
        let items = s.queued().await;
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|i| i.rendered_text.contains("godzina 7:10.")));
    }

    #[tokio::test]
    async fn a_long_cycle_expires_after_fifteen_minutes() {
        let s = cycle_station().await;
        s.put("module:bulletin:interval_min", "30").await;
        s.runner().tick(t("2026-11-23 07:30:05")).await.unwrap();
        assert_eq!(s.queued().await[0].expires_at, Some(t("2026-11-23 07:45")));
    }

    #[tokio::test]
    async fn after_downtime_the_cycle_resumes_without_an_avalanche() {
        let s = cycle_station().await;
        s.runner().tick(t("2026-11-23 07:00:05")).await.unwrap();
        // 2 h przestoju: minione cykle spóźnione, nadany tylko bieżący.
        let tick = s.runner().tick(t("2026-11-23 09:00:30")).await.unwrap();
        assert!(tick.queued.is_some());
        assert_eq!(tick.skipped.len(), 11, "{:?}", tick.skipped);
        assert!(tick.skipped.iter().all(|(_, r)| r == LATE));
        assert_eq!(s.queued().await.len(), 2);
    }

    #[tokio::test]
    async fn quiet_hours_skip_each_cycle() {
        let now = t("2026-11-23 07:00:05");
        let window = format!(
            "{}-{}",
            local_hhmm(now - TimeDelta::hours(1)),
            local_hhmm(now + TimeDelta::hours(1))
        );
        let s = cycle_station().await;
        s.put("quiet_hours", &window).await;
        s.put("quiet_min_priority", &(Priority::High as i64).to_string())
            .await;
        let tick = s.runner().tick(now).await.unwrap();
        assert_eq!(tick.queued, None);
        assert_eq!(
            tick.skipped.last(),
            Some(&(t("2026-11-23 07:00"), QUIET.to_owned()))
        );
        let tick = s.runner().tick(t("2026-11-23 07:10:05")).await.unwrap();
        assert_eq!(tick.skipped, [(t("2026-11-23 07:10"), QUIET.to_owned())]);
        assert!(s.queued().await.is_empty());
        // Stare wpisy znikają także wtedy, gdy nic nie idzie na antenę.
        let old = t("2026-11-10 07:00");
        BulletinFireRepo::new(s.pool.clone()).claim(old, Some(QUIET)).await.unwrap();
        s.runner().tick(t("2026-11-23 07:20:05")).await.unwrap();
        assert!(BulletinFireRepo::new(s.pool.clone()).get(old).await.unwrap().is_none());
    }
}
