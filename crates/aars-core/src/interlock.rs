use std::sync::Mutex;

use crate::priority::Priority;

/// Dlaczego sekwencer odmówił nadania. Odmowa nie gubi komunikatu — wraca
/// do kolejki z tym powodem widocznym w panelu (spec §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// Watchdog musiał interweniować; wymagana decyzja człowieka.
    Poisoned,
    /// Globalny kill switch.
    TxDisabled,
    /// Fizyczne wejście blokujące.
    Inhibited,
    /// Okno ciszy, a priorytet za niski.
    QuietHours,
    /// Kanał zajęty (w MVP nieosiągalne — backendy zwracają `None`).
    ChannelBusy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterlockState {
    pub poisoned: bool,
    pub tx_enabled: bool,
    pub inhibited: bool,
    pub quiet_now: bool,
    pub quiet_min_priority: Priority,
}

pub trait Interlocks: Send + Sync {
    fn snapshot(&self) -> InterlockState;

    /// Sprawdza wszystkie blokady w kolejności z §4.4 speca.
    fn check(&self, priority: Priority) -> Result<(), RefusalReason> {
        let state = self.snapshot();
        if state.poisoned {
            return Err(RefusalReason::Poisoned);
        }
        if !state.tx_enabled {
            return Err(RefusalReason::TxDisabled);
        }
        if state.inhibited {
            return Err(RefusalReason::Inhibited);
        }
        if state.quiet_now && state.quiet_min_priority.outranks(priority) {
            return Err(RefusalReason::QuietHours);
        }
        Ok(())
    }
}

/// Interlocki trzymane w pamięci: do testów i jako cache stanu z bazy.
pub struct InMemoryInterlocks {
    state: Mutex<InterlockState>,
}

impl InMemoryInterlocks {
    /// Stan, w którym wszystko wolno — punkt wyjścia dla testów.
    pub fn permissive() -> Self {
        Self {
            state: Mutex::new(InterlockState {
                poisoned: false,
                tx_enabled: true,
                inhibited: false,
                quiet_now: false,
                quiet_min_priority: Priority::Emergency,
            }),
        }
    }

    pub fn set_tx_enabled(&self, value: bool) {
        self.state.lock().expect("mutex interlocków").tx_enabled = value;
    }

    pub fn set_poisoned(&self, value: bool) {
        self.state.lock().expect("mutex interlocków").poisoned = value;
    }

    pub fn set_inhibited(&self, value: bool) {
        self.state.lock().expect("mutex interlocków").inhibited = value;
    }

    pub fn set_quiet(&self, quiet_now: bool, min_priority: Priority) {
        let mut state = self.state.lock().expect("mutex interlocków");
        state.quiet_now = quiet_now;
        state.quiet_min_priority = min_priority;
    }
}

impl Interlocks for InMemoryInterlocks {
    fn snapshot(&self) -> InterlockState {
        *self.state.lock().expect("mutex interlocków")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interlocks_are_checked_in_the_order_from_the_spec() {
        let locks = InMemoryInterlocks::permissive();
        assert!(locks.check(Priority::Low).is_ok());

        // Poisoned bije wszystko, także Emergency.
        locks.set_poisoned(true);
        locks.set_tx_enabled(false);
        assert_eq!(locks.check(Priority::Emergency), Err(RefusalReason::Poisoned));

        locks.set_poisoned(false);
        assert_eq!(locks.check(Priority::Emergency), Err(RefusalReason::TxDisabled));

        locks.set_tx_enabled(true);
        locks.set_inhibited(true);
        assert_eq!(locks.check(Priority::Emergency), Err(RefusalReason::Inhibited));
    }

    #[test]
    fn quiet_hours_compare_priorities_by_rank_not_by_number() {
        let locks = InMemoryInterlocks::permissive();
        locks.set_quiet(true, Priority::High);

        assert_eq!(locks.check(Priority::Normal), Err(RefusalReason::QuietHours));
        assert_eq!(locks.check(Priority::Low), Err(RefusalReason::QuietHours));
        assert!(locks.check(Priority::High).is_ok(), "próg jest włączny");
        assert!(locks.check(Priority::Emergency).is_ok());
    }
}
