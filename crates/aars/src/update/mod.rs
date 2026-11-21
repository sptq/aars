//! Aktualizacja z GitHub Releases (etap 10): pobranie i weryfikacja wydania
//! (`fetch`, `verify`), układ na dysku (`paths`), instalacja i strażnik
//! startu (`install`, `guard`). Czyste typy (manifest, próba, wynik) są w
//! `aars_core::update`.

pub mod control;
pub mod fetch;
pub mod guard;
pub mod health;
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

/// Próba, która jeszcze trwa: `trial.json` bez potwierdzenia tej wersji.
/// Po zapisaniu `confirmed` strażnik zamknie próbę dopiero przy następnym
/// starcie, a do tego czasu panel i instalacja mają ją widzieć jako
/// zakończoną (przegląd 5–7 M-6).
pub fn open_trial(p: &UpdatePaths) -> Option<Trial> {
    let trial = read_trial(p)?;
    let confirmed = read_service_file(&p.confirmed(), install::SMALL_FILE)
        .ok()
        .flatten()
        .map(|t| t.trim().to_owned());
    (confirmed.as_deref() != Some(trial.version.as_str())).then_some(trial)
}

/// Usuwa żądanie instalacji i pobrane pliki, których żaden koniec procesu
/// nie przekaże strażnikowi (przegląd 8–9 I-1): żądanie sprzed startu tego
/// przebiegu, po restarcie w procesie albo przy końcu procesu innym niż
/// aktualizacja. Instalacja dzieje się tylko w chwili wybranej przez admina.
///
/// Wołają ją tylko miejsca, w których żadne pobranie nie może już należeć do
/// żywego przebiegu (start przebiegu, koniec procesu), więc czyści cały
/// `staging/`, także resztki po przerwanych pobraniach.
pub fn discard_request(p: &paths::UpdatePaths, why: &str) -> bool {
    let request = p.request();
    let _ = std::fs::remove_dir_all(p.staging());
    if std::fs::symlink_metadata(&request).is_err() {
        return false;
    }
    if let Err(err) = std::fs::remove_file(&request) {
        tracing::error!(%err, "nie da się usunąć żądania instalacji");
        return false;
    }
    tracing::warn!(reason = why, "żądanie instalacji wycofane — zleć aktualizację jeszcze raz");
    true
}

/// Wycofanie przez nieaktualny uchwyt (przegląd końcowy M-4): tylko własny
/// katalog `staged` i żądanie, gdy jego treść to dokładnie `request` z
/// identyfikatorem tego pobrania. Żądanie i pobranie nowszego przebiegu
/// zostają. Zwraca, czy usunęła żądanie.
pub fn discard_own_request(p: &paths::UpdatePaths, request: &str, staged: &std::path::Path, why: &str) -> bool {
    let _ = std::fs::remove_dir_all(staged);
    let ours = matches!(
        read_small_nofollow(&p.request(), install::SMALL_FILE),
        Ok(Some(text)) if text == request
    );
    if !ours {
        tracing::warn!(reason = why, "pobranie wycofane; żądanie innego pobrania zostaje");
        return false;
    }
    if let Err(err) = std::fs::remove_file(p.request()) {
        tracing::error!(%err, "nie da się usunąć żądania instalacji");
        return false;
    }
    tracing::warn!(reason = why, "żądanie instalacji wycofane — zleć aktualizację jeszcze raz");
    true
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
            // Po zamknięciu próby instaluje strażnik z `current`, nie ten z
            // `previous` (przegląd końcowy I-1): `exec`, a w nim już bez
            // drugiego przekazania.
            let key = verify::TrustedKey::embedded();
            let handed_over = std::env::var_os(guard::HANDED_OVER_ENV);
            let exec = guard::ExecCurrent;
            let current = guard::current_guard(handed_over.as_deref(), &exec);
            guard::guard_and_hand_over(
                &p,
                key.as_ref(),
                &install::ProcessSmoke,
                &install::ServiceOwner::default(),
                chrono::Utc::now(),
                current,
                &mut |action| {
                    tracing::info!("{action}");
                    println!("{action}");
                },
            );
        }
        UpdateAction::Rollback { force } => {
            // R7: kopia i przywrócenie bazy tylko przy zatrzymanej usłudze
            // (przegląd 5–7 M-7). Działające `aars run` trzyma blokadę, a jej
            // brak to „nie wiadomo”, nie „zatrzymana” (przegląd 8–9 M-5).
            match paths::service_state(&p) {
                paths::ServiceState::Running => anyhow::bail!(
                    "usługa aars działa — najpierw `sudo systemctl stop aars`, potem powtórz polecenie"
                ),
                paths::ServiceState::Unknown if !force => anyhow::bail!(
                    "nie wiadomo, czy usługa aars działa (brak {}) — sprawdź `systemctl is-active aars`, \
                     zatrzymaj usługę i powtórz z --force",
                    paths::service_lock_path(&p).display()
                ),
                _ => {}
            }
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

    /// Archiwum wydania `version`: skrypt `aars` wypisujący `aars {version}`,
    /// `VERSION` i `LICENSE`, jak z `release.yml`.
    pub fn release_archive(version: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let script = format!("#!/bin/sh\necho \"aars {version}\"\n");
        let version_file = format!("{version}\n");
        for (name, data, mode) in [
            ("aars", script.as_bytes(), 0o755),
            ("VERSION", version_file.as_bytes(), 0o644),
            ("LICENSE", b"MIT".as_slice(), 0o644),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_path(name).unwrap();
            header.set_mode(mode);
            header.set_size(data.len() as u64);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder.append(&header, data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
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

    /// Fałszywy serwer wydań na 127.0.0.1 z wydaniem `version` podpisanym
    /// kluczem `key` pod `/latest/download/` i `/download/v…/`.
    pub async fn release_server(
        key: &TestKey,
        version: &str,
        archive: &[u8],
    ) -> wiremock::MockServer {
        use aars_core::update::{MANIFEST_NAME, SIGNATURE_NAME};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let manifest = manifest_for(version, archive);
        let sig = key.sign(&manifest, &format!("aars {version}"));
        Mock::given(method("GET"))
            .and(path(format!("/latest/download/{MANIFEST_NAME}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(manifest))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/latest/download/{SIGNATURE_NAME}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(sig))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/download/v{version}/{}",
                binary_archive_name(version)
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive.to_vec())
                    .set_delay(std::time::Duration::from_millis(300)),
            )
            .mount(&server)
            .await;
        server
    }
}
