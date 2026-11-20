//! Źródła danych (spec §7): trait `DataSource` i ogólny `SourceRunner`, który
//! pilnuje idempotencji, odwołań i wycofywania po błędach.

pub mod runner;
pub mod source;

pub use runner::{CycleReport, SourceRunner, next_delay};
pub use source::{DataSource, FetchFuture, SourceCtx, SourceError, SourceItem, SourceOutput};
