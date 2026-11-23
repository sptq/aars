//! Ustawienia sprzętu zmieniane z panelu, nakładane na `aars.toml` (etap 9, R1–R2).
//!
//! Panel ustawia tylko 13 kluczy z [`HW_KEYS`]. Ścieżki wykonywalne i katalogi
//! zostają w pliku, bo inaczej panel byłby drogą do uruchomienia dowolnego programu.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use sqlx::SqlitePool;

use crate::config::{Config, PttBackendKind, SerialPttLine};
use crate::error::DbError;
use crate::settings::SettingsRepo;

pub mod probe;

pub const HW_PREFIX: &str = "hw:";
pub const HW_GOOD_PREFIX: &str = "hw_good:";
pub const HW_STATE_PREFIX: &str = "hw_state:";
pub const HW_KEYS: [&str; 13] = [
    "server.bind",
    "audio.device",
    "audio.sample_rate",
    "audio.gain_db",
    "ptt.backend",
    "ptt.tx_tail_ms",
    "ptt.gpio.chip",
    "ptt.gpio.line",
    "ptt.gpio.active_low",
    "ptt.gpio.inhibit_line",
    "ptt.serial.port",
    "ptt.serial.line",
    "tts.model",
];
pub const SAMPLE_RATES: [u32; 6] = [8000, 16000, 22050, 32000, 44100, 48000];

#[derive(Debug, Clone, PartialEq)]
pub struct HardwareSettings {
    pub bind: String,
    pub audio_device: String,
    pub sample_rate: u32,
    pub gain_db: f32,
    pub ptt_backend: PttBackendKind,
    pub tx_tail_ms: u64,
    pub gpio_chip: String,
    pub gpio_line: u32,
    pub gpio_active_low: bool,
    pub gpio_inhibit_line: Option<u32>,
    pub serial_port: String,
    pub serial_line: SerialPttLine,
    pub voice_model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: &'static str,
    pub message: String,
}

fn err(field: &'static str, message: &str) -> FieldError {
    FieldError { field, message: message.to_owned() }
}

fn device_ok(v: &str) -> bool {
    (1..=128).contains(&v.chars().count()) && !v.chars().any(char::is_control)
}

fn backend_text(b: PttBackendKind) -> &'static str {
    match b {
        PttBackendKind::Gpio => "gpio",
        PttBackendKind::Serial => "serial",
        PttBackendKind::Vox => "vox",
    }
}

fn line_text(l: SerialPttLine) -> &'static str {
    match l {
        SerialPttLine::Dtr => "dtr",
        SerialPttLine::Rts => "rts",
    }
}

/// Czyta jeden klucz: wartość z pary, a gdy jest nieczytelna, zostawia `slot`
/// (wartość z pliku) i dopisuje ostrzeżenie.
fn take<T>(
    key: &str,
    pairs: &BTreeMap<String, String>,
    from_file: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
    parse: impl Fn(&str) -> Option<T>,
    slot: &mut T,
) {
    let Some(v) = pairs.get(key) else { return };
    match parse(v) {
        Some(parsed) => *slot = parsed,
        None => warnings.push(format!(
            "{key}: nieczytelna wartość „{v}”, używam {}",
            from_file.get(key).map(String::as_str).filter(|v| !v.is_empty()).unwrap_or("brak")
        )),
    }
}

impl HardwareSettings {
    pub fn from_config(c: &Config) -> Self {
        Self {
            bind: c.server.bind.clone(),
            audio_device: c.audio.device.clone(),
            sample_rate: c.audio.sample_rate,
            gain_db: c.audio.gain_db,
            ptt_backend: c.ptt.backend,
            tx_tail_ms: c.ptt.tx_tail_ms,
            gpio_chip: c.ptt.gpio.chip.clone(),
            gpio_line: c.ptt.gpio.line,
            gpio_active_low: c.ptt.gpio.active_low,
            gpio_inhibit_line: c.ptt.gpio.inhibit_line,
            serial_port: c.ptt.serial.port.clone(),
            serial_line: c.ptt.serial.line,
            voice_model: c.tts.model.clone(),
        }
    }

    /// Nakłada ustawienia na `base`; pola poza `HW_KEYS` zostają z `base`.
    pub fn apply_to(&self, base: &Config) -> Config {
        let mut c = base.clone();
        c.server.bind = self.bind.clone();
        c.audio.device = self.audio_device.clone();
        c.audio.sample_rate = self.sample_rate;
        c.audio.gain_db = self.gain_db;
        c.ptt.backend = self.ptt_backend;
        c.ptt.tx_tail_ms = self.tx_tail_ms;
        c.ptt.gpio.chip = self.gpio_chip.clone();
        c.ptt.gpio.line = self.gpio_line;
        c.ptt.gpio.active_low = self.gpio_active_low;
        c.ptt.gpio.inhibit_line = self.gpio_inhibit_line;
        c.ptt.serial.port = self.serial_port.clone();
        c.ptt.serial.line = self.serial_line;
        c.tts.model = self.voice_model.clone();
        c
    }

    pub fn to_pairs(&self) -> BTreeMap<String, String> {
        let pairs: [(&str, String); 13] = [
            ("server.bind", self.bind.clone()),
            ("audio.device", self.audio_device.clone()),
            ("audio.sample_rate", self.sample_rate.to_string()),
            ("audio.gain_db", self.gain_db.to_string()),
            ("ptt.backend", backend_text(self.ptt_backend).to_owned()),
            ("ptt.tx_tail_ms", self.tx_tail_ms.to_string()),
            ("ptt.gpio.chip", self.gpio_chip.clone()),
            ("ptt.gpio.line", self.gpio_line.to_string()),
            ("ptt.gpio.active_low", self.gpio_active_low.to_string()),
            (
                "ptt.gpio.inhibit_line",
                self.gpio_inhibit_line.map(|l| l.to_string()).unwrap_or_default(),
            ),
            ("ptt.serial.port", self.serial_port.clone()),
            ("ptt.serial.line", line_text(self.serial_line).to_owned()),
            ("tts.model", self.voice_model.clone()),
        ];
        pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
    }

    /// Każdy klucz osobno: wartość z pary albo, gdy jej brak lub jest
    /// nieczytelna, z `base` (nieczytelna dodatkowo z ostrzeżeniem).
    pub fn from_pairs(base: &Config, pairs: &BTreeMap<String, String>) -> (Self, Vec<String>) {
        let mut s = Self::from_config(base);
        let mut warnings = Vec::new();
        let file = s.to_pairs();
        let (d, w) = (&file, &mut warnings);
        let text = |v: &str| Some(v.to_owned());

        take(
            "server.bind",
            pairs,
            d,
            w,
            |v| v.parse::<SocketAddr>().ok().filter(|a| a.port() >= 1024).map(|_| v.to_owned()),
            &mut s.bind,
        );
        take(
            "audio.device",
            pairs,
            d,
            w,
            |v| device_ok(v).then(|| v.to_owned()),
            &mut s.audio_device,
        );
        take(
            "audio.sample_rate",
            pairs,
            d,
            w,
            |v| v.trim().parse().ok().filter(|r| SAMPLE_RATES.contains(r)),
            &mut s.sample_rate,
        );
        take(
            "audio.gain_db",
            pairs,
            d,
            w,
            |v| v.trim().replace(',', ".").parse::<f32>().ok().filter(|g| (-30.0..=0.0).contains(g)),
            &mut s.gain_db,
        );
        take(
            "ptt.backend",
            pairs,
            d,
            w,
            |v| match v.trim() {
                "gpio" => Some(PttBackendKind::Gpio),
                "serial" => Some(PttBackendKind::Serial),
                "vox" => Some(PttBackendKind::Vox),
                _ => None,
            },
            &mut s.ptt_backend,
        );
        take(
            "ptt.tx_tail_ms",
            pairs,
            d,
            w,
            |v| v.trim().parse().ok().filter(|t| *t <= 2000),
            &mut s.tx_tail_ms,
        );
        take("ptt.gpio.chip", pairs, d, w, text, &mut s.gpio_chip);
        take(
            "ptt.gpio.line",
            pairs,
            d,
            w,
            |v| v.trim().parse().ok().filter(|l| *l <= 511),
            &mut s.gpio_line,
        );
        take(
            "ptt.gpio.active_low",
            pairs,
            d,
            w,
            |v| match v.trim() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            &mut s.gpio_active_low,
        );
        take(
            "ptt.gpio.inhibit_line",
            pairs,
            d,
            w,
            |v| {
                let v = v.trim();
                if v.is_empty() { Some(None) } else { v.parse().ok().filter(|l| *l <= 511).map(Some) }
            },
            &mut s.gpio_inhibit_line,
        );
        take(
            "ptt.serial.port",
            pairs,
            d,
            w,
            |v| v.starts_with("/dev/").then(|| v.to_owned()),
            &mut s.serial_port,
        );
        take(
            "ptt.serial.line",
            pairs,
            d,
            w,
            |v| match v.trim() {
                "dtr" => Some(SerialPttLine::Dtr),
                "rts" => Some(SerialPttLine::Rts),
                _ => None,
            },
            &mut s.serial_line,
        );
        take(
            "tts.model",
            pairs,
            d,
            w,
            |v| v.ends_with(".onnx").then(|| v.to_owned()),
            &mut s.voice_model,
        );
        (s, warnings)
    }

    /// Zakresy z R9. Niczego nie pyta hosta.
    pub fn validate(&self) -> Result<(), Vec<FieldError>> {
        let mut e = Vec::new();
        match self.bind.parse::<SocketAddr>() {
            Err(_) => e.push(err("server.bind", "adres panelu: podaj adres:port, np. 0.0.0.0:8080")),
            Ok(a) if a.port() < 1024 => {
                e.push(err("server.bind", "port panelu poniżej 1024 wymaga uprawnień roota"))
            }
            Ok(_) => {}
        }
        if !device_ok(&self.audio_device) {
            e.push(err(
                "audio.device",
                "urządzenie audio: 1–128 znaków bez znaków sterujących",
            ));
        }
        if !SAMPLE_RATES.contains(&self.sample_rate) {
            e.push(err(
                "audio.sample_rate",
                "częstotliwość próbkowania: 8000, 16000, 22050, 32000, 44100 albo 48000 Hz",
            ));
        }
        if !(-30.0..=0.0).contains(&self.gain_db) {
            e.push(err("audio.gain_db", "poziom wyjścia: od −30 do 0 dBFS"));
        }
        if self.tx_tail_ms > 2000 {
            e.push(err("ptt.tx_tail_ms", "opóźnienie PTT: 0–2000 ms"));
        }
        match self.ptt_backend {
            PttBackendKind::Gpio => {
                if self.gpio_line > 511 {
                    e.push(err("ptt.gpio.line", "linia GPIO: 0–511"));
                }
                if let Some(inhibit) = self.gpio_inhibit_line {
                    if inhibit > 511 {
                        e.push(err("ptt.gpio.inhibit_line", "linia GPIO: 0–511"));
                    } else if inhibit == self.gpio_line {
                        e.push(err(
                            "ptt.gpio.inhibit_line",
                            "linia blokująca musi być inna niż linia PTT",
                        ));
                    }
                }
            }
            PttBackendKind::Serial => {
                if !self.serial_port.starts_with("/dev/") {
                    e.push(err("ptt.serial.port", "port szeregowy: podaj ścieżkę /dev/…"));
                }
            }
            PttBackendKind::Vox => {}
        }
        if !self.voice_model.ends_with(".onnx") {
            e.push(err("tts.model", "model głosu: plik .onnx"));
        }
        if e.is_empty() { Ok(()) } else { Err(e) }
    }

    /// `["ptt.backend: vox → gpio", …]` w kolejności `HW_KEYS`.
    pub fn diff(&self, other: &Self) -> Vec<String> {
        let (a, b) = (self.to_pairs(), other.to_pairs());
        HW_KEYS
            .iter()
            .filter(|k| a[**k] != b[**k])
            .map(|k| format!("{k}: {} → {}", a[*k], b[*k]))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOrigin {
    Panel,
    LastGood,
    File,
    None,
}

impl ConfigOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Panel => "panel",
            Self::LastGood => "last_good",
            Self::File => "file",
            Self::None => "none",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "panel" => Self::Panel,
            "last_good" => Self::LastGood,
            "file" => Self::File,
            "none" => Self::None,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Panel => "ustawienia z panelu",
            Self::LastGood => "ostatnia działająca konfiguracja",
            Self::File => "plik aars.toml",
            Self::None => "brak — nadawanie niedostępne",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyStatus {
    pub origin: ConfigOrigin,
    pub error: Option<String>,
}

pub struct HardwareRepo {
    pool: SqlitePool,
}

impl HardwareRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    fn kv(&self) -> SettingsRepo {
        SettingsRepo::new(self.pool.clone())
    }

    pub async fn overrides(&self) -> Result<BTreeMap<String, String>, DbError> {
        self.kv().kv_prefix(HW_PREFIX).await
    }

    pub async fn last_good(&self) -> Result<BTreeMap<String, String>, DbError> {
        self.kv().kv_prefix(HW_GOOD_PREFIX).await
    }

    async fn put_all(&self, prefix: &str, pairs: &BTreeMap<String, String>) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        for (key, value) in pairs {
            sqlx::query(
                "INSERT INTO settings (key, value) VALUES (?, ?) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
                 updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
            )
            .bind(format!("{prefix}{key}"))
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn save(&self, s: &HardwareSettings) -> Result<(), DbError> {
        self.put_all(HW_PREFIX, &s.to_pairs()).await
    }

    pub async fn save_last_good(&self, pairs: &BTreeMap<String, String>) -> Result<(), DbError> {
        self.put_all(HW_GOOD_PREFIX, pairs).await
    }

    /// Kasuje `hw:*`, `hw_good:*` i `hw_state:*`; reszta `settings` zostaje.
    pub async fn clear(&self) -> Result<u64, DbError> {
        let kv = self.kv();
        let mut n = 0;
        for prefix in [HW_PREFIX, HW_GOOD_PREFIX, HW_STATE_PREFIX] {
            n += kv.kv_delete_prefix(prefix).await?;
        }
        Ok(n)
    }

    pub async fn set_apply_status(&self, s: &ApplyStatus) -> Result<(), DbError> {
        let kv = self.kv();
        kv.kv_put("hw_state:origin", s.origin.as_str()).await?;
        kv.kv_put("hw_state:error", s.error.as_deref().unwrap_or("")).await
    }

    pub async fn apply_status(&self) -> Result<Option<ApplyStatus>, DbError> {
        let kv = self.kv();
        let Some(origin) = kv.kv_get("hw_state:origin").await? else {
            return Ok(None);
        };
        let Some(origin) = ConfigOrigin::parse(&origin) else {
            return Ok(None);
        };
        let error = kv.kv_get("hw_state:error").await?.filter(|e| !e.is_empty());
        Ok(Some(ApplyStatus { origin, error }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> HardwareSettings {
        HardwareSettings::from_config(&Config::default())
    }

    fn only(s: &HardwareSettings) -> Vec<FieldError> {
        s.validate().unwrap_err()
    }

    fn one(field: &'static str, message: &str) -> Vec<FieldError> {
        vec![FieldError { field, message: message.to_owned() }]
    }

    #[test]
    fn pairs_round_trip_through_from_pairs() {
        let cfg = Config::default();
        let s = HardwareSettings::from_config(&cfg);
        let (back, warnings) = HardwareSettings::from_pairs(&cfg, &s.to_pairs());
        assert_eq!(back, s);
        assert!(warnings.is_empty());
        assert_eq!(s.to_pairs().len(), 13);
        assert_eq!(s.to_pairs()["ptt.gpio.inhibit_line"], "");
        assert_eq!(s.to_pairs()["ptt.gpio.active_low"], "false");

        let mut with = s.clone();
        with.gpio_inhibit_line = Some(5);
        with.gpio_active_low = true;
        let (back, w) = HardwareSettings::from_pairs(&cfg, &with.to_pairs());
        assert_eq!(back, with);
        assert!(w.is_empty());
    }

    #[test]
    fn panel_values_override_the_file() {
        let cfg = Config::default();
        let pairs: BTreeMap<String, String> = [
            ("ptt.backend", "gpio"),
            ("ptt.gpio.line", "22"),
            ("audio.device", "hw:1,0"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let (s, warnings) = HardwareSettings::from_pairs(&cfg, &pairs);
        assert!(warnings.is_empty());
        let out = s.apply_to(&cfg);
        assert_eq!(out.ptt.backend, PttBackendKind::Gpio);
        assert_eq!(out.ptt.gpio.line, 22);
        assert_eq!(out.audio.device, "hw:1,0");
        assert_eq!(out.storage, cfg.storage);
        assert_eq!(out.tts.piper_bin, cfg.tts.piper_bin);
        assert_eq!(out.tts.voices_dir, cfg.tts.voices_dir);
    }

    #[test]
    fn a_broken_stored_value_falls_back_to_the_file_and_is_reported() {
        let cfg = Config::default();
        let pairs = BTreeMap::from([("audio.sample_rate".to_owned(), "abc".to_owned())]);
        let (s, warnings) = HardwareSettings::from_pairs(&cfg, &pairs);
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("audio.sample_rate"), "{warnings:?}");
    }

    #[test]
    fn a_decimal_comma_is_accepted_for_the_gain() {
        let pairs = BTreeMap::from([("audio.gain_db".to_owned(), "-4,5".to_owned())]);
        let (s, w) = HardwareSettings::from_pairs(&Config::default(), &pairs);
        assert_eq!(s.gain_db, -4.5);
        assert!(w.is_empty());
    }

    #[test]
    fn validation_rejects_out_of_range_values() {
        assert!(base().validate().is_ok());

        let mut s = base();
        s.bind = "nonsens".into();
        assert_eq!(only(&s), one("server.bind", "adres panelu: podaj adres:port, np. 0.0.0.0:8080"));
        s.bind = "0.0.0.0:80".into();
        assert_eq!(only(&s), one("server.bind", "port panelu poniżej 1024 wymaga uprawnień roota"));

        let mut s = base();
        s.sample_rate = 12_345;
        assert_eq!(
            only(&s),
            one(
                "audio.sample_rate",
                "częstotliwość próbkowania: 8000, 16000, 22050, 32000, 44100 albo 48000 Hz"
            )
        );

        for g in [0.5, -30.5, f32::NAN] {
            let mut s = base();
            s.gain_db = g;
            assert_eq!(only(&s), one("audio.gain_db", "poziom wyjścia: od −30 do 0 dBFS"));
        }
        let mut s = base();
        s.gain_db = -30.0;
        assert!(s.validate().is_ok());

        let mut s = base();
        s.tx_tail_ms = 2001;
        assert_eq!(only(&s), one("ptt.tx_tail_ms", "opóźnienie PTT: 0–2000 ms"));

        let mut s = base();
        s.ptt_backend = PttBackendKind::Gpio;
        s.gpio_line = 512;
        assert_eq!(only(&s), one("ptt.gpio.line", "linia GPIO: 0–511"));

        let mut s = base();
        s.ptt_backend = PttBackendKind::Gpio;
        s.gpio_inhibit_line = Some(s.gpio_line);
        assert_eq!(
            only(&s),
            one("ptt.gpio.inhibit_line", "linia blokująca musi być inna niż linia PTT")
        );

        let mut s = base();
        s.ptt_backend = PttBackendKind::Serial;
        s.serial_port = "COM3".into();
        assert_eq!(only(&s), one("ptt.serial.port", "port szeregowy: podaj ścieżkę /dev/…"));

        let mut s = base();
        s.voice_model = "/x/v.bin".into();
        assert_eq!(only(&s), one("tts.model", "model głosu: plik .onnx"));

        for dev in ["".to_owned(), "a\nb".to_owned(), "x".repeat(129)] {
            let mut s = base();
            s.audio_device = dev;
            assert_eq!(
                only(&s),
                one("audio.device", "urządzenie audio: 1–128 znaków bez znaków sterujących")
            );
        }
    }

    #[test]
    fn unused_backend_fields_do_not_block_saving() {
        let mut s = base();
        s.serial_port = String::new();
        s.gpio_chip = String::new();
        s.gpio_line = 9999;
        assert!(s.validate().is_ok(), "vox");
        s.ptt_backend = PttBackendKind::Gpio;
        s.gpio_line = 17;
        assert!(s.validate().is_ok(), "gpio z pustym portem szeregowym");
        s.ptt_backend = PttBackendKind::Serial;
        s.serial_port = "/dev/ttyUSB0".into();
        assert!(s.validate().is_ok(), "serial z pustym układem GPIO");
    }

    #[test]
    fn a_stored_value_that_fails_validation_falls_back_like_an_unreadable_one() {
        let cfg = Config::default();
        for (key, bad) in [
            ("audio.sample_rate", "12345"),
            ("ptt.tx_tail_ms", "999999"),
            ("server.bind", "x"),
            ("tts.model", "foo"),
        ] {
            let pairs = BTreeMap::from([(key.to_owned(), bad.to_owned())]);
            let (s, w) = HardwareSettings::from_pairs(&cfg, &pairs);
            assert_eq!(s, HardwareSettings::from_config(&cfg), "{key}");
            assert_eq!(w.len(), 1, "{key}");
            assert!(w[0].contains(key) && w[0].contains(bad), "{w:?}");
        }
    }

    #[test]
    fn diff_names_each_changed_key() {
        let a = base();
        let mut b = base();
        b.audio_device = "hw:1,0".into();
        b.ptt_backend = PttBackendKind::Gpio;
        assert_eq!(
            a.diff(&b),
            vec!["audio.device: default → hw:1,0".to_owned(), "ptt.backend: vox → gpio".to_owned()]
        );
        assert!(a.diff(&a).is_empty());
    }

    #[test]
    fn origins_have_labels_and_round_trip() {
        for o in [ConfigOrigin::Panel, ConfigOrigin::LastGood, ConfigOrigin::File, ConfigOrigin::None] {
            assert_eq!(ConfigOrigin::parse(o.as_str()), Some(o));
        }
        assert_eq!(ConfigOrigin::parse("?"), None);
        assert_eq!(ConfigOrigin::File.label(), "plik aars.toml");
        assert_eq!(ConfigOrigin::None.label(), "brak — nadawanie niedostępne");
    }

    #[tokio::test]
    async fn repo_saves_all_keys_and_clear_removes_them() {
        let pool = crate::db::test_pool().await;
        let repo = HardwareRepo::new(pool.clone());
        repo.save(&base()).await.unwrap();
        assert_eq!(repo.overrides().await.unwrap().len(), 13);
        repo.save_last_good(&base().to_pairs()).await.unwrap();
        assert_eq!(repo.last_good().await.unwrap().len(), 13);
        repo.set_apply_status(&ApplyStatus { origin: ConfigOrigin::File, error: None })
            .await
            .unwrap();

        assert_eq!(repo.clear().await.unwrap(), 13 + 13 + 2);
        assert!(repo.overrides().await.unwrap().is_empty());
        assert!(repo.last_good().await.unwrap().is_empty());
        assert_eq!(repo.apply_status().await.unwrap(), None);
        let tx: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'tx_enabled'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tx, "true");
    }

    #[tokio::test]
    async fn apply_status_round_trips() {
        let repo = HardwareRepo::new(crate::db::test_pool().await);
        assert_eq!(repo.apply_status().await.unwrap(), None);
        let failed = ApplyStatus {
            origin: ConfigOrigin::LastGood,
            error: Some("brak portu /dev/ttyUSB0".to_owned()),
        };
        repo.set_apply_status(&failed).await.unwrap();
        assert_eq!(repo.apply_status().await.unwrap(), Some(failed));
        let ok = ApplyStatus { origin: ConfigOrigin::Panel, error: None };
        repo.set_apply_status(&ok).await.unwrap();
        assert_eq!(repo.apply_status().await.unwrap(), Some(ok));
    }
}
