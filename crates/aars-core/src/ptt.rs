use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, thiserror::Error)]
pub enum PttError {
    #[error("błąd sprzętu PTT: {0}")]
    Hardware(String),
    #[error("backend PTT niedostępny na tej platformie: {0}")]
    Unsupported(String),
}

pub trait PttBackend: Send + Sync {
    /// Podnosi PTT — radio przechodzi na nadawanie.
    fn key(&self) -> Result<(), PttError>;

    /// Opuszcza PTT.
    fn unkey(&self) -> Result<(), PttError>;

    /// Awaryjne opuszczenie PTT. Synchroniczne, infallible i bezpieczne z
    /// obcego wątku — watchdog nie może czekać, logować ani alokować.
    fn force_unkey(&self);

    /// Czy kanał jest zajęty. `None` znaczy „nie wiem" i w MVP zwracają to
    /// wszystkie backendy (spec §4.4).
    fn channel_busy(&self) -> Option<bool> {
        None
    }

    /// Nazwa backendu do dziennika nadań.
    fn name(&self) -> &'static str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttEvent {
    Key,
    Unkey,
    ForceUnkey,
}

/// PTT testowe: zapisuje przejścia, umie udawać awarię sprzętu.
pub struct FakePtt {
    keyed: AtomicBool,
    fail_next_key: AtomicBool,
    events: Mutex<Vec<PttEvent>>,
}

impl FakePtt {
    pub fn new() -> Self {
        Self {
            keyed: AtomicBool::new(false),
            fail_next_key: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<PttEvent> {
        self.events.lock().expect("mutex fake'a").clone()
    }

    pub fn is_keyed(&self) -> bool {
        self.keyed.load(Ordering::SeqCst)
    }

    pub fn fail_next_key(&self) {
        self.fail_next_key.store(true, Ordering::SeqCst);
    }
}

impl Default for FakePtt {
    fn default() -> Self {
        Self::new()
    }
}

impl PttBackend for FakePtt {
    fn key(&self) -> Result<(), PttError> {
        if self.fail_next_key.swap(false, Ordering::SeqCst) {
            return Err(PttError::Hardware("awaria wymuszona w teście".to_owned()));
        }
        self.keyed.store(true, Ordering::SeqCst);
        self.events.lock().expect("mutex fake'a").push(PttEvent::Key);
        Ok(())
    }

    fn unkey(&self) -> Result<(), PttError> {
        self.keyed.store(false, Ordering::SeqCst);
        self.events.lock().expect("mutex fake'a").push(PttEvent::Unkey);
        Ok(())
    }

    fn force_unkey(&self) {
        self.keyed.store(false, Ordering::SeqCst);
        if let Ok(mut events) = self.events.lock() {
            events.push(PttEvent::ForceUnkey);
        }
    }

    fn name(&self) -> &'static str {
        "fake"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_ptt_records_every_transition() {
        let ptt = FakePtt::new();
        ptt.key().unwrap();
        ptt.unkey().unwrap();
        ptt.key().unwrap();
        ptt.force_unkey();

        assert_eq!(
            ptt.events(),
            vec![PttEvent::Key, PttEvent::Unkey, PttEvent::Key, PttEvent::ForceUnkey]
        );
        assert!(!ptt.is_keyed(), "po force_unkey PTT musi być opuszczone");
    }

    #[test]
    fn fake_ptt_can_be_told_to_fail() {
        let ptt = FakePtt::new();
        ptt.fail_next_key();
        assert!(matches!(ptt.key(), Err(PttError::Hardware(_))));
        assert!(!ptt.is_keyed(), "nieudane podniesienie nie może zostawić stanu keyed");
    }

    #[test]
    fn channel_busy_is_unknown_by_default() {
        assert_eq!(FakePtt::new().channel_busy(), None);
    }
}
