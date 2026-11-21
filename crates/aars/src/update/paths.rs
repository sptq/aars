//! Układ aktualizacji na dysku (R6) i bezpieczne operacje na plikach.
//!
//! Strażnik działa jako root, a katalog roboczy usługi (`state_dir`) należy
//! do użytkownika `aars`. Dlatego wszystko, co strażnik czyta stamtąd, idzie
//! przez `O_NOFOLLOW` z limitem, a zapis — przez plik tymczasowy
//! `O_CREAT|O_EXCL|O_NOFOLLOW` i `rename`. Strażnik biegnie w `ExecStartPre`,
//! gdy żaden proces `aars` nie żyje, więc katalogu nikt w tym czasie nie
//! podmienia.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use aars_core::config::Config;

/// Wszystkie ścieżki aktualizacji, wstrzykiwane (testy: `tempdir`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePaths {
    /// `/opt/aars`: `releases/`, `current`, `previous`.
    pub install_root: PathBuf,
    /// `/var/lib/aars-update` (root): `trial.json`, `snapshots/`, `incoming/`.
    pub guard_dir: PathBuf,
    /// `/var/lib/aars/update` (`aars`): `staging/`, `request`, `confirmed`,
    /// `verdict`, `last-result.json`.
    pub state_dir: PathBuf,
    /// Baza stacji.
    pub db_path: PathBuf,
}

impl UpdatePaths {
    pub fn from_config(c: &Config) -> Self {
        Self {
            install_root: c.update.install_root.clone(),
            guard_dir: c.update.guard_dir.clone(),
            state_dir: c.update.state_dir.clone(),
            db_path: c.storage.db_path.clone(),
        }
    }

    pub fn current(&self) -> PathBuf {
        self.install_root.join("current")
    }

    pub fn previous(&self) -> PathBuf {
        self.install_root.join("previous")
    }

    pub fn releases(&self) -> PathBuf {
        self.install_root.join("releases")
    }

    pub fn trial(&self) -> PathBuf {
        self.guard_dir.join("trial.json")
    }

    pub fn snapshots(&self) -> PathBuf {
        self.guard_dir.join("snapshots")
    }

    pub fn incoming(&self) -> PathBuf {
        self.guard_dir.join("incoming")
    }

    pub fn staging(&self) -> PathBuf {
        self.state_dir.join("staging")
    }

    pub fn request(&self) -> PathBuf {
        self.state_dir.join("request")
    }

    pub fn confirmed(&self) -> PathBuf {
        self.state_dir.join("confirmed")
    }

    pub fn verdict(&self) -> PathBuf {
        self.state_dir.join("verdict")
    }

    pub fn last_result(&self) -> PathBuf {
        self.state_dir.join("last-result.json")
    }
}

/// Otwarcie do odczytu bez podążania za dowiązaniem i bez czekania na FIFO.
fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// Zwykły plik otwarty bez podążania za dowiązaniem; `None`, gdy go nie ma.
/// Dowiązanie daje błąd (`ELOOP`), FIFO, urządzenie czy katalog — błąd
/// „nie zwykły plik”, a twarde dowiązanie albo plik innego właściciela niż
/// katalog — odmowę.
pub fn open_regular_nofollow(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    let file = match open_nofollow(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} nie jest zwykłym plikiem", path.display()),
        ));
    }
    // `O_NOFOLLOW` nie łapie twardych dowiązań: usługa mogłaby podpiąć plik
    // roota z tego samego systemu plików (np. `/etc/shadow`) pod nazwę ze
    // swojego katalogu. Przyjmujemy więc tylko plik z jednym dowiązaniem,
    // którego właściciel jest właścicielem katalogu.
    use std::os::unix::fs::MetadataExt;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir_uid = std::fs::symlink_metadata(parent)?.uid();
    if meta.nlink() != 1 || meta.uid() != dir_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{}: plik z {} dowiązaniami albo cudzy (uid {}, katalog {dir_uid})",
                path.display(),
                meta.nlink(),
                meta.uid()
            ),
        ));
    }
    Ok(Some(file))
}

/// Mały plik jako bajty, najwyżej `max`; dłuższy to błąd.
pub fn read_small_bytes_nofollow(path: &Path, max: usize) -> std::io::Result<Option<Vec<u8>>> {
    let Some(file) = open_regular_nofollow(path)? else {
        return Ok(None);
    };
    let mut data = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut data)?;
    if data.len() > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} dłuższy niż {max} B", path.display()),
        ));
    }
    Ok(Some(data))
}

/// Mały plik tekstowy (O_NOFOLLOW, limit `max`); `None`, gdy go nie ma.
pub fn read_small_nofollow(path: &Path, max: usize) -> std::io::Result<Option<String>> {
    match read_small_bytes_nofollow(path, max)? {
        None => Ok(None),
        Some(data) => String::from_utf8(data).map(Some).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} nie jest tekstem UTF-8", path.display()),
            )
        }),
    }
}

/// Katalog nadrzędny musi być prawdziwym katalogiem, nie dowiązaniem.
fn real_parent(path: &Path) -> std::io::Result<&Path> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let meta = std::fs::symlink_metadata(parent)?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} nie jest katalogiem", parent.display()),
        ));
    }
    Ok(parent)
}

fn temp_name(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    path.with_file_name(format!(".{name}.tmp-{}-{nanos}", std::process::id()))
}

/// Zapis atomowy: plik tymczasowy `O_CREAT|O_EXCL|O_NOFOLLOW` obok, potem
/// `rename`. `rename` zastępuje podłożone dowiązanie, nie jego cel.
pub fn write_atomic_nofollow(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    real_parent(path)?;
    let tmp = temp_name(path);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Atomowa podmiana dowiązania: nowe pod nazwą tymczasową i `rename(2)`.
pub fn swap_link(link: &Path, target: &Path) -> std::io::Result<()> {
    real_parent(link)?;
    let tmp = temp_name(link);
    std::os::unix::fs::symlink(target, &tmp)?;
    if let Err(err) = std::fs::rename(&tmp, link) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// Wersja z pliku `VERSION` w katalogu wydania (albo przez dowiązanie).
pub fn installed_version(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("VERSION")).ok()?;
    let version = text.trim();
    semver::Version::parse(version).ok()?;
    Some(version.to_owned())
}

/// Czy dowiązanie wskazuje działający katalog wydania: jest binarka i wersja.
pub fn is_working(link: &Path) -> bool {
    link.join("aars").is_file() && installed_version(link).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_reads_refuse_symlinks_fifos_and_long_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plik");
        std::fs::write(&file, "0.11.0\n").unwrap();
        assert_eq!(
            read_small_nofollow(&file, 64).unwrap().as_deref(),
            Some("0.11.0\n")
        );
        assert_eq!(
            read_small_nofollow(&dir.path().join("brak"), 64).unwrap(),
            None
        );

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(read_small_nofollow(&link, 64).is_err());

        assert!(read_small_nofollow(&file, 3).is_err(), "limit");
        assert!(read_small_nofollow(dir.path(), 64).is_err(), "katalog");

        // Twarde dowiązanie (np. usługa podpina `/etc/shadow` z tego samego
        // systemu plików) nie jest czytane.
        let hard = dir.path().join("twarde");
        std::fs::hard_link(&file, &hard).unwrap();
        assert!(read_small_nofollow(&hard, 64).is_err(), "nlink 2");
        assert!(
            read_small_nofollow(&file, 64).is_err(),
            "oryginał też ma nlink 2"
        );
        std::fs::remove_file(&hard).unwrap();

        let fifo = dir.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        assert!(read_small_nofollow(&fifo, 64).is_err(), "FIFO bez czekania");
    }

    #[test]
    fn atomic_writes_replace_a_planted_symlink_not_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("ofiara");
        std::fs::write(&victim, "nietknięta").unwrap();
        let path = dir.path().join("last-result.json");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        write_atomic_nofollow(&path, b"{}").unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "nietknięta");
        assert!(
            !std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        // Bez śmieci po plikach tymczasowych.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn atomic_writes_refuse_a_symlinked_parent() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("gdzie-indziej");
        std::fs::create_dir(&elsewhere).unwrap();
        let state = dir.path().join("update");
        std::os::unix::fs::symlink(&elsewhere, &state).unwrap();

        assert!(write_atomic_nofollow(&state.join("last-result.json"), b"{}").is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn swapping_a_link_is_atomic_and_versions_come_from_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        for v in ["0.10.0", "0.11.0"] {
            let release = dir.path().join("releases").join(v);
            std::fs::create_dir_all(&release).unwrap();
            std::fs::write(release.join("VERSION"), format!("{v}\n")).unwrap();
            std::fs::write(release.join("aars"), "#!/bin/sh\n").unwrap();
        }
        let current = dir.path().join("current");
        swap_link(&current, Path::new("releases/0.10.0")).unwrap();
        assert_eq!(installed_version(&current).as_deref(), Some("0.10.0"));
        swap_link(&current, Path::new("releases/0.11.0")).unwrap();
        assert_eq!(installed_version(&current).as_deref(), Some("0.11.0"));
        assert!(is_working(&current));
        assert!(!is_working(&dir.path().join("previous")));
        assert_eq!(installed_version(dir.path()), None);
    }
}
