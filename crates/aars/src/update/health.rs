//! Potwierdzenie zdrowia nowej wersji (R9) i wynik ostatniej instalacji w
//! audycie (R18).
//!
//! Zdrowie nie opiera się na `WATCHDOG=1`: to dowodzi tylko, że runtime
//! tokio żyje. Stacja sama wpisuje do `HealthCell`, czy zbudowała stos
//! nadawczy i czy dyspozytor pracuje (puls z jego pętli, nie samo „zadanie
//! się nie skończyło”).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aars_core::sink::CancelToken;
use aars_core::update::{CONFIRM_AFTER, LastResult, Trial};
use sqlx::SqlitePool;
use tokio::time::Instant;

use super::install::SMALL_FILE;
use super::paths::{UpdatePaths, read_service_file, write_atomic_nofollow};
use super::read_trial;

/// Klucz `settings` z ostatnim wynikiem (dla panelu, Task 8).
pub const LAST_RESULT_KEY: &str = "update:last_result";

/// Stan zdrowia stacji, wpisywany przez pętlę stacji.
#[derive(Debug)]
pub struct HealthCell {
    radio_up: AtomicBool,
    dispatcher_alive: AtomicBool,
    /// Przebieg stacji się przebudowuje albo drenuje przed restartem w
    /// procesie: ocena czeka, zamiast uznać wersję za złą (przegląd 5–7 I-2).
    restarting: AtomicBool,
    last_error: Mutex<Option<String>>,
    /// Powód, dla którego proces ma się zakończyć błędem (werdykt `bad`).
    fatal: Mutex<Option<String>>,
    /// Pierwsza gotowość w tym procesie (`READY=1`).
    ready_at: std::sync::OnceLock<Instant>,
    /// Limit drenażu bieżącego przebiegu w ms (limit strażnika TX + zapas).
    drain_limit_ms: std::sync::atomic::AtomicU64,
}

impl Default for HealthCell {
    fn default() -> Self {
        Self {
            radio_up: AtomicBool::new(false),
            dispatcher_alive: AtomicBool::new(false),
            restarting: AtomicBool::new(false),
            last_error: Mutex::new(None),
            fatal: Mutex::new(None),
            ready_at: std::sync::OnceLock::new(),
            drain_limit_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl HealthCell {
    /// Wynik budowy stosu nadawczego w tym przebiegu stacji.
    pub fn set_radio(&self, up: bool, error: Option<String>) {
        self.radio_up.store(up, Ordering::SeqCst);
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = error;
    }

    /// Dyspozytor pracuje (albo, bez stosu, nie ma czego pilnować).
    pub fn set_dispatcher_alive(&self, alive: bool) {
        self.dispatcher_alive.store(alive, Ordering::SeqCst);
    }

    /// Przebieg stacji startuje albo drenuje (`true`) lub pracuje w pętli.
    pub fn set_restarting(&self, restarting: bool) {
        self.restarting.store(restarting, Ordering::SeqCst);
    }

    pub fn restarting(&self) -> bool {
        self.restarting.load(Ordering::SeqCst)
    }

    pub fn radio_up(&self) -> bool {
        self.radio_up.load(Ordering::SeqCst)
    }

    pub fn dispatcher_alive(&self) -> bool {
        self.dispatcher_alive.load(Ordering::SeqCst)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Stacja wysłała `READY=1`; liczy się pierwszy raz w procesie.
    pub fn mark_ready(&self) {
        let _ = self.ready_at.set(Instant::now());
    }

    pub fn ready_at(&self) -> Option<Instant> {
        self.ready_at.get().copied()
    }

    /// Limit drenażu przebiegu stacji: od niego zależą `restart_wait`
    /// i `fatal_grace` (przegląd 8–9 M-1).
    pub fn set_drain_limit(&self, limit: Duration) {
        let ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX);
        self.drain_limit_ms.store(ms, Ordering::SeqCst);
    }

    pub fn drain_limit(&self) -> Duration {
        Duration::from_millis(self.drain_limit_ms.load(Ordering::SeqCst))
    }

    pub fn set_fatal(&self, reason: String) {
        *self.fatal.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
    }

    pub fn fatal(&self) -> Option<String> {
        self.fatal.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Confirm,
    /// Treść pliku `verdict`: `bad {wersja}: {powód}`.
    Bad(String),
    Wait,
}

/// Ocena próby (R9). Dotyczy tylko próby wersji, która właśnie działa, i
/// dopiero po `CONFIRM_AFTER` od gotowości.
pub fn judge(
    trial: Option<&Trial>,
    running: &str,
    cell: &HealthCell,
    since_ready: Duration,
) -> Verdict {
    let Some(trial) = trial.filter(|t| t.version == running) else {
        return Verdict::Wait;
    };
    if since_ready < CONFIRM_AFTER || cell.restarting() {
        return Verdict::Wait;
    }
    if trial.radio_was_up && !cell.radio_up() {
        let error = cell
            .last_error()
            .unwrap_or_else(|| "brak stosu nadawczego".to_owned());
        return Verdict::Bad(format!("bad {running}: nadawanie nie wstało: {error}"));
    }
    if !cell.dispatcher_alive() {
        return Verdict::Bad(format!("bad {running}: dyspozytor nie pracuje"));
    }
    Verdict::Confirm
}

/// Najkrótsze czekanie oceny na koniec restartu w procesie i na koniec
/// procesu po werdykcie `bad`.
#[cfg(not(test))]
const MIN_WAIT: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const MIN_WAIT: Duration = Duration::from_secs(20);
#[cfg(not(test))]
const MIN_GRACE: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const MIN_GRACE: Duration = Duration::from_secs(5);
/// Zapas ponad limit drenażu: budowa następnego przebiegu albo koniec procesu.
#[cfg(not(test))]
const OVER_DRAIN: Duration = Duration::from_secs(2 * 60);
#[cfg(test)]
const OVER_DRAIN: Duration = Duration::from_secs(2);

/// Jak długo ocena czeka na koniec restartu w procesie: zawsze dłużej niż
/// drenaż (limit strażnika TX + 30 s), także przy `tx_max_duration_s = 600`
/// (przegląd 8–9 M-1).
pub(crate) fn restart_wait(drain_limit: Duration) -> Duration {
    MIN_WAIT.max(drain_limit + OVER_DRAIN)
}

/// Zapas na zakończenie przez drenaż po werdykcie `bad`; potem `stop`.
/// Dłuższy niż drenaż, więc awaryjny `stop` nie utnie pełnego nadania.
pub(crate) fn fatal_grace(drain_limit: Duration) -> Duration {
    MIN_GRACE.max(drain_limit + OVER_DRAIN)
}

/// Raz na proces: po `CONFIRM_AFTER` od `READY=1` zapisuje `confirmed` albo
/// `verdict`. W trakcie restartu w procesie czeka (I-2). Przy `bad` oznacza
/// proces do zakończenia błędem: stacja kończy go drenażem, czyli po
/// bieżącym nadaniu, a nie przez `stop` (to przerwałoby nadanie). `stop`
/// idzie dopiero awaryjnie, gdy drenaż nie skończył procesu w `fatal_grace`.
/// Strażnik przy następnym starcie wraca do poprzedniej.
pub async fn confirm_task(
    paths: UpdatePaths,
    cell: Arc<HealthCell>,
    running: String,
    stop: CancelToken,
) {
    confirm_after(paths, cell, running, stop, CONFIRM_AFTER).await;
}

pub(crate) async fn confirm_after(
    paths: UpdatePaths,
    cell: Arc<HealthCell>,
    running: String,
    stop: CancelToken,
    after: Duration,
) {
    let Some(trial) = read_trial(&paths).filter(|t| t.version == running) else {
        return;
    };
    let ready_at = loop {
        if stop.is_cancelled() {
            return;
        }
        if let Some(at) = cell.ready_at() {
            break at;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let deadline = ready_at + after;
    while Instant::now() < deadline {
        if stop.is_cancelled() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200).min(deadline - Instant::now())).await;
    }
    // `judge` mierzy od gotowości; tu czas już minął, więc podajemy pełny.
    let wait = restart_wait(cell.drain_limit());
    let give_up = Instant::now() + wait;
    let verdict = loop {
        if stop.is_cancelled() {
            return;
        }
        let since = ready_at.elapsed().max(CONFIRM_AFTER);
        let verdict = judge(Some(&trial), &running, &cell, since);
        // Drugi odczyt `restarting` po werdykcie `bad`: przebieg mógł właśnie
        // wyjść z pętli i ustawić `restarting` przed `alive = false`
        // (przegląd 8–9 M-2).
        if matches!(verdict, Verdict::Bad(_)) && cell.restarting() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if Instant::now() < give_up {
                continue;
            }
        }
        if verdict != Verdict::Wait || !cell.restarting() {
            break verdict;
        }
        if Instant::now() >= give_up {
            break Verdict::Bad(format!(
                "bad {running}: stacja nie wstała po restarcie w {} s",
                wait.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    match verdict {
        Verdict::Confirm => {
            if let Err(err) = write_atomic_nofollow(&paths.confirmed(), running.as_bytes()) {
                tracing::error!(%err, "zapis potwierdzenia nowej wersji zawiódł");
            } else {
                tracing::info!(version = %running, "nowa wersja potwierdzona po próbie");
            }
        }
        Verdict::Bad(reason) => {
            tracing::error!(%reason, "nowa wersja niezdrowa — kończę, strażnik wróci do poprzedniej");
            if let Err(err) = write_atomic_nofollow(&paths.verdict(), reason.as_bytes()) {
                tracing::error!(%err, "zapis werdyktu zawiódł");
            }
            cell.set_fatal(reason);
            // Stacja widzi `fatal` w swojej pętli i kończy przez drenaż.
            tokio::time::sleep(fatal_grace(cell.drain_limit())).await;
            if !stop.is_cancelled() {
                tracing::error!("drenaż po werdykcie nie skończył procesu — zatrzymuję stację");
                stop.cancel();
            }
        }
        Verdict::Wait => {}
    }
}

/// Raz przy starcie: wynik ostatniej instalacji, odmowy albo powrotu idzie
/// do audytu (bez użytkownika) i do `settings` dla panelu, a plik znika, więc
/// drugi start nic nie dokłada. Przy błędzie zapisu do bazy plik zostaje.
pub async fn audit_last_result(paths: &UpdatePaths, pool: &SqlitePool) -> Option<LastResult> {
    let path = paths.last_result();
    let text = match read_service_file(&path, SMALL_FILE) {
        Ok(Some(text)) => text,
        Ok(None) => return None,
        Err(err) => {
            tracing::error!(%err, "last-result.json nieczytelny — usuwam");
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    let result: LastResult = match serde_json::from_str(&text) {
        Ok(result) => result,
        Err(err) => {
            tracing::error!(%err, "last-result.json zepsuty — usuwam");
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    let mut detail = format!("{} → {}", result.from, result.version);
    if !result.detail.is_empty() {
        detail.push_str(": ");
        detail.push_str(&result.detail);
    }
    let audited =
        sqlx::query("INSERT INTO audit_log (user_id, action, detail) VALUES (NULL, ?, ?)")
            .bind(result.audit_action())
            .bind(&detail)
            .execute(pool)
            .await;
    if let Err(err) = audited {
        tracing::error!(%err, "zapis wyniku aktualizacji do audytu zawiódł");
        return Some(result);
    }
    let json = serde_json::to_string(&result).unwrap_or_default();
    if let Err(err) = aars_core::settings::SettingsRepo::new(pool.clone())
        .kv_put(LAST_RESULT_KEY, &json)
        .await
    {
        tracing::error!(%err, "zapis ostatniego wyniku aktualizacji zawiódł");
    }
    let _ = std::fs::remove_file(&path);
    Some(result)
}

/// Ostatni wynik zapamiętany w `settings` (panel).
pub async fn stored_last_result(pool: &SqlitePool) -> Option<LastResult> {
    let text = aars_core::settings::SettingsRepo::new(pool.clone())
        .kv_get(LAST_RESULT_KEY)
        .await
        .ok()??;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_outlast_the_longest_drain() {
        // tx_max_duration_s = 600, tx_tail 2 s, strażnik +2 s, zapas 30 s.
        let drain = Duration::from_secs(600 + 2 + 2 + 30);
        assert!(restart_wait(drain) > drain);
        assert!(fatal_grace(drain) > drain);
        assert!(restart_wait(Duration::ZERO) >= MIN_WAIT);
        assert!(fatal_grace(Duration::ZERO) >= MIN_GRACE);
    }
    use aars_core::update::ResultKind;

    fn trial(version: &str, radio_was_up: bool) -> Trial {
        Trial {
            format: 1,
            version: version.to_owned(),
            previous: "0.10.0".to_owned(),
            installed_at: "2026-11-21T12:00:00Z".to_owned(),
            attempts: 0,
            radio_was_up,
        }
    }

    fn healthy() -> HealthCell {
        let cell = HealthCell::default();
        cell.set_radio(true, None);
        cell.set_dispatcher_alive(true);
        cell
    }

    const MINUTE: Duration = Duration::from_secs(60);

    #[test]
    fn judge_waits_for_sixty_seconds() {
        let t = trial("0.11.0", true);
        let cell = healthy();
        assert_eq!(
            judge(Some(&t), "0.11.0", &cell, Duration::from_secs(59)),
            Verdict::Wait
        );
        assert_eq!(judge(Some(&t), "0.11.0", &cell, MINUTE), Verdict::Confirm);
    }

    #[test]
    fn judge_waits_while_the_station_restarts() {
        let t = trial("0.11.0", true);
        // W czasie drenażu dyspozytor nie tyka, a stos bywa w przebudowie.
        let cell = HealthCell::default();
        cell.set_restarting(true);
        assert_eq!(judge(Some(&t), "0.11.0", &cell, MINUTE), Verdict::Wait);
        cell.set_restarting(false);
        assert!(matches!(judge(Some(&t), "0.11.0", &cell, MINUTE), Verdict::Bad(_)));
    }

    #[tokio::test]
    async fn the_confirm_task_waits_for_a_restart_to_finish() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        crate::update::install::write_trial(&p, &trial("0.11.0", true)).unwrap();
        let cell = Arc::new(HealthCell::default());
        cell.mark_ready();
        cell.set_restarting(true);
        let stop = CancelToken::new();
        let task = tokio::spawn(confirm_after(
            p.clone(),
            cell.clone(),
            "0.11.0".into(),
            stop.clone(),
            Duration::ZERO,
        ));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(!p.verdict().exists() && !p.confirmed().exists(), "ocena czeka");
        // Nowy przebieg wstał zdrowy.
        cell.set_radio(true, None);
        cell.set_dispatcher_alive(true);
        cell.set_restarting(false);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read_to_string(p.confirmed()).unwrap(), "0.11.0");
        assert!(!p.verdict().exists());
        assert!(!stop.is_cancelled());
    }

    #[test]
    fn judge_confirms_a_healthy_trial_of_its_own_version() {
        let cell = healthy();
        assert_eq!(
            judge(Some(&trial("0.11.0", true)), "0.11.0", &cell, MINUTE * 2),
            Verdict::Confirm
        );
    }

    #[test]
    fn judge_is_bad_when_the_radio_was_up_before_and_is_not_now() {
        let cell = HealthCell::default();
        cell.set_radio(false, Some("plik: brak urządzenia hw:1,0".to_owned()));
        cell.set_dispatcher_alive(true);
        let verdict = judge(Some(&trial("0.11.0", true)), "0.11.0", &cell, MINUTE);
        assert_eq!(
            verdict,
            Verdict::Bad(
                "bad 0.11.0: nadawanie nie wstało: plik: brak urządzenia hw:1,0".to_owned()
            )
        );
    }

    #[test]
    fn judge_is_bad_when_the_dispatcher_does_not_work() {
        let cell = healthy();
        cell.set_dispatcher_alive(false);
        assert!(matches!(
            judge(Some(&trial("0.11.0", true)), "0.11.0", &cell, MINUTE),
            Verdict::Bad(reason) if reason.starts_with("bad 0.11.0: ")
        ));
    }

    #[test]
    fn judge_ignores_a_trial_of_another_version() {
        let cell = HealthCell::default();
        assert_eq!(
            judge(Some(&trial("0.11.0", true)), "0.10.0", &cell, MINUTE),
            Verdict::Wait
        );
        assert_eq!(judge(None, "0.10.0", &cell, MINUTE), Verdict::Wait);
    }

    #[test]
    fn radio_less_is_fine_when_it_was_radio_less_before() {
        let cell = HealthCell::default();
        cell.set_radio(false, Some("brak karty".to_owned()));
        cell.set_dispatcher_alive(true);
        assert_eq!(
            judge(Some(&trial("0.11.0", false)), "0.11.0", &cell, MINUTE),
            Verdict::Confirm
        );
    }

    fn paths(dir: &std::path::Path) -> UpdatePaths {
        let paths = UpdatePaths {
            install_root: dir.join("opt"),
            guard_dir: dir.join("guard"),
            state_dir: dir.join("state"),
            db_path: dir.join("aars.db"),
        };
        std::fs::create_dir_all(&paths.guard_dir).unwrap();
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        paths
    }

    #[tokio::test]
    async fn the_confirm_task_writes_confirmed_or_a_verdict_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        crate::update::install::write_trial(&p, &trial("0.11.0", true)).unwrap();
        let cell = Arc::new(healthy());
        cell.mark_ready();
        let stop = CancelToken::new();
        let short = Duration::from_millis(50);

        confirm_after(
            p.clone(),
            cell.clone(),
            "0.11.0".into(),
            stop.clone(),
            short,
        )
        .await;
        assert_eq!(std::fs::read_to_string(p.confirmed()).unwrap(), "0.11.0");
        assert!(!stop.is_cancelled());

        std::fs::remove_file(p.confirmed()).unwrap();
        cell.set_radio(false, Some("brak karty".to_owned()));
        confirm_after(
            p.clone(),
            cell.clone(),
            "0.11.0".into(),
            stop.clone(),
            short,
        )
        .await;
        assert!(!p.confirmed().exists());
        let verdict = std::fs::read_to_string(p.verdict()).unwrap();
        assert!(
            verdict.starts_with("bad 0.11.0: nadawanie nie wstało: brak karty"),
            "{verdict}"
        );
        assert!(cell.fatal().is_some(), "proces kończy się błędem");
        // `stop` dopiero awaryjnie, po `fatal_grace`: zwykle kończy drenaż.
        assert!(stop.is_cancelled(), "bez stacji, która by się skończyła, idzie stop");

        // Bez próby albo próba innej wersji: nic.
        std::fs::remove_file(p.verdict()).unwrap();
        let stop = CancelToken::new();
        confirm_after(p.clone(), cell, "0.10.0".into(), stop.clone(), short).await;
        assert!(!p.verdict().exists() && !p.confirmed().exists());
        assert!(!stop.is_cancelled());
    }

    #[tokio::test]
    async fn the_last_result_is_audited_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let pool = aars_core::db::test_pool().await;
        let result = LastResult {
            format: 1,
            at: "2026-11-21T12:05:00Z".to_owned(),
            kind: ResultKind::RolledBack,
            version: "0.11.0".to_owned(),
            from: "0.10.0".to_owned(),
            detail: "wersja 0.11.0: nie wstała w 3 próbach".to_owned(),
        };
        std::fs::write(p.last_result(), serde_json::to_vec(&result).unwrap()).unwrap();

        assert_eq!(audit_last_result(&p, &pool).await, Some(result.clone()));
        assert_eq!(
            audit_last_result(&p, &pool).await,
            None,
            "drugi start nic nie dokłada"
        );

        let rows: Vec<(Option<i64>, String, String)> = sqlx::query_as(
            "SELECT user_id, action, detail FROM audit_log WHERE action LIKE 'update_%'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, None, "wpis systemowy, bez użytkownika");
        assert_eq!(rows[0].1, "update_rolled_back");
        assert!(
            rows[0].2.contains("nie wstała w 3 próbach"),
            "{}",
            rows[0].2
        );
        assert!(!p.last_result().exists());
        assert_eq!(stored_last_result(&pool).await, Some(result));
    }
}
