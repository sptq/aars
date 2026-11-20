use std::sync::Mutex;

use aars_core::config::SerialPttLine;
use aars_core::ptt::{PttBackend, PttError};

/// PTT przez podniesienie DTR lub RTS na porcie szeregowym — klasyka
/// interfejsów typu SignaLink i kabli CAT.
pub struct SerialPtt {
    port: Mutex<Box<dyn serialport::SerialPort>>,
    line: SerialPttLine,
}

impl SerialPtt {
    pub fn open(path: &str, line: SerialPttLine) -> Result<Self, PttError> {
        let port = serialport::new(path, 9600)
            .timeout(std::time::Duration::from_millis(100))
            .open()
            .map_err(|e| PttError::Hardware(format!("otwarcie portu {path}: {e}")))?;

        let backend = Self { port: Mutex::new(port), line };
        // Stan początkowy musi być „nie nadaje" — nie zakładamy, w jakim
        // stanie zostawił port poprzedni proces.
        backend.set(false)?;
        Ok(backend)
    }

    fn set(&self, keyed: bool) -> Result<(), PttError> {
        let mut port = self
            .port
            .lock()
            .map_err(|_| PttError::Hardware("mutex portu zatruty".to_owned()))?;
        let result = match self.line {
            SerialPttLine::Dtr => port.write_data_terminal_ready(keyed),
            SerialPttLine::Rts => port.write_request_to_send(keyed),
        };
        result.map_err(|e| PttError::Hardware(format!("ustawienie linii PTT: {e}")))
    }
}

impl PttBackend for SerialPtt {
    fn key(&self) -> Result<(), PttError> {
        self.set(true)
    }

    fn unkey(&self) -> Result<(), PttError> {
        self.set(false)
    }

    fn force_unkey(&self) {
        // Awaryjnie: `try_lock`, by nigdy nie zablokować watchdoga. Gdy mutex
        // jest zajęty, zapis i tak trafi do deskryptora przy zwolnieniu przez
        // właściciela, który kończy nadanie.
        if let Ok(mut port) = self.port.try_lock() {
            let _ = match self.line {
                SerialPttLine::Dtr => port.write_data_terminal_ready(false),
                SerialPttLine::Rts => port.write_request_to_send(false),
            };
        }
    }

    fn name(&self) -> &'static str {
        "serial"
    }
}
