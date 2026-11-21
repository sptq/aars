//! Czyste typy aktualizacji (etap 10): manifest wydania, porównanie wersji,
//! stan próby nowej wersji i wynik ostatniej instalacji. Pobieranie,
//! weryfikacja podpisu, instalacja i strażnik żyją w binarce `aars`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const MANIFEST_NAME: &str = "aars-manifest.json";
pub const SIGNATURE_NAME: &str = "aars-manifest.json.minisig";
/// Jedyna platforma, na którą idą wydania (R5, R11).
pub const TARGET: &str = "aarch64-unknown-linux-gnu";
/// Tyle startów bez potwierdzenia, zanim strażnik wróci do poprzedniej (R8).
pub const MAX_ATTEMPTS: u32 = 3;
/// Po takim czasie zdrowej pracy nowa wersja zapisuje potwierdzenie (R9).
pub const CONFIRM_AFTER: Duration = Duration::from_secs(60);

/// Wersja formatu manifestu, stanu próby i wyniku.
const FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    pub kind: AssetKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssetKind {
    Binary,
    Image,
    ImagerList,
    Source,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub version: String,
    pub target: String,
    pub published_at: String,
    pub notes: String,
    pub assets: Vec<Asset>,
}

impl Manifest {
    /// Odczyt i sprawdzenie kształtu manifestu: `format` 1, wersja semver,
    /// dokładnie jedna binarka, nazwy plików bez `/` (i bez `..`), sumy
    /// SHA-256 jako 64 małe znaki szesnastkowe. Podpis sprawdza binarka.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let manifest: Manifest =
            serde_json::from_slice(bytes).map_err(|e| format!("manifest nieczytelny: {e}"))?;
        if manifest.format != FORMAT {
            return Err(format!(
                "nieznany format manifestu {} (obsługiwany: {FORMAT})",
                manifest.format
            ));
        }
        parse_version(&manifest.version)?;
        for asset in &manifest.assets {
            if !safe_name(&asset.name) {
                return Err(format!(
                    "niedozwolona nazwa pliku w manifeście: {:?}",
                    asset.name
                ));
            }
            if !is_sha256_hex(&asset.sha256) {
                return Err(format!(
                    "zła suma SHA-256 pliku {}: {:?}",
                    asset.name, asset.sha256
                ));
            }
        }
        for (i, asset) in manifest.assets.iter().enumerate() {
            if manifest.assets[..i].iter().any(|a| a.name == asset.name) {
                return Err(format!(
                    "plik {} występuje w manifeście dwa razy",
                    asset.name
                ));
            }
        }
        let binaries = manifest
            .assets
            .iter()
            .filter(|a| a.kind == AssetKind::Binary)
            .count();
        if binaries != 1 {
            return Err(format!(
                "manifest musi mieć dokładnie jedną binarkę, ma {binaries}"
            ));
        }
        Ok(manifest)
    }

    /// Archiwum binarki. `parse` gwarantuje, że jest dokładnie jedno.
    pub fn binary(&self) -> &Asset {
        self.assets
            .iter()
            .find(|a| a.kind == AssetKind::Binary)
            .expect("Manifest::parse sprawdza, że binarka jest")
    }

    /// Komentarz zaufany, którego wymagamy w podpisie (R4).
    pub fn trusted_comment(&self) -> String {
        format!("aars {}", self.version)
    }
}

/// Nazwa pliku wydania: trafia do URL-a i do ścieżki w `staging/`, więc
/// tylko biała lista `[A-Za-z0-9._+-]`, bez wiodącej kropki i minusa,
/// najwyżej 255 bajtów.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn parse_version(value: &str) -> Result<semver::Version, String> {
    semver::Version::parse(value).map_err(|e| format!("niepoprawna wersja {value:?}: {e}"))
}

/// Czy `candidate` jest nowsza od `current` wg semver. Wersja przedpremierowa
/// (`-rc.1` itp.) nigdy nie jest nowsza: stacje dostają tylko wydania (R5).
pub fn is_newer(candidate: &str, current: &str) -> Result<bool, String> {
    let candidate = parse_version(candidate)?;
    let current = parse_version(current)?;
    // `cmp_precedence` pomija metadane budowy: `0.10.0+1` to ta sama wersja
    // co `0.10.0` (SemVer §10), a `Ord` z crate'a semver by je porównał.
    Ok(candidate.pre.is_empty()
        && candidate.cmp_precedence(&current) == std::cmp::Ordering::Greater)
}

/// Nazwa archiwum binarki w wydaniu.
pub fn binary_archive_name(version: &str) -> String {
    format!("aars-{version}-aarch64-linux-gnu.tar.gz")
}

/// Trwająca próba nowej wersji (`trial.json` strażnika, R8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trial {
    pub format: u32,
    pub version: String,
    pub previous: String,
    pub installed_at: String,
    pub attempts: u32,
    pub radio_was_up: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    Installed,
    RolledBack,
    Failed,
}

/// Wynik ostatniej instalacji, odmowy albo powrotu (`last-result.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastResult {
    pub format: u32,
    pub at: String,
    pub kind: ResultKind,
    pub version: String,
    pub from: String,
    pub detail: String,
}

impl LastResult {
    /// Akcja w `audit_log` (R18).
    pub fn audit_action(&self) -> &'static str {
        match self.kind {
            ResultKind::Installed => "update_installed",
            ResultKind::RolledBack => "update_rolled_back",
            ResultKind::Failed => "update_failed",
        }
    }
}

/// Co panel pokazuje (R11, R18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStatus {
    pub current: String,
    pub previous: Option<String>,
    pub trial: Option<Trial>,
    /// Powód, dla którego aktualizacja z panelu jest wyłączona.
    pub unsupported: Option<String>,
    pub last: Option<LastResult>,
}

/// Nowsze wydanie, które można zainstalować.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub version: String,
    pub notes: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateRefusal {
    #[error("Aktualizacja z panelu działa tylko w obrazie Raspberry Pi (usługa systemd).")]
    Unsupported,
    #[error("Czeka pilny komunikat — aktualizacja możliwa po jego nadaniu.")]
    UrgentPending,
    #[error("Trwa próba wersji {0} — poczekaj na jej potwierdzenie.")]
    TrialRunning(String),
    #[error("Wersja {0} nie jest nowsza od działającej.")]
    NotNewer(String),
    #[error("Inna aktualizacja jest właśnie zlecana — spróbuj ponownie za chwilę.")]
    InProgress,
    #[error("{0}")]
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "cf757ee307e3a7c23d5d8f69934deab51683d01c1c6be376d3e13146020f1530";

    fn asset(name: &str, kind: &str) -> serde_json::Value {
        serde_json::json!({ "name": name, "sha256": SHA, "size": 1234, "kind": kind })
    }

    fn manifest_json(assets: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "format": 1,
            "version": "0.10.0",
            "target": TARGET,
            "published_at": "2026-11-21T12:00:00Z",
            "notes": "Pierwsze wydanie z obrazem.",
            "assets": assets,
        })
    }

    fn full() -> serde_json::Value {
        manifest_json(vec![
            asset("aars-0.10.0-aarch64-linux-gnu.tar.gz", "binary"),
            asset("aars-0.10.0-rpi.img.xz", "image"),
            asset("aars-0.10.0-imager.json", "imager-list"),
            asset("piper_tts-1.8.0.tar.gz", "source"),
        ])
    }

    fn parse(value: &serde_json::Value) -> Result<Manifest, String> {
        Manifest::parse(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn a_valid_manifest_parses() {
        let manifest = parse(&full()).unwrap();
        assert_eq!(manifest.version, "0.10.0");
        assert_eq!(manifest.target, TARGET);
        assert_eq!(manifest.assets.len(), 4);
        assert_eq!(manifest.binary().name, binary_archive_name("0.10.0"));
        assert_eq!(manifest.binary().size, 1234);
        assert_eq!(manifest.assets[2].kind, AssetKind::ImagerList);
        assert_eq!(manifest.assets[3].kind, AssetKind::Source);
        assert_eq!(manifest.trusted_comment(), "aars 0.10.0");
    }

    #[test]
    fn manifests_with_two_binaries_bad_hashes_or_paths_are_refused() {
        let mut cases = Vec::new();
        cases.push((
            "dwie binarki",
            manifest_json(vec![
                asset("a.tar.gz", "binary"),
                asset("b.tar.gz", "binary"),
            ]),
        ));
        cases.push((
            "bez binarki",
            manifest_json(vec![asset("x.img.xz", "image")]),
        ));
        let long = "a".repeat(256);
        for name in [
            "../aars",
            "a/b",
            "..",
            "",
            "a\\b",
            "aars?x",
            "a#b",
            "%2e%2e%2faars",
            "a\nb",
            "a b",
            ".ukryty",
            "-opcja",
            "zażółć",
            long.as_str(),
        ] {
            cases.push(("nazwa", manifest_json(vec![asset(name, "binary")])));
        }
        let mut short = full();
        short["assets"][0]["sha256"] = serde_json::json!(&SHA[1..]);
        cases.push(("sha 63 znaki", short));
        let mut upper = full();
        upper["assets"][0]["sha256"] = serde_json::json!(SHA.to_uppercase());
        cases.push(("wielkie litery", upper));
        let mut not_hex = full();
        not_hex["assets"][1]["sha256"] = serde_json::json!(format!("{}g", &SHA[1..]));
        cases.push(("nie hex", not_hex));
        let mut format2 = full();
        format2["format"] = serde_json::json!(2);
        cases.push(("format 2", format2));
        let mut bad_version = full();
        bad_version["version"] = serde_json::json!("abc");
        cases.push(("wersja abc", bad_version));
        let mut twice = full();
        twice["assets"][1]["name"] = twice["assets"][0]["name"].clone();
        twice["assets"][1]["kind"] = serde_json::json!("image");
        cases.push(("ten sam plik dwa razy", twice));
        let mut unknown_kind = full();
        unknown_kind["assets"][1]["kind"] = serde_json::json!("script");
        cases.push(("nieznany rodzaj", unknown_kind));

        for (what, value) in cases {
            assert!(parse(&value).is_err(), "{what}: {value}");
        }
        assert!(Manifest::parse(b"nie json").is_err());
    }

    #[test]
    fn asset_names_follow_the_allow_list() {
        for name in [
            "aars-0.10.0-aarch64-linux-gnu.tar.gz",
            "piper_tts-1.8.0.tar.gz",
            "aars-0.10.0+rpi.img.xz",
            &"a".repeat(255),
        ] {
            assert!(safe_name(name), "{name}");
        }
    }

    #[test]
    fn newer_is_semver_not_string_order() {
        assert_eq!(is_newer("0.10.0", "0.9.9"), Ok(true));
        assert_eq!(is_newer("0.10.0", "0.10.0"), Ok(false));
        assert_eq!(is_newer("0.9.9", "0.10.0"), Ok(false));
        assert_eq!(is_newer("0.10.1", "0.10.0"), Ok(true));
        // R5: wydanie przedpremierowe nigdy.
        assert_eq!(is_newer("0.11.0-rc.1", "0.10.0"), Ok(false));
        // Metadane budowy nie czynią wersji nowszą.
        assert_eq!(is_newer("0.10.0+1", "0.10.0"), Ok(false));
        assert_eq!(is_newer("0.10.0+rpi2", "0.10.0+rpi1"), Ok(false));
        assert_eq!(is_newer("0.10.1+1", "0.10.0"), Ok(true));
        assert!(is_newer("abc", "0.10.0").is_err());
        assert!(is_newer("0.10.0", "abc").is_err());
    }

    #[test]
    fn trial_and_last_result_round_trip_as_json() {
        let trial = Trial {
            format: 1,
            version: "0.11.0".to_owned(),
            previous: "0.10.0".to_owned(),
            installed_at: "2026-11-21T12:00:00Z".to_owned(),
            attempts: 2,
            radio_was_up: true,
        };
        let text = serde_json::to_string(&trial).unwrap();
        assert!(text.contains("\"format\":1"), "{text}");
        assert_eq!(serde_json::from_str::<Trial>(&text).unwrap(), trial);

        let result = LastResult {
            format: 1,
            at: "2026-11-21T12:05:00Z".to_owned(),
            kind: ResultKind::RolledBack,
            version: "0.11.0".to_owned(),
            from: "0.10.0".to_owned(),
            detail: "trzy starty bez potwierdzenia".to_owned(),
        };
        let text = serde_json::to_string(&result).unwrap();
        assert!(text.contains("\"kind\":\"rolled_back\""), "{text}");
        assert_eq!(serde_json::from_str::<LastResult>(&text).unwrap(), result);

        // Starsza binarka czyta plik nowszej: nieznane pole jest pomijane.
        let mut newer: serde_json::Value = serde_json::from_str(&text).unwrap();
        newer["extra"] = serde_json::json!("z przyszłości");
        assert_eq!(serde_json::from_value::<LastResult>(newer).unwrap(), result);
        let mut newer_trial = serde_json::to_value(&trial).unwrap();
        newer_trial["extra"] = serde_json::json!(1);
        assert_eq!(serde_json::from_value::<Trial>(newer_trial).unwrap(), trial);
    }

    #[test]
    fn audit_actions_per_result_kind() {
        let mut result = LastResult {
            format: 1,
            at: String::new(),
            kind: ResultKind::Installed,
            version: "0.11.0".to_owned(),
            from: "0.10.0".to_owned(),
            detail: String::new(),
        };
        assert_eq!(result.audit_action(), "update_installed");
        result.kind = ResultKind::RolledBack;
        assert_eq!(result.audit_action(), "update_rolled_back");
        result.kind = ResultKind::Failed;
        assert_eq!(result.audit_action(), "update_failed");
    }

    #[test]
    fn refusals_speak_polish() {
        assert_eq!(
            UpdateRefusal::UrgentPending.to_string(),
            "Czeka pilny komunikat — aktualizacja możliwa po jego nadaniu."
        );
        assert_eq!(
            UpdateRefusal::TrialRunning("0.11.0".to_owned()).to_string(),
            "Trwa próba wersji 0.11.0 — poczekaj na jej potwierdzenie."
        );
    }
}
