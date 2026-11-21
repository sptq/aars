//! Aktualizacja z GitHub Releases (etap 10): pobranie i weryfikacja wydania
//! (`fetch`, `verify`). Czyste typy (manifest, próba, wynik) są w
//! `aars_core::update`.

// Do czasu, aż stacja i strażnik zaczną tego używać (zadania 6–7).
#![allow(dead_code)]

pub mod fetch;
pub mod verify;

/// Klucz i podpisy wygenerowane w teście: prawdziwy klucz wydań trzyma
/// właściciel offline, a w repo leży `UNSET` (R4).
#[cfg(test)]
pub mod testkit {
    use std::io::Cursor;

    use aars_core::update::{AssetKind, binary_archive_name};
    use sha2::{Digest, Sha256};

    use super::verify::TrustedKey;

    pub struct TestKey {
        pair: minisign::KeyPair,
    }

    impl TestKey {
        pub fn generate() -> Self {
            Self {
                pair: minisign::KeyPair::generate_unencrypted_keypair().unwrap(),
            }
        }

        /// Plik klucza publicznego w formacie `minisign.pub`.
        pub fn public_text(&self) -> String {
            self.pair.pk.to_box().unwrap().into_string()
        }

        pub fn trusted(&self) -> TrustedKey {
            TrustedKey::from_text(&self.public_text()).unwrap()
        }

        /// Podpis `.minisig` z zadanym komentarzem zaufanym.
        pub fn sign(&self, data: &[u8], trusted_comment: &str) -> Vec<u8> {
            minisign::sign(
                Some(&self.pair.pk),
                &self.pair.sk,
                Cursor::new(data),
                Some(trusted_comment),
                None,
            )
            .unwrap()
            .into_string()
            .into_bytes()
        }
    }

    pub fn sha256_hex(data: &[u8]) -> String {
        let digest = Sha256::digest(data);
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Manifest wydania `version` z archiwum binarki o treści `archive`.
    pub fn manifest_for(version: &str, archive: &[u8]) -> Vec<u8> {
        let kind = |k: AssetKind| serde_json::to_value(k).unwrap();
        serde_json::to_vec_pretty(&serde_json::json!({
            "format": 1,
            "version": version,
            "target": aars_core::update::TARGET,
            "published_at": "2026-11-21T12:00:00Z",
            "notes": "Wydanie testowe.",
            "assets": [
                {
                    "name": binary_archive_name(version),
                    "sha256": sha256_hex(archive),
                    "size": archive.len(),
                    "kind": kind(AssetKind::Binary),
                },
                {
                    "name": format!("aars-{version}-rpi.img.xz"),
                    "sha256": sha256_hex(b"obraz"),
                    "size": 5,
                    "kind": kind(AssetKind::Image),
                },
            ],
        }))
        .unwrap()
    }
}
