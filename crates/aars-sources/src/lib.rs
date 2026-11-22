//! Źródła danych (spec §7): trait `DataSource` i ogólny `SourceRunner`, który
//! pilnuje idempotencji, odwołań i wycofywania po błędach.

pub mod emsc;
pub mod gios;
pub mod http;
pub mod imgw_meteo;
pub mod rso;
pub mod runner;
pub mod source;
pub mod speech;
pub mod swpc;

pub use runner::{CycleReport, SourceRunner, next_delay};
pub use source::{DataSource, FetchFuture, SourceCtx, SourceError, SourceItem, SourceOutput};

use std::sync::Arc;

use aars_core::config::Config;
use aars_core::module::ModuleDescriptor;

/// Źródło jako wtyczka (R20): opis dla panelu plus fabryka źródła.
pub struct SourceModule {
    pub descriptor: ModuleDescriptor,
    pub build: fn(&Config) -> Result<Arc<dyn DataSource>, SourceError>,
}

fn build_imgw_meteo(config: &Config) -> Result<Arc<dyn DataSource>, SourceError> {
    Ok(Arc::new(imgw_meteo::ImgwMeteo::new(&config.sources.imgw_meteo_url)?))
}

/// Wszystkie źródła tej binarki; nowe źródło to wpis tutaj.
pub fn modules() -> Vec<SourceModule> {
    vec![SourceModule { descriptor: imgw_meteo::DESCRIPTOR, build: build_imgw_meteo }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imgw_meteo_is_registered_as_a_source_module() {
        let all = modules();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].descriptor.id, "imgw-meteo");
        let built = (all[0].build)(&Config::default()).unwrap();
        assert_eq!(built.id(), "imgw-meteo");
    }
}
