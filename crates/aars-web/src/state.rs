use std::sync::Arc;

use crate::preview::PreviewRenderer;

use aars_core::interlock::InMemoryInterlocks;
use aars_core::queue::QueueRepo;
use sqlx::SqlitePool;

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
}

/// Wywołanie zdejmujące lepką flagę strażnika (`WatchdogMarker::clear_poison`).
pub type WatchdogClear = Arc<dyn Fn() + Send + Sync>;

impl AppState {
    pub fn new(
        pool: SqlitePool,
        queue: Arc<QueueRepo>,
        interlocks: Arc<InMemoryInterlocks>,
        secure_cookies: bool,
    ) -> Self {
        Self { pool, queue, interlocks, secure_cookies, preview: None, watchdog_clear: None }
    }

    pub fn with_watchdog_clear(mut self, clear: WatchdogClear) -> Self {
        self.watchdog_clear = Some(clear);
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
        }
    }
}
