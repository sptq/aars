use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::preview::PreviewRenderer;

use aars_core::config::{Config, FirstAdmin};
use aars_core::dispatcher::{StationTest, TestOutcome};
use aars_core::hardware::probe::HardwareProbe;
use aars_core::hardware::{ApplyStatus, HardwareSettings};
use aars_core::interlock::InMemoryInterlocks;
use aars_core::module::ModuleRegistry;
use aars_core::netlocal::LocalNet;
use aars_core::queue::QueueRepo;
use aars_core::settings::StationSettings;
use aars_core::update::{Available, UpdateRefusal, UpdateStatus};
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

pub type UpdateFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Aktualizacja z panelu (etap 10, R10, R11, R18). Pobieranie, weryfikacja
/// i instalacja żyją w binarce; panel zna tylko ten trait i czyste typy z
/// `aars_core::update`, więc nie zależy od kodu aktualizacji.
pub trait UpdateControl: Send + Sync {
    fn status(&self) -> UpdateStatus;
    /// Najnowsze wydanie; `None` = brak nowszego od działającego.
    fn check(&self) -> UpdateFuture<'_, Result<Option<Available>, UpdateRefusal>>;
    /// Pobiera i sprawdza wydanie `version`, potem prosi stację o
    /// zakończenie po bieżącym nadaniu; instaluje strażnik przy starcie.
    fn install(&self, version: &str, by: &str) -> UpdateFuture<'_, Result<(), UpdateRefusal>>;
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
    /// Wersja produktu do stopki i `/healthz` (R17); ustawia ją binarka.
    pub version: &'static str,
    /// Aktualizacja z panelu (etap 10); `None` = panel bez stacji.
    pub update: Option<Arc<dyn UpdateControl>>,
    /// Jak powstaje pierwsze konto admina (R12); `Wizard` włącza `/setup`.
    pub first_admin: FirstAdmin,
    /// Adresy interfejsów stacji do sprawdzania „sieci lokalnej” (R12).
    pub local_nets: Arc<Vec<LocalNet>>,
    /// Nazwy stacji dopuszczone w `Host` kreatora (DNS rebinding): nazwa
    /// hosta, `{nazwa}.local`, host z `server.bind`. Literały IP i
    /// `localhost` przechodzą zawsze.
    pub host_names: Arc<Vec<String>>,
    /// Kontrola `Host` na całym panelu (przegląd 8–9 M-4). Stacja ją
    /// włącza; testy bez nagłówka `Host` działają bez niej. `/setup`
    /// sprawdza `Host` zawsze.
    pub check_hosts: bool,
    /// Cały panel tylko dla klientów z sieci lokalnej (przegląd N-1), ta
    /// sama reguła co kreator (`is_local_client` z `local_nets`). Stacja
    /// włącza ją, gdy `server.allow_remote = false` (domyślnie); testy bez
    /// `ConnectInfo` działają bez niej. `/setup` sprawdza adres zawsze.
    pub lan_only: bool,
    /// Podgląd biuletynu (etap 11b); `None` = panel bez stacji.
    pub bulletin_preview: Option<BulletinPreview>,
    /// Jeden podgląd naraz: każdy pyta sieć o wszystkie części.
    pub bulletin_preview_gate: Arc<tokio::sync::Semaphore>,
    /// Ostrzeżenia meteo dla mapy (etap 11c); `None` = panel bez stacji.
    pub warnings_map: Option<WarningsMap>,
}

/// Jedno ostrzeżenie meteo dla jednego powiatu (kod TERYT 4-cyfrowy).
#[derive(Debug, Clone, PartialEq)]
pub struct MapWarning {
    pub teryt: String,
    pub level: u8,
    pub event: String,
    pub until: chrono::DateTime<chrono::Utc>,
}

/// Wszystkie ostrzeżenia z ostatniego udanego pobrania `imgw-meteo`.
#[derive(Debug, Clone, PartialEq)]
pub struct WarningsSnapshot {
    pub fetched_at: chrono::DateTime<chrono::Utc>,
    pub warnings: Vec<MapWarning>,
}

/// Pamięć ostrzeżeń źródła bez typów z crate'a źródeł; ustawia ją binarka.
/// `None` = źródło jeszcze nic nie pobrało.
pub type WarningsMap = Arc<dyn Fn() -> Option<WarningsSnapshot> + Send + Sync>;

/// Wynik podglądu biuletynu bez typów z crate'a źródeł: tekst (albo powód,
/// dla którego nic by nie poszło) i `(id części, opis wyniku)`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BulletinPreviewView {
    pub text: Option<String>,
    pub parts: Vec<(String, String)>,
    pub skipped_reason: Option<String>,
    /// Długość tekstu każdego klocka w biuletynie (w kolejności `parts`;
    /// `None` — klocek nie wszedł). Edytor liczy z niej oś czasu.
    pub part_chars: Vec<Option<usize>>,
}

impl BulletinPreviewView {
    /// Treść do pokazania w panelu (znaczniki pauz czytelnie); do odsłuchu
    /// idzie surowy `text`.
    pub fn display_text(&self) -> Option<String> {
        self.text.as_deref().map(aars_core::pause::display)
    }
}

pub type BulletinPreviewFuture =
    Pin<Box<dyn Future<Output = Result<BulletinPreviewView, String>> + Send>>;

/// Składa biuletyn na teraz bez zapisu do kolejki; ustawia go binarka.
/// `Some(układ)` — podgląd niezapisanego układu z edytora (baza bez zmian),
/// `None` — zapisany układ (nadanie, podgląd zapisanego).
pub type BulletinPreview = Arc<
    dyn Fn(Option<aars_core::bulletin_layout::Layout>) -> BulletinPreviewFuture + Send + Sync,
>;

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
            version: "0.0.0",
            update: None,
            first_admin: FirstAdmin::Console,
            local_nets: Arc::new(Vec::new()),
            host_names: Arc::new(Vec::new()),
            check_hosts: false,
            lan_only: false,
            bulletin_preview: None,
            bulletin_preview_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            warnings_map: None,
        }
    }

    pub fn with_version(mut self, version: &'static str) -> Self {
        self.version = version;
        self
    }

    pub fn with_first_admin(mut self, mode: FirstAdmin) -> Self {
        self.first_admin = mode;
        self
    }

    pub fn with_host_names(mut self, names: Vec<String>) -> Self {
        self.host_names = Arc::new(names);
        self
    }

    /// Panel odpowiada tylko pod nazwami z `host_names` i literałami IP.
    pub fn with_host_check(mut self, on: bool) -> Self {
        self.check_hosts = on;
        self
    }

    /// Panel odpowiada tylko klientom z sieci lokalnej; reszta dostaje `403`.
    pub fn with_lan_only(mut self, on: bool) -> Self {
        self.lan_only = on;
        self
    }

    pub fn with_local_nets(mut self, nets: Vec<LocalNet>) -> Self {
        self.local_nets = Arc::new(nets);
        self
    }

    pub fn with_update(mut self, update: Arc<dyn UpdateControl>) -> Self {
        self.update = Some(update);
        self
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

    pub fn with_warnings_map(mut self, map: WarningsMap) -> Self {
        self.warnings_map = Some(map);
        self
    }

    pub fn with_bulletin_preview(mut self, preview: BulletinPreview) -> Self {
        self.bulletin_preview = Some(preview);
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
                // Schemat jak `imgw-meteo` z crate'a źródeł, nazwa jak na stacji.
                all.push(aars_core::module::ModuleDescriptor {
                    name: "Ostrzeżenia meteorologiczne IMGW",
                    ..aars_core::module::sample_source("imgw-meteo")
                });
                // Źródła alarmowe etapu 11a i `pogoda` z 11b: w panelu liczy się tylko ich id.
                for id in ["rso", "swpc", "gios-smog", "emsc", "pogoda"] {
                    all.push(aars_core::module::sample_source(id));
                }
                all
            })),
            base_config: Arc::new(Config::default()),
            source_refresh: None,
            version: "0.0.0-test",
            update: None,
            first_admin: FirstAdmin::Console,
            local_nets: Arc::new(Vec::new()),
            host_names: Arc::new(Vec::new()),
            check_hosts: false,
            lan_only: false,
            bulletin_preview: None,
            bulletin_preview_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            warnings_map: None,
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
                    speech_rate: 0.87,
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

    /// Aktualizacja do testów panelu: stały stan, zadany wynik sprawdzenia
    /// i instalacji, zapisane zlecenia `(wersja, kto)`.
    pub struct FakeUpdate {
        pub status: Mutex<UpdateStatus>,
        pub available: Mutex<Result<Option<Available>, UpdateRefusal>>,
        pub install_result: Mutex<Result<(), UpdateRefusal>>,
        pub installs: Mutex<Vec<(String, String)>>,
    }

    impl FakeUpdate {
        /// Działa `current`, bez poprzedniej, bez próby; brak nowszej wersji,
        /// a instalacja się udaje.
        pub fn new(current: &str) -> Self {
            Self {
                status: Mutex::new(UpdateStatus {
                    current: current.to_owned(),
                    previous: None,
                    trial: None,
                    unsupported: None,
                    last: None,
                }),
                available: Mutex::new(Ok(None)),
                install_result: Mutex::new(Ok(())),
                installs: Mutex::new(Vec::new()),
            }
        }
    }

    impl UpdateControl for FakeUpdate {
        fn status(&self) -> UpdateStatus {
            self.status.lock().unwrap().clone()
        }
        fn check(&self) -> UpdateFuture<'_, Result<Option<Available>, UpdateRefusal>> {
            Box::pin(async move { self.available.lock().unwrap().clone() })
        }
        fn install(&self, version: &str, by: &str) -> UpdateFuture<'_, Result<(), UpdateRefusal>> {
            let entry = (version.to_owned(), by.to_owned());
            Box::pin(async move {
                let result = self.install_result.lock().unwrap().clone();
                if result.is_ok() {
                    self.installs.lock().unwrap().push(entry);
                }
                result
            })
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
