//! Aktualizacja z GitHub Releases (etap 10): pobranie i weryfikacja wydania
//! (`fetch`, `verify`), układ na dysku (`paths`), instalacja i strażnik
//! startu (`install`, `guard`). Czyste typy (manifest, próba, wynik) są w
//! `aars_core::update`.

// Do czasu, aż stacja i strażnik zaczną tego używać (zadania 6–7).
#![allow(dead_code)]

pub mod fetch;
pub mod guard;
pub mod install;
pub mod paths;
pub mod verify;

use aars_core::config::Config;
use aars_core::update::{LastResult, Trial, is_newer};
use anyhow::{Context, Result};

use crate::cli::UpdateAction;
use crate::service::VERSION;
use paths::{UpdatePaths, installed_version, is_working, read_service_file, read_small_nofollow};

/// Trwająca próba z `trial.json`, jeśli jest czytelna.
pub fn read_trial(p: &UpdatePaths) -> Option<Trial> {
    let text = read_small_nofollow(&p.trial(), install::SMALL_FILE).ok()??;
    serde_json::from_str(&text).ok()
}

/// Ostatni wynik z `last-result.json`, jeśli jest czytelny.
pub fn read_last_result(p: &UpdatePaths) -> Option<LastResult> {
    let text = read_service_file(&p.last_result(), install::SMALL_FILE).ok()??;
    serde_json::from_str(&text).ok()
}

/// Wiersze `aars update status`.
pub fn status_lines(p: &UpdatePaths) -> Vec<String> {
    let mut lines = vec![format!(
        "bieżąca:    {}",
        installed_version(&p.current()).unwrap_or_else(|| format!("{VERSION} (poza /opt/aars)"))
    )];
    lines.push(format!(
        "poprzednia: {}",
        installed_version(&p.previous()).unwrap_or_else(|| "brak".to_owned())
    ));
    match read_trial(p) {
        Some(trial) => lines.push(format!(
            "próba:      {} (od {}, startów bez potwierdzenia: {})",
            trial.version, trial.installed_at, trial.attempts
        )),
        None => lines.push("próba:      brak".to_owned()),
    }
    match read_last_result(p) {
        Some(last) => lines.push(format!(
            "ostatnio:   {} {} → {} ({}){}",
            last.audit_action(),
            last.from,
            last.version,
            last.at,
            if last.detail.is_empty() {
                String::new()
            } else {
                format!(": {}", last.detail)
            }
        )),
        None => lines.push("ostatnio:   brak wyniku".to_owned()),
    }
    if verify::TrustedKey::embedded().is_none() {
        lines.push(verify::NO_KEY.to_owned());
    }
    lines
}

/// `aars update …`.
pub async fn cli(config: &Config, action: UpdateAction) -> Result<()> {
    let p = UpdatePaths::from_config(config);
    match action {
        UpdateAction::Status => {
            for line in status_lines(&p) {
                println!("{line}");
            }
        }
        UpdateAction::Guard => {
            // R8: strażnik nigdy nie blokuje startu — każdy wynik to kod 0.
            let key = verify::TrustedKey::embedded();
            let actions = guard::run_guard(
                &p,
                key.as_ref(),
                &install::ProcessSmoke,
                &install::ServiceOwner::default(),
                chrono::Utc::now(),
            );
            for action in actions {
                tracing::info!("{action}");
                println!("{action}");
            }
        }
        UpdateAction::Rollback => {
            if !is_working(&p.previous()) {
                anyhow::bail!("brak poprzedniej wersji w {}", p.previous().display());
            }
            let previous = installed_version(&p.previous()).unwrap_or_default();
            let snapshot = p.snapshots().join(format!("aars-{previous}.db"));
            let taken = std::fs::metadata(&snapshot)
                .and_then(|m| m.modified())
                .map(|t| {
                    chrono::DateTime::<chrono::Local>::from(t)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                });
            match taken {
                Ok(taken) => println!(
                    "Przywracam {previous} i kopię bazy z {taken} — zmiany zapisane od tego czasu przepadną."
                ),
                Err(_) => {
                    println!("Przywracam {previous}; kopii bazy nie ma, baza zostaje bez zmian.")
                }
            }
            let version = guard::rollback(
                &p,
                &install::ServiceOwner::default(),
                "ręczny powrót (aars update rollback)",
                chrono::Utc::now(),
            )
            .map_err(anyhow::Error::msg)?;
            println!("bieżąca wersja: {version}");
        }
        UpdateAction::Check => {
            let key =
                verify::TrustedKey::embedded().ok_or_else(|| anyhow::anyhow!(verify::NO_KEY))?;
            let client = fetch::ReleaseClient::new(&config.update.releases_url, key)
                .map_err(anyhow::Error::msg)?;
            let signed = client.latest().await.map_err(anyhow::Error::msg)?;
            let manifest = &signed.manifest;
            let newer = is_newer(&manifest.version, VERSION)
                .map_err(anyhow::Error::msg)
                .context("porównanie wersji")?;
            println!(
                "najnowsze wydanie: {} ({}, {} B){}",
                manifest.version,
                manifest.published_at,
                manifest.binary().size,
                if newer {
                    " — nowsze od działającej"
                } else {
                    " — nie nowsze od działającej"
                }
            );
            if !manifest.notes.is_empty() {
                println!("{}", manifest.notes);
            }
        }
    }
    Ok(())
}

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
