use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::preview::PreviewRenderer;

use aars_core::dispatcher::{StationTest, TestOutcome};
use aars_core::hardware::{ApplyStatus, HardwareSettings};
use aars_core::interlock::InMemoryInterlocks;
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
    /// Sprzęt z `aars.toml` do odczytu na stronie ustawień: (nazwa, wartość).
    pub hardware: Vec<(String, String)>,
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
            hardware: Vec::new(),
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

    pub fn with_hardware(mut self, hardware: Vec<(String, String)>) -> Self {
        self.hardware = hardware;
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
            hardware: Vec::new(),
            source_refresh: None,
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
