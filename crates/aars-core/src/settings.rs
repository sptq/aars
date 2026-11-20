//! Parametry stacji z tabeli `settings` (spec §9) i okno ciszy nocnej.

use chrono::NaiveTime;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::ident::IdMode;
use crate::priority::Priority;

/// Okno ciszy w czasie lokalnym; może przechodzić przez północ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    pub start: NaiveTime,
    pub end: NaiveTime,
}

impl QuietHours {
    /// `"HH:MM-HH:MM"`.
    pub fn parse(text: &str) -> Result<QuietHours, String> {
        let bad = || format!("okno ciszy „{text}” ma mieć postać HH:MM-HH:MM");
        let (start, end) = text.trim().split_once('-').ok_or_else(bad)?;
        let time = |part: &str| NaiveTime::parse_from_str(part.trim(), "%H:%M").map_err(|_| bad());
        let (start, end) = (time(start)?, time(end)?);
        if start == end {
            return Err("początek i koniec okna ciszy nie mogą być równe".to_owned());
        }
        Ok(QuietHours { start, end })
    }

    /// Początek włącznie, koniec wyłącznie.
    pub fn contains(&self, t: NaiveTime) -> bool {
        if self.start < self.end {
            self.start <= t && t < self.end
        } else {
            t >= self.start || t < self.end
        }
    }

    pub fn as_text(&self) -> String {
        format!(
            "{}-{}",
            self.start.format("%H:%M"),
            self.end.format("%H:%M")
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationSettings {
    pub callsign: String,
    pub id_mode: IdMode,
    pub id_interval_s: u64,
    pub tx_max_duration_s: u64,
    pub misfire_grace_s: u64,
    pub quiet_hours: Option<QuietHours>,
    pub quiet_min_priority: Priority,
}

impl StationSettings {
    /// Zamienia znak na wielkie litery i sprawdza zakresy.
    pub fn validate(&mut self) -> Result<(), String> {
        self.callsign = self.callsign.trim().to_uppercase();
        let len = self.callsign.chars().count();
        let chars_ok = self
            .callsign
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '/' || c == '-');
        if !self.callsign.is_empty() && (!(3..=10).contains(&len) || !chars_ok) {
            return Err("znak wywoławczy: 3–10 znaków A–Z, 0–9, / lub -".to_owned());
        }
        if !(60..=3600).contains(&self.id_interval_s) {
            return Err("odstęp identyfikacji: 60–3600 s".to_owned());
        }
        if !(10..=600).contains(&self.tx_max_duration_s) {
            return Err("limit czasu nadawania: 10–600 s".to_owned());
        }
        if self.misfire_grace_s > 86_400 {
            return Err("tolerancja spóźnień: 0–86400 s".to_owned());
        }
        Ok(())
    }
}

pub struct SettingsRepo {
    pool: SqlitePool,
}

impl SettingsRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn get(&self, key: &str) -> Result<String, DbError> {
        Ok(
            sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
                .bind(key)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Wartości, których nie da się odczytać, wracają do domyślnych — tak samo
    /// jak przy starcie stacji.
    pub async fn load(&self) -> Result<StationSettings, DbError> {
        let number = |text: String, default: u64| text.parse().unwrap_or(default);
        Ok(StationSettings {
            callsign: self.get("callsign").await?,
            id_mode: if self.get("id_mode").await? == "cw" {
                IdMode::Cw
            } else {
                IdMode::Voice
            },
            id_interval_s: number(self.get("id_interval_s").await?, 600),
            tx_max_duration_s: number(self.get("tx_max_duration_s").await?, 180),
            misfire_grace_s: number(self.get("misfire_grace_s").await?, 600),
            quiet_hours: QuietHours::parse(&self.get("quiet_hours").await?).ok(),
            quiet_min_priority: self
                .get("quiet_min_priority")
                .await?
                .parse()
                .ok()
                .and_then(Priority::from_i64)
                .unwrap_or(Priority::Emergency),
        })
    }

    /// Zapisuje tylko własne klucze; `tx_enabled` i `poisoned` mają swoje
    /// sterowanie.
    pub async fn save(&self, s: &StationSettings) -> Result<(), DbError> {
        let pairs = [
            ("callsign", s.callsign.clone()),
            (
                "id_mode",
                if s.id_mode == IdMode::Cw {
                    "cw"
                } else {
                    "voice"
                }
                .to_owned(),
            ),
            ("id_interval_s", s.id_interval_s.to_string()),
            ("tx_max_duration_s", s.tx_max_duration_s.to_string()),
            ("misfire_grace_s", s.misfire_grace_s.to_string()),
            (
                "quiet_hours",
                s.quiet_hours.map(|q| q.as_text()).unwrap_or_default(),
            ),
            (
                "quiet_min_priority",
                (s.quiet_min_priority as i64).to_string(),
            ),
        ];
        let mut tx = self.pool.begin().await?;
        for (key, value) in pairs {
            sqlx::query(
                "UPDATE settings SET value = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') \
                 WHERE key = ?",
            )
            .bind(value)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn quiet_hours_cross_midnight() {
        let q = QuietHours::parse("22:00-06:00").unwrap();
        assert!(q.contains(t(23, 30)));
        assert!(q.contains(t(5, 59)));
        assert!(!q.contains(t(6, 0)));
        assert!(!q.contains(t(12, 0)));
    }

    #[test]
    fn quiet_hours_within_one_day() {
        let q = QuietHours::parse("12:00-14:00").unwrap();
        assert!(q.contains(t(12, 0)));
        assert!(!q.contains(t(14, 0)));
        assert_eq!(q.as_text(), "12:00-14:00");
    }

    #[test]
    fn quiet_hours_reject_bad_input() {
        assert!(QuietHours::parse("25:00-06:00").is_err());
        assert!(QuietHours::parse("22:00").is_err());
        assert!(QuietHours::parse("abc").is_err());
        assert!(QuietHours::parse("10:00-10:00").is_err());
    }

    fn sample() -> StationSettings {
        StationSettings {
            callsign: "SP7TQ".into(),
            id_mode: IdMode::Cw,
            id_interval_s: 900,
            tx_max_duration_s: 120,
            misfire_grace_s: 300,
            quiet_hours: Some(QuietHours::parse("22:00-06:00").unwrap()),
            quiet_min_priority: Priority::High,
        }
    }

    #[tokio::test]
    async fn settings_round_trip_through_the_database() {
        let repo = SettingsRepo::new(crate::db::test_pool().await);
        let defaults = repo.load().await.unwrap();
        assert_eq!(defaults.tx_max_duration_s, 180);
        assert_eq!(defaults.quiet_hours, None);
        assert_eq!(defaults.id_mode, IdMode::Voice);

        repo.save(&sample()).await.unwrap();
        assert_eq!(repo.load().await.unwrap(), sample());
    }

    #[tokio::test]
    async fn saving_leaves_tx_enabled_and_poisoned_alone() {
        let pool = crate::db::test_pool().await;
        sqlx::query("UPDATE settings SET value = 'true' WHERE key = 'poisoned'")
            .execute(&pool)
            .await
            .unwrap();
        SettingsRepo::new(pool.clone())
            .save(&sample())
            .await
            .unwrap();
        let v: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'poisoned'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(v, "true");
    }

    #[test]
    fn validation_uppercases_and_checks_ranges() {
        let mut s = sample();
        s.callsign = "sp7tq/p".into();
        s.validate().unwrap();
        assert_eq!(s.callsign, "SP7TQ/P");

        for bad in ["ab", "SP7 TQ", "TOOLONGCALLSIGN", "SP7TQ!"] {
            let mut s = sample();
            s.callsign = bad.into();
            assert!(s.validate().is_err(), "{bad}");
        }
        let mut s = sample();
        s.callsign.clear();
        assert!(
            s.validate().is_ok(),
            "pusty znak jest dozwolony (z ostrzeżeniem)"
        );

        for (i, tx, g) in [
            (59, 180, 0),
            (3601, 180, 0),
            (600, 9, 0),
            (600, 601, 0),
            (600, 180, 86401),
        ] {
            let mut s = sample();
            s.id_interval_s = i;
            s.tx_max_duration_s = tx;
            s.misfire_grace_s = g;
            assert!(s.validate().is_err(), "{i} {tx} {g}");
        }
    }
}
