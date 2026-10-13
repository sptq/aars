use std::sync::Arc;

use aars_core::config::{PttBackendKind, PttConfig};
use aars_core::ptt::{PttBackend, PttError};

pub mod gpio;
pub mod serial;
pub mod vox;

/// Buduje backend PTT z konfiguracji. Błąd tutaj zatrzymuje start stacji —
/// nadawanie bez pewności, co steruje nadajnikiem, jest nieodpowiedzialne.
pub fn build(config: &PttConfig) -> Result<Arc<dyn PttBackend>, PttError> {
    match config.backend {
        PttBackendKind::Vox => Ok(Arc::new(vox::VoxPtt)),
        PttBackendKind::Serial => {
            Ok(Arc::new(serial::SerialPtt::open(&config.serial.port, config.serial.line)?))
        }
        PttBackendKind::Gpio => {
            #[cfg(target_os = "linux")]
            {
                Ok(Arc::new(gpio::GpioPtt::open(&config.gpio)?))
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(PttError::Unsupported(
                    "backend GPIO wymaga Linuksa z /dev/gpiochip*".to_owned(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::config::{Config, PttBackendKind};

    #[test]
    fn vox_backend_is_always_available_and_never_touches_hardware() {
        let mut config = Config::default().ptt;
        config.backend = PttBackendKind::Vox;

        let ptt = build(&config).unwrap();
        assert_eq!(ptt.name(), "vox");
        ptt.key().unwrap();
        ptt.unkey().unwrap();
        ptt.force_unkey();
        assert_eq!(ptt.channel_busy(), None);
    }

    #[test]
    fn a_serial_backend_on_a_missing_port_fails_with_a_clear_message() {
        let mut config = Config::default().ptt;
        config.backend = PttBackendKind::Serial;
        config.serial.port = "/dev/nie-ma-takiego-portu".to_owned();

        // `unwrap_err` wymagałoby Debug na `Arc<dyn PttBackend>`, którego
        // trait obiektowy nie ma.
        let Err(err) = build(&config) else {
            panic!("otwarcie nieistniejącego portu musi się nie udać");
        };
        assert!(err.to_string().contains("nie-ma-takiego-portu"), "{err}");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn the_gpio_backend_reports_itself_unsupported_off_linux() {
        let mut config = Config::default().ptt;
        config.backend = PttBackendKind::Gpio;

        let Err(err) = build(&config) else {
            panic!("GPIO poza Linuksem nie może się zbudować");
        };
        assert!(matches!(err, aars_core::ptt::PttError::Unsupported(_)), "{err:?}");
    }
}
