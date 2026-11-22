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
    pub sources: SourcesConfig,
    /// Aktualizacja z wydań GitHub (etap 10). Tylko w pliku: kto zmienia
    /// źródło aktualizacji, ten wybiera kod do uruchomienia (R5).
    #[serde(default)]
    pub update: UpdateConfig,
    /// Zakładanie pierwszego konta (etap 10, R12).
    #[serde(default)]
    pub setup: SetupConfig,
}

/// Skąd stacja bierze wydania i gdzie trzyma stan aktualizacji (R5, R6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// Adres wydań; manifest leży pod `{releases_url}/latest/download/…`.
    pub releases_url: String,
    /// Katalog roboczy usługi (użytkownik `aars`).
    pub state_dir: PathBuf,
    /// Stan strażnika startu (root).
    pub guard_dir: PathBuf,
    /// Katalog z `releases/`, `current` i `previous`.
    pub install_root: PathBuf,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            releases_url: "https://github.com/sptq/aars/releases".to_owned(),
            state_dir: PathBuf::from("/var/lib/aars/update"),
            guard_dir: PathBuf::from("/var/lib/aars-update"),
            install_root: PathBuf::from("/opt/aars"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SetupConfig {
    pub first_admin: FirstAdmin,
}

/// Jak powstaje pierwsze konto administratora (R12).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirstAdmin {
    /// Hasło startowe wypisane na konsoli (jak przed etapem 10).
    #[default]
    Console,
    /// Kreator `/setup` w panelu, z sieci lokalnej (obraz Raspberry Pi).
    Wizard,
}

/// Adresy źródeł danych: to rozruch stacji, nie operacja (włączanie, obszar
/// i interwał żyją w bazie).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcesConfig {
    #[serde(default = "default_imgw_meteo_url")]
    pub imgw_meteo_url: String,
    #[serde(default = "default_rso_url")]
    pub rso_url: String,
    #[serde(default = "default_swpc_url")]
    pub swpc_url: String,
    #[serde(default = "default_gios_url")]
    pub gios_url: String,
    #[serde(default = "default_emsc_url")]
    pub emsc_url: String,
    #[serde(default = "default_open_meteo_url")]
    pub open_meteo_url: String,
    #[serde(default = "default_met_no_url")]
    pub met_no_url: String,
}

fn default_imgw_meteo_url() -> String {
    "https://danepubliczne.imgw.pl/api/data/warningsmeteo".to_owned()
}

fn default_rso_url() -> String {
    "https://komunikaty.tvp.pl/komunikatyxml".to_owned()
}

fn default_swpc_url() -> String {
    "https://services.swpc.noaa.gov/products/noaa-scales.json".to_owned()
}

fn default_gios_url() -> String {
    "https://api.gios.gov.pl/pjp-api/v1/rest".to_owned()
}

fn default_emsc_url() -> String {
    "https://www.seismicportal.eu/fdsnws/event/1/query".to_owned()
}

fn default_open_meteo_url() -> String {
    "https://api.open-meteo.com/v1/forecast".to_owned()
}

fn default_met_no_url() -> String {
    "https://api.met.no/weatherapi/locationforecast/2.0/compact".to_owned()
}

impl SourcesConfig {
    /// Każdy adres musi być URL-em http(s); literówka ma wyjść przy starcie,
    /// nie jako wieczna porażka odpytywania.
    fn validate(&self) -> Result<(), ConfigError> {
        let pairs = [
            ("imgw_meteo_url", &self.imgw_meteo_url),
            ("rso_url", &self.rso_url),
            ("swpc_url", &self.swpc_url),
            ("gios_url", &self.gios_url),
            ("emsc_url", &self.emsc_url),
            ("open_meteo_url", &self.open_meteo_url),
            ("met_no_url", &self.met_no_url),
        ];
        for (key, url) in pairs {
            let url = url.as_str();
            let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
            match rest {
                Some(rest) if !rest.is_empty() && !rest.starts_with('/') && !url.contains(char::is_whitespace) => {}
                _ => {
                    return Err(ConfigError::Invalid(Box::new(figment::Error::from(format!(
                        "sources.{key}: „{url}” nie jest adresem http:// ani https://"
                    )))))
                }
            }
        }
        Ok(())
    }
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            imgw_meteo_url: default_imgw_meteo_url(),
            rso_url: default_rso_url(),
            swpc_url: default_swpc_url(),
            gios_url: default_gios_url(),
            emsc_url: default_emsc_url(),
            open_meteo_url: default_open_meteo_url(),
            met_no_url: default_met_no_url(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: String,
    /// Dodatkowe nazwy, pod którymi panel odpowiada (obrona przed DNS
    /// rebinding). Zawsze przechodzą literały IP, `localhost`, nazwa hosta,
    /// `{nazwa}.local`, `{nazwa}-{N}.local`, `{nazwa}.lan`, `.home`,
    /// `.home.arpa`, `.localdomain` i host z `bind`. Tylko w pliku.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// `false` (domyślnie): cały panel odpowiada tylko klientom z sieci
    /// lokalnej stacji, reszta dostaje `403`. `true`: każdemu klientowi.
    /// Tylko w pliku.
    #[serde(default)]
    pub allow_remote: bool,
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
    /// Katalog z modelami głosów. Tylko w pliku: panel go nie zmienia.
    #[serde(default = "default_voices_dir")]
    pub voices_dir: PathBuf,
}

fn default_voices_dir() -> PathBuf {
    PathBuf::from("/var/lib/aars/voices")
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
            server: ServerConfig {
                bind: "0.0.0.0:8080".to_owned(),
                allowed_hosts: Vec::new(),
                allow_remote: false,
            },
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
                voices_dir: default_voices_dir(),
            },
            storage: StorageConfig {
                db_path: PathBuf::from("var/aars.db"),
                cache_dir: PathBuf::from("var/cache"),
                cache_max_bytes: 512 * 1024 * 1024,
            },
            sources: SourcesConfig::default(),
            update: UpdateConfig::default(),
            setup: SetupConfig::default(),
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

        let config: Config = figment
            .merge(Env::prefixed("AARS_").split("__"))
            .extract()
            .map_err(|e| ConfigError::Invalid(Box::new(e)))?;
        config.sources.validate()?;
        Ok(config)
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

    #[test]
    fn a_config_without_voices_dir_uses_the_default() {
        let old: TtsConfig =
            parse_tts("piper_bin = \"piper\"\nmodel = \"/x/v.onnx\"\n");
        assert_eq!(old.voices_dir, PathBuf::from("/var/lib/aars/voices"));
    }

    fn parse_tts(toml: &str) -> TtsConfig {
        Figment::from(Toml::string(toml)).extract().unwrap()
    }

    /// `aars.example.toml` ma tylko ścieżki (etap 9): reszta to wartości
    /// domyślne, nadpisywane z panelu.
    #[test]
    fn the_paths_only_example_config_loads() {
        let example =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../aars.example.toml");
        let text = std::fs::read_to_string(&example).unwrap();
        figment::Jail::expect_with(|jail| {
            jail.create_file("aars.toml", &text)?;
            let cfg = Config::load(Some(std::path::Path::new("aars.toml"))).unwrap();
            let defaults = Config::default();
            assert_eq!(cfg.server, defaults.server);
            assert_eq!(cfg.audio, defaults.audio);
            assert_eq!(cfg.ptt, defaults.ptt);
            assert_eq!(cfg.tts.model, defaults.tts.model);
            assert_eq!(
                cfg.tts.voices_dir,
                std::path::PathBuf::from("/var/lib/aars/voices")
            );
            Ok(())
        });
    }

    #[test]
    fn a_config_without_sources_uses_the_imgw_default() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("aars.toml", "[server]\nbind = \"127.0.0.1:9000\"\n")?;
            let cfg = Config::load(Some(std::path::Path::new("aars.toml"))).unwrap();
            assert_eq!(
                cfg.sources.imgw_meteo_url,
                "https://danepubliczne.imgw.pl/api/data/warningsmeteo"
            );
            Ok(())
        });
    }

    #[test]
    fn the_imgw_url_can_be_overridden_from_the_environment() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("AARS_SOURCES__IMGW_METEO_URL", "http://127.0.0.1:9/x");
            let cfg = Config::load(None).unwrap();
            assert_eq!(cfg.sources.imgw_meteo_url, "http://127.0.0.1:9/x");
            Ok(())
        });
    }

    #[test]
    fn a_malformed_imgw_url_is_a_config_error() {
        for bad in ["", "ftp://x/y", "danepubliczne.imgw.pl", "http://", "https:// x"] {
            figment::Jail::expect_with(|jail| {
                jail.set_env("AARS_SOURCES__IMGW_METEO_URL", bad);
                let err = Config::load(None).unwrap_err().to_string();
                assert!(err.contains("imgw_meteo_url"), "{bad:?}: {err}");
                Ok(())
            });
        }
    }

    #[test]
    fn a_partial_sources_block_keeps_the_other_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("aars.toml", "[sources]\nimgw_meteo_url = \"http://127.0.0.1:9/x\"\n")?;
            let cfg = Config::load(Some(std::path::Path::new("aars.toml"))).unwrap();
            assert_eq!(cfg.sources.imgw_meteo_url, "http://127.0.0.1:9/x");
            assert_eq!(cfg.sources.rso_url, "https://komunikaty.tvp.pl/komunikatyxml");
            assert_eq!(cfg.sources.swpc_url, "https://services.swpc.noaa.gov/products/noaa-scales.json");
            assert_eq!(cfg.sources.gios_url, "https://api.gios.gov.pl/pjp-api/v1/rest");
            assert_eq!(cfg.sources.emsc_url, "https://www.seismicportal.eu/fdsnws/event/1/query");
            assert_eq!(cfg.sources.open_meteo_url, "https://api.open-meteo.com/v1/forecast");
            assert_eq!(
                cfg.sources.met_no_url,
                "https://api.met.no/weatherapi/locationforecast/2.0/compact"
            );
            Ok(())
        });
    }

    #[test]
    fn every_source_url_is_validated() {
        for key in ["IMGW_METEO_URL", "RSO_URL", "SWPC_URL", "GIOS_URL", "EMSC_URL", "OPEN_METEO_URL", "MET_NO_URL"] {
            figment::Jail::expect_with(|jail| {
                jail.set_env(format!("AARS_SOURCES__{key}"), "ftp://x");
                let err = Config::load(None).unwrap_err().to_string();
                assert!(err.contains(&key.to_lowercase()), "{key}: {err}");
                Ok(())
            });
        }
    }

    // Każdy test dotykający konfiguracji pracuje w `figment::Jail`, który
    // trzyma globalny zamek. Bez tego zmienne `AARS_*` z jednego testu
    // wyciekają do równolegle biegnącego drugiego.
    #[test]
    fn defaults_are_usable_without_any_file() {
        figment::Jail::expect_with(|_jail| {
            let cfg = Config::load(None).expect("domyślna konfiguracja musi się wczytać");
            assert_eq!(cfg.server.bind, "0.0.0.0:8080");
            assert!(!cfg.server.allow_remote, "panel domyślnie tylko dla sieci lokalnej");
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
                allow_remote = true

                [ptt]
                backend = "gpio"

                [ptt.gpio]
                line = 17
                "#,
            )?;
            jail.set_env("AARS_AUDIO__SAMPLE_RATE", "44100");

            let cfg = Config::load(Some(std::path::Path::new("aars.toml"))).unwrap();
            assert_eq!(cfg.server.bind, "127.0.0.1:9000");
            assert!(cfg.server.allow_remote);
            assert_eq!(cfg.ptt.backend, PttBackendKind::Gpio);
            assert_eq!(cfg.ptt.gpio.line, 17);
            assert_eq!(cfg.audio.sample_rate, 44_100, "env musi wygrywać z plikiem");
            assert_eq!(cfg.ptt.gpio.chip, "/dev/gpiochip0", "nietknięte pola zachowują domyślne");
            Ok(())
        });
    }

    #[test]
    fn a_config_without_update_and_setup_uses_the_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("aars.toml", "[server]\nbind = \"127.0.0.1:9000\"\n")?;
            let cfg = Config::load(Some(std::path::Path::new("aars.toml"))).unwrap();
            assert_eq!(
                cfg.update.releases_url,
                "https://github.com/sptq/aars/releases"
            );
            assert_eq!(cfg.update.state_dir, PathBuf::from("/var/lib/aars/update"));
            assert_eq!(cfg.update.guard_dir, PathBuf::from("/var/lib/aars-update"));
            assert_eq!(cfg.update.install_root, PathBuf::from("/opt/aars"));
            assert_eq!(cfg.setup.first_admin, FirstAdmin::Console);
            Ok(())
        });
        // Sama sekcja bez części pól też się wczytuje (obraz ustawia tylko `wizard`).
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(
                "[setup]\nfirst_admin = \"wizard\"\n[update]\ninstall_root = \"/tmp/x\"\n",
            ))
            .extract()
            .unwrap();
        assert_eq!(cfg.setup.first_admin, FirstAdmin::Wizard);
        assert_eq!(cfg.update.install_root, PathBuf::from("/tmp/x"));
        assert_eq!(cfg.update.guard_dir, PathBuf::from("/var/lib/aars-update"));
        let partial: UpdateConfig = Figment::from(Toml::string("install_root = \"/tmp/x\"\n"))
            .extract()
            .unwrap();
        assert_eq!(partial.releases_url, UpdateConfig::default().releases_url);
        let empty: SetupConfig = Figment::from(Toml::string("")).extract().unwrap();
        assert_eq!(empty.first_admin, FirstAdmin::Console);
    }

    #[test]
    fn missing_explicit_config_file_is_an_error() {
        let err = Config::load(Some(std::path::Path::new("/nie/ma/takiego.toml"))).unwrap_err();
        assert!(matches!(err, crate::error::ConfigError::NotFound { .. }));
    }
}
