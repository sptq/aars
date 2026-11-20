use aars_core::priority::Priority;
use aars_core::user::Role;
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};

use crate::auth::CurrentUser;

/// Wspólna rama każdej strony panelu: kto jest zalogowany, jaki ma token CSRF
/// i które pozycje nawigacji mu przysługują.
pub struct Chrome {
    pub username: String,
    pub role: String,
    pub csrf: String,
    pub can_operate: bool,
    pub is_admin: bool,
}

impl From<&CurrentUser> for Chrome {
    fn from(current: &CurrentUser) -> Self {
        Chrome {
            username: current.user.username.clone(),
            role: current.user.role.as_str().to_owned(),
            csrf: current.csrf_token.clone(),
            can_operate: current.user.role.can(Role::Operator),
            is_admin: current.user.role.can(Role::Admin),
        }
    }
}

pub fn priority_label(priority: Priority) -> &'static str {
    match priority {
        Priority::Emergency => "Alarmowy",
        Priority::High => "Wysoki",
        Priority::Normal => "Zwykły",
        Priority::Low => "Niski",
    }
}

pub fn priority_value(priority: Priority) -> &'static str {
    match priority {
        Priority::Emergency => "emergency",
        Priority::High => "high",
        Priority::Normal => "normal",
        Priority::Low => "low",
    }
}

pub fn parse_priority(value: &str) -> Option<Priority> {
    match value {
        "emergency" => Some(Priority::Emergency),
        "high" => Some(Priority::High),
        "normal" => Some(Priority::Normal),
        "low" => Some(Priority::Low),
        _ => None,
    }
}

/// Chwila w obu zapisach: `18:00 UTC (19:00 lokalnie)`; z datą (UTC), gdy
/// lokalna data chwili różni się od `today`.
pub fn both_times<Tz: TimeZone>(at: DateTime<Utc>, local: &Tz, today: NaiveDate) -> String {
    let local_at = at.with_timezone(local);
    let utc = if local_at.date_naive() == today {
        at.format("%H:%M UTC").to_string()
    } else {
        at.format("%Y-%m-%d %H:%M UTC").to_string()
    };
    format!("{utc} ({} lokalnie)", local_at.naive_local().format("%H:%M"))
}

/// `both_times` w strefie systemowej urządzenia i z dzisiejszą datą.
pub fn both_times_now(at: DateTime<Utc>) -> String {
    both_times(at, &Local, Local::now().date_naive())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priorities_have_polish_labels() {
        assert_eq!(priority_label(Priority::Emergency), "Alarmowy");
        assert_eq!(priority_label(Priority::High), "Wysoki");
        assert_eq!(priority_label(Priority::Normal), "Zwykły");
        assert_eq!(priority_label(Priority::Low), "Niski");
    }

    #[test]
    fn priority_value_round_trips_through_parse() {
        for p in [Priority::Emergency, Priority::High, Priority::Normal, Priority::Low] {
            assert_eq!(parse_priority(priority_value(p)), Some(p));
        }
    }

    #[test]
    fn times_are_shown_in_utc_and_local() {
        let warsaw_winter = chrono::FixedOffset::east_opt(3600).unwrap();
        let at = Utc.with_ymd_and_hms(2026, 11, 20, 18, 0, 0).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        assert_eq!(both_times(at, &warsaw_winter, today), "18:00 UTC (19:00 lokalnie)");
        let tomorrow = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();
        assert_eq!(
            both_times(at, &warsaw_winter, tomorrow),
            "2026-11-20 18:00 UTC (19:00 lokalnie)"
        );
    }
}
