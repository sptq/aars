//! Aktualizacja z panelu po stronie stacji (R10, R11, R18): `UpdateControl`
//! dla `aars-web`. Pobieranie i sprawdzenie idą, zanim stacja zacznie się
//! zatrzymywać (R10d); potem żądanie, audyt i prośba do stacji o koniec po
//! bieżącym nadaniu (R10a).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aars_core::config::Config;
use aars_core::interlock::{InMemoryInterlocks, Interlocks, RefusalReason};
use aars_core::priority::Priority;
use aars_core::queue::QueueRepo;
use aars_core::update::{Available, LastResult, UpdateRefusal, UpdateStatus, is_newer};
use aars_web::state::{UpdateControl, UpdateFuture};
use sqlx::SqlitePool;

use super::fetch::ReleaseClient;
use super::paths::{UpdatePaths, installed_version, write_atomic_nofollow};
use super::verify::{NO_KEY, TrustedKey};

/// Horyzont pilnych komunikatów, przy których odmawiamy (R10c).
pub const URGENT_HORIZON: Duration = Duration::from_secs(60);

/// To, czego aktualizacja potrzebuje od stacji: koniec po bieżącym nadaniu.
pub trait UpdateTarget: Send + Sync {
    fn request_update(&self);
}

/// Dlaczego aktualizacja z panelu jest wyłączona (R11); `None` = działa.
pub fn unsupported_reason(under_systemd: bool, platform_ok: bool, has_key: bool) -> Option<String> {
    if !platform_ok || !under_systemd {
        return Some(UpdateRefusal::Unsupported.to_string());
    }
    if !has_key {
        return Some(NO_KEY.to_owned());
    }
    None
}

/// Czy binarka jest na platformie wydań (Linux aarch64).
pub const PLATFORM_OK: bool = cfg!(all(target_os = "linux", target_arch = "aarch64"));

pub struct UpdateHandle {
    paths: UpdatePaths,
    client: Option<ReleaseClient>,
    station: Arc<dyn UpdateTarget>,
    queue: Arc<QueueRepo>,
    pool: SqlitePool,
    interlocks: Arc<InMemoryInterlocks>,
    running: String,
    unsupported: Option<String>,
    last: Option<LastResult>,
    in_progress: AtomicBool,
}

pub struct UpdateDeps {
    pub pool: SqlitePool,
    pub queue: Arc<QueueRepo>,
    pub interlocks: Arc<InMemoryInterlocks>,
    pub station: Arc<dyn UpdateTarget>,
    /// Ostatni wynik (z `audit_last_result` albo `stored_last_result`).
    pub last: Option<LastResult>,
}

impl UpdateHandle {
    /// Prawdziwa stacja: klucz wkompilowany, `releases_url` z pliku, R11.
    pub fn new(config: &Config, deps: UpdateDeps) -> Self {
        let key = TrustedKey::embedded();
        let mut unsupported =
            unsupported_reason(crate::service::under_systemd(), PLATFORM_OK, key.is_some());
        let client = match key.map(|k| ReleaseClient::new(&config.update.releases_url, k)) {
            Some(Ok(client)) => Some(client),
            Some(Err(err)) => {
                unsupported.get_or_insert(err);
                None
            }
            None => None,
        };
        Self::with_client(
            UpdatePaths::from_config(config),
            client,
            crate::service::VERSION,
            unsupported,
            deps,
        )
    }

    /// Wszystko wstrzyknięte (testy).
    pub fn with_client(
        paths: UpdatePaths,
        client: Option<ReleaseClient>,
        running: &str,
        unsupported: Option<String>,
        deps: UpdateDeps,
    ) -> Self {
        let unsupported = match (&client, unsupported) {
            (None, None) => Some(NO_KEY.to_owned()),
            (_, reason) => reason,
        };
        Self {
            paths,
            client,
            station: deps.station,
            queue: deps.queue,
            pool: deps.pool,
            interlocks: deps.interlocks,
            running: running.to_owned(),
            unsupported,
            last: deps.last,
            in_progress: AtomicBool::new(false),
        }
    }

    fn client(&self) -> Result<&ReleaseClient, UpdateRefusal> {
        match (&self.unsupported, &self.client) {
            (None, Some(client)) => Ok(client),
            _ => Err(UpdateRefusal::Unsupported),
        }
    }

    /// Odmowa, gdy w kolejce czeka pilny komunikat (R10c). Gdy nadawanie
    /// jest wstrzymane, komunikat sam nie wyjdzie, więc mówimy, co zrobić.
    async fn refuse_if_urgent(&self) -> Result<(), UpdateRefusal> {
        let due = self
            .queue
            .urgent_due(URGENT_HORIZON)
            .await
            .map_err(|e| UpdateRefusal::Failed(format!("odczyt kolejki: {e}")))?;
        if !due {
            return Ok(());
        }
        match self.interlocks.check(Priority::High) {
            Ok(()) => Err(UpdateRefusal::UrgentPending),
            Err(reason) => Err(UpdateRefusal::Failed(format!(
                "{} Nadawanie jest teraz wstrzymane ({}), więc komunikat sam nie wyjdzie — \
                 włącz nadawanie albo anuluj go w kolejce.",
                UpdateRefusal::UrgentPending,
                refusal_text(reason)
            ))),
        }
    }

    async fn install_inner(&self, version: &str, by: &str) -> Result<(), UpdateRefusal> {
        self.refuse_if_urgent().await?;
        let client = self.client()?;
        let signed = client.latest().await.map_err(UpdateRefusal::Failed)?;
        let latest = &signed.manifest.version;
        if latest != version {
            return Err(UpdateRefusal::Failed(format!(
                "Najnowsze wydanie to {latest}, a nie {version} — sprawdź dostępność jeszcze raz."
            )));
        }
        if !is_newer(latest, &self.running).map_err(UpdateRefusal::Failed)? {
            return Err(UpdateRefusal::NotNewer(latest.clone()));
        }
        let staged = client
            .stage(&signed, &self.running, &self.paths.state_dir)
            .await
            .map_err(UpdateRefusal::Failed)?;

        // Pobieranie trwało: pilny komunikat mógł wpaść w tym czasie.
        let requested = async {
            self.refuse_if_urgent().await?;
            write_atomic_nofollow(&self.paths.request(), version.as_bytes())
                .map_err(|e| UpdateRefusal::Failed(format!("zapis żądania: {e}")))
        }
        .await;
        if let Err(refusal) = requested {
            let _ = std::fs::remove_dir_all(&staged.dir);
            return Err(refusal);
        }

        let detail = format!("{} → {version}: {by}", self.running);
        if let Err(err) = sqlx::query(
            "INSERT INTO audit_log (user_id, action, detail) \
             VALUES ((SELECT id FROM users WHERE username = ?), 'update_install', ?)",
        )
        .bind(by)
        .bind(&detail)
        .execute(&self.pool)
        .await
        {
            tracing::error!(%err, "zapis update_install do audytu zawiódł");
        }
        tracing::info!(%detail, "aktualizacja pobrana i sprawdzona — stacja skończy po bieżącym nadaniu");
        self.station.request_update();
        Ok(())
    }
}

fn refusal_text(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::Poisoned => "blokada watchdoga",
        RefusalReason::TxDisabled => "wyłącznik nadawania",
        RefusalReason::Inhibited => "blokada sprzętowa",
        RefusalReason::QuietHours => "okno ciszy",
        RefusalReason::ChannelBusy => "kanał zajęty",
    }
}

impl UpdateControl for UpdateHandle {
    fn status(&self) -> UpdateStatus {
        UpdateStatus {
            current: self.running.clone(),
            previous: installed_version(&self.paths.previous()),
            trial: super::read_trial(&self.paths),
            unsupported: self.unsupported.clone(),
            last: super::read_last_result(&self.paths).or_else(|| self.last.clone()),
        }
    }

    fn check(&self) -> UpdateFuture<'_, Result<Option<Available>, UpdateRefusal>> {
        Box::pin(async move {
            let client = self.client()?;
            let signed = client.latest().await.map_err(UpdateRefusal::Failed)?;
            let manifest = signed.manifest;
            if !is_newer(&manifest.version, &self.running).map_err(UpdateRefusal::Failed)? {
                return Ok(None);
            }
            Ok(Some(Available {
                size: manifest.binary().size,
                version: manifest.version,
                notes: manifest.notes,
            }))
        })
    }

    /// Kolejność: R11 → próba trwa → już trwa → pilny komunikat → najnowsze
    /// == żądane → pobranie → żądanie → audyt → stacja. Błąd przed prośbą do
    /// stacji niczego nie zostawia (staging usunięty, bez żądania).
    fn install(&self, version: &str, by: &str) -> UpdateFuture<'_, Result<(), UpdateRefusal>> {
        let version = version.to_owned();
        let by = by.to_owned();
        Box::pin(async move {
            self.client()?;
            if let Some(trial) = super::read_trial(&self.paths) {
                return Err(UpdateRefusal::TrialRunning(trial.version));
            }
            if self.in_progress.swap(true, Ordering::SeqCst) {
                return Err(UpdateRefusal::InProgress);
            }
            let result = self.install_inner(&version, &by).await;
            if result.is_err() {
                // Po sukcesie zostaje „trwa”: stacja właśnie się zamyka.
                self.in_progress.store(false, Ordering::SeqCst);
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use aars_core::queue::EnqueueRequest;
    use aars_core::update::binary_archive_name;

    use crate::update::testkit::{TestKey, release_server, sha256_hex};

    const ARCHIVE: &[u8] = b"archiwum binarki 0.11.0";

    #[derive(Default)]
    struct FakeStation {
        requests: AtomicUsize,
    }

    impl UpdateTarget for FakeStation {
        fn request_update(&self) {
            self.requests.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        paths: UpdatePaths,
        pool: SqlitePool,
        queue: Arc<QueueRepo>,
        interlocks: Arc<InMemoryInterlocks>,
        station: Arc<FakeStation>,
        handle: UpdateHandle,
    }

    impl Setup {
        fn requests(&self) -> usize {
            self.station.requests.load(Ordering::SeqCst)
        }

        fn staging_files(&self) -> usize {
            std::fs::read_dir(self.paths.staging()).map_or(0, |d| d.count())
        }
    }

    async fn setup(
        server: &wiremock::MockServer,
        key: &TestKey,
        unsupported: Option<String>,
    ) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let paths = UpdatePaths {
            install_root: dir.path().join("opt"),
            guard_dir: dir.path().join("guard"),
            state_dir: dir.path().join("state"),
            db_path: dir.path().join("aars.db"),
        };
        let pool = aars_core::db::test_pool().await;
        let queue = Arc::new(QueueRepo::new(pool.clone()));
        let interlocks = Arc::new(InMemoryInterlocks::permissive());
        let station = Arc::new(FakeStation::default());
        let client = ReleaseClient::new(&server.uri(), key.trusted()).unwrap();
        let handle = UpdateHandle::with_client(
            paths.clone(),
            Some(client),
            "0.10.0",
            unsupported,
            UpdateDeps {
                pool: pool.clone(),
                queue: queue.clone(),
                interlocks: interlocks.clone(),
                station: station.clone(),
                last: None,
            },
        );
        Setup {
            _dir: dir,
            paths,
            pool,
            queue,
            interlocks,
            station,
            handle,
        }
    }

    async fn enqueue(queue: &QueueRepo, priority: Priority) {
        queue
            .enqueue(EnqueueRequest {
                message_id: None,
                rendered_text: "pilne".to_owned(),
                priority,
                interrupting: false,
                not_before: None,
                expires_at: None,
                repeat_count: 1,
                repeat_interval: Duration::ZERO,
                created_by: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn install_stages_writes_the_request_and_audits() {
        let key = TestKey::generate();
        let server = release_server(&key, "0.11.0", ARCHIVE).await;
        let s = setup(&server, &key, None).await;
        aars_core::user::UserRepo::new(s.pool.clone())
            .create("admin", "haslo", aars_core::user::Role::Admin)
            .await
            .unwrap();

        s.handle.install("0.11.0", "admin").await.unwrap();

        assert_eq!(
            std::fs::read_to_string(s.paths.request()).unwrap(),
            "0.11.0"
        );
        let archive = s
            .paths
            .staging()
            .join("0.11.0")
            .join(binary_archive_name("0.11.0"));
        assert_eq!(
            sha256_hex(&std::fs::read(archive).unwrap()),
            sha256_hex(ARCHIVE)
        );
        let (user, detail): (Option<i64>, String) =
            sqlx::query_as("SELECT user_id, detail FROM audit_log WHERE action = 'update_install'")
                .fetch_one(&s.pool)
                .await
                .unwrap();
        assert_eq!(detail, "0.10.0 → 0.11.0: admin");
        assert!(user.is_some(), "wpis z użytkownikiem");
        assert_eq!(s.requests(), 1, "stacja poproszona raz");
        assert_eq!(
            s.handle.install("0.11.0", "admin").await,
            Err(UpdateRefusal::InProgress),
            "po prośbie do stacji następna instalacja czeka"
        );
    }

    #[tokio::test]
    async fn an_update_is_refused_while_an_emergency_is_due() {
        let key = TestKey::generate();
        let server = release_server(&key, "0.11.0", ARCHIVE).await;
        let s = setup(&server, &key, None).await;
        enqueue(&s.queue, Priority::Emergency).await;

        assert_eq!(
            s.handle.install("0.11.0", "admin").await,
            Err(UpdateRefusal::UrgentPending)
        );
        assert!(!s.paths.request().exists());
        assert_eq!(s.requests(), 0);
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "odmowa, zanim cokolwiek pobierzemy"
        );

        // Nadawanie wstrzymane: komunikat sam nie wyjdzie, odmowa mówi, co robić.
        s.interlocks.set_tx_enabled(false);
        match s.handle.install("0.11.0", "admin").await {
            Err(UpdateRefusal::Failed(text)) => {
                assert!(text.contains("Czeka pilny komunikat"), "{text}");
                assert!(text.contains("wyłącznik nadawania"), "{text}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(s.requests(), 0);
    }

    #[tokio::test]
    async fn a_normal_message_does_not_block_the_update() {
        let key = TestKey::generate();
        let server = release_server(&key, "0.11.0", ARCHIVE).await;
        let s = setup(&server, &key, None).await;
        enqueue(&s.queue, Priority::Normal).await;
        assert_eq!(s.handle.install("0.11.0", "admin").await, Ok(()));
    }

    #[tokio::test]
    async fn a_failed_download_requests_nothing() {
        let key = TestKey::generate();
        let other = TestKey::generate();
        let server = release_server(&other, "0.11.0", ARCHIVE).await;
        let s = setup(&server, &key, None).await;

        let result = s.handle.install("0.11.0", "admin").await;
        assert!(
            matches!(&result, Err(UpdateRefusal::Failed(e)) if e.contains("podpis nie pasuje")),
            "{result:?}"
        );
        assert!(!s.paths.request().exists());
        assert_eq!(s.staging_files(), 0, "staging pusty");
        assert_eq!(s.requests(), 0);
        // Po błędzie można spróbować jeszcze raz (nie „już trwa”).
        assert!(matches!(
            s.handle.install("0.11.0", "admin").await,
            Err(UpdateRefusal::Failed(_))
        ));
    }

    #[tokio::test]
    async fn a_request_for_another_version_than_the_latest_is_refused() {
        let key = TestKey::generate();
        let server = release_server(&key, "0.11.0", ARCHIVE).await;
        let s = setup(&server, &key, None).await;
        let result = s.handle.install("0.12.0", "admin").await;
        assert!(
            matches!(&result, Err(UpdateRefusal::Failed(e)) if e.contains("0.11.0")),
            "{result:?}"
        );
        assert_eq!(s.requests(), 0);

        let old = release_server(&key, "0.10.0", ARCHIVE).await;
        let s = setup(&old, &key, None).await;
        assert_eq!(
            s.handle.install("0.10.0", "admin").await,
            Err(UpdateRefusal::NotNewer("0.10.0".to_owned()))
        );
    }

    #[tokio::test]
    async fn install_is_refused_during_a_trial_and_when_unsupported() {
        let key = TestKey::generate();
        let server = release_server(&key, "0.11.0", ARCHIVE).await;
        let s = setup(&server, &key, None).await;
        crate::update::install::write_trial(
            &s.paths,
            &aars_core::update::Trial {
                format: 1,
                version: "0.10.0".to_owned(),
                previous: "0.9.0".to_owned(),
                installed_at: String::new(),
                attempts: 0,
                radio_was_up: true,
            },
        )
        .unwrap();
        assert_eq!(
            s.handle.install("0.11.0", "admin").await,
            Err(UpdateRefusal::TrialRunning("0.10.0".to_owned()))
        );
        assert_eq!(s.handle.status().trial.unwrap().version, "0.10.0");

        let s = setup(&server, &key, Some(UpdateRefusal::Unsupported.to_string())).await;
        assert_eq!(
            s.handle.install("0.11.0", "admin").await,
            Err(UpdateRefusal::Unsupported)
        );
        assert_eq!(s.handle.check().await, Err(UpdateRefusal::Unsupported));
        assert!(s.handle.status().unsupported.is_some());
        assert_eq!(s.requests(), 0);
    }

    #[tokio::test]
    async fn a_second_install_while_one_runs_is_in_progress() {
        let key = TestKey::generate();
        // Archiwum przychodzi z opóźnieniem, więc pierwsza instalacja trwa.
        let server = release_server(&key, "0.11.0", ARCHIVE).await;
        let s = setup(&server, &key, None).await;

        let (first, second) = tokio::join!(s.handle.install("0.11.0", "admin"), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            s.handle.install("0.11.0", "admin").await
        });
        assert_eq!(first, Ok(()));
        assert_eq!(second, Err(UpdateRefusal::InProgress));
        assert_eq!(s.requests(), 1);
    }

    #[tokio::test]
    async fn check_reports_a_newer_release_only() {
        let key = TestKey::generate();
        let server = release_server(&key, "0.11.0", ARCHIVE).await;
        let s = setup(&server, &key, None).await;
        let available = s.handle.check().await.unwrap().unwrap();
        assert_eq!(available.version, "0.11.0");
        assert_eq!(available.size, ARCHIVE.len() as u64);
        assert_eq!(s.staging_files(), 0, "sprawdzenie niczego nie pobiera");

        let same = release_server(&key, "0.10.0", ARCHIVE).await;
        let s = setup(&same, &key, None).await;
        assert_eq!(s.handle.check().await, Ok(None));
        let status = s.handle.status();
        assert_eq!(status.current, "0.10.0");
        assert_eq!(status.previous, None);
    }

    #[test]
    fn updates_need_systemd_the_platform_and_a_key() {
        assert_eq!(unsupported_reason(true, true, true), None);
        let unsupported = Some(UpdateRefusal::Unsupported.to_string());
        assert_eq!(unsupported_reason(false, true, true), unsupported);
        assert_eq!(unsupported_reason(true, false, true), unsupported);
        assert_eq!(
            unsupported_reason(true, true, false),
            Some(NO_KEY.to_owned())
        );
    }
}
