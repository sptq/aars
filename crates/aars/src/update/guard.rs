//! Strażnik startu (`aars update guard`, R8): samonaprawa dowiązań, licznik
//! prób nowej wersji, potwierdzenie albo powrót i instalacja żądania.
//! Działa jako root w `ExecStartPre=-+`, zawsze kończy się kodem 0 i nigdy
//! nie dotyka PTT ani audio.

use aars_core::update::{LastResult, MAX_ATTEMPTS, ResultKind, Trial};
use chrono::{DateTime, Utc};

use super::install::{
    Owner, SMALL_FILE, SmokeTest, install_requested, radio_was_up, requested_version,
    restore_database, write_trial,
};
use super::paths::{
    UpdatePaths, installed_version, is_working, read_small_nofollow, swap_link,
    write_atomic_nofollow,
};
use super::verify::{NO_KEY, TrustedKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardAction {
    Nothing,
    /// `current` nie działał, wskazuje teraz to co `previous`.
    Healed,
    /// Kolejny start nowej wersji bez potwierdzenia.
    Counted(u32),
    /// Próba zamknięta potwierdzeniem nowej wersji.
    Confirmed(String),
    /// Powrót do wersji (z kopią bazy).
    RolledBack(String),
    Installed(String),
    InstallFailed(String),
}

impl std::fmt::Display for GuardAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nothing => write!(f, "strażnik: nic do zrobienia"),
            Self::Healed => write!(f, "strażnik: naprawiono dowiązanie current z previous"),
            Self::Counted(n) => write!(f, "strażnik: start nowej wersji bez potwierdzenia nr {n}"),
            Self::Confirmed(v) => write!(f, "strażnik: wersja {v} potwierdzona"),
            Self::RolledBack(v) => write!(f, "strażnik: powrót do wersji {v}"),
            Self::Installed(v) => write!(f, "strażnik: zainstalowano wersję {v}, próba trwa"),
            Self::InstallFailed(e) => write!(f, "strażnik: instalacja nieudana: {e}"),
        }
    }
}

/// Jeden przebieg strażnika (R8). Nigdy nie panikuje i nie zwraca błędu:
/// porażka kroku trafia do wyniku i do logu, a stacja i tak wstaje.
pub fn run_guard(
    p: &UpdatePaths,
    key: Option<&TrustedKey>,
    smoke: &dyn SmokeTest,
    owner: &dyn Owner,
    now: DateTime<Utc>,
) -> Vec<GuardAction> {
    let mut actions = Vec::new();

    // 1. Samonaprawa po przerwie w prądzie między dowiązaniami.
    if !is_working(&p.current())
        && is_working(&p.previous())
        && let Ok(target) = std::fs::read_link(p.previous())
        && swap_link(&p.current(), &target).is_ok()
    {
        actions.push(GuardAction::Healed);
    }

    // 2. Trwająca próba.
    let trial = read_trial(p);
    if let Some(trial) = trial {
        actions.push(judge_trial(p, owner, trial, now));
    } else if let Ok(Some(version)) = requested_version(p) {
        // 3. Instalacja tylko poza próbą.
        actions.push(install(p, key, smoke, &version, now));
    }

    if actions.is_empty() {
        actions.push(GuardAction::Nothing);
    }
    actions
}

/// `trial.json` albo `None`. Zepsuty plik idzie na bok jako `trial.json.bad`,
/// żeby nie blokował każdego kolejnego startu.
fn read_trial(p: &UpdatePaths) -> Option<Trial> {
    let text = match read_small_nofollow(&p.trial(), SMALL_FILE) {
        Ok(Some(text)) => text,
        Ok(None) => return None,
        Err(err) => {
            tracing::error!(%err, "trial.json nieczytelny — odkładam na bok");
            set_aside(p);
            return None;
        }
    };
    match serde_json::from_str::<Trial>(&text) {
        Ok(trial) => Some(trial),
        Err(err) => {
            tracing::error!(%err, "trial.json zepsuty — odkładam na bok");
            set_aside(p);
            None
        }
    }
}

fn set_aside(p: &UpdatePaths) {
    let bad = p.guard_dir.join("trial.json.bad");
    if std::fs::rename(p.trial(), &bad).is_err() {
        let _ = std::fs::remove_file(p.trial());
    }
}

/// Treść małego pliku z katalogu usługi, przycięta; dowiązanie albo błąd
/// odczytu to „brak” (O_NOFOLLOW).
fn service_file(path: &std::path::Path) -> Option<String> {
    match read_small_nofollow(path, SMALL_FILE) {
        Ok(text) => text.map(|t| t.trim().to_owned()),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "plik usługi pominięty");
            None
        }
    }
}

fn judge_trial(
    p: &UpdatePaths,
    owner: &dyn Owner,
    mut trial: Trial,
    now: DateTime<Utc>,
) -> GuardAction {
    // Próba innej wersji niż ta, która stoi (np. po samonaprawie): nie ma już
    // czego sprawdzać.
    if installed_version(&p.current()).as_deref() != Some(trial.version.as_str()) {
        tracing::warn!(version = %trial.version, "próba nie dotyczy bieżącej wersji — zamykam ją");
        let _ = std::fs::remove_file(p.trial());
        return GuardAction::Nothing;
    }

    let confirmed = service_file(&p.confirmed());
    if confirmed.as_deref() == Some(trial.version.as_str()) {
        let _ = std::fs::remove_file(p.trial());
        let _ = std::fs::remove_file(p.confirmed());
        let _ = std::fs::remove_file(p.verdict());
        return GuardAction::Confirmed(trial.version);
    }

    let verdict = service_file(&p.verdict());
    let bad_prefix = format!("bad {}", trial.version);
    if let Some(verdict) = verdict.as_deref() {
        let ours = verdict == bad_prefix || verdict.starts_with(&format!("{bad_prefix}:"));
        if ours {
            return rolled_back(p, owner, &trial, verdict, now);
        }
        // Werdykt z innej próby nie może zabić tej.
        let _ = std::fs::remove_file(p.verdict());
    }

    if trial.attempts >= MAX_ATTEMPTS {
        let reason = format!("nie wstała w {MAX_ATTEMPTS} próbach");
        return rolled_back(p, owner, &trial, &reason, now);
    }
    trial.attempts += 1;
    if let Err(err) = write_trial(p, &trial) {
        tracing::error!(%err, "zapis licznika prób zawiódł");
    }
    GuardAction::Counted(trial.attempts)
}

fn rolled_back(
    p: &UpdatePaths,
    owner: &dyn Owner,
    trial: &Trial,
    reason: &str,
    now: DateTime<Utc>,
) -> GuardAction {
    let reason = format!("wersja {}: {reason}", trial.version);
    match rollback(p, owner, &reason, now) {
        Ok(version) => GuardAction::RolledBack(version),
        Err(err) => {
            tracing::error!(%err, "powrót do poprzedniej wersji zawiódł");
            GuardAction::InstallFailed(format!("powrót zawiódł: {err}"))
        }
    }
}

/// Powrót do `previous` z kopią bazy (R7). Zamyka próbę i zapisuje wynik
/// `rolled_back`. Zwraca wersję, do której wróciliśmy.
pub fn rollback(
    p: &UpdatePaths,
    owner: &dyn Owner,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<String, String> {
    if !is_working(&p.previous()) {
        return Err("brak działającej poprzedniej wersji".to_owned());
    }
    let target = std::fs::read_link(p.previous()).map_err(|e| e.to_string())?;
    let back_to = installed_version(&p.previous()).ok_or("brak wersji poprzedniej")?;
    let from = installed_version(&p.current()).unwrap_or_else(|| "?".to_owned());

    let restored = restore_database(p, owner, &back_to)?;
    swap_link(&p.current(), &target).map_err(|e| format!("przełączenie current: {e}"))?;
    let _ = std::fs::remove_file(p.previous());
    let _ = std::fs::remove_file(p.trial());
    let _ = std::fs::remove_file(p.confirmed());
    let _ = std::fs::remove_file(p.verdict());

    let mut detail = reason.to_owned();
    if restored {
        detail.push_str(" — baza przywrócona z kopii sprzed aktualizacji; zmiany zapisane od tego czasu przepadły");
    } else {
        detail.push_str(" — brak kopii bazy, baza bez zmian");
    }
    record(p, ResultKind::RolledBack, &from, &back_to, &detail, now);
    Ok(back_to)
}

fn install(
    p: &UpdatePaths,
    key: Option<&TrustedKey>,
    smoke: &dyn SmokeTest,
    version: &str,
    now: DateTime<Utc>,
) -> GuardAction {
    let current = installed_version(&p.current());
    let from = current.clone().unwrap_or_else(|| "?".to_owned());
    let result = match (key, current) {
        (None, _) => {
            let _ = std::fs::remove_file(p.request());
            Err(NO_KEY.to_owned())
        }
        (_, None) => {
            let _ = std::fs::remove_file(p.request());
            Err("brak działającej bieżącej wersji".to_owned())
        }
        (Some(key), Some(current)) => {
            let radio = radio_was_up(&p.db_path);
            install_requested(p, key, smoke, &current, radio, now)
        }
    };
    match result {
        Ok(trial) => {
            record(
                p,
                ResultKind::Installed,
                &trial.version,
                &trial.previous,
                "",
                now,
            );
            GuardAction::Installed(trial.version)
        }
        Err(err) => {
            record(p, ResultKind::Failed, version, &from, &err, now);
            GuardAction::InstallFailed(err)
        }
    }
}

/// `last-result.json` w katalogu usługi: zapis atomowy, bez podążania za
/// dowiązaniem (R6, R8 krok 4).
fn record(
    p: &UpdatePaths,
    kind: ResultKind,
    version: &str,
    from: &str,
    detail: &str,
    now: DateTime<Utc>,
) {
    let result = LastResult {
        format: 1,
        at: now.to_rfc3339(),
        kind,
        version: version.to_owned(),
        from: from.to_owned(),
        detail: detail.to_owned(),
    };
    let written = serde_json::to_vec_pretty(&result)
        .map_err(|e| e.to_string())
        // Katalogu usługi nie zakładamy jako root: należy do `aars`.
        .and_then(|json| write_atomic_nofollow(&p.last_result(), &json).map_err(|e| e.to_string()));
    if let Err(err) = written {
        tracing::error!(%err, "zapis last-result.json zawiódł");
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use aars_core::update::{MANIFEST_NAME, SIGNATURE_NAME, binary_archive_name};

    use super::*;
    use crate::update::install::NoOwner;
    use crate::update::testkit::{TestKey, manifest_for};

    /// Test dymny bez uruchamiania: wersja z `VERSION` obok binarki albo
    /// błąd podany przez test.
    struct FakeSmoke(Mutex<Option<String>>);

    impl FakeSmoke {
        fn ok() -> Self {
            Self(Mutex::new(None))
        }

        fn failing(err: &str) -> Self {
            Self(Mutex::new(Some(err.to_owned())))
        }
    }

    impl SmokeTest for FakeSmoke {
        fn version_of(&self, binary: &Path) -> Result<String, String> {
            if let Some(err) = self.0.lock().unwrap().clone() {
                return Err(err);
            }
            installed_version(binary.parent().unwrap()).ok_or_else(|| "brak VERSION".to_owned())
        }
    }

    struct Station {
        _dir: tempfile::TempDir,
        paths: UpdatePaths,
        key: TestKey,
    }

    fn now() -> DateTime<Utc> {
        "2026-11-21T12:00:00Z".parse().unwrap()
    }

    /// Plik w archiwum: nazwa, treść, rodzaj (`None` = zwykły plik,
    /// `Some(cel)` = dowiązanie).
    type Entry<'a> = (&'a str, &'a [u8], Option<&'a str>);

    fn tar_gz(entries: &[Entry<'_>]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (name, data, link) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            match link {
                None => {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_size(data.len() as u64);
                }
                Some(_) => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                }
            }
            // `set_path` odrzuca `..`, więc nazwę wpisujemy w nagłówek wprost.
            let raw = &mut header.as_old_mut().name;
            raw[..name.len()].copy_from_slice(name.as_bytes());
            if let Some(target) = link {
                header.set_link_name(target).unwrap();
            }
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn script(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"aars {version}\"\n").into_bytes()
    }

    fn good_archive(version: &str) -> Vec<u8> {
        let aars = script(version);
        let version_file = format!("{version}\n");
        tar_gz(&[
            ("aars", &aars, None),
            ("VERSION", version_file.as_bytes(), None),
            ("LICENSE", b"MIT", None),
        ])
    }

    fn install_release(root: &Path, version: &str) {
        let dir = root.join("releases").join(version);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aars"), script(version)).unwrap();
        std::fs::write(dir.join("VERSION"), format!("{version}\n")).unwrap();
    }

    /// Stacja na 0.10.0 z bazą `baza 0.10.0`.
    fn station() -> Station {
        let dir = tempfile::tempdir().unwrap();
        let paths = UpdatePaths {
            install_root: dir.path().join("opt/aars"),
            guard_dir: dir.path().join("var/lib/aars-update"),
            state_dir: dir.path().join("var/lib/aars/update"),
            db_path: dir.path().join("var/lib/aars/aars.db"),
        };
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        install_release(&paths.install_root, "0.10.0");
        swap_link(&paths.current(), Path::new("releases/0.10.0")).unwrap();
        std::fs::write(&paths.db_path, "baza 0.10.0").unwrap();
        Station {
            _dir: dir,
            paths,
            key: TestKey::generate(),
        }
    }

    impl Station {
        /// Usługa pobrała wydanie do `staging/` i zapisała żądanie.
        fn stage(&self, version: &str, archive: &[u8]) -> PathBuf {
            let dir = self.paths.staging().join(version);
            std::fs::create_dir_all(&dir).unwrap();
            let manifest = manifest_for(version, archive);
            std::fs::write(
                dir.join(SIGNATURE_NAME),
                self.key.sign(&manifest, &format!("aars {version}")),
            )
            .unwrap();
            std::fs::write(dir.join(MANIFEST_NAME), manifest).unwrap();
            std::fs::write(dir.join(binary_archive_name(version)), archive).unwrap();
            std::fs::write(self.paths.request(), format!("{version}\n")).unwrap();
            dir
        }

        fn guard(&self) -> Vec<GuardAction> {
            self.guard_with(&FakeSmoke::ok())
        }

        fn guard_with(&self, smoke: &dyn SmokeTest) -> Vec<GuardAction> {
            run_guard(
                &self.paths,
                Some(&self.key.trusted()),
                smoke,
                &NoOwner,
                now(),
            )
        }

        fn current(&self) -> Option<String> {
            installed_version(&self.paths.current())
        }

        fn link(&self, link: &Path) -> PathBuf {
            std::fs::read_link(link).unwrap()
        }

        fn trial(&self) -> Option<Trial> {
            std::fs::read_to_string(self.paths.trial())
                .ok()
                .map(|t| serde_json::from_str(&t).unwrap())
        }

        fn last(&self) -> LastResult {
            serde_json::from_str(&std::fs::read_to_string(self.paths.last_result()).unwrap())
                .unwrap()
        }

        fn releases(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.paths.releases())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        fn installed(&self, version: &str) {
            self.stage(version, &good_archive(version));
            assert_eq!(
                self.guard(),
                vec![GuardAction::Installed(version.to_owned())]
            );
        }
    }

    #[test]
    fn install_puts_the_release_in_place_and_starts_a_trial() {
        let st = station();
        st.stage("0.11.0", &good_archive("0.11.0"));

        assert_eq!(
            st.guard(),
            vec![GuardAction::Installed("0.11.0".to_owned())]
        );

        assert_eq!(st.link(&st.paths.current()), Path::new("releases/0.11.0"));
        assert_eq!(st.link(&st.paths.previous()), Path::new("releases/0.10.0"));
        assert_eq!(st.current().as_deref(), Some("0.11.0"));
        let release = st.paths.releases().join("0.11.0");
        assert_eq!(
            std::fs::read(release.join("aars")).unwrap(),
            script("0.11.0")
        );
        assert_eq!(
            std::fs::read_to_string(release.join("LICENSE")).unwrap(),
            "MIT"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(release.join("aars"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);

        let trial = st.trial().expect("próba trwa");
        assert_eq!(trial.version, "0.11.0");
        assert_eq!(trial.previous, "0.10.0");
        assert_eq!(trial.attempts, 0);
        assert!(!trial.radio_was_up, "baza testowa to nie SQLite");
        assert_eq!(
            std::fs::read_to_string(st.paths.snapshots().join("aars-0.10.0.db")).unwrap(),
            "baza 0.10.0"
        );
        assert!(!st.paths.request().exists(), "żądanie usunięte");
        assert!(!st.paths.staging().exists(), "staging posprzątany");
        let last = st.last();
        assert_eq!(last.kind, ResultKind::Installed);
        assert_eq!(
            (last.version.as_str(), last.from.as_str()),
            ("0.11.0", "0.10.0")
        );

        // Drugi przebieg (pierwszy restart nowej wersji) liczy próbę.
        assert_eq!(st.guard(), vec![GuardAction::Counted(1)]);
    }

    #[test]
    fn a_smoke_test_failure_leaves_current_untouched() {
        let st = station();
        st.stage("0.11.0", &good_archive("0.11.0"));

        let actions = st.guard_with(&FakeSmoke::failing("Exec format error"));

        assert_eq!(
            actions,
            vec![GuardAction::InstallFailed("Exec format error".to_owned())]
        );
        assert_eq!(st.link(&st.paths.current()), Path::new("releases/0.10.0"));
        assert!(!st.paths.previous().exists());
        assert!(
            !st.paths.releases().join("0.11.0").exists(),
            "wydanie usunięte"
        );
        assert!(st.trial().is_none());
        assert!(!st.paths.request().exists(), "nieudane żądanie nie wraca");
        let last = st.last();
        assert_eq!(last.kind, ResultKind::Failed);
        assert_eq!(last.version, "0.11.0");
        assert!(last.detail.contains("Exec format error"));
        // Kolejny start: nic do zrobienia.
        assert_eq!(st.guard(), vec![GuardAction::Nothing]);
    }

    #[test]
    fn the_guard_re_verifies_the_signature() {
        let st = station();
        let dir = st.stage("0.11.0", &good_archive("0.11.0"));
        // Usługa (albo ktoś za nią) podmienia podpis po swojej weryfikacji.
        let other = TestKey::generate();
        let manifest = std::fs::read(dir.join(MANIFEST_NAME)).unwrap();
        std::fs::write(
            dir.join(SIGNATURE_NAME),
            other.sign(&manifest, "aars 0.11.0"),
        )
        .unwrap();

        let actions = st.guard();

        assert!(
            matches!(&actions[..], [GuardAction::InstallFailed(e)] if e.contains("podpis nie pasuje")),
            "{actions:?}"
        );
        assert_eq!(
            st.releases(),
            vec!["0.10.0".to_owned()],
            "nic nie rozpakowane"
        );
        assert_eq!(st.current().as_deref(), Some("0.10.0"));
    }

    #[test]
    fn a_tampered_archive_in_staging_is_refused() {
        let st = station();
        let dir = st.stage("0.11.0", &good_archive("0.11.0"));
        let mut archive = good_archive("0.11.0");
        let last = archive.len() - 1;
        archive[last] ^= 1;
        std::fs::write(dir.join(binary_archive_name("0.11.0")), archive).unwrap();

        let actions = st.guard();

        assert!(
            matches!(&actions[..], [GuardAction::InstallFailed(e)] if e.contains("suma")),
            "{actions:?}"
        );
        assert_eq!(st.releases(), vec!["0.10.0".to_owned()]);
        assert!(
            std::fs::read_dir(st.paths.incoming())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn archives_with_paths_links_or_extra_files_are_refused() {
        let aars = script("0.11.0");
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "ścieżka ../",
                tar_gz(&[("../aars", &aars, None), ("VERSION", b"0.11.0\n", None)]),
            ),
            (
                "podkatalog",
                tar_gz(&[("bin/aars", &aars, None), ("VERSION", b"0.11.0\n", None)]),
            ),
            (
                "dowiązanie",
                tar_gz(&[
                    ("aars", b"", Some("/bin/sh")),
                    ("VERSION", b"0.11.0\n", None),
                ]),
            ),
            (
                "dodatkowy plik",
                tar_gz(&[
                    ("aars", &aars, None),
                    ("VERSION", b"0.11.0\n", None),
                    ("x", b"x", None),
                ]),
            ),
            (
                "dwa razy",
                tar_gz(&[
                    ("aars", &aars, None),
                    ("aars", &aars, None),
                    ("VERSION", b"0.11.0\n", None),
                ]),
            ),
            ("bez VERSION", tar_gz(&[("aars", &aars, None)])),
            (
                "inna wersja w VERSION",
                tar_gz(&[("aars", &aars, None), ("VERSION", b"0.12.0\n", None)]),
            ),
            ("nie gzip", b"to nie jest archiwum".to_vec()),
        ];
        for (what, archive) in cases {
            let st = station();
            st.stage("0.11.0", &archive);

            let actions = st.guard();

            assert!(
                matches!(&actions[..], [GuardAction::InstallFailed(_)]),
                "{what}: {actions:?}"
            );
            assert_eq!(st.releases(), vec!["0.10.0".to_owned()], "{what}");
            assert_eq!(
                st.link(&st.paths.current()),
                Path::new("releases/0.10.0"),
                "{what}"
            );
            assert!(st.trial().is_none(), "{what}");
        }
        // Wiodące `./` jest dozwolone (tak pakuje `tar -C dir .`).
        let st = station();
        st.stage(
            "0.11.0",
            &tar_gz(&[("./aars", &aars, None), ("./VERSION", b"0.11.0\n", None)]),
        );
        assert_eq!(
            st.guard(),
            vec![GuardAction::Installed("0.11.0".to_owned())]
        );
    }

    #[test]
    fn an_older_or_same_request_is_refused() {
        let st = station();
        st.stage("0.9.0", &good_archive("0.9.0"));
        let actions = st.guard();
        assert!(
            matches!(&actions[..], [GuardAction::InstallFailed(_)]),
            "{actions:?}"
        );
        assert_eq!(st.current().as_deref(), Some("0.10.0"));
        assert!(!st.paths.request().exists());
    }

    #[test]
    fn without_a_release_key_nothing_is_installed() {
        let st = station();
        st.stage("0.11.0", &good_archive("0.11.0"));
        let actions = run_guard(&st.paths, None, &FakeSmoke::ok(), &NoOwner, now());
        assert_eq!(actions, vec![GuardAction::InstallFailed(NO_KEY.to_owned())]);
        assert_eq!(st.releases(), vec!["0.10.0".to_owned()]);
    }

    #[test]
    fn a_confirmed_trial_is_closed() {
        let st = station();
        st.installed("0.11.0");
        std::fs::write(st.paths.confirmed(), "0.11.0\n").unwrap();

        assert_eq!(
            st.guard(),
            vec![GuardAction::Confirmed("0.11.0".to_owned())]
        );
        assert!(st.trial().is_none());
        assert!(!st.paths.confirmed().exists());
        assert_eq!(
            st.link(&st.paths.previous()),
            Path::new("releases/0.10.0"),
            "previous zostaje"
        );
        assert!(
            st.paths.snapshots().join("aars-0.10.0.db").exists(),
            "kopia do ręcznego powrotu"
        );
        assert_eq!(st.guard(), vec![GuardAction::Nothing]);
    }

    #[test]
    fn three_unconfirmed_starts_roll_back_with_the_database() {
        let st = station();
        st.installed("0.11.0");
        // Nowa wersja zmienia bazę (migracje) i zostawia pliki pomocnicze.
        std::fs::write(&st.paths.db_path, "baza 0.11.0").unwrap();
        let wal = st.paths.db_path.with_file_name("aars.db-wal");
        let shm = st.paths.db_path.with_file_name("aars.db-shm");
        std::fs::write(&wal, "wal").unwrap();
        std::fs::write(&shm, "shm").unwrap();

        for n in 1..=3 {
            assert_eq!(st.guard(), vec![GuardAction::Counted(n)]);
            assert_eq!(st.trial().unwrap().attempts, n);
        }
        assert_eq!(
            st.guard(),
            vec![GuardAction::RolledBack("0.10.0".to_owned())]
        );

        assert_eq!(st.link(&st.paths.current()), Path::new("releases/0.10.0"));
        assert_eq!(st.current().as_deref(), Some("0.10.0"));
        assert_eq!(
            std::fs::read_to_string(&st.paths.db_path).unwrap(),
            "baza 0.10.0"
        );
        assert!(!wal.exists() && !shm.exists(), "-wal/-shm usunięte");
        assert!(st.trial().is_none());
        let last = st.last();
        assert_eq!(last.kind, ResultKind::RolledBack);
        assert_eq!(
            (last.version.as_str(), last.from.as_str()),
            ("0.11.0", "0.10.0")
        );
        assert!(
            last.detail.contains("nie wstała w 3 próbach"),
            "{}",
            last.detail
        );
        assert!(last.detail.contains("przepadły"), "{}", last.detail);
        assert_eq!(st.guard(), vec![GuardAction::Nothing]);
    }

    #[test]
    fn a_bad_verdict_rolls_back_at_once() {
        let st = station();
        st.installed("0.11.0");
        std::fs::write(
            st.paths.verdict(),
            "bad 0.11.0: nadawanie nie wstało: brak urządzenia hw:1,0\n",
        )
        .unwrap();

        assert_eq!(
            st.guard(),
            vec![GuardAction::RolledBack("0.10.0".to_owned())]
        );
        assert_eq!(st.current().as_deref(), Some("0.10.0"));
        assert!(!st.paths.verdict().exists());
        let last = st.last();
        assert_eq!(last.kind, ResultKind::RolledBack);
        assert!(
            last.detail.contains("brak urządzenia hw:1,0"),
            "{}",
            last.detail
        );
    }

    #[test]
    fn a_verdict_for_another_version_is_ignored() {
        let st = station();
        st.installed("0.11.0");
        std::fs::write(st.paths.verdict(), "bad 0.11.1: coś innego\n").unwrap();
        std::fs::write(st.paths.confirmed(), "0.11.1\n").unwrap();

        assert_eq!(st.guard(), vec![GuardAction::Counted(1)]);
        assert_eq!(st.current().as_deref(), Some("0.11.0"));
    }

    #[test]
    fn a_missing_current_link_is_healed_from_previous() {
        let st = station();
        st.installed("0.11.0");
        std::fs::write(st.paths.confirmed(), "0.11.0").unwrap();
        assert_eq!(
            st.guard(),
            vec![GuardAction::Confirmed("0.11.0".to_owned())]
        );
        // Przerwa w prądzie: `current` zniknęło albo wskazuje pusty katalog.
        std::fs::remove_file(st.paths.current()).unwrap();

        assert_eq!(st.guard(), vec![GuardAction::Healed]);
        assert_eq!(st.current().as_deref(), Some("0.10.0"));

        swap_link(&st.paths.current(), Path::new("releases/nie-ma")).unwrap();
        assert_eq!(st.guard(), vec![GuardAction::Healed]);
        assert_eq!(st.current().as_deref(), Some("0.10.0"));
    }

    #[test]
    fn a_power_cut_after_the_switch_still_starts_the_trial() {
        let st = station();
        st.installed("0.11.0");
        // Przełączenie było, zapisu próby nie: żądanie wciąż leży.
        std::fs::remove_file(st.paths.trial()).unwrap();
        std::fs::write(st.paths.request(), "0.11.0").unwrap();

        assert_eq!(
            st.guard(),
            vec![GuardAction::Installed("0.11.0".to_owned())]
        );
        let trial = st.trial().unwrap();
        assert_eq!(
            (trial.version.as_str(), trial.previous.as_str()),
            ("0.11.0", "0.10.0")
        );
    }

    #[test]
    fn the_guard_does_not_follow_symlinks_planted_by_the_service() {
        let st = station();
        st.installed("0.11.0");
        let outside = st._dir.path().join("poza");
        std::fs::create_dir_all(&outside).unwrap();
        let fake_confirm = outside.join("potwierdzenie");
        std::fs::write(&fake_confirm, "0.11.0").unwrap();
        std::os::unix::fs::symlink(&fake_confirm, st.paths.confirmed()).unwrap();
        let victim = outside.join("ofiara");
        std::fs::write(&victim, "nietknięta").unwrap();
        std::fs::remove_file(st.paths.last_result()).unwrap();
        std::os::unix::fs::symlink(&victim, st.paths.last_result()).unwrap();

        // Dowiązanie zamiast `confirmed` nie potwierdza.
        assert_eq!(st.guard(), vec![GuardAction::Counted(1)]);

        std::fs::write(st.paths.verdict(), "bad 0.11.0: test").unwrap();
        assert_eq!(
            st.guard(),
            vec![GuardAction::RolledBack("0.10.0".to_owned())]
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "nietknięta");
        let meta = std::fs::symlink_metadata(st.paths.last_result()).unwrap();
        assert!(meta.file_type().is_file(), "wynik jako zwykły plik");
        assert_eq!(st.last().kind, ResultKind::RolledBack);
    }

    #[test]
    fn a_database_symlink_is_not_copied_into_the_snapshot() {
        let st = station();
        let secret = st._dir.path().join("shadow");
        std::fs::write(&secret, "tajne").unwrap();
        std::fs::remove_file(&st.paths.db_path).unwrap();
        std::os::unix::fs::symlink(&secret, &st.paths.db_path).unwrap();
        st.stage("0.11.0", &good_archive("0.11.0"));

        let actions = st.guard();

        assert!(
            matches!(&actions[..], [GuardAction::InstallFailed(_)]),
            "{actions:?}"
        );
        assert!(!st.paths.snapshots().join("aars-0.10.0.db").exists());
        assert_eq!(st.current().as_deref(), Some("0.10.0"));
    }

    #[test]
    fn the_guard_never_fails() {
        // Brak katalogów w ogóle.
        let dir = tempfile::tempdir().unwrap();
        let paths = UpdatePaths {
            install_root: dir.path().join("brak/opt"),
            guard_dir: dir.path().join("brak/guard"),
            state_dir: dir.path().join("brak/state"),
            db_path: dir.path().join("brak/aars.db"),
        };
        assert_eq!(
            run_guard(&paths, None, &FakeSmoke::ok(), &NoOwner, now()),
            vec![GuardAction::Nothing]
        );

        // Zepsuty `trial.json` idzie na bok.
        let st = station();
        std::fs::create_dir_all(&st.paths.guard_dir).unwrap();
        std::fs::write(st.paths.trial(), "{ to nie json").unwrap();
        assert_eq!(st.guard(), vec![GuardAction::Nothing]);
        assert!(!st.paths.trial().exists());
        assert_eq!(
            std::fs::read_to_string(st.paths.guard_dir.join("trial.json.bad")).unwrap(),
            "{ to nie json"
        );

        // Żądanie bez staging, śmieci w żądaniu.
        std::fs::write(st.paths.request(), "0.11.0").unwrap();
        assert!(matches!(&st.guard()[..], [GuardAction::InstallFailed(_)]));
        std::fs::write(st.paths.request(), vec![0xff; 10]).unwrap();
        let _ = st.guard();
        std::fs::write(st.paths.request(), "x".repeat(10_000)).unwrap();
        let _ = st.guard();
        assert_eq!(st.current().as_deref(), Some("0.10.0"));
    }

    #[test]
    fn only_three_releases_are_kept() {
        let st = station();
        install_release(&st.paths.install_root, "0.9.0");
        st.installed("0.11.0");
        std::fs::write(st.paths.confirmed(), "0.11.0").unwrap();
        st.guard();
        st.installed("0.12.0");

        assert_eq!(
            st.releases(),
            vec!["0.11.0".to_owned(), "0.12.0".to_owned()]
        );
        assert_eq!(st.link(&st.paths.previous()), Path::new("releases/0.11.0"));
        assert!(st.paths.snapshots().join("aars-0.11.0.db").exists());
        assert!(
            !st.paths.snapshots().join("aars-0.10.0.db").exists(),
            "jedna kopia"
        );
    }

    #[test]
    fn manual_rollback_needs_previous() {
        let st = station();
        assert!(rollback(&st.paths, &NoOwner, "ręcznie", now()).is_err());
        st.installed("0.11.0");
        std::fs::write(&st.paths.db_path, "baza 0.11.0").unwrap();
        assert_eq!(
            rollback(&st.paths, &NoOwner, "ręcznie", now()),
            Ok("0.10.0".to_owned())
        );
        assert_eq!(
            std::fs::read_to_string(&st.paths.db_path).unwrap(),
            "baza 0.10.0"
        );
        assert!(st.trial().is_none());
    }
}
