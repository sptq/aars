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
/// Dowiązanie daje błąd (`ELOOP`), a FIFO, urządzenie czy katalog — błąd
/// „nie zwykły plik”. Dla plików roota (`guard_dir`); pliki z katalogu
/// usługi czyta `open_service_file`.
pub fn open_regular_nofollow(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    let file = match open_nofollow(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} nie jest zwykłym plikiem", path.display()),
        ));
    }
    Ok(Some(file))
}

/// Czy plik z katalogu usługi wolno czytać rootowi: jedno dowiązanie i
/// właściciel taki jak katalogu. Pliki, które strażnik zapisuje do katalogu
/// usługi, dostają właściciela katalogu w `write_atomic_nofollow`, więc
/// wyjątku dla roota nie ma.
/// `O_NOFOLLOW` nie łapie twardych dowiązań: usługa mogłaby podpiąć plik
/// roota z tego samego systemu plików (np. `/etc/shadow`) pod nazwę ze
/// swojego katalogu — wtedy `nlink` ≥ 2.
pub fn service_file_ok(nlink: u64, file_uid: u32, dir_uid: u32) -> bool {
    nlink == 1 && file_uid == dir_uid
}

/// Plik z katalogu usługi (`state_dir`, baza): jak `open_regular_nofollow`,
/// a do tego `service_file_ok`.
pub fn open_service_file(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::fs::MetadataExt;
    let Some(file) = open_regular_nofollow(path)? else {
        return Ok(None);
    };
    let meta = file.metadata()?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir_uid = std::fs::symlink_metadata(parent)?.uid();
    if !service_file_ok(meta.nlink(), meta.uid(), dir_uid) {
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

fn read_limited(
    file: Option<std::fs::File>,
    path: &Path,
    max: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let Some(file) = file else {
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

fn utf8(path: &Path, data: Option<Vec<u8>>) -> std::io::Result<Option<String>> {
    match data {
        None => Ok(None),
        Some(data) => String::from_utf8(data).map(Some).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} nie jest tekstem UTF-8", path.display()),
            )
        }),
    }
}

/// Mały plik roota (O_NOFOLLOW, limit `max`); `None`, gdy go nie ma.
pub fn read_small_nofollow(path: &Path, max: usize) -> std::io::Result<Option<String>> {
    utf8(path, read_limited(open_regular_nofollow(path)?, path, max)?)
}

/// Mały plik z katalogu usługi jako bajty (`open_service_file`, limit `max`).
pub fn read_service_bytes(path: &Path, max: usize) -> std::io::Result<Option<Vec<u8>>> {
    read_limited(open_service_file(path)?, path, max)
}

/// Mały plik tekstowy z katalogu usługi (`open_service_file`, limit `max`).
pub fn read_service_file(path: &Path, max: usize) -> std::io::Result<Option<String>> {
    utf8(path, read_service_bytes(path, max)?)
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
/// `rename`. `rename` zastępuje podłożone dowiązanie, nie jego cel. Jako
/// root plik dostaje (przez `fchown` na deskryptorze, przed `rename`)
/// właściciela katalogu: wynik strażnika w katalogu usługi przechodzi potem
/// zwykły warunek `service_file_ok`. Bez roota plik i tak należy do nas.
pub fn write_atomic_nofollow(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let parent = real_parent(path)?;
    let dir = std::fs::symlink_metadata(parent)?;
    // SAFETY: `geteuid` nie ma warunków wstępnych.
    let as_root = unsafe { libc::geteuid() } == 0;
    let tmp = temp_name(path);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        if as_root {
            std::os::unix::fs::fchown(&file, Some(dir.uid()), Some(dir.gid()))?;
        }
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Usuwa plik z katalogu usługi (`request`, `confirmed`, `verdict`) przez
/// `unlinkat` względem deskryptora katalogu otwartego z
/// `O_DIRECTORY|O_NOFOLLOW` (przegląd 5–7 M-3): gdy usługa podmieni
/// `state_dir` na dowiązanie, root nie usunie pliku w katalogu docelowym.
/// Brak pliku to nie błąd.
pub fn remove_service_entry(path: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "ścieżka bez nazwy pliku")
    })?;
    let c_parent = CString::new(parent.as_os_str().as_bytes())?;
    let c_name = CString::new(name.as_bytes())?;
    // SAFETY: obie ścieżki to poprawne C-stringi żyjące do końca wywołań.
    let fd = unsafe {
        libc::open(
            c_parent.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return if err.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(err)
        };
    }
    // SAFETY: `fd` właśnie otwarty i nigdzie indziej nie posiadany.
    let dir = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: deskryptor żyje (`dir`), nazwa to poprawny C-string.
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), c_name.as_ptr(), 0) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::NotFound {
            return Err(err);
        }
    }
    Ok(())
}

/// Plik blokady trzymany przez działające `aars run` (przegląd 5–7 M-7).
pub fn service_lock_path(p: &UpdatePaths) -> PathBuf {
    p.state_dir.join("run.lock")
}

/// Blokada `flock` na czas życia procesu `aars run`. `None` (z ostrzeżeniem
/// w logu), gdy katalogu usługi nie ma albo blokady nie da się założyć —
/// stacja i tak działa, a ręczny powrót traci tylko to zabezpieczenie.
pub fn hold_service_lock(p: &UpdatePaths) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let path = service_lock_path(p);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .inspect_err(|err| {
            tracing::warn!(path = %path.display(), %err, "blokada usługi niedostępna");
        })
        .ok()?;
    // SAFETY: deskryptor należy do `file`, który żyje w tym wywołaniu.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        tracing::warn!(path = %path.display(), "blokadę usługi trzyma inny proces aars");
        return None;
    }
    Some(file)
}

/// Czy działa `aars run` (trzyma blokadę z `hold_service_lock`). Plik
/// otwarty bez podążania za dowiązaniem i bez czekania na FIFO.
pub fn service_running(p: &UpdatePaths) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(Some(file)) = open_regular_nofollow(&service_lock_path(p)) else {
        return false;
    };
    // SAFETY: deskryptor należy do `file`, który żyje w tym wywołaniu.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // SAFETY: jak wyżej; zwalniamy własną, chwilową blokadę.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        false
    } else {
        true
    }
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
        assert!(read_service_file(&hard, 64).is_err(), "nlink 2");
        assert!(
            read_service_file(&file, 64).is_err(),
            "oryginał też ma nlink 2"
        );
        assert!(
            read_small_nofollow(&hard, 64).is_ok(),
            "pliki roota bez tego warunku"
        );
        std::fs::remove_file(&hard).unwrap();

        let fifo = dir.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        assert!(read_small_nofollow(&fifo, 64).is_err(), "FIFO bez czekania");
    }

    #[test]
    fn service_files_need_one_link_and_the_directory_owner() {
        assert!(service_file_ok(1, 1000, 1000));
        assert!(
            !service_file_ok(1, 0, 1000),
            "plik roota w katalogu usługi (np. baza)"
        );
        assert!(!service_file_ok(2, 1000, 1000), "twarde dowiązanie");
        assert!(
            !service_file_ok(2, 0, 1000),
            "twarde dowiązanie do pliku roota"
        );
        assert!(!service_file_ok(1, 1001, 1000), "cudzy plik");

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plik");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(read_service_file(&file, 8).unwrap().as_deref(), Some("x"));
        assert_eq!(
            read_service_file(&dir.path().join("brak"), 8).unwrap(),
            None
        );
    }

    #[test]
    fn removing_a_service_entry_does_not_go_through_a_swapped_directory() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("cudzy");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("request"), "0.11.0").unwrap();
        let state = dir.path().join("update");
        std::os::unix::fs::symlink(&elsewhere, &state).unwrap();

        assert!(remove_service_entry(&state.join("request")).is_err());
        assert!(elsewhere.join("request").exists(), "plik za dowiązaniem zostaje");

        std::fs::remove_file(&state).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("request"), "0.11.0").unwrap();
        remove_service_entry(&state.join("request")).unwrap();
        assert!(!state.join("request").exists());
        // Brak pliku i brak katalogu to nie błąd.
        remove_service_entry(&state.join("request")).unwrap();
        remove_service_entry(&dir.path().join("nie-ma/request")).unwrap();
    }

    #[test]
    fn the_service_lock_is_seen_while_held() {
        let dir = tempfile::tempdir().unwrap();
        let p = UpdatePaths {
            install_root: dir.path().join("opt"),
            guard_dir: dir.path().join("guard"),
            state_dir: dir.path().to_path_buf(),
            db_path: dir.path().join("aars.db"),
        };
        assert!(!service_running(&p), "bez pliku blokady usługa nie działa");
        let lock = hold_service_lock(&p).expect("blokada");
        assert!(service_running(&p));
        assert!(hold_service_lock(&p).is_none(), "drugi proces nie dostanie blokady");
        drop(lock);
        assert!(!service_running(&p));
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
