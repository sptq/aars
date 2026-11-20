use aars_core::ptt::{PttBackend, PttError};

/// Brak sterowania: radio nadaje na VOX, wyzwalane samym sygnałem audio.
/// Przerwy przed i po treści (`tx_tail_ms`) dają VOX-owi czas na reakcję.
pub struct VoxPtt;

impl PttBackend for VoxPtt {
    fn key(&self) -> Result<(), PttError> {
        tracing::debug!("PTT vox: nic do zrobienia, nadawanie wyzwoli audio");
        Ok(())
    }

    fn unkey(&self) -> Result<(), PttError> {
        Ok(())
    }

    fn force_unkey(&self) {}

    fn name(&self) -> &'static str {
        "vox"
    }
}
