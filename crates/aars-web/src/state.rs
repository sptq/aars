use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::preview::PreviewRenderer;

use aars_core::config::Config;
use aars_core::dispatcher::{StationTest, TestOutcome};
use aars_core::hardware::probe::HardwareProbe;
use aars_core::hardware::{ApplyStatus, HardwareSettings};
use aars_core::interlock::InMemoryInterlocks;
use aars_core::module::ModuleRegistry;
use aars_core::queue::QueueRepo;
use aars_core::settings::StationSettings;
use sqlx::SqlitePool;

pub type TestFuture<'a> = Pin<Box<dyn Future<Output = TestOutcome> + Send + 'a>>;

/// Odpowiedź na test, gdy stacja działa bez stosu nadawczego (R6).
pub const NO_RADIO: &str = "stacja działa bez sprzętu nadawczego — popraw ustawienia sprzętu";

/// Panel o działającej stacji (etap 9): z jakiej konfiguracji wstała, czym
/// nadaje, żądanie restartu w procesie (R4) i testy sprzętu (R10, R11).
/// Implementacja żyje w binarce, bo tylko ona zna stos nadawczy.
pub trait StationControl: Send + Sync {
    fn apply_status(&self) -> ApplyStatus;
    /// Sprzęt, którym stacja nadaje; `None` = praca bez nadawania.
    fn running_hardware(&self) -> Option<HardwareSettings>;
    /// Ustawienia stacji wczytane przy starcie (lista „czeka na restart”).
    fn running_station(&self) -> StationSettings;
    fn request_restart(&self);
    /// Bez stosu nadawczego: `TestOutcome::Unsupported(NO_RADIO)`.
    fn run_test(&self, test: StationTest) -> TestFuture<'_>;
}

/// Wszystko, czego panel potrzebuje od rdzenia. Panel nie trzyma uchwytu do
/// sekwencera — tylko wkłada zadania do kolejki (spec §10).
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub queue: Arc<QueueRepo>,
    pub interlocks: Arc<InMemoryInterlocks>,
    /// Czy cookie sesji ma mieć flagę `Secure`.
    pub secure_cookies: bool,
    /// Synteza do odsłuchu w panelu; `None`, gdy nie skonfigurowano.
    pub preview: Option<Arc<dyn PreviewRenderer>>,
    /// Zdejmuje flagę z samego watchdoga (jego wątek żyje w binarce);
    /// `None`, gdy panel działa bez stosu nadawczego.
    pub watchdog_clear: Option<WatchdogClear>,
    /// Działająca stacja (apply status, restart, testy); `None` = panel bez stacji.
    pub station: Option<Arc<dyn StationControl>>,
    /// Lista urządzeń hosta do formularza sprzętu; `None` = bez sprawdzania.
    pub probe: Option<Arc<dyn HardwareProbe>>,
    /// Rejestr modułów (etap 9).
    pub modules: Arc<ModuleRegistry>,
    /// Konfiguracja z pliku i `AARS_*`, bez nadpisań z panelu.
    pub base_config: Arc<Config>,
    /// Budzi źródło danych o podanym id; `None`, gdy panel działa bez źródeł.
    pub source_refresh: Option<SourceRefresh>,
}

/// „Odśwież teraz” dla źródła: `true`, gdy źródło o tym id istnieje i zostało
/// obudzone, `false` dla nieznanego id. Panel nie zna crate'a źródeł (spec §3).
pub type SourceRefresh = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Wywołanie zdejmujące lepką flagę strażnika (`WatchdogMarker::clear_poison`).
pub type WatchdogClear = Arc<dyn Fn() + Send + Sync>;

impl AppState {
    pub fn new(
        pool: SqlitePool,
        queue: Arc<QueueRepo>,
        interlocks: Arc<InMemoryInterlocks>,
        secure_cookies: bool,
    ) -> Self {
        Self {
            pool,
            queue,
            interlocks,
            secure_cookies,
            preview: None,
            watchdog_clear: None,
            station: None,
            probe: None,
            modules: Arc::new(ModuleRegistry::new(aars_core::module::builtin())),
            base_config: Arc::new(Config::default()),
            source_refresh: None,
        }
    }

    pub fn with_watchdog_clear(mut self, clear: WatchdogClear) -> Self {
        self.watchdog_clear = Some(clear);
        self
    }

    pub fn with_source_refresh(mut self, refresh: SourceRefresh) -> Self {
        self.source_refresh = Some(refresh);
        self
    }

    pub fn with_station(mut self, station: Arc<dyn StationControl>) -> Self {
        self.station = Some(station);
        self
    }

    pub fn with_probe(mut self, probe: Arc<dyn HardwareProbe>) -> Self {
        self.probe = Some(probe);
        self
    }

    pub fn with_modules(mut self, modules: Arc<ModuleRegistry>) -> Self {
        self.modules = modules;
        self
    }

    pub fn with_base_config(mut self, config: Arc<Config>) -> Self {
        self.base_config = config;
        self
    }

    pub fn with_preview(mut self, preview: Arc<dyn PreviewRenderer>) -> Self {
        self.preview = Some(preview);
        self
    }

    /// Stan na potrzeby testów: własna kolejka, interlocki przepuszczające.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_tests(pool: SqlitePool) -> Self {
        Self {
            queue: Arc::new(QueueRepo::new(pool.clone())),
            interlocks: Arc::new(InMemoryInterlocks::permissive()),
            pool,
            secure_cookies: false,
            preview: None,
            watchdog_clear: None,
            station: None,
            probe: None,
            modules: Arc::new(ModuleRegistry::new({
                let mut all = aars_core::module::builtin();
                all.push(aars_core::module::sample_source("imgw-meteo"));
                all
            })),
            base_config: Arc::new(Config::default()),
            source_refresh: None,
        }
    }
}

/// Stacja do testów panelu: stały stan, zliczone restarty, zapisane testy.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct FakeStation {
        pub status: ApplyStatus,
        pub hardware: Option<HardwareSettings>,
        pub station: StationSettings,
        pub restarts: AtomicUsize,
        pub outcome: Mutex<TestOutcome>,
        pub tests: Mutex<Vec<StationTest>>,
    }

    impl FakeStation {
        pub fn new(hardware: Option<HardwareSettings>) -> Self {
            Self {
                status: ApplyStatus {
                    origin: aars_core::hardware::ConfigOrigin::File,
                    error: None,
                },
                hardware,
                station: StationSettings {
                    callsign: String::new(),
                    id_mode: aars_core::ident::IdMode::Voice,
                    id_interval_s: 600,
                    tx_max_duration_s: 180,
                    misfire_grace_s: 600,
                    quiet_hours: None,
                    quiet_min_priority: aars_core::priority::Priority::Emergency,
                },
                restarts: AtomicUsize::new(0),
                outcome: Mutex::new(TestOutcome::Done {
                    played: std::time::Duration::from_secs(2),
                    note: None,
                }),
                tests: Mutex::new(Vec::new()),
            }
        }

        pub fn restarts(&self) -> usize {
            self.restarts.load(Ordering::SeqCst)
        }
    }

    impl StationControl for FakeStation {
        fn apply_status(&self) -> ApplyStatus {
            self.status.clone()
        }
        fn running_hardware(&self) -> Option<HardwareSettings> {
            self.hardware.clone()
        }
        fn running_station(&self) -> StationSettings {
            self.station.clone()
        }
        fn request_restart(&self) {
            self.restarts.fetch_add(1, Ordering::SeqCst);
        }
        fn run_test(&self, test: StationTest) -> TestFuture<'_> {
            Box::pin(async move {
                self.tests.lock().unwrap().push(test);
                self.outcome.lock().unwrap().clone()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn source_refresh_is_absent_until_wired() {
        let state = AppState::for_tests(aars_core::db::test_pool().await);
        assert!(state.source_refresh.is_none());

        let state = state.with_source_refresh(Arc::new(|id| id == "imgw-meteo"));
        let refresh = state.source_refresh.as_ref().unwrap();
        assert!(refresh("imgw-meteo"));
        assert!(!refresh("nieznane"));
    }
}
