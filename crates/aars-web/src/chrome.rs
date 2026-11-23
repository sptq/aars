use aars_core::priority::Priority;
use aars_core::user::Role;
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};

use crate::auth::CurrentUser;
use crate::state::AppState;
use crate::status::{StatusBar, load_status, safe_back};

/// Wspólna rama każdej strony panelu: kto jest zalogowany, jaki ma token CSRF,
/// które pozycje nawigacji mu przysługują i co pokazuje pasek stanu.
#[derive(Clone)]
pub struct Chrome {
    pub username: String,
    /// Polska nazwa roli do plakietki w nagłówku.
    pub role: &'static str,
    pub csrf: String,
    pub can_operate: bool,
    pub is_admin: bool,
    /// Wersja produktu w stopce (każda rola, R17).
    pub version: &'static str,
    /// Ścieżka żądania.
    pub path: String,
    /// Dokąd wraca wyłącznik nadawania: ścieżka strony dla `GET`, inaczej `/`
    /// (strona po `POST` nie ma pod swoim adresem `GET`, D6).
    pub back: String,
    /// Dane paska stanu; `None` we fragmentach, które paska nie renderują.
    pub status: Option<StatusBar>,
    /// Klucz bieżącej pozycji menu (`nav_section`).
    pub section: &'static str,
    /// Hasło tymczasowe (D8): rama bez menu i paska stanu.
    pub locked: bool,
}

impl Chrome {
    /// Rama bez paska stanu — tylko dla fragmentów htmx.
    pub fn bare(current: &CurrentUser) -> Chrome {
        Chrome {
            username: current.user.username.clone(),
            role: role_label(current.user.role),
            csrf: current.csrf_token.clone(),
            can_operate: current.user.role.can(Role::Operator),
            is_admin: current.user.role.can(Role::Admin),
            version: current.version,
            path: current.path.clone(),
            back: if current.is_get {
                safe_back(Some(&current.path)).to_owned()
            } else {
                "/".to_owned()
            },
            status: None,
            section: nav_section(&current.path),
            locked: current.user.must_change_password,
        }
    }

    /// Rama strony z paskiem stanu (odczyt lokalny, nigdy nie zawodzi).
    pub async fn load(state: &AppState, current: &CurrentUser) -> Chrome {
        Chrome {
            status: Some(load_status(state, Local::now()).await),
            ..Chrome::bare(current)
        }
    }
}

/// Pozycja menu bocznego, do której należy ścieżka (`aria-current`);
/// ścieżka spoza menu → `""`.
pub fn nav_section(path: &str) -> &'static str {
    let under = |prefix: &str| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    };
    if path == "/" {
        "dashboard"
    } else if under("/settings/hardware") {
        "hardware"
    } else if under("/settings/update") {
        "update"
    } else if path == "/settings" || under("/settings/position") {
        "settings"
    } else {
        [
            ("/compose", "compose"),
            ("/messages", "messages"),
            ("/queue", "queue"),
            ("/sources", "sources"),
            ("/modules", "modules"),
            ("/map", "map"),
            ("/users", "users"),
            ("/logs", "logs"),
        ]
        .into_iter()
        .find(|(prefix, _)| under(prefix))
        .map_or("", |(_, key)| key)
    }
}

pub fn role_label(role: Role) -> &'static str {
    match role {
        Role::Viewer => "Podgląd",
        Role::Operator => "Operator",
        Role::Admin => "Administrator",
    }
}

/// Polska nazwa akcji z `audit_log`; nieznany klucz zostaje bez zmian.
pub fn audit_action_label(action: &str) -> &str {
    match action {
        "compose" => "nowe nadanie",
        "tx_enabled" => "nadawanie wł./wył.",
        "queue_cancel" => "anulowanie zadania",
        "queue_cancel_group" => "anulowanie powtórzeń",
        "message_create" => "nowa definicja",
        "message_update" => "zmiana definicji",
        "message_enabled" => "definicja wł./wył.",
        "message_delete" => "usunięcie definicji",
        "message_send" => "nadanie definicji",
        "password_change" => "zmiana hasła",
        "settings" => "zmiana ustawień",
        "user_create" => "nowy użytkownik",
        "user_role" => "zmiana roli",
        "user_disabled" => "konto wł./wył.",
        "user_reset" => "reset hasła",
        "user_delete" => "usunięcie użytkownika",
        "watchdog_clear" => "zdjęcie blokady watchdoga",
        "source_enabled" => "źródło wł./wył.",
        "source_refresh" => "odświeżenie źródła",
        "source_settings" => "ustawienia źródła",
        "hardware" => "zmiana sprzętu",
        "hardware_apply_failed" => "sprzęt nie wstał",
        "station_restart" => "restart stacji",
        "test_ptt" => "test PTT",
        "test_audio" => "test audio",
        "position" => "pozycja stacji",
        "module_enabled" => "moduł wł./wył.",
        "module_settings" => "ustawienia modułu",
        "update_install" => "aktualizacja zlecona",
        "update_installed" => "aktualizacja zainstalowana",
        "update_rolled_back" => "aktualizacja cofnięta",
        "update_failed" => "aktualizacja nieudana",
        "first_admin" => "pierwsze konto admina",
        "admin_reset" => "reset kont admina",
        other => other,
    }
}

/// Polska odmiana rzeczownika przy liczebniku: 1 → `one`, 2–4 (poza 12–14) →
/// `few`, reszta → `many`.
pub fn plural<'a>(n: u64, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) {
        few
    } else {
        many
    }
}

/// Polska nazwa źródła danych; nieznany id zostaje bez zmian.
pub fn source_label(id: &str) -> &str {
    match id {
        "imgw-meteo" => "Ostrzeżenia meteorologiczne IMGW",
        "imgw-hydro" => "Ostrzeżenia hydrologiczne IMGW",
        "imgw-wodowskazy" => "Stany wodowskazów IMGW",
        "rso" => "Regionalny System Ostrzegania (RSO)",
        "swpc" => "Pogoda kosmiczna NOAA",
        "gios-smog" => "Alerty smogowe GIOŚ",
        "emsc" => "Wstrząsy sejsmiczne (EMSC)",
        "pogoda" => "Pogoda (Open-Meteo)",
        other => other,
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

/// Chwila w obu zapisach: `18:00 UTC (19:00 lokalnie)`. Data (UTC) dochodzi,
/// gdy lokalna data chwili różni się od `today`; gdy daty UTC i lokalna się
/// różnią (okolice północy), każda stoi przy swojej godzinie:
/// `2026-11-20 23:30 UTC (2026-11-21 00:30 lokalnie)`.
pub fn both_times<Tz: TimeZone>(at: DateTime<Utc>, local: &Tz, today: NaiveDate) -> String {
    let local_at = at.with_timezone(local).naive_local();
    if local_at.date() != at.date_naive() {
        format!(
            "{} ({} lokalnie)",
            at.format("%Y-%m-%d %H:%M UTC"),
            local_at.format("%Y-%m-%d %H:%M")
        )
    } else if local_at.date() != today {
        format!("{} ({} lokalnie)", at.format("%Y-%m-%d %H:%M UTC"), local_at.format("%H:%M"))
    } else {
        format!("{} ({} lokalnie)", at.format("%H:%M UTC"), local_at.format("%H:%M"))
    }
}

/// `both_times` w strefie systemowej urządzenia i z dzisiejszą datą.
pub fn both_times_now(at: DateTime<Utc>) -> String {
    both_times(at, &Local, Local::now().date_naive())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_and_audit_actions_have_polish_labels() {
        assert_eq!(role_label(Role::Viewer), "Podgląd");
        assert_eq!(role_label(Role::Operator), "Operator");
        assert_eq!(role_label(Role::Admin), "Administrator");
        assert_eq!(audit_action_label("watchdog_clear"), "zdjęcie blokady watchdoga");
        assert_eq!(audit_action_label("user_role"), "zmiana roli");
        assert_eq!(audit_action_label("cos_nowego"), "cos_nowego");
        assert_eq!(audit_action_label("source_enabled"), "źródło wł./wył.");
        assert_eq!(audit_action_label("source_refresh"), "odświeżenie źródła");
        assert_eq!(audit_action_label("source_settings"), "ustawienia źródła");
        for (key, label) in [
            ("hardware", "zmiana sprzętu"),
            ("hardware_apply_failed", "sprzęt nie wstał"),
            ("station_restart", "restart stacji"),
            ("test_ptt", "test PTT"),
            ("test_audio", "test audio"),
            ("position", "pozycja stacji"),
            ("module_enabled", "moduł wł./wył."),
            ("module_settings", "ustawienia modułu"),
        ] {
            assert_eq!(audit_action_label(key), label);
        }
        assert_eq!(source_label("imgw-meteo"), "Ostrzeżenia meteorologiczne IMGW");
        assert_eq!(source_label("imgw-hydro"), "Ostrzeżenia hydrologiczne IMGW");
        assert_eq!(source_label("imgw-wodowskazy"), "Stany wodowskazów IMGW");
        assert_eq!(source_label("rso"), "Regionalny System Ostrzegania (RSO)");
        assert_eq!(source_label("swpc"), "Pogoda kosmiczna NOAA");
        assert_eq!(source_label("gios-smog"), "Alerty smogowe GIOŚ");
        assert_eq!(source_label("emsc"), "Wstrząsy sejsmiczne (EMSC)");
        assert_eq!(source_label("pogoda"), "Pogoda (Open-Meteo)");
        assert_eq!(source_label("inne"), "inne");
    }

    #[test]
    fn nav_section_maps_paths_to_menu_entries() {
        assert_eq!(nav_section("/"), "dashboard");
        assert_eq!(nav_section("/messages/3/edit"), "messages");
        assert_eq!(nav_section("/settings"), "settings");
        assert_eq!(nav_section("/settings/position"), "settings");
        assert_eq!(nav_section("/settings/hardware"), "hardware");
        assert_eq!(nav_section("/settings/update"), "update");
        assert_eq!(nav_section("/account/password"), "");
        for (path, key) in [
            ("/compose", "compose"),
            ("/messages", "messages"),
            ("/messages/new", "messages"),
            ("/queue", "queue"),
            ("/sources", "sources"),
            ("/modules", "modules"),
            ("/map", "map"),
            ("/settings/hardware/test-ptt", "hardware"),
            ("/settings/update/check", "update"),
            ("/users", "users"),
            ("/logs", "logs"),
            ("/messagesx", ""),
            ("/settingsx", ""),
            ("/fragments/status", ""),
        ] {
            assert_eq!(nav_section(path), key, "{path}");
        }
    }

    #[test]
    fn plural_follows_the_polish_rule() {
        let f = |n| plural(n, "wpis", "wpisy", "wpisów");
        for (n, want) in [(1, "wpis"), (2, "wpisy"), (4, "wpisy"), (5, "wpisów"), (12, "wpisów"),
            (14, "wpisów"), (22, "wpisy"), (25, "wpisów"), (112, "wpisów")] {
            assert_eq!(f(n), want, "{n}");
        }
    }

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

    #[test]
    fn around_midnight_each_date_stands_next_to_its_own_time() {
        let warsaw_winter = chrono::FixedOffset::east_opt(3600).unwrap();
        let at = Utc.with_ymd_and_hms(2026, 11, 20, 23, 30, 0).unwrap();
        let expected = "2026-11-20 23:30 UTC (2026-11-21 00:30 lokalnie)";
        for today in [(2026, 11, 20), (2026, 11, 21)] {
            let today = NaiveDate::from_ymd_opt(today.0, today.1, today.2).unwrap();
            assert_eq!(both_times(at, &warsaw_winter, today), expected);
        }
    }
}
