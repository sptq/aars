//! Źródła danych (spec §7): trait `DataSource` i ogólny `SourceRunner`, który
//! pilnuje idempotencji, odwołań i wycofywania po błędach.

pub mod bulletin;
pub mod emsc;
pub mod forecast;
pub mod gios;
pub mod http;
pub mod imgw_hydro;
pub mod imgw_meteo;
pub mod imgw_wodowskazy;
pub mod pogoda;
pub mod propagacja;
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

fn build_imgw_hydro(config: &Config) -> Result<Arc<dyn DataSource>, SourceError> {
    Ok(Arc::new(imgw_hydro::ImgwHydro::new(&config.sources.imgw_hydro_url)?))
}

fn build_imgw_wodowskazy(config: &Config) -> Result<Arc<dyn DataSource>, SourceError> {
    Ok(Arc::new(imgw_wodowskazy::ImgwWodowskazy::new(&config.sources.imgw_hydro_stations_url)?))
}

fn build_rso(config: &Config) -> Result<Arc<dyn DataSource>, SourceError> {
    Ok(Arc::new(rso::Rso::new(&config.sources.rso_url)?))
}

fn build_swpc(config: &Config) -> Result<Arc<dyn DataSource>, SourceError> {
    Ok(Arc::new(swpc::Swpc::new(&config.sources.swpc_url)?))
}

fn build_gios(config: &Config) -> Result<Arc<dyn DataSource>, SourceError> {
    Ok(Arc::new(gios::Gios::new(&config.sources.gios_url)?))
}

fn build_emsc(config: &Config) -> Result<Arc<dyn DataSource>, SourceError> {
    Ok(Arc::new(emsc::Emsc::new(&config.sources.emsc_url)?))
}

fn build_pogoda(config: &Config) -> Result<Arc<dyn DataSource>, SourceError> {
    Ok(Arc::new(pogoda::Pogoda::new(&config.sources)?))
}

/// Wszystkie źródła tej binarki; nowe źródło to wpis tutaj.
pub fn modules() -> Vec<SourceModule> {
    vec![
        SourceModule { descriptor: imgw_meteo::DESCRIPTOR, build: build_imgw_meteo },
        SourceModule { descriptor: rso::DESCRIPTOR, build: build_rso },
        SourceModule { descriptor: swpc::DESCRIPTOR, build: build_swpc },
        SourceModule { descriptor: gios::DESCRIPTOR, build: build_gios },
        SourceModule { descriptor: emsc::DESCRIPTOR, build: build_emsc },
        SourceModule { descriptor: pogoda::DESCRIPTOR, build: build_pogoda },
        SourceModule { descriptor: imgw_hydro::DESCRIPTOR, build: build_imgw_hydro },
        SourceModule { descriptor: imgw_wodowskazy::DESCRIPTOR, build: build_imgw_wodowskazy },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::module::SettingKind;

    #[test]
    fn all_sources_are_registered_in_order() {
        let all = modules();
        let ids: Vec<_> = all.iter().map(|m| m.descriptor.id).collect();
        assert_eq!(ids, ["imgw-meteo", "rso", "swpc", "gios-smog", "emsc", "pogoda", "imgw-hydro", "imgw-wodowskazy"]);
        for m in &all {
            let built = (m.build)(&Config::default()).unwrap();
            assert_eq!(built.id(), m.descriptor.id);
            assert_eq!(built.needs(), m.descriptor.needs, "{}", m.descriptor.id);
            assert!(!m.descriptor.default_enabled, "{}", m.descriptor.id);
        }
    }

    #[test]
    fn every_descriptor_interval_fits_the_runner_range() {
        for m in modules() {
            let spec = m
                .descriptor
                .settings
                .iter()
                .find(|s| s.key == "interval_s")
                .unwrap_or_else(|| panic!("{} nie ma interval_s", m.descriptor.id));
            let SettingKind::Integer { min, max } = spec.kind else {
                panic!("{}: interval_s nie jest liczbą całkowitą", m.descriptor.id);
            };
            let default: i64 = spec.default.parse().unwrap();
            assert!(min >= 60 && max <= 3600, "{}", m.descriptor.id);
            assert!((min..=max).contains(&default), "{}", m.descriptor.id);
        }
    }
}
