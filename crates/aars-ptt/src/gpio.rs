#[cfg(target_os = "linux")]
mod linux {
    use aars_core::config::GpioConfig;
    use aars_core::ptt::{PttBackend, PttError};
    use gpiocdev::line::{Bias, Value};
    use gpiocdev::request::Request;

    /// PTT przez pin GPIO. Linia wyjściowa inicjowana w stanie „nie nadaje",
    /// opcjonalna linia wejściowa blokuje nadawanie (spec §4.4 krok 3).
    pub struct GpioPtt {
        request: Request,
        line: u32,
        active_low: bool,
        inhibit: Option<(Request, u32)>,
    }

    impl GpioPtt {
        pub fn open(config: &GpioConfig) -> Result<Self, PttError> {
            let inactive = if config.active_low { Value::Active } else { Value::Inactive };

            let request = Request::builder()
                .on_chip(&config.chip)
                .with_line(config.line)
                .as_output(inactive)
                .request()
                .map_err(|e| {
                    PttError::Hardware(format!(
                        "otwarcie linii {} na {}: {e}",
                        config.line, config.chip
                    ))
                })?;

            let inhibit = match config.inhibit_line {
                None => None,
                Some(line) => {
                    let req = Request::builder()
                        .on_chip(&config.chip)
                        .with_line(line)
                        .as_input()
                        .with_bias(Bias::PullUp)
                        .request()
                        .map_err(|e| {
                            PttError::Hardware(format!("otwarcie linii blokującej {line}: {e}"))
                        })?;
                    Some((req, line))
                }
            };

            Ok(Self { request, line: config.line, active_low: config.active_low, inhibit })
        }

        fn value(&self, keyed: bool) -> Value {
            if keyed != self.active_low { Value::Active } else { Value::Inactive }
        }

        /// Czy fizyczne wejście blokuje nadawanie.
        pub fn inhibited(&self) -> bool {
            match &self.inhibit {
                None => false,
                Some((req, line)) => {
                    // Zwarcie do masy (Inactive przy pull-upie) znaczy blokadę.
                    matches!(req.value(*line), Ok(Value::Inactive))
                }
            }
        }
    }

    impl PttBackend for GpioPtt {
        fn key(&self) -> Result<(), PttError> {
            self.request
                .set_value(self.line, self.value(true))
                .map_err(|e| PttError::Hardware(format!("podniesienie PTT: {e}")))
        }

        fn unkey(&self) -> Result<(), PttError> {
            self.request
                .set_value(self.line, self.value(false))
                .map_err(|e| PttError::Hardware(format!("opuszczenie PTT: {e}")))
        }

        fn force_unkey(&self) {
            // Jeden zapis do deskryptora, bez alokacji i bez logowania.
            let _ = self.request.set_value(self.line, self.value(false));
        }

        fn name(&self) -> &'static str {
            "gpio"
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::GpioPtt;
