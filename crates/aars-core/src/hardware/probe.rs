//! Co jest na hoście (etap 9, R8): wyłącznie listowanie. Nic tu nie otwiera
//! portu szeregowego, urządzenia audio ani linii GPIO, bo otwarcie tty
//! podnosi DTR/RTS i może zakluczować radio.

use std::path::Path;

use super::{FieldError, HardwareSettings};
use crate::config::PttBackendKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpioChipInfo {
    pub path: String,
    pub label: String,
    pub lines: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceFile {
    pub path: String,
    /// Nazwa pliku bez `.onnx`.
    pub name: String,
}

pub trait HardwareProbe: Send + Sync {
    fn audio_outputs(&self) -> Result<Vec<String>, String>;
    fn serial_ports(&self) -> Result<Vec<String>, String>;
    fn gpio_chips(&self) -> Result<Vec<GpioChipInfo>, String>;
}

pub struct HostLists {
    pub audio: Result<Vec<String>, String>,
    pub serial: Result<Vec<String>, String>,
    pub gpio: Result<Vec<GpioChipInfo>, String>,
    pub voices: Vec<VoiceFile>,
}

/// Pliki `*.onnx` z `*.onnx.json` obok, posortowane po nazwie. Brak katalogu
/// to pusta lista.
pub fn list_voices(dir: &Path) -> Vec<VoiceFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut voices: Vec<VoiceFile> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let file = path.file_name()?.to_str()?;
            let name = file.strip_suffix(".onnx")?.to_owned();
            if !path.is_file() || !sidecar_exists(&path.to_string_lossy()) {
                return None;
            }
            Some(VoiceFile { path: path.to_string_lossy().into_owned(), name })
        })
        .collect();
    voices.sort_by(|a, b| a.name.cmp(&b.name));
    voices
}

fn sidecar_exists(model: &str) -> bool {
    Path::new(&format!("{model}.json")).is_file()
}

pub fn host_lists(probe: &dyn HardwareProbe, voices_dir: &Path) -> HostLists {
    HostLists {
        audio: probe.audio_outputs(),
        serial: probe.serial_ports(),
        gpio: probe.gpio_chips(),
        voices: list_voices(voices_dir),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct HostCheck {
    pub errors: Vec<FieldError>,
    pub warnings: Vec<String>,
}

fn or_none(items: &[String]) -> String {
    if items.is_empty() { "brak".to_owned() } else { items.join(", ") }
}

/// Pełna ścieżka musi się zgadzać; sama nazwa (`gpiochip0`) pasuje do `/dev/gpiochip0`.
fn chip_matches(info: &GpioChipInfo, wanted: &str) -> bool {
    info.path == wanted
        || (!wanted.contains('/') && info.path.rsplit('/').next() == Some(wanted))
}

/// Porównuje ustawienia z tym, co host naprawdę ma (R8).
pub fn check_against_host(s: &HardwareSettings, lists: &HostLists) -> HostCheck {
    let mut out = HostCheck::default();
    let mut error = |field: &'static str, message: String| out.errors.push(FieldError { field, message });
    let mut warnings = Vec::new();

    // `default` jest zawsze poprawnym urządzeniem.
    if s.audio_device != "default" {
        match &lists.audio {
            Ok(devices) if !devices.contains(&s.audio_device) => error(
                "audio.device",
                format!(
                    "urządzenie audio „{}” nie istnieje na tym komputerze (dostępne: {})",
                    s.audio_device,
                    or_none(devices)
                ),
            ),
            Ok(_) => {}
            Err(why) => warnings.push(format!("urządzenie audio: nie sprawdzono — {why}")),
        }
    }

    match s.ptt_backend {
        PttBackendKind::Serial => match &lists.serial {
            Ok(ports) if !ports.contains(&s.serial_port) => error(
                "ptt.serial.port",
                format!("port {} nie istnieje (dostępne: {})", s.serial_port, or_none(ports)),
            ),
            Ok(_) => {}
            Err(why) => warnings.push(format!("port szeregowy: nie sprawdzono — {why}")),
        },
        PttBackendKind::Gpio => match &lists.gpio {
            Ok(chips) => match chips.iter().find(|c| chip_matches(c, &s.gpio_chip)) {
                None => {
                    let paths: Vec<String> = chips.iter().map(|c| c.path.clone()).collect();
                    error(
                        "ptt.gpio.chip",
                        format!("układ GPIO {} nie istnieje (dostępne: {})", s.gpio_chip, or_none(&paths)),
                    );
                }
                Some(chip) => {
                    let lines = [("ptt.gpio.line", Some(s.gpio_line)), ("ptt.gpio.inhibit_line", s.gpio_inhibit_line)];
                    for (field, line) in lines {
                        if let Some(n) = line.filter(|n| *n >= chip.lines) {
                            error(
                                field,
                                format!(
                                    "linia {n} poza zakresem {} (0–{})",
                                    s.gpio_chip,
                                    chip.lines.saturating_sub(1)
                                ),
                            );
                        }
                    }
                }
            },
            Err(why) => warnings.push(format!("układ GPIO: nie sprawdzono — {why}")),
        },
        PttBackendKind::Vox => {}
    }

    let listed = lists.voices.iter().any(|v| v.path == s.voice_model);
    let on_disk = Path::new(&s.voice_model).is_file() && sidecar_exists(&s.voice_model);
    if !listed && !on_disk {
        error(
            "tts.model",
            format!("model głosu {0}: brak pliku albo brak {0}.json obok", s.voice_model),
        );
    }

    out.warnings = warnings;
    out
}

#[cfg(any(test, feature = "testing"))]
pub struct FakeProbe {
    pub audio: Result<Vec<String>, String>,
    pub serial: Result<Vec<String>, String>,
    pub gpio: Result<Vec<GpioChipInfo>, String>,
}

#[cfg(any(test, feature = "testing"))]
impl HardwareProbe for FakeProbe {
    fn audio_outputs(&self) -> Result<Vec<String>, String> {
        self.audio.clone()
    }
    fn serial_ports(&self) -> Result<Vec<String>, String> {
        self.serial.clone()
    }
    fn gpio_chips(&self) -> Result<Vec<GpioChipInfo>, String> {
        self.gpio.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn fake() -> FakeProbe {
        FakeProbe {
            audio: Ok(vec!["USB Audio".to_owned(), "hw:1,0".to_owned()]),
            serial: Ok(vec!["/dev/ttyUSB0".to_owned()]),
            gpio: Ok(vec![GpioChipInfo { path: "/dev/gpiochip0".into(), label: "pinctrl".into(), lines: 28 }]),
        }
    }

    /// Katalog z głosem `a`, a w nim model, który istnieje.
    fn voices_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in ["a.onnx", "a.onnx.json", "b.onnx", "c.txt"] {
            std::fs::write(dir.path().join(f), "x").unwrap();
        }
        dir
    }

    fn settings_with(model: &str) -> HardwareSettings {
        let mut s = HardwareSettings::from_config(&Config::default());
        s.voice_model = model.to_owned();
        s
    }

    fn check(s: &HardwareSettings, probe: &FakeProbe, dir: &Path) -> HostCheck {
        check_against_host(s, &host_lists(probe, dir))
    }

    fn valid_voice(dir: &tempfile::TempDir) -> HardwareSettings {
        settings_with(&dir.path().join("a.onnx").to_string_lossy())
    }

    #[test]
    fn voices_need_their_json_sidecar() {
        let dir = voices_dir();
        let voices = list_voices(dir.path());
        assert_eq!(voices.len(), 1);
        assert_eq!(voices[0].name, "a");
        assert!(voices[0].path.ends_with("a.onnx"));
    }

    #[test]
    fn voices_are_sorted_by_name() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["z.onnx", "z.onnx.json", "m.onnx", "m.onnx.json"] {
            std::fs::write(dir.path().join(f), "x").unwrap();
        }
        let names: Vec<_> = list_voices(dir.path()).into_iter().map(|v| v.name).collect();
        assert_eq!(names, ["m", "z"]);
    }

    #[test]
    fn a_missing_voices_dir_is_an_empty_list() {
        assert!(list_voices(Path::new("/nie/ma/takiego/katalogu")).is_empty());
    }

    #[test]
    fn default_audio_is_always_accepted() {
        let dir = voices_dir();
        let mut probe = fake();
        probe.audio = Ok(vec![]);
        let r = check(&valid_voice(&dir), &probe, dir.path());
        assert_eq!(r, HostCheck::default());
    }

    #[test]
    fn an_unknown_audio_device_is_an_error_listing_the_available_ones() {
        let dir = voices_dir();
        let mut s = valid_voice(&dir);
        s.audio_device = "Nieistniejące".into();
        let r = check(&s, &fake(), dir.path());
        assert_eq!(
            r.errors,
            vec![FieldError {
                field: "audio.device",
                message: "urządzenie audio „Nieistniejące” nie istnieje na tym komputerze (dostępne: USB Audio, hw:1,0)"
                    .into()
            }]
        );
        s.audio_device = "USB Audio".into();
        assert!(check(&s, &fake(), dir.path()).errors.is_empty());
    }

    #[test]
    fn an_unreadable_list_is_a_warning_not_an_error() {
        let dir = voices_dir();
        let mut s = valid_voice(&dir);
        s.ptt_backend = PttBackendKind::Serial;
        let mut probe = fake();
        probe.serial = Err("brak uprawnień".into());
        let r = check(&s, &probe, dir.path());
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.warnings, vec!["port szeregowy: nie sprawdzono — brak uprawnień".to_owned()]);
    }

    #[test]
    fn an_unknown_serial_port_is_an_error() {
        let dir = voices_dir();
        let mut s = valid_voice(&dir);
        s.ptt_backend = PttBackendKind::Serial;
        s.serial_port = "/dev/ttyUSB9".into();
        let mut r = check(&s, &fake(), dir.path());
        assert_eq!(r.errors.remove(0).message, "port /dev/ttyUSB9 nie istnieje (dostępne: /dev/ttyUSB0)");
        let mut probe = fake();
        probe.serial = Ok(vec![]);
        let mut r = check(&s, &probe, dir.path());
        assert_eq!(r.errors.remove(0).message, "port /dev/ttyUSB9 nie istnieje (dostępne: brak)");
    }

    #[test]
    fn gpio_fields_are_checked_only_for_the_gpio_backend() {
        let dir = voices_dir();
        let mut s = valid_voice(&dir);
        s.gpio_chip = "/dev/gpiochip9".into();
        s.serial_port = "/dev/ttyNIE".into();
        assert_eq!(s.ptt_backend, PttBackendKind::Vox);
        assert_eq!(check(&s, &fake(), dir.path()), HostCheck::default());

        s.ptt_backend = PttBackendKind::Gpio;
        let r = check(&s, &fake(), dir.path());
        assert_eq!(
            r.errors[0].message,
            "układ GPIO /dev/gpiochip9 nie istnieje (dostępne: /dev/gpiochip0)"
        );
    }

    #[test]
    fn a_gpio_line_beyond_the_chip_is_refused() {
        let dir = voices_dir();
        let mut s = valid_voice(&dir);
        s.ptt_backend = PttBackendKind::Gpio;
        s.gpio_chip = "gpiochip0".into();
        s.gpio_line = 28;
        s.gpio_inhibit_line = Some(30);
        let r = check(&s, &fake(), dir.path());
        let messages: Vec<_> = r.errors.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            messages,
            ["linia 28 poza zakresem gpiochip0 (0–27)", "linia 30 poza zakresem gpiochip0 (0–27)"]
        );
        s.gpio_line = 27;
        s.gpio_inhibit_line = Some(5);
        assert!(check(&s, &fake(), dir.path()).errors.is_empty());
    }

    #[test]
    fn the_voice_must_be_a_listed_model_or_an_existing_one_with_json() {
        let dir = voices_dir();
        let probe = fake();
        assert!(check(&valid_voice(&dir), &probe, dir.path()).errors.is_empty());

        // Istnieje, ale bez .json.
        let no_json = dir.path().join("b.onnx").to_string_lossy().into_owned();
        let r = check(&settings_with(&no_json), &probe, dir.path());
        assert_eq!(
            r.errors,
            vec![FieldError {
                field: "tts.model",
                message: format!("model głosu {no_json}: brak pliku albo brak {no_json}.json obok")
            }]
        );

        // Poza katalogiem głosów, ale cały: przechodzi.
        let other = tempfile::tempdir().unwrap();
        for f in ["x.onnx", "x.onnx.json"] {
            std::fs::write(other.path().join(f), "x").unwrap();
        }
        let outside = other.path().join("x.onnx").to_string_lossy().into_owned();
        assert!(check(&settings_with(&outside), &probe, dir.path()).errors.is_empty());

        let r = check(&settings_with("/nie/ma/v.onnx"), &probe, dir.path());
        assert_eq!(r.errors.len(), 1);
    }
}
