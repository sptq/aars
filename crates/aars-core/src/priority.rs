use serde::{Deserialize, Serialize};

/// Niższa liczba znaczy wyższy priorytet — tak samo w bazie i w kolejce,
/// żeby `ORDER BY priority` i porównania w kodzie zgadzały się bez konwersji.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(i64)]
pub enum Priority {
    Emergency = 0,
    High = 1,
    Normal = 2,
    Low = 3,
}

impl Priority {
    /// Czy `self` jest ściśle wyższym priorytetem niż `other`.
    pub fn outranks(self, other: Priority) -> bool {
        (self as i64) < (other as i64)
    }

    pub fn from_i64(value: i64) -> Option<Self> {
        match value {
            0 => Some(Self::Emergency),
            1 => Some(Self::High),
            2 => Some(Self::Normal),
            3 => Some(Self::Low),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_number_means_higher_priority() {
        assert!(Priority::Emergency.outranks(Priority::High));
        assert!(Priority::High.outranks(Priority::Normal));
        assert!(!Priority::Normal.outranks(Priority::Normal));
        assert!(!Priority::Low.outranks(Priority::Emergency));
    }

    #[test]
    fn priority_round_trips_through_its_database_representation() {
        for p in [Priority::Emergency, Priority::High, Priority::Normal, Priority::Low] {
            assert_eq!(Priority::from_i64(p as i64), Some(p));
        }
        assert_eq!(Priority::from_i64(7), None);
    }
}
