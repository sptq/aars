//! Pobranie wydania z GitHub Releases (R5): manifest i podpis spod
//! `{releases_url}/latest/download/`, archiwum spod
//! `{releases_url}/download/v{wersja}/`. Bez API GitHuba.

use std::path::{Path, PathBuf};
use std::time::Duration;

use aars_core::update::{
    MANIFEST_NAME, Manifest, SIGNATURE_NAME, UpdateRefusal, binary_archive_name, is_newer,
};

use super::verify::{MAX_ARCHIVE, MAX_SIGNATURE, TrustedKey, check_asset, verify_manifest};

/// Limity rozmiaru pobieranych plików (R5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub manifest: u64,
    pub signature: u64,
    pub archive: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            manifest: 64 * 1024,
            signature: MAX_SIGNATURE as u64,
            archive: MAX_ARCHIVE,
        }
    }
}

/// Najdłuższe pojedyncze żądanie (R5).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Dopisek do błędu TLS: Pi bez RTC po starcie bez sieci ma zły zegar (R5).
pub const CLOCK_HINT: &str = "sprawdź datę i godzinę stacji (synchronizacja NTP)";

/// Manifest po weryfikacji razem z surowymi bajtami: strażnik sprawdza
/// podpis jeszcze raz, więc oba pliki lądują obok archiwum.
#[derive(Debug, Clone)]
pub struct SignedManifest {
    pub manifest: Manifest,
    pub bytes: Vec<u8>,
    pub signature: Vec<u8>,
}

/// Pobrane i sprawdzone wydanie w `{state_dir}/staging/{wersja}-{run}`.
#[derive(Debug, Clone)]
pub struct Staged {
    /// Sprawdzony manifest wydania; stacja potrzebuje dziś tylko katalogu,
    /// a manifest czytają testy.
    #[allow(dead_code)]
    pub manifest: Manifest,
    pub dir: PathBuf,
}

pub struct ReleaseClient {
    http: reqwest::Client,
    base: String,
    key: TrustedKey,
    limits: Limits,
}

impl ReleaseClient {
    /// Klient z User-Agent `aars/{wersja}` i limitem 120 s na żądanie (R5).
    pub fn new(base: &str, key: TrustedKey) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(format!("aars/{}", crate::service::VERSION))
            .build()
            .map_err(|e| format!("klient HTTP: {e}"))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            key,
            limits: Limits::default(),
        })
    }

    /// Inne limity (testy).
    #[cfg(test)]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Najnowsze wydanie, z weryfikacją podpisu.
    pub async fn latest(&self) -> Result<SignedManifest, String> {
        let bytes = self
            .fetch_small(
                &format!("{}/latest/download/{MANIFEST_NAME}", self.base),
                MANIFEST_NAME,
                self.limits.manifest,
            )
            .await?;
        let signature = self
            .fetch_small(
                &format!("{}/latest/download/{SIGNATURE_NAME}", self.base),
                SIGNATURE_NAME,
                self.limits.signature,
            )
            .await?;
        let manifest = verify_manifest(&self.key, &bytes, &signature)?;
        Ok(SignedManifest {
            manifest,
            bytes,
            signature,
        })
    }

    /// Pobiera archiwum binarki nowszego wydania do `staging/{wersja}-{run}`:
    /// strumieniowo do `.part` z limitem, potem rozmiar i suma, `rename` i
    /// oba pliki manifestu obok. Katalog należy tylko do tego pobrania
    /// (przegląd końcowy M-4): cudzych katalogów w `staging/` nie ruszamy,
    /// bo mogą należeć do pobrania z nowszego przebiegu stacji. Resztki
    /// usuwa start przebiegu (`discard_request`). Przy błędzie znika nowy
    /// katalog.
    pub async fn stage(
        &self,
        signed: &SignedManifest,
        current: &str,
        state_dir: &Path,
        run: &str,
    ) -> Result<Staged, String> {
        let manifest = &signed.manifest;
        if !is_newer(&manifest.version, current)? {
            return Err(UpdateRefusal::NotNewer(manifest.version.clone()).to_string());
        }
        let asset = manifest.binary().clone();
        let expected = binary_archive_name(&manifest.version);
        if asset.name != expected {
            return Err(format!(
                "archiwum binarki nazywa się {}, a powinno {expected}",
                asset.name
            ));
        }

        if asset.size > self.limits.archive {
            return Err(too_big(&asset.name, self.limits.archive));
        }

        let staging = state_dir.join("staging");
        let dir = super::install::Request::new(&manifest.version, run).staged_dir(&staging)?;
        if std::fs::symlink_metadata(&dir).is_ok() {
            clear_dir(&dir).map_err(|e| format!("czyszczenie {}: {e}", dir.display()))?;
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("tworzenie {}: {e}", dir.display()))?;

        let result = self.stage_into(signed, &asset.name, &dir).await;
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        result.map(|()| Staged {
            manifest: manifest.clone(),
            dir,
        })
    }

    async fn stage_into(
        &self,
        signed: &SignedManifest,
        name: &str,
        dir: &Path,
    ) -> Result<(), String> {
        let asset = signed.manifest.binary().clone();
        let part = dir.join(format!("{name}.part"));
        let url = format!("{}/download/v{}/{name}", self.base, signed.manifest.version);
        self.download(&url, name, &part, self.limits.archive)
            .await?;
        let checked = part.clone();
        tokio::task::spawn_blocking(move || check_asset(&checked, &asset))
            .await
            .map_err(|e| format!("sprawdzanie sumy: {e}"))??;
        let io = |e: std::io::Error| format!("zapis w {}: {e}", dir.display());
        std::fs::rename(&part, dir.join(name)).map_err(io)?;
        std::fs::write(dir.join(MANIFEST_NAME), &signed.bytes).map_err(io)?;
        std::fs::write(dir.join(SIGNATURE_NAME), &signed.signature).map_err(io)?;
        Ok(())
    }

    async fn get(&self, url: &str, name: &str, max: u64) -> Result<reqwest::Response, String> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| format!("{name}: {}", with_clock_hint(error_chain(&e))))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(format!("{name}: nie znaleziono wydania (HTTP 404)"));
        }
        if !status.is_success() {
            return Err(format!(
                "{name}: serwer wydań odpowiedział HTTP {}",
                status.as_u16()
            ));
        }
        if response.content_length().is_some_and(|len| len > max) {
            return Err(too_big(name, max));
        }
        Ok(response)
    }

    /// Mały plik (manifest, podpis) w pamięci, z limitem.
    async fn fetch_small(&self, url: &str, name: &str, max: u64) -> Result<Vec<u8>, String> {
        let mut response = self.get(url, name, max).await?;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("{name}: {}", with_clock_hint(error_chain(&e))))?
        {
            if body.len() as u64 + chunk.len() as u64 > max {
                return Err(too_big(name, max));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Archiwum strumieniowo do pliku, z limitem; przy błędzie plik znika.
    async fn download(&self, url: &str, name: &str, dest: &Path, max: u64) -> Result<(), String> {
        let result = self.download_inner(url, name, dest, max).await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(dest).await;
        }
        result
    }

    async fn download_inner(
        &self,
        url: &str,
        name: &str,
        dest: &Path,
        max: u64,
    ) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;

        let mut response = self.get(url, name, max).await?;
        let io = |e: std::io::Error| format!("{name}: zapis {}: {e}", dest.display());
        let mut file = tokio::fs::File::create(dest).await.map_err(io)?;
        let mut total: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("{name}: {}", with_clock_hint(error_chain(&e))))?
        {
            total += chunk.len() as u64;
            if total > max {
                return Err(too_big(name, max));
            }
            file.write_all(&chunk).await.map_err(io)?;
        }
        file.flush().await.map_err(io)?;
        file.sync_all().await.map_err(io)?;
        Ok(())
    }
}

fn too_big(name: &str, max: u64) -> String {
    format!("{name}: plik za duży (limit {max} B)")
}

/// Usuwa całą zawartość katalogu (tworzy go, gdy go nie ma).
fn clear_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Błąd `reqwest` z całym łańcuchem przyczyn: przyczyna TLS (np. certyfikat
/// „jeszcze nieważny”) siedzi głęboko.
fn error_chain(err: &reqwest::Error) -> String {
    let mut text = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// Błąd certyfikatu na Pi bez RTC to zwykle zły zegar (R5).
fn with_clock_hint(error: String) -> String {
    if error.to_lowercase().contains("certificate") {
        format!("{error} — {CLOCK_HINT}")
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::testkit::{TestKey, manifest_for};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ARCHIVE: &[u8] = b"to jest archiwum binarki";

    /// Serwer z wydaniem `version` pod `/latest/download/` i `/download/v…/`.
    async fn release_server(key: &TestKey, version: &str, archive: &[u8]) -> MockServer {
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
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive.to_vec()))
            .mount(&server)
            .await;
        server
    }

    fn client(server: &MockServer, key: &TestKey) -> ReleaseClient {
        ReleaseClient::new(&server.uri(), key.trusted()).unwrap()
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.path().is_dir() {
                    out.extend(
                        files(&entry.path())
                            .into_iter()
                            .map(|f| format!("{name}/{f}")),
                    );
                } else {
                    out.push(name);
                }
            }
        }
        out.sort();
        out
    }

    #[tokio::test]
    async fn a_signed_manifest_from_latest_is_accepted() {
        let key = TestKey::generate();
        let server = release_server(&key, "0.11.0", ARCHIVE).await;

        let signed = client(&server, &key).latest().await.unwrap();
        assert_eq!(signed.manifest.version, "0.11.0");
        assert_eq!(signed.manifest.binary().size, ARCHIVE.len() as u64);

        let requests = server.received_requests().await.unwrap();
        let agent = requests[0]
            .headers
            .get("user-agent")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(agent, format!("aars/{}", crate::service::VERSION));
    }

    #[tokio::test]
    async fn a_signature_by_another_key_is_refused() {
        let key = TestKey::generate();
        let other = TestKey::generate();
        let server = release_server(&other, "0.11.0", ARCHIVE).await;

        let err = client(&server, &key).latest().await.unwrap_err();
        assert!(err.contains("podpis nie pasuje do klucza wydań"), "{err}");
    }

    #[tokio::test]
    async fn a_tampered_manifest_is_refused() {
        let key = TestKey::generate();
        let server = MockServer::start().await;
        let manifest = manifest_for("0.11.0", ARCHIVE);
        let sig = key.sign(&manifest, "aars 0.11.0");
        let tampered = String::from_utf8(manifest)
            .unwrap()
            .replace("Wydanie testowe.", "Wydanie testowa.");
        Mock::given(path(format!("/latest/download/{MANIFEST_NAME}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(tampered))
            .mount(&server)
            .await;
        Mock::given(path(format!("/latest/download/{SIGNATURE_NAME}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(sig))
            .mount(&server)
            .await;

        let err = client(&server, &key).latest().await.unwrap_err();
        assert!(err.contains("podpis nie pasuje do klucza wydań"), "{err}");
    }

    #[tokio::test]
    async fn an_old_manifest_signed_for_another_version_is_refused() {
        let key = TestKey::generate();
        let server = MockServer::start().await;
        let manifest = manifest_for("0.11.0", ARCHIVE);
        let sig = key.sign(&manifest, "aars 0.9.0");
        Mock::given(path(format!("/latest/download/{MANIFEST_NAME}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(manifest))
            .mount(&server)
            .await;
        Mock::given(path(format!("/latest/download/{SIGNATURE_NAME}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(sig))
            .mount(&server)
            .await;

        let err = client(&server, &key).latest().await.unwrap_err();
        assert!(
            err.contains("„aars 0.9.0” nie pasuje do wersji 0.11.0"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn staging_puts_the_archive_and_both_manifest_files_in_place() {
        let key = TestKey::generate();
        let server = release_server(&key, "0.11.0", ARCHIVE).await;
        let state = tempfile::tempdir().unwrap();
        // Pobranie innego przebiegu stacji: nie nasze, więc zostaje (M-4).
        std::fs::create_dir_all(state.path().join("staging/0.11.0-cd34")).unwrap();
        std::fs::write(state.path().join("staging/0.11.0-cd34/cudzy"), b"x").unwrap();
        // Resztka naszego katalogu znika przed pobraniem.
        std::fs::create_dir_all(state.path().join("staging/0.11.0-ab12")).unwrap();
        std::fs::write(state.path().join("staging/0.11.0-ab12/stary"), b"x").unwrap();

        let client = client(&server, &key);
        let signed = client.latest().await.unwrap();
        let staged = client.stage(&signed, "0.10.0", state.path(), "ab12").await.unwrap();

        assert_eq!(staged.dir, state.path().join("staging/0.11.0-ab12"));
        assert_eq!(staged.manifest.version, "0.11.0");
        assert_eq!(
            files(&state.path().join("staging")),
            vec![
                format!("0.11.0-ab12/{}", binary_archive_name("0.11.0")),
                format!("0.11.0-ab12/{MANIFEST_NAME}"),
                format!("0.11.0-ab12/{SIGNATURE_NAME}"),
                "0.11.0-cd34/cudzy".to_owned(),
            ]
        );
        let archive = std::fs::read(staged.dir.join(binary_archive_name("0.11.0"))).unwrap();
        assert_eq!(archive, ARCHIVE);
        // Strażnik sprawdzi podpis jeszcze raz na tych samych bajtach.
        let bytes = std::fs::read(staged.dir.join(MANIFEST_NAME)).unwrap();
        let sig = std::fs::read(staged.dir.join(SIGNATURE_NAME)).unwrap();
        assert!(verify_manifest(&key.trusted(), &bytes, &sig).is_ok());
    }

    #[tokio::test]
    async fn staging_checks_size_and_hash() {
        let key = TestKey::generate();
        let server = MockServer::start().await;
        // Manifest mówi o innym archiwum (ta sama długość, inna treść).
        let manifest = manifest_for("0.11.0", b"to jest archiwum binarkI");
        let sig = key.sign(&manifest, "aars 0.11.0");
        Mock::given(path(format!("/latest/download/{MANIFEST_NAME}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(manifest))
            .mount(&server)
            .await;
        Mock::given(path(format!("/latest/download/{SIGNATURE_NAME}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(sig))
            .mount(&server)
            .await;
        Mock::given(path(format!(
            "/download/v0.11.0/{}",
            binary_archive_name("0.11.0")
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(ARCHIVE.to_vec()))
        .mount(&server)
        .await;
        let state = tempfile::tempdir().unwrap();

        let client = client(&server, &key);
        let signed = client.latest().await.unwrap();
        let err = client
            .stage(&signed, "0.10.0", state.path(), "ab12")
            .await
            .unwrap_err();
        assert!(err.contains("suma się nie zgadza"), "{err}");
        assert!(
            files(&state.path().join("staging")).is_empty(),
            "nic nie zostaje w staging/"
        );
    }

    /// Serwer HTTP/1.1 bez `Content-Length`: ciało kończy zamknięcie połączenia.
    fn raw_server(body: Vec<u8>) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
                for chunk in body.chunks(64 * 1024) {
                    if stream.write_all(chunk).is_err() {
                        return;
                    }
                }
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn an_oversized_download_is_cut_off() {
        let key = TestKey::generate();
        let limits = Limits {
            archive: 1024 * 1024,
            ..Limits::default()
        };
        let big = vec![7u8; 1024 * 1024 + 1];
        // Manifest mieści się w limicie, a serwer śle więcej: odcina limit
        // w trakcie strumienia, nie sprawdzenie rozmiaru po pobraniu.
        let manifest = manifest_for("0.11.0", &big[..1024 * 1024]);
        let signed = SignedManifest {
            manifest: Manifest::parse(&manifest).unwrap(),
            signature: key.sign(&manifest, "aars 0.11.0"),
            bytes: manifest,
        };
        let state = tempfile::tempdir().unwrap();
        let base = raw_server(big);
        // Serwer bez `Content-Length` odpowiada na każdą ścieżkę tym samym.
        let client = ReleaseClient::new(&base, key.trusted())
            .unwrap()
            .with_limits(limits);

        let err = client
            .stage(&signed, "0.10.0", state.path(), "ab12")
            .await
            .unwrap_err();
        assert!(err.contains("za duży"), "{err}");
        assert!(
            files(&state.path().join("staging")).is_empty(),
            "plik .part usunięty"
        );
    }

    #[tokio::test]
    async fn an_oversized_manifest_is_refused_by_content_length() {
        let key = TestKey::generate();
        let server = MockServer::start().await;
        Mock::given(path(format!("/latest/download/{MANIFEST_NAME}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b' '; 64 * 1024 + 1]))
            .mount(&server)
            .await;

        let err = client(&server, &key).latest().await.unwrap_err();
        assert!(err.contains("za duży"), "{err}");
    }

    #[tokio::test]
    async fn a_version_not_newer_is_not_downloaded() {
        let key = TestKey::generate();
        let server = release_server(&key, "0.10.0", ARCHIVE).await;
        let state = tempfile::tempdir().unwrap();

        let client = client(&server, &key);
        let signed = client.latest().await.unwrap();
        let err = client
            .stage(&signed, "0.10.0", state.path(), "ab12")
            .await
            .unwrap_err();
        assert_eq!(err, "Wersja 0.10.0 nie jest nowsza od działającej.");

        let archive_requests = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().starts_with("/download/"))
            .count();
        assert_eq!(archive_requests, 0, "archiwum nie może być pobrane");
    }

    #[tokio::test]
    async fn http_errors_are_reported_in_polish() {
        let key = TestKey::generate();
        let server = MockServer::start().await;
        Mock::given(path(format!("/latest/download/{MANIFEST_NAME}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = client(&server, &key).latest().await.unwrap_err();
        assert!(err.contains("nie znaleziono wydania (HTTP 404)"), "{err}");

        // Nikt nie słucha: błąd połączenia, bez paniki.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let err = ReleaseClient::new(&url, key.trusted())
            .unwrap()
            .latest()
            .await
            .unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn certificate_errors_get_the_clock_hint() {
        assert_eq!(
            with_clock_hint("invalid peer certificate: Expired".to_owned()),
            format!("invalid peer certificate: Expired — {CLOCK_HINT}")
        );
        assert_eq!(
            with_clock_hint("connection refused".to_owned()),
            "connection refused"
        );
    }
}
