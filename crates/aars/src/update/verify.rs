//! Zaufanie do wydania (R4): podpis minisign nad manifestem, komentarz
//! zaufany `aars {wersja}` i sumy SHA-256 plików.

use std::io::Read;
use std::path::Path;

use aars_core::update::{Asset, Manifest, TARGET};
use sha2::{Digest, Sha256};

/// Klucz publiczny wydań wkompilowany w binarkę. `UNSET` znaczy, że ta
/// kompilacja nie ma klucza i aktualizacja z panelu jest wyłączona.
pub const RELEASE_KEY: &str = include_str!("../../../../keys/aars-release.pub");

/// Komunikat, gdy kompilacja nie ma klucza wydań (R4).
pub const NO_KEY: &str = "Brak klucza wydań w tej kompilacji — aktualizacja z panelu wyłączona.";

/// Najdłuższy podpis, jaki czytamy (R5).
pub const MAX_SIGNATURE: usize = 4 * 1024;

/// Największe archiwum binarki, jakie przyjmujemy (R5).
pub const MAX_ARCHIVE: u64 = 64 * 1024 * 1024;

/// Klucz publiczny, któremu ufamy przy sprawdzaniu manifestu.
#[derive(Clone)]
pub struct TrustedKey(minisign_verify::PublicKey);

impl TrustedKey {
    /// Klucz wkompilowany w binarkę; `None`, gdy plik w repo to `UNSET`.
    pub fn embedded() -> Option<Self> {
        Self::from_text(RELEASE_KEY).ok()
    }

    /// Plik `minisign.pub` (komentarz i klucz) albo sam klucz w base64.
    pub fn from_text(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.contains("UNSET") {
            return Err(NO_KEY.to_owned());
        }
        let key = if text.lines().count() >= 2 {
            minisign_verify::PublicKey::decode(text)
        } else {
            minisign_verify::PublicKey::from_base64(text)
        };
        key.map(Self)
            .map_err(|e| format!("nieczytelny klucz wydań: {e}"))
    }
}

/// Sprawdza podpis manifestu i jego komentarz zaufany, potem kształt
/// manifestu i platformę. Zwraca manifest, któremu można ufać.
pub fn verify_manifest(
    key: &TrustedKey,
    manifest: &[u8],
    signature: &[u8],
) -> Result<Manifest, String> {
    if signature.len() > MAX_SIGNATURE {
        return Err("podpis manifestu za duży".to_owned());
    }
    let text = std::str::from_utf8(signature)
        .map_err(|_| "podpis manifestu nieczytelny (nie UTF-8)".to_owned())?;
    let sig = minisign_verify::Signature::decode(text)
        .map_err(|e| format!("podpis manifestu nieczytelny: {e}"))?;
    // Tylko podpis z haszem wstępnym (domyślny w minisign od 0.8); starszy
    // tryb nie ma powodu się tu pojawić.
    key.0
        .verify(manifest, &sig, false)
        .map_err(|_| "podpis nie pasuje do klucza wydań".to_owned())?;
    let parsed = Manifest::parse(manifest)?;
    // Komentarz zaufany jest podpisany: broni przed podsunięciem starszego,
    // prawdziwie podpisanego manifestu jako najnowszego (R4).
    let expected = parsed.trusted_comment();
    if sig.trusted_comment() != expected {
        return Err(format!(
            "komentarz podpisu „{}” nie pasuje do wersji {}",
            sig.trusted_comment(),
            parsed.version
        ));
    }
    if parsed.target != TARGET {
        return Err(format!(
            "wydanie na inną platformę: {} (stacja: {TARGET})",
            parsed.target
        ));
    }
    if parsed.binary().size > MAX_ARCHIVE {
        return Err(format!(
            "archiwum binarki za duże: {} B (limit {MAX_ARCHIVE} B)",
            parsed.binary().size
        ));
    }
    Ok(parsed)
}

/// Suma SHA-256 pliku jako 64 małe znaki szesnastkowe.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Rozmiar, potem suma pliku względem wpisu w manifeście.
pub fn check_asset(path: &Path, asset: &Asset) -> Result<(), String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", asset.name))?
        .len();
    if size != asset.size {
        return Err(format!(
            "{}: rozmiar {size} B zamiast {} B",
            asset.name, asset.size
        ));
    }
    let sum = sha256_file(path).map_err(|e| format!("{}: {e}", asset.name))?;
    if sum != asset.sha256 {
        return Err(format!("{}: suma się nie zgadza", asset.name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::testkit::{TestKey, manifest_for, sha256_hex};

    #[test]
    fn a_signed_manifest_is_verified() {
        let key = TestKey::generate();
        let bytes = manifest_for("0.11.0", b"archiwum");
        let sig = key.sign(&bytes, "aars 0.11.0");

        let manifest = verify_manifest(&key.trusted(), &bytes, &sig).unwrap();
        assert_eq!(manifest.version, "0.11.0");
    }

    #[test]
    fn a_signature_by_another_key_is_refused() {
        let key = TestKey::generate();
        let other = TestKey::generate();
        let bytes = manifest_for("0.11.0", b"archiwum");
        let sig = other.sign(&bytes, "aars 0.11.0");

        let err = verify_manifest(&key.trusted(), &bytes, &sig).unwrap_err();
        assert_eq!(err, "podpis nie pasuje do klucza wydań");
    }

    #[test]
    fn a_tampered_manifest_is_refused() {
        let key = TestKey::generate();
        let bytes = manifest_for("0.11.0", b"archiwum");
        let sig = key.sign(&bytes, "aars 0.11.0");
        let text = String::from_utf8(bytes).unwrap();
        let tampered = text.replace("Wydanie testowe.", "Wydanie testowa.");
        assert_ne!(text, tampered);

        let err = verify_manifest(&key.trusted(), tampered.as_bytes(), &sig).unwrap_err();
        assert_eq!(err, "podpis nie pasuje do klucza wydań");
    }

    #[test]
    fn an_old_manifest_signed_for_another_version_is_refused() {
        let key = TestKey::generate();
        let bytes = manifest_for("0.11.0", b"archiwum");
        let sig = key.sign(&bytes, "aars 0.9.0");

        let err = verify_manifest(&key.trusted(), &bytes, &sig).unwrap_err();
        assert_eq!(
            err,
            "komentarz podpisu „aars 0.9.0” nie pasuje do wersji 0.11.0"
        );
    }

    #[test]
    fn a_manifest_for_another_platform_is_refused() {
        let key = TestKey::generate();
        let text = String::from_utf8(manifest_for("0.11.0", b"x")).unwrap();
        let bytes = text
            .replace(TARGET, "x86_64-unknown-linux-gnu")
            .into_bytes();
        let sig = key.sign(&bytes, "aars 0.11.0");

        let err = verify_manifest(&key.trusted(), &bytes, &sig).unwrap_err();
        assert!(err.contains("x86_64-unknown-linux-gnu"), "{err}");
    }

    #[test]
    fn a_binary_over_the_size_limit_is_refused() {
        let key = TestKey::generate();
        let mut value: serde_json::Value =
            serde_json::from_slice(&manifest_for("0.11.0", b"x")).unwrap();
        value["assets"][0]["size"] = serde_json::json!(MAX_ARCHIVE + 1);
        let bytes = serde_json::to_vec(&value).unwrap();
        let sig = key.sign(&bytes, "aars 0.11.0");

        let err = verify_manifest(&key.trusted(), &bytes, &sig).unwrap_err();
        assert!(err.contains("za duże"), "{err}");
    }

    #[test]
    fn garbage_signatures_are_refused_without_panic() {
        let key = TestKey::generate();
        let bytes = manifest_for("0.11.0", b"x");
        for sig in [
            &b""[..],
            b"nie podpis",
            &[0xff, 0xfe][..],
            &vec![b'a'; 5000][..],
        ] {
            assert!(verify_manifest(&key.trusted(), &bytes, sig).is_err());
        }
    }

    #[test]
    fn the_embedded_unset_key_disables_updates() {
        assert_eq!(
            TrustedKey::from_text("UNSET").err().as_deref(),
            Some(NO_KEY)
        );
        assert_eq!(
            TrustedKey::from_text("UNSET\n").err().as_deref(),
            Some(NO_KEY)
        );
        assert!(TrustedKey::from_text("").is_err());
        assert!(TrustedKey::from_text("untrusted comment: x\nnie-base64").is_err());
        // Dopóki w repo leży `UNSET`, ta kompilacja nie może aktualizować.
        assert_eq!(
            TrustedKey::embedded().is_none(),
            RELEASE_KEY.contains("UNSET")
        );

        // Prawdziwy plik klucza i sam klucz base64 są przyjmowane.
        let key = TestKey::generate();
        let text = key.public_text();
        assert!(TrustedKey::from_text(&text).is_ok());
        let b64 = text.lines().nth(1).unwrap();
        assert!(TrustedKey::from_text(b64).is_ok());
    }

    #[test]
    fn assets_are_checked_by_size_then_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tar.gz");
        std::fs::write(&path, b"archiwum").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_hex(b"archiwum"));

        let mut asset = Asset {
            name: "a.tar.gz".to_owned(),
            sha256: sha256_hex(b"archiwum"),
            size: 8,
            kind: aars_core::update::AssetKind::Binary,
        };
        assert_eq!(check_asset(&path, &asset), Ok(()));

        asset.size = 9;
        let err = check_asset(&path, &asset).unwrap_err();
        assert!(err.starts_with("a.tar.gz: "), "{err}");
        assert!(err.contains("rozmiar"), "{err}");

        asset.size = 8;
        asset.sha256 = sha256_hex(b"archiwuM");
        assert_eq!(
            check_asset(&path, &asset).unwrap_err(),
            "a.tar.gz: suma się nie zgadza"
        );
    }
}
