use std::path::{Path, PathBuf};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub audio: AudioConfig,
    pub ptt: PttConfig,
    pub tts: TtsConfig,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    /// Nazwa urządzenia wyjściowego; "default" znaczy domyślne urządzenie systemu.
    pub device: String,
    pub sample_rate: u32,
    /// Docelowe wzmocnienie po normalizacji, w dB względem pełnej skali.
    pub gain_db: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PttBackendKind {
    Gpio,
    Serial,
    Vox,
}

impl PttBackendKind {
    /// Nazwa dla człowieka (panel), nie `{:?}`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Gpio => "GPIO",
            Self::Serial => "port szeregowy",
            Self::Vox => "VOX",
        }
    }
}

/// Źródło informacji o zajętości kanału. W MVP zawsze `None` — spec §4.4
/// zostawia to pole, by dołożenie COS/RMS nie wymagało ruszania sekwencera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelBusyKind {
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PttConfig {
    pub backend: PttBackendKind,
    pub channel_busy: ChannelBusyKind,
    /// Czas, jaki radio potrzebuje na przełączenie się na nadawanie, w ms.
    pub tx_tail_ms: u64,
    pub gpio: GpioConfig,
    pub serial: SerialConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpioConfig {
    pub chip: String,
    pub line: u32,
    pub active_low: bool,
    /// Opcjonalne wejście blokujące nadawanie (spec §4.4 krok 3).
    pub inhibit_line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerialPttLine {
    Dtr,
    Rts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SerialConfig {
    pub port: String,
    pub line: SerialPttLine,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TtsConfig {
    /// Ścieżka do binarki `piper`.
    pub piper_bin: String,
    /// Ścieżka do modelu głosu .onnx (obok musi leżeć .onnx.json).
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub db_path: PathBuf,
    pub cache_dir: PathBuf,
    pub cache_max_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig { bind: "0.0.0.0:8080".to_owned() },
            audio: AudioConfig {
                device: "default".to_owned(),
                sample_rate: 48_000,
                gain_db: -3.0,
            },
            ptt: PttConfig {
                backend: PttBackendKind::Vox,
                channel_busy: ChannelBusyKind::None,
                tx_tail_ms: 250,
                gpio: GpioConfig {
                    chip: "/dev/gpiochip0".to_owned(),
                    line: 17,
                    active_low: false,
                    inhibit_line: None,
                },
                serial: SerialConfig {
                    port: "/dev/ttyUSB0".to_owned(),
                    line: SerialPttLine::Dtr,
                },
            },
            tts: TtsConfig {
                piper_bin: "piper".to_owned(),
                model: "/var/lib/aars/voices/pl_PL-darkman-medium.onnx".to_owned(),
            },
            storage: StorageConfig {
                db_path: PathBuf::from("var/aars.db"),
                cache_dir: PathBuf::from("var/cache"),
                cache_max_bytes: 512 * 1024 * 1024,
            },
        }
    }
}

impl Config {
    /// Wczytuje konfigurację: domyślne wartości, potem plik, potem `AARS_*`.
    ///
    /// Gdy `path` jest `Some`, plik musi istnieć — cicha praca na domyślnych
    /// wartościach po literówce w ścieżce byłaby groźna dla stacji nadawczej.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut figment = Figment::from(Serialized::defaults(Config::default()));

        if let Some(path) = path {
            if !path.exists() {
                return Err(ConfigError::NotFound { path: path.to_path_buf() });
            }
            figment = figment.merge(Toml::file(path));
        }

        figment
            .merge(Env::prefixed("AARS_").split("__"))
            .extract()
            .map_err(|e| ConfigError::Invalid(Box::new(e)))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ptt_backends_have_human_labels() {
        use super::PttBackendKind;
        assert_eq!(PttBackendKind::Gpio.label(), "GPIO");
        assert_eq!(PttBackendKind::Serial.label(), "port szeregowy");
        assert_eq!(PttBackendKind::Vox.label(), "VOX");
    }

    use super::*;

    // Każdy test dotykający konfiguracji pracuje w `figment::Jail`, który
    // trzyma globalny zamek. Bez tego zmienne `AARS_*` z jednego testu
    // wyciekają do równolegle biegnącego drugiego.
    #[test]
    fn defaults_are_usable_without_any_file() {
        figment::Jail::expect_with(|_jail| {
            let cfg = Config::load(None).expect("domyślna konfiguracja musi się wczytać");
            assert_eq!(cfg.server.bind, "0.0.0.0:8080");
            assert_eq!(cfg.audio.sample_rate, 48_000);
            assert_eq!(cfg.ptt.backend, PttBackendKind::Vox);
            assert_eq!(cfg.ptt.channel_busy, ChannelBusyKind::None);
            Ok(())
        });
    }

    #[test]
    fn toml_overrides_defaults_and_env_overrides_toml() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "aars.toml",
                r#"
                [server]
                bind = "127.0.0.1:9000"

                [ptt]
                backend = "gpio"

                [ptt.gpio]
                line = 17
                "#,
            )?;
            jail.set_env("AARS_AUDIO__SAMPLE_RATE", "44100");

            let cfg = Config::load(Some(std::path::Path::new("aars.toml"))).unwrap();
            assert_eq!(cfg.server.bind, "127.0.0.1:9000");
            assert_eq!(cfg.ptt.backend, PttBackendKind::Gpio);
            assert_eq!(cfg.ptt.gpio.line, 17);
            assert_eq!(cfg.audio.sample_rate, 44_100, "env musi wygrywać z plikiem");
            assert_eq!(cfg.ptt.gpio.chip, "/dev/gpiochip0", "nietknięte pola zachowują domyślne");
            Ok(())
        });
    }

    #[test]
    fn missing_explicit_config_file_is_an_error() {
        let err = Config::load(Some(std::path::Path::new("/nie/ma/takiego.toml"))).unwrap_err();
        assert!(matches!(err, crate::error::ConfigError::NotFound { .. }));
    }
}
