use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use aars_core::area::AreaEntry;
use aars_core::geo::StationPosition;
use aars_core::module::Needs;
use aars_core::priority::Priority;
use chrono::{DateTime, Utc};

pub type FetchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SourceOutput, SourceError>> + Send + 'a>>;

/// Źródło jest czystą funkcją odpowiedzi → pozycje; porównanie z bazą robi
/// `SourceRunner` (R2).
pub trait DataSource: Send + Sync {
    fn id(&self) -> &'static str;
    fn default_interval(&self) -> Duration;
    /// Czego źródło potrzebuje, żeby cykl miał sens (A1); bez tego runner
    /// pomija cykl.
    fn needs(&self) -> Needs {
        Needs::Area
    }
    /// Zakres R9 (A2): pozycje śledzone przy innym zakresie znikają po cichu.
    fn scope(&self, ctx: &SourceCtx) -> String {
        ctx.area_text.clone()
    }
    fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a>;
}

pub struct SourceCtx {
    pub now: DateTime<Utc>,
    pub area: Vec<AreaEntry>,
    /// Kanoniczny tekst obszaru (jak w `source_state`).
    pub area_text: String,
    /// Pozycja stacji (R20); `None`, gdy nieustawiona albo nieczytelna.
    pub position: Option<StationPosition>,
    /// Ustawienia modułu bez prefiksu `source:{id}:`.
    pub settings: BTreeMap<String, String>,
    /// Klucze aktywnych pozycji tego źródła (A4), sprzed tego cyklu.
    pub tracked_keys: HashSet<String>,
}

pub struct SourceOutput {
    pub items: Vec<SourceItem>,
    /// `false`, gdy część odpowiedzi była nieczytelna: brak pozycji nie
    /// oznacza wtedy odwołania.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceItem {
    /// Tożsamość (np. id ostrzeżenia).
    pub key: String,
    /// Zjawisko — łączy stare i nowe id tego samego ostrzeżenia.
    pub topic: String,
    /// Zmiana odcisku to nowe zdarzenie.
    pub fingerprint: String,
    pub text: String,
    pub change_text: String,
    pub cancel_text: String,
    /// Wiersz do panelu.
    pub summary: String,
    pub priority: Priority,
    pub interrupting: bool,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("błąd połączenia: {0}")]
    Http(String),
    #[error("serwer odpowiedział kodem {0}")]
    Status(u16),
    #[error("odpowiedź większa niż {0} bajtów")]
    TooLarge(usize),
    #[error("nieczytelna odpowiedź: {0}")]
    Parse(String),
}
