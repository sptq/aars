//! Instalacja wydania przez strażnika startu (R6, R7, R8 krok 3). Działa
//! jako root w `ExecStartPre`, gdy procesu `aars` już nie ma. Nigdy nie
//! dotyka PTT, karty dźwiękowej ani portu szeregowego.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use aars_core::update::{MANIFEST_NAME, Manifest, SIGNATURE_NAME, Trial, is_newer};
use chrono::{DateTime, Utc};

use super::fetch::Limits;
use super::paths::{
    UpdatePaths, installed_version, open_service_file, read_service_bytes, read_service_file,
    read_small_nofollow, swap_link, write_atomic_nofollow,
};
use super::verify::{MAX_ARCHIVE, TrustedKey, check_asset, verify_manifest};

/// Limit odczytu małych plików z katalogu usługi (R6).
pub const SMALL_FILE: usize = 4 * 1024;
/// Pliki, które przyjmujemy z archiwum (R6).
const RELEASE_FILES: [&str; 3] = ["aars", "VERSION", "LICENSE"];
/// Największy rozpakowany plik (binarka z `debug` mieści się z zapasem).
const MAX_UNPACKED: u64 = 256 * 1024 * 1024;
/// Test dymny nowej binarki (R8 krok 3).
const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Test dymny: wersja, którą nowa binarka podaje w `--version`.
pub trait SmokeTest {
    fn version_of(&self, binary: &Path) -> Result<String, String>;
}

/// Prawdziwy test: `{binarka} --version` musi wypisać `aars {wersja}` w 10 s.
/// Łapie złą architekturę i zbyt stare glibc przed przełączeniem.
pub struct ProcessSmoke;

impl SmokeTest for ProcessSmoke {
    fn version_of(&self, binary: &Path) -> Result<String, String> {
        use std::process::{Command, Stdio};
        let mut child = Command::new(binary)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("test dymny: {e}"))?;
        let deadline = std::time::Instant::now() + SMOKE_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "test dymny: brak odpowiedzi w {} s",
                        SMOKE_TIMEOUT.as_secs()
                    ));
                }
                Err(err) => return Err(format!("test dymny: {err}")),
            }
        };
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut out) = child.stdout.take() {
            let _ = out.read_to_string(&mut stdout);
        }
        if let Some(mut err) = child.stderr.take() {
            let _ = err.read_to_string(&mut stderr);
        }
        if !status.success() {
            return Err(format!(
                "test dymny: {status}: {}",
                stderr.lines().next().unwrap_or_default()
            ));
        }
        parse_version_line(&stdout)
    }
}

/// `aars 0.11.0` → `0.11.0`.
fn parse_version_line(stdout: &str) -> Result<String, String> {
    let line = stdout.lines().next().unwrap_or_default().trim();
    match line.strip_prefix("aars ") {
        Some(version) if !version.trim().is_empty() => Ok(version.trim().to_owned()),
        _ => Err(format!("test dymny: nieoczekiwana odpowiedź {line:?}")),
    }
}

/// Właściciel plików, które strażnik oddaje usłudze (kopia bazy po powrocie).
pub trait Owner {
    fn chown_to_service(&self, path: &Path) -> std::io::Result<()>;
}

/// Użytkownik i grupa usługi z `/etc/passwd` (domyślnie `aars`).
pub struct ServiceOwner {
    pub user: String,
}

impl Default for ServiceOwner {
    fn default() -> Self {
        Self {
            user: "aars".to_owned(),
        }
    }
}

impl Owner for ServiceOwner {
    fn chown_to_service(&self, path: &Path) -> std::io::Result<()> {
        let passwd = std::fs::read_to_string("/etc/passwd")?;
        let (uid, gid) = passwd
            .lines()
            .find_map(|line| {
                let mut parts = line.split(':');
                if parts.next()? != self.user {
                    return None;
                }
                let _password = parts.next()?;
                let uid = parts.next()?.parse::<u32>().ok()?;
                let gid = parts.next()?.parse::<u32>().ok()?;
                Some((uid, gid))
            })
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("brak użytkownika {}", self.user),
                )
            })?;
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))
    }
}

/// Bez zmiany właściciela (testy, praca bez roota).
#[cfg(test)]
pub struct NoOwner;

#[cfg(test)]
impl Owner for NoOwner {
    fn chown_to_service(&self, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }
}

/// Czy przed instalacją stacja nadawała: `hw_state:origin` ≠ `none` (R8).
/// Czyta **kopię** bazy z `snapshots/` (plik i katalog roota, przegląd 5–7
/// I-1, M-2), nie bazę w katalogu usługi: kopia powstała przez
/// `open_service_file`, więc nie idzie za dowiązaniem ani twardym
/// dowiązaniem. Otwarcie zwykłe, tylko do odczytu, bez `immutable`, żeby
/// SQLite nałożył `-wal`: stacja nie robi checkpointu przy wyjściu, więc
/// świeży stan bywa wyłącznie w dzienniku. Pliki `-shm` powstają w katalogu
/// roota. Każdy błąd (brak kopii, brak klucza) daje `false`.
pub fn radio_was_up(snapshot: &Path) -> bool {
    match std::fs::symlink_metadata(snapshot) {
        Ok(meta) if meta.file_type().is_file() => {}
        _ => return false,
    }
    let path = snapshot.to_owned();
    // Własny wątek z własnym runtime: strażnik bywa wołany z wnętrza tokio.
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async move {
            use sqlx::ConnectOptions;
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .read_only(true)
                .create_if_missing(false);
            let mut conn = options.connect().await.ok()?;
            let origin: Option<String> =
                sqlx::query_scalar("SELECT value FROM settings WHERE key = 'hw_state:origin'")
                    .fetch_optional(&mut conn)
                    .await
                    .ok()?;
            let _ = sqlx::Connection::close(conn).await;
            Some(origin.is_some_and(|o| o != "none"))
        })
    })
    .join()
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// Żądanie instalacji z pliku `request`: wersja w pierwszym wierszu i
/// identyfikator pobrania w drugim (przegląd końcowy M-4). Identyfikator
/// wiąże żądanie z jego katalogiem `staging/{wersja}-{id}`, więc nieaktualny
/// przebieg usuwa tylko swoje pliki. Bez identyfikatora katalog to
/// `staging/{wersja}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub version: String,
    pub run: Option<String>,
}

/// Najdłuższy identyfikator pobrania (cyfry szesnastkowe).
const MAX_RUN_ID: usize = 32;

impl Request {
    pub fn new(version: &str, run: &str) -> Self {
        Self {
            version: version.to_owned(),
            run: Some(run.to_owned()),
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut lines = text.lines().map(str::trim);
        let version = lines.next().unwrap_or_default().to_owned();
        let run = lines.next().filter(|r| !r.is_empty()).map(str::to_owned);
        Self { version, run }
    }

    /// Treść pliku `request`.
    pub fn text(&self) -> String {
        match &self.run {
            Some(run) => format!("{}\n{run}\n", self.version),
            None => format!("{}\n", self.version),
        }
    }

    /// Katalog pobranego wydania w `staging`. Wersja i identyfikator
    /// przychodzą od usługi, więc tylko bezpieczne znaki (bez `/` i `..`).
    pub fn staged_dir(&self, staging: &Path) -> Result<PathBuf, String> {
        let version_ok = !self.version.is_empty()
            && !self.version.starts_with('.')
            && self
                .version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
        if !version_ok {
            return Err(format!("nieczytelna wersja w żądaniu: {:?}", self.version));
        }
        match &self.run {
            None => Ok(staging.join(&self.version)),
            Some(run)
                if !run.is_empty()
                    && run.len() <= MAX_RUN_ID
                    && run.chars().all(|c| c.is_ascii_hexdigit()) =>
            {
                Ok(staging.join(format!("{}-{run}", self.version)))
            }
            Some(run) => Err(format!("nieczytelny identyfikator pobrania w żądaniu: {run:?}")),
        }
    }
}

/// Żądanie zapisane w `request` przez usługę.
pub fn requested(p: &UpdatePaths) -> Result<Option<Request>, String> {
    read_service_file(&p.request(), SMALL_FILE)
        .map(|text| text.map(|t| Request::parse(&t)))
        .map_err(|e| format!("odczyt żądania: {e}"))
}

/// Wersja zapisana w `request` przez usługę.
pub fn requested_version(p: &UpdatePaths) -> Result<Option<String>, String> {
    requested(p).map(|r| r.map(|r| r.version))
}

/// Instalacja żądanego wydania (R6 + R7 + R8 krok 3): ponowna weryfikacja
/// podpisu, kopia archiwum do `incoming/` i suma tam (bez TOCTOU),
/// rozpakowanie tylko `aars`/`VERSION`/`LICENSE`, test dymny, kopia bazy,
/// przełączenie `previous` i `current`, `trial.json`. Żądanie jest usuwane
/// zawsze, także po błędzie, żeby nieudana instalacja nie wracała przy
/// każdym starcie. Do błędu `current` się nie zmienia.
pub fn install_requested(
    p: &UpdatePaths,
    key: &TrustedKey,
    smoke: &dyn SmokeTest,
    current: &str,
    now: DateTime<Utc>,
) -> Result<Trial, String> {
    let requested = requested(p);
    if let Err(err) = super::paths::remove_service_entry(&p.request()) {
        tracing::warn!(%err, "usunięcie żądania pominięte");
    }
    let request = requested?.ok_or_else(|| "brak żądania instalacji".to_owned())?;
    let version = request.version.clone();
    semver::Version::parse(&version)
        .map_err(|_| format!("nieczytelna wersja w żądaniu: {version:?}"))?;

    // Ta sama wersja nigdy nie dostaje nowej próby: żądanie bieżącej wersji
    // pozwalałoby wymusić powrót do starszej. Przerwaną instalację kończy
    // `finish_interrupted` ze znacznika roota, bez żądania usługi.
    if version == current {
        return Err(format!("wersja {version} już działa"));
    }
    if !is_newer(&version, current)? {
        return Err(format!("wersja {version} nie jest nowsza od {current}"));
    }

    let staged = request.staged_dir(&p.staging())?;
    let manifest = read_signed_manifest(&staged, key)?;
    if manifest.version != version {
        return Err(format!(
            "manifest w staging jest dla wersji {}, żądanie dla {version}",
            manifest.version
        ));
    }

    let archive = copy_to_incoming(p, &staged, &manifest)?;
    let releases = p.releases();
    std::fs::create_dir_all(&releases).map_err(|e| io_err(&releases, e))?;
    let release = releases.join(&version);
    let keep = linked_releases(p);
    if keep.contains(&version) {
        return Err(format!("wydanie {version} jest w użyciu"));
    }
    let unpacked = unpack(&archive, &releases, &version);
    let _ = std::fs::remove_file(&archive);
    unpacked?;

    match smoke.version_of(&release.join("aars")) {
        Ok(reported) if reported == version => {}
        Ok(reported) => {
            let _ = std::fs::remove_dir_all(&release);
            return Err(format!(
                "test dymny: nowa binarka podaje wersję {reported}, oczekiwana {version}"
            ));
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(&release);
            return Err(err);
        }
    }

    if let Err(err) = snapshot_database(p, current) {
        let _ = std::fs::remove_dir_all(&release);
        return Err(err);
    }
    // Stan sprzed instalacji z kopii roota, z nałożonym `-wal` (I-1).
    let radio = radio_was_up(&p.snapshots().join(snapshot_name(current)));

    // Znacznik roota z oczekiwaną próbą: gdy prąd zniknie między
    // przełączeniem a zapisem próby, następny przebieg strażnika zamieni go
    // w próbę (`finish_interrupted`). Każde inne wyjście z tej funkcji
    // znacznik usuwa (strażnik zakresu), więc nie zostaje nieaktualny.
    let trial = new_trial(&version, current, radio, now);
    let marker = MarkerGuard::write(p, &trial)?;
    let old_target = std::fs::read_link(p.current()).map_err(|e| io_err(&p.current(), e))?;
    let old_previous = std::fs::read_link(p.previous()).ok();
    // Przełączenie: najpierw `previous` na działającą, potem `current`.
    swap_link(&p.previous(), &old_target).map_err(|e| io_err(&p.previous(), e))?;
    swap_link(&p.current(), &relative_release(&version)).map_err(|e| io_err(&p.current(), e))?;
    if let Err(err) = write_trial(p, &trial) {
        // Bez zapisanej próby nowa wersja chodziłaby bez licznika i bez
        // powrotu (przegląd 5–7 M-5): wracamy od razu do starej. Baza nie
        // była jeszcze otwarta przez nową wersję, więc kopii nie trzeba.
        let reverted = swap_link(&p.current(), &old_target).and_then(|()| match &old_previous {
            Some(target) => swap_link(&p.previous(), target),
            None => std::fs::remove_file(p.previous()),
        });
        if let Err(revert) = reverted {
            // Znacznik zostaje: następny przebieg zamieni go w próbę.
            std::mem::forget(marker);
            return Err(format!(
                "zapis próby zawiódł ({err}), a cofnięcie przełączenia też ({revert})"
            ));
        }
        let _ = std::fs::remove_dir_all(&release);
        return Err(format!("zapis próby zawiódł, przełączenie cofnięte: {err}"));
    }
    drop(marker);
    prune_releases(p);
    let _ = std::fs::remove_dir_all(p.staging());
    Ok(trial)
}

/// `guard_dir/switching`: próba, którą strażnik zakłada przy przełączeniu.
pub fn switching_marker(p: &UpdatePaths) -> PathBuf {
    p.guard_dir.join("switching")
}

/// Znacznik przełączenia usuwany przy każdym wyjściu z instalacji. Przerwa
/// w prądzie (bez `Drop`) zostawia go dla `finish_interrupted`.
struct MarkerGuard(PathBuf);

impl MarkerGuard {
    fn write(p: &UpdatePaths, trial: &Trial) -> Result<Self, String> {
        let path = switching_marker(p);
        let json = serde_json::to_vec(trial).map_err(|e| e.to_string())?;
        write_atomic_nofollow(&path, &json).map_err(|e| io_err(&path, e))?;
        Ok(Self(path))
    }
}

impl Drop for MarkerGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Znacznik przełączenia z poprzedniego przebiegu (R8, przerwa w prądzie).
/// Gdy `current` stoi już na wersji ze znacznika, a próby nie ma, znacznik
/// staje się próbą. W każdym innym przypadku (próba jest, przełączenie się
/// nie odbyło, znacznik zepsuty) znacznik po prostu znika. Zwraca założoną
/// próbę.
pub fn finish_interrupted(p: &UpdatePaths, trial_exists: bool) -> Option<Trial> {
    let path = switching_marker(p);
    let text = match read_small_nofollow(&path, SMALL_FILE) {
        Ok(Some(text)) => text,
        Ok(None) => return None,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    let _ = std::fs::remove_file(&path);
    if trial_exists {
        return None;
    }
    let trial: Trial = serde_json::from_str(&text).ok()?;
    if installed_version(&p.current()).as_deref() != Some(trial.version.as_str())
        || installed_version(&p.previous()).as_deref() != Some(trial.previous.as_str())
    {
        return None;
    }
    write_trial(p, &trial).ok()?;
    Some(trial)
}

fn new_trial(version: &str, previous: &str, radio_was_up: bool, now: DateTime<Utc>) -> Trial {
    Trial {
        format: 1,
        version: version.to_owned(),
        previous: previous.to_owned(),
        installed_at: now.to_rfc3339(),
        // Przebieg z instalacją poprzedza pierwszy start nowej wersji, więc
        // liczy się jako start nr 1 (R8: trzy starty, przegląd 5–7 M-1).
        attempts: 1,
        radio_was_up,
    }
}

pub fn write_trial(p: &UpdatePaths, trial: &Trial) -> Result<(), String> {
    std::fs::create_dir_all(&p.guard_dir).map_err(|e| io_err(&p.guard_dir, e))?;
    let json = serde_json::to_vec_pretty(trial).map_err(|e| e.to_string())?;
    write_atomic_nofollow(&p.trial(), &json).map_err(|e| io_err(&p.trial(), e))
}

/// Dowiązanie względne: korzeń instalacji można przenieść (testy).
fn relative_release(version: &str) -> PathBuf {
    Path::new("releases").join(version)
}

/// Nazwy wydań, na które wskazują `current` i `previous`.
fn linked_releases(p: &UpdatePaths) -> BTreeSet<String> {
    [p.current(), p.previous()]
        .iter()
        .filter_map(|link| std::fs::read_link(link).ok())
        .filter_map(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect()
}

/// Najwyżej trzy wydania: bieżące, poprzednie i nowe (R6). Po przełączeniu
/// nowe jest bieżącym, więc zostają tylko te z dowiązań.
fn prune_releases(p: &UpdatePaths) {
    let keep = linked_releases(p);
    let Ok(entries) = std::fs::read_dir(p.releases()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn io_err(path: &Path, err: std::io::Error) -> String {
    format!("{}: {err}", path.display())
}

/// Manifest z `staging/{wersja}` sprawdzony jeszcze raz kluczem wydań.
/// Odczyt przez `O_NOFOLLOW` z limitem; nawet gdyby katalog był podłożony,
/// treść musi mieć podpis właściciela.
fn read_signed_manifest(staged: &Path, key: &TrustedKey) -> Result<Manifest, String> {
    let limits = Limits::default();
    let read = |name: &str, max: u64| {
        read_service_bytes(&staged.join(name), max as usize)
            .map_err(|e| format!("{name}: {e}"))?
            .ok_or_else(|| format!("{name}: brak pliku w staging"))
    };
    let bytes = read(MANIFEST_NAME, limits.manifest)?;
    let signature = read(SIGNATURE_NAME, limits.signature)?;
    verify_manifest(key, &bytes, &signature)
}

/// Kopia archiwum z katalogu usługi do `incoming/` roota i suma na kopii.
fn copy_to_incoming(
    p: &UpdatePaths,
    staged: &Path,
    manifest: &Manifest,
) -> Result<PathBuf, String> {
    let asset = manifest.binary();
    let incoming = p.incoming();
    let _ = std::fs::remove_dir_all(&incoming);
    std::fs::create_dir_all(&incoming).map_err(|e| io_err(&incoming, e))?;
    let source = staged.join(&asset.name);
    let file = open_service_file(&source)
        .map_err(|e| io_err(&source, e))?
        .ok_or_else(|| format!("{}: brak archiwum w staging", asset.name))?;
    let dest = incoming.join(&asset.name);
    let copied = (|| {
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)?;
        std::io::copy(&mut file.take(MAX_ARCHIVE.min(asset.size) + 1), &mut out)?;
        out.sync_all()
    })();
    if let Err(err) = copied {
        let _ = std::fs::remove_file(&dest);
        return Err(io_err(&dest, err));
    }
    if let Err(err) = check_asset(&dest, asset) {
        let _ = std::fs::remove_file(&dest);
        return Err(err);
    }
    Ok(dest)
}

/// Rozpakowuje do `releases/.{wersja}.part`, potem `rename` na
/// `releases/{wersja}`. Przyjmuje tylko zwykłe pliki z `RELEASE_FILES`
/// (z ewentualnym `./` na początku), każdy raz; `aars` i `VERSION` są
/// wymagane, a `VERSION` musi mówić to samo co manifest.
fn unpack(archive: &Path, releases: &Path, version: &str) -> Result<(), String> {
    let part = releases.join(format!(".{version}.part"));
    let release = releases.join(version);
    let _ = std::fs::remove_dir_all(&part);
    let _ = std::fs::remove_dir_all(&release);
    let result = unpack_into(archive, &part, version)
        .and_then(|()| std::fs::rename(&part, &release).map_err(|e| io_err(&release, e)));
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&part);
    }
    result
}

fn unpack_into(archive: &Path, dir: &Path, version: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    let file = std::fs::File::open(archive).map_err(|e| io_err(archive, e))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut seen = BTreeSet::new();
    let entries = tar.entries().map_err(|e| format!("archiwum: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("archiwum: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("archiwum: {e}"))?
            .into_owned();
        let shown = path.display().to_string();
        if !entry.header().entry_type().is_file() {
            return Err(format!("archiwum: {shown} nie jest zwykłym plikiem"));
        }
        let parts: Vec<Component> = path
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        let name = match parts.as_slice() {
            [Component::Normal(name)] => name.to_string_lossy().into_owned(),
            _ => return Err(format!("archiwum: niedozwolona ścieżka {shown}")),
        };
        if !RELEASE_FILES.contains(&name.as_str()) {
            return Err(format!("archiwum: niedozwolony plik {shown}"));
        }
        if !seen.insert(name.clone()) {
            return Err(format!("archiwum: plik {name} występuje dwa razy"));
        }
        if entry.size() > MAX_UNPACKED {
            return Err(format!("archiwum: plik {name} za duży"));
        }
        let mode = if name == "aars" { 0o755 } else { 0o644 };
        let dest = dir.join(&name);
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&dest)
            .map_err(|e| io_err(&dest, e))?;
        let copied = std::io::copy(&mut (&mut entry).take(MAX_UNPACKED + 1), &mut out)
            .map_err(|e| format!("archiwum: {name}: {e}"))?;
        if copied > MAX_UNPACKED {
            return Err(format!("archiwum: plik {name} za duży"));
        }
        out.sync_all().map_err(|e| io_err(&dest, e))?;
    }
    for required in ["aars", "VERSION"] {
        if !seen.contains(required) {
            return Err(format!("archiwum: brak pliku {required}"));
        }
    }
    let inside = installed_version(dir);
    if inside.as_deref() != Some(version) {
        return Err(format!(
            "archiwum: VERSION mówi {}, a manifest {version}",
            inside.as_deref().unwrap_or("?")
        ));
    }
    Ok(())
}

fn snapshot_name(version: &str) -> String {
    format!("aars-{version}.db")
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Kopia bazy i `-wal` (jeśli jest) do `snapshots/aars-{stara}.db` (R7).
/// Usługa stoi, więc zwykła kopia jest spójna. Baza leży w katalogu usługi,
/// więc odczyt idzie przez `O_NOFOLLOW` (podłożone dowiązanie do pliku roota
/// nie trafi do kopii, a potem do usługi). Zostaje tylko najnowsza kopia.
fn snapshot_database(p: &UpdatePaths, current: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    let dir = p.snapshots();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
    let target = dir.join(snapshot_name(current));
    for (source, dest) in [
        (p.db_path.clone(), target.clone()),
        (sidecar(&p.db_path, "-wal"), sidecar(&target, "-wal")),
    ] {
        let Some(mut file) = open_service_file(&source).map_err(|e| io_err(&source, e))? else {
            continue;
        };
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&dest)
            .map_err(|e| io_err(&dest, e))?;
        std::io::copy(&mut file, &mut out).map_err(|e| io_err(&dest, e))?;
        out.sync_all().map_err(|e| io_err(&dest, e))?;
    }
    Ok(())
}

/// Przywraca kopię bazy wersji `version` (R7): plik tymczasowy obok bazy,
/// właściciel usługi, `rename`; stare `-wal`/`-shm` znikają. Zwraca `false`,
/// gdy kopii nie ma.
pub fn restore_database(p: &UpdatePaths, owner: &dyn Owner, version: &str) -> Result<bool, String> {
    let snapshot = p.snapshots().join(snapshot_name(version));
    if !snapshot.is_file() {
        return Ok(false);
    }
    for suffix in ["-wal", "-shm"] {
        let path = sidecar(&p.db_path, suffix);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            // Niezwykły wpis (np. katalog podłożony przez usługę) idzie na
            // bok zamiast blokować powrót przy każdym starcie (M-4).
            Err(err) => {
                let aside = sidecar(&path, &format!(".bad-{}", std::process::id()));
                std::fs::rename(&path, &aside).map_err(|_| io_err(&path, err))?;
                tracing::warn!(path = %path.display(), "niezwykły plik bazy odsunięty na bok");
            }
        }
    }
    copy_for_service(&snapshot, &p.db_path, owner)?;
    let wal = sidecar(&snapshot, "-wal");
    if wal.is_file() {
        copy_for_service(&wal, &sidecar(&p.db_path, "-wal"), owner)?;
    }
    Ok(true)
}

fn copy_for_service(source: &Path, dest: &Path, owner: &dyn Owner) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    let tmp = dest.with_file_name(format!(
        ".{}.restore-{}",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let result = (|| {
        let mut input = std::fs::File::open(source)?;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o640)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        std::io::copy(&mut input, &mut out)?;
        out.flush()?;
        out.sync_all()?;
        owner.chown_to_service(&tmp)?;
        std::fs::rename(&tmp, dest)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| io_err(dest, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_line_is_parsed() {
        assert_eq!(parse_version_line("aars 0.11.0\n"), Ok("0.11.0".to_owned()));
        assert!(parse_version_line("bash: nie można wykonać").is_err());
        assert!(parse_version_line("").is_err());
    }

    #[test]
    fn the_process_smoke_test_runs_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("aars");
        std::fs::write(&script, "#!/bin/sh\necho \"aars 0.11.0\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(ProcessSmoke.version_of(&script), Ok("0.11.0".to_owned()));

        std::fs::write(&script, "#!/bin/sh\nexit 3\n").unwrap();
        assert!(ProcessSmoke.version_of(&script).is_err());
        assert!(ProcessSmoke.version_of(&dir.path().join("brak")).is_err());
    }

    /// Kopia `aars.db` i `-wal` bez checkpointu, jak `snapshot_database`
    /// przy usłudze, która nie zamknęła bazy czysto.
    fn copy_without_checkpoint(db: &Path, dest: &Path) {
        std::fs::copy(db, dest).unwrap();
        let wal = sidecar(db, "-wal");
        if wal.exists() {
            std::fs::copy(&wal, sidecar(dest, "-wal")).unwrap();
        }
    }

    #[tokio::test]
    async fn radio_was_up_sees_a_state_written_only_to_the_wal() {
        use aars_core::hardware::{ApplyStatus, ConfigOrigin, HardwareRepo};

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("aars.db");
        let snap = dir.path().join("snap");
        std::fs::create_dir_all(&snap).unwrap();
        let copy = snap.join("aars-0.10.0.db");
        assert!(!radio_was_up(&copy), "brak kopii");
        assert!(!copy.exists(), "odczyt nie zakłada bazy");

        let pool = aars_core::db::connect(&db).await.unwrap();
        aars_core::db::migrate(&pool).await.unwrap();
        let repo = HardwareRepo::new(pool.clone());
        repo.set_apply_status(&ApplyStatus {
            origin: ConfigOrigin::None,
            error: Some("nic nie wstało".to_owned()),
        })
        .await
        .unwrap();
        // `none` w głównym pliku, potem `panel` tylko w dzienniku WAL.
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&pool)
            .await
            .unwrap();
        repo.set_apply_status(&ApplyStatus {
            origin: ConfigOrigin::Panel,
            error: None,
        })
        .await
        .unwrap();
        assert!(sidecar(&db, "-wal").metadata().unwrap().len() > 0, "stan jest w WAL");
        copy_without_checkpoint(&db, &copy);
        assert!(radio_was_up(&copy), "stan z WAL musi się liczyć (I-1)");

        // I odwrotnie: tryb bez nadawania tylko w WAL.
        repo.set_apply_status(&ApplyStatus {
            origin: ConfigOrigin::None,
            error: Some("karta zniknęła".to_owned()),
        })
        .await
        .unwrap();
        for f in [copy.clone(), sidecar(&copy, "-wal"), sidecar(&copy, "-shm")] {
            let _ = std::fs::remove_file(f);
        }
        copy_without_checkpoint(&db, &copy);
        assert!(!radio_was_up(&copy), "tryb bez nadawania");
        pool.close().await;

        std::fs::write(&copy, "to nie baza").unwrap();
        let _ = std::fs::remove_file(sidecar(&copy, "-wal"));
        let _ = std::fs::remove_file(sidecar(&copy, "-shm"));
        assert!(!radio_was_up(&copy));
    }

    #[test]
    fn radio_was_up_does_not_follow_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cel.db");
        std::fs::write(&target, "x").unwrap();
        let link = dir.path().join("aars-0.10.0.db");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(!radio_was_up(&link));
    }
}
