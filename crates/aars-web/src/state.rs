use std::sync::Arc;

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
}

impl AppState {
    pub fn new(
        pool: SqlitePool,
        queue: Arc<QueueRepo>,
        interlocks: Arc<InMemoryInterlocks>,
        secure_cookies: bool,
    ) -> Self {
        Self { pool, queue, interlocks, secure_cookies }
    }

    /// Stan na potrzeby testów: własna kolejka, interlocki przepuszczające.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_tests(pool: SqlitePool) -> Self {
        Self {
            queue: Arc::new(QueueRepo::new(pool.clone())),
            interlocks: Arc::new(InMemoryInterlocks::permissive()),
            pool,
            secure_cookies: false,
        }
    }
}
