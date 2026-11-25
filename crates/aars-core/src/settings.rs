//! Parametry stacji z tabeli `settings` (spec §9) i okno ciszy nocnej.

use std::collections::BTreeMap;

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

/// Najwolniejsze i najszybsze dozwolone tempo mowy (mnożnik tempa głosu).
pub const SPEECH_RATE_RANGE: std::ops::RangeInclusive<f32> = 0.70..=1.20;
/// Domyślnie ok. 15% wolniej niż tempo modelu: właściciel uznał je za za szybkie.
pub const DEFAULT_SPEECH_RATE: f32 = 0.87;

/// Tempo zapisane jako tekst („0.87”, przecinek też przejdzie); `None`, gdy
/// to nie liczba albo wypada poza zakresem.
pub fn parse_speech_rate(text: &str) -> Option<f32> {
    let rate: f32 = text.trim().replace(',', ".").parse().ok()?;
    SPEECH_RATE_RANGE.contains(&rate).then_some(rate)
}

#[derive(Debug, Clone, PartialEq)]
pub struct StationSettings {
    pub callsign: String,
    pub id_mode: IdMode,
    pub id_interval_s: u64,
    pub tx_max_duration_s: u64,
    pub misfire_grace_s: u64,
    pub quiet_hours: Option<QuietHours>,
    pub quiet_min_priority: Priority,
    /// Mnożnik tempa mowy (`VoiceParams.rate`), 0.70–1.20.
    pub speech_rate: f32,
}

impl StationSettings {
    /// Czy o tej porze lokalnej trwa okno ciszy.
    pub fn quiet_now(&self, local: NaiveTime) -> bool {
        self.quiet_hours.is_some_and(|window| window.contains(local))
    }

    /// Zamienia znak na wielkie litery i sprawdza zakresy.
    pub fn validate(&mut self) -> Result<(), String> {
        self.callsign = self.callsign.trim().to_uppercase();
        let len = self.callsign.chars().count();
        let chars_ok = self
            .callsign
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '/');
        if !self.callsign.is_empty() && (!(3..=10).contains(&len) || !chars_ok) {
            return Err("znak wywoławczy: 3–10 znaków A–Z, 0–9 lub /".to_owned());
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
        if !SPEECH_RATE_RANGE.contains(&self.speech_rate) {
            return Err("tempo mowy: 70–120%".to_owned());
        }
        // Dwa miejsca po przecinku, jak w formularzu (87%, nie 87,0000001%).
        self.speech_rate = (self.speech_rate * 100.0).round() / 100.0;
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

    pub async fn kv_get(&self, key: &str) -> Result<Option<String>, DbError> {
        Ok(sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn kv_put(&self, key: &str, value: &str) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Klucze z danym prefiksem, w wyniku bez prefiksu.
    pub async fn kv_prefix(&self, prefix: &str) -> Result<BTreeMap<String, String>, DbError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT key, value FROM settings WHERE substr(key, 1, ?) = ?")
                .bind(prefix.chars().count() as i64)
                .bind(prefix)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(k, v)| (k[prefix.len()..].to_owned(), v))
            .collect())
    }

    pub async fn kv_delete_prefix(&self, prefix: &str) -> Result<u64, DbError> {
        Ok(
            sqlx::query("DELETE FROM settings WHERE substr(key, 1, ?) = ?")
                .bind(prefix.chars().count() as i64)
                .bind(prefix)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    async fn get(&self, key: &str) -> Result<String, DbError> {
        Ok(
            sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
                .bind(key)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Tempo mowy do syntezy. Czytane przy każdym renderze, więc zmiana z
    /// panelu działa od następnego nadania i podglądu, bez restartu. Błąd
    /// odczytu albo wartość spoza zakresu dają tempo domyślne.
    pub async fn speech_rate(&self) -> f32 {
        match self.kv_get("speech_rate").await {
            Ok(Some(text)) => parse_speech_rate(&text).unwrap_or(DEFAULT_SPEECH_RATE),
            Ok(None) => DEFAULT_SPEECH_RATE,
            Err(err) => {
                tracing::error!(%err, "odczyt tempa mowy zawiódł, użyto domyślnego");
                DEFAULT_SPEECH_RATE
            }
        }
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
            speech_rate: parse_speech_rate(&self.get("speech_rate").await?)
                .unwrap_or(DEFAULT_SPEECH_RATE),
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
            ("speech_rate", format!("{:.2}", s.speech_rate)),
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

    #[test]
    fn quiet_now_follows_the_window() {
        let s = sample();
        assert!(s.quiet_now(t(23, 0)));
        assert!(!s.quiet_now(t(12, 0)));
        assert!(!StationSettings { quiet_hours: None, ..s }.quiet_now(t(23, 0)));
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
            speech_rate: 1.0,
        }
    }

    #[tokio::test]
    async fn settings_round_trip_through_the_database() {
        let repo = SettingsRepo::new(crate::db::test_pool().await);
        let defaults = repo.load().await.unwrap();
        assert_eq!(defaults.tx_max_duration_s, 180);
        assert_eq!(defaults.quiet_hours, None);
        assert_eq!(defaults.id_mode, IdMode::Voice);
        assert_eq!(defaults.speech_rate, DEFAULT_SPEECH_RATE);

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

    #[tokio::test]
    async fn kv_helpers_upsert_and_strip_prefixes() {
        let repo = SettingsRepo::new(crate::db::test_pool().await);
        assert_eq!(repo.kv_get("x:a").await.unwrap(), None);
        repo.kv_put("x:a", "1").await.unwrap();
        repo.kv_put("x:a", "2").await.unwrap();
        repo.kv_put("x:b", "3").await.unwrap();
        repo.kv_put("y:c", "4").await.unwrap();
        assert_eq!(repo.kv_get("x:a").await.unwrap().as_deref(), Some("2"));
        let got = repo.kv_prefix("x:").await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got["b"], "3");
        assert_eq!(repo.kv_delete_prefix("x:").await.unwrap(), 2);
        assert_eq!(repo.kv_get("y:c").await.unwrap().as_deref(), Some("4"));
    }

    #[test]
    fn speech_rate_is_validated_and_rounded() {
        for ok in [0.70, 0.87, 1.0, 1.20] {
            let mut s = sample();
            s.speech_rate = ok;
            assert!(s.validate().is_ok(), "{ok}");
        }
        for bad in [0.69, 1.21, 0.0, -1.0, f32::NAN, f32::INFINITY] {
            let mut s = sample();
            s.speech_rate = bad;
            assert!(s.validate().is_err(), "{bad}");
        }
        let mut s = sample();
        s.speech_rate = 0.870_000_1;
        s.validate().unwrap();
        assert_eq!(s.speech_rate, 0.87);
        assert_eq!(parse_speech_rate("0,9"), Some(0.9));
        assert_eq!(parse_speech_rate("2"), None);
        assert_eq!(parse_speech_rate("abc"), None);
    }

    #[tokio::test]
    async fn the_speech_rate_is_read_live_and_unreadable_values_fall_back() {
        let repo = SettingsRepo::new(crate::db::test_pool().await);
        assert_eq!(repo.speech_rate().await, DEFAULT_SPEECH_RATE);
        repo.kv_put("speech_rate", "1.10").await.unwrap();
        assert_eq!(repo.speech_rate().await, 1.10);
        repo.kv_put("speech_rate", "9").await.unwrap();
        assert_eq!(repo.speech_rate().await, DEFAULT_SPEECH_RATE);
        repo.kv_put("speech_rate", "x").await.unwrap();
        assert_eq!(repo.load().await.unwrap().speech_rate, DEFAULT_SPEECH_RATE);
    }

    #[test]
    fn validation_uppercases_and_checks_ranges() {
        let mut s = sample();
        s.callsign = "sp7tq/p".into();
        s.validate().unwrap();
        assert_eq!(s.callsign, "SP7TQ/P");

        // '-' nie ma kodu w identyfikacji CW, więc znak by się w niej zgubił.
        for bad in ["ab", "SP7 TQ", "TOOLONGCALLSIGN", "SP7TQ!", "SP7TQ-1"] {
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
