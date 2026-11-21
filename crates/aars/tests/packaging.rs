//! Pliki pakowania obrazu Raspberry Pi (etap 10, Task 11 i 12). Testy czytają
//! pliki z repo; nie potrzebują systemd, sieci ani roota.

use std::path::{Path, PathBuf};

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(path)
}

fn read(path: &str) -> String {
    std::fs::read_to_string(repo(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Wiersze `klucz=wartość` unitu systemd, bez komentarzy.
fn directives(unit: &str) -> Vec<(String, String)> {
    unit.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(['#', ';', '[']))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

fn values<'a>(d: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    d.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str()).collect()
}

fn one<'a>(d: &'a [(String, String)], key: &str) -> &'a str {
    match values(d, key).as_slice() {
        [v] => v,
        other => panic!("{key}: oczekiwana jedna wartość, jest {other:?}"),
    }
}

fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[test]
fn the_service_runs_unprivileged_and_restarts_always() {
    let unit = read("packaging/systemd/aars.service");
    let d = directives(&unit);
    assert_eq!(one(&d, "User"), "aars");
    assert_eq!(one(&d, "Group"), "aars");
    assert_eq!(one(&d, "Restart"), "always");
    assert_eq!(one(&d, "StartLimitIntervalSec"), "0");
    assert_eq!(one(&d, "Type"), "notify");
    assert_eq!(one(&d, "NotifyAccess"), "main");
    assert_eq!(one(&d, "WatchdogSec"), "60");
    assert_eq!(one(&d, "TimeoutStartSec"), "180");
    assert_eq!(one(&d, "KillMode"), "mixed");
    assert_eq!(one(&d, "NoNewPrivileges"), "yes");
    assert_eq!(one(&d, "ProtectSystem"), "strict");
    assert_eq!(one(&d, "ProtectHome"), "yes");
    assert_eq!(one(&d, "PrivateTmp"), "yes");
    assert_eq!(one(&d, "ReadWritePaths"), "/var/lib/aars");
    assert_eq!(one(&d, "StateDirectory"), "aars");
    assert_eq!(one(&d, "SupplementaryGroups"), "audio gpio dialout");
    assert_eq!(one(&d, "WantedBy"), "multi-user.target");
    assert!(values(&d, "DeviceAllow").is_empty());
    assert_eq!(
        one(&d, "ExecStart"),
        "/opt/aars/current/aars --config /etc/aars/aars.toml run"
    );
    // Jedyna linia z uprawnieniami roota to strażnik startu (R8).
    let privileged: Vec<_> = d
        .iter()
        .filter(|(k, v)| k.starts_with("Exec") && v.trim_start_matches(['-', '@', ':', '!']).starts_with('+'))
        .collect();
    assert_eq!(
        privileged,
        [&(
            "ExecStartPre".to_owned(),
            "-+/opt/aars/bin/aars-update-guard --config /etc/aars/aars.toml".to_owned()
        )]
    );
    assert!(d.iter().all(|(k, v)| k != "ExecStart" || !v.starts_with(['+', '-', '!', '@'])));
    let after = one(&d, "After");
    for unit in ["network-online.target", "sound.target", "aars-reset-admin.service"] {
        assert!(after.split_whitespace().any(|u| u == unit), "After= bez {unit}");
    }
}

#[test]
fn the_reset_unit_hands_the_request_to_the_service_user() {
    let d = directives(&read("packaging/systemd/aars-reset-admin.service"));
    assert_eq!(one(&d, "Type"), "oneshot");
    assert_eq!(one(&d, "User"), "aars");
    assert_eq!(one(&d, "ConditionPathExists"), "/boot/firmware/aars-reset-admin");
    assert_eq!(one(&d, "Before"), "aars.service");
    assert_eq!(
        values(&d, "ExecStart"),
        [
            "/usr/bin/touch /var/lib/aars/aars-reset-admin",
            "+/usr/bin/rm -f /boot/firmware/aars-reset-admin"
        ]
    );
}

#[test]
fn the_service_user_and_its_directories() {
    let sysusers = read("packaging/sysusers.d/aars.conf");
    let lines: Vec<Vec<&str>> = sysusers
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(lines[0], ["u", "aars", "-", "\"AARS\"", "/var/lib/aars"]);
    for group in ["audio", "gpio", "dialout"] {
        assert!(lines.contains(&vec!["m", "aars", group]), "brak grupy {group}");
    }
    let tmpfiles = read("packaging/tmpfiles.d/aars.conf");
    for line in [
        "d /var/lib/aars-update 0755 root root -",
        "d /var/lib/aars/update 0750 aars aars -",
        "d /var/lib/aars/voices 0755 aars aars -",
    ] {
        assert!(tmpfiles.lines().any(|l| l == line), "brak {line:?}");
    }
}

#[test]
fn the_image_config_loads_and_uses_the_wizard_and_vox() {
    use aars_core::config::{Config, FirstAdmin, PttBackendKind, UpdateConfig};

    let config = Config::load(Some(&repo("packaging/etc/aars.toml"))).unwrap();
    assert_eq!(config.setup.first_admin, FirstAdmin::Wizard);
    assert_eq!(config.ptt.backend, PttBackendKind::Vox);
    let bind: std::net::SocketAddr = config.server.bind.parse().unwrap();
    assert!(bind.port() >= 1024, "{bind}");
    assert!(bind.ip().is_unspecified(), "{bind}");
    assert!(Path::new(&config.tts.model).starts_with(&config.tts.voices_dir));
    assert_eq!(config.tts.piper_bin, "/opt/aars/piper/bin/piper");
    assert_eq!(config.storage.db_path, Path::new("/var/lib/aars/aars.db"));
    assert_eq!(config.update, UpdateConfig::default());
    // Stan usługi tylko w katalogu, do którego usługa może pisać.
    assert!(config.storage.db_path.starts_with("/var/lib/aars"));
    assert!(config.storage.cache_dir.starts_with("/var/lib/aars"));
    assert!(config.update.state_dir.starts_with("/var/lib/aars"));
    // Głos z voices.lock jest tym, który konfiguracja wskazuje.
    let model = Path::new(&config.tts.model).file_name().unwrap().to_str().unwrap();
    assert!(voice_lock().iter().any(|e| e.name == model), "{model} poza voices.lock");
}

struct VoiceEntry {
    name: String,
    url: String,
    sha256: String,
    size: u64,
}

fn voice_lock() -> Vec<VoiceEntry> {
    read("packaging/voices.lock")
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            assert_eq!(f.len(), 4, "zły wiersz voices.lock: {l}");
            VoiceEntry {
                name: f[0].to_owned(),
                url: f[1].to_owned(),
                sha256: f[2].to_owned(),
                size: f[3].parse().unwrap(),
            }
        })
        .collect()
}

#[test]
fn the_voice_lock_pins_a_revision_and_hashes() {
    let entries = voice_lock();
    assert!(!entries.is_empty());
    for e in &entries {
        let (_, rest) = e.url.split_once("/resolve/").expect("URL bez /resolve/");
        let revision = rest.split('/').next().unwrap();
        assert!(is_hex(revision, 40), "rewizja nie jest przypięta: {}", e.url);
        assert!(e.url.starts_with("https://huggingface.co/rhasspy/piper-voices/"), "{}", e.url);
        assert!(e.url.ends_with(&format!("/{}", e.name)), "{}", e.url);
        assert!(is_hex(&e.sha256, 64), "{}", e.sha256);
        assert!(e.size > 0);
    }
    for model in entries.iter().filter(|e| e.name.ends_with(".onnx")) {
        let config = format!("{}.json", model.name);
        assert!(entries.iter().any(|e| e.name == config), "brak {config}");
    }
    assert!(entries.iter().any(|e| e.name == "pl_PL-darkman-medium.onnx"));
}

#[test]
fn every_python_requirement_has_a_hash() {
    let text = read("packaging/piper/requirements.txt");
    let requirements: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("--only-binary"))
        .collect();
    assert!(text.lines().any(|l| l.trim() == "--only-binary :all:"));
    assert!(requirements.iter().any(|l| l.starts_with(
        "piper-tts==1.8.0 --hash=sha256:3f60c1917de6d8e8033f395878ad3f88f6dfee88a8b05f98971a275f76a38484"
    )));
    for line in &requirements {
        assert!(!line.ends_with('\\'), "kontynuacja wiersza nieobsługiwana w teście: {line}");
        let (spec, hash) = line.split_once(" --hash=sha256:").unwrap_or_else(|| panic!("bez hasha: {line}"));
        assert!(spec.contains("=="), "wersja nieprzypięta: {line}");
        assert!(is_hex(hash.trim(), 64), "zły hash: {line}");
    }
    assert!(requirements.len() >= 2, "brak zależności piper-tts");
}

#[cfg(unix)]
#[test]
fn the_guard_script_is_posix_and_executable() {
    use std::os::unix::fs::PermissionsExt;

    let script = repo("packaging/aars-update-guard");
    let status = std::process::Command::new("sh").arg("-n").arg(&script).status().unwrap();
    assert!(status.success(), "sh -n {}", script.display());
    let mode = std::fs::metadata(&script).unwrap().permissions().mode();
    assert!(mode & 0o111 != 0, "brak bitu x: {mode:o}");
}

#[test]
fn third_party_licenses_cover_piper_and_the_voice() {
    let text = read("packaging/THIRD-PARTY.md");
    for needle in ["piper-tts", "GPL-3.0", "onnxruntime", "espeak-ng", "CC0", "MIT", "piper_tts-1.8.0.tar.gz"] {
        assert!(text.contains(needle), "THIRD-PARTY.md bez {needle}");
    }
}

#[test]
fn the_base_image_lock_pins_both_hashes() {
    let text = read("packaging/image/base.lock");
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("brak {key} w base.lock"))
            .to_owned()
    };
    let url = value("URL");
    assert!(url.starts_with("https://downloads.raspberrypi.com/"), "{url}");
    assert!(url.ends_with("-raspios-trixie-arm64-lite.img.xz"), "{url}");
    assert!(is_hex(&value("ARCHIVE_SHA256"), 64));
    assert!(is_hex(&value("IMAGE_SHA256"), 64));
    assert_ne!(value("ARCHIVE_SHA256"), value("IMAGE_SHA256"));
    assert!(value("IMAGE_SIZE").parse::<u64>().unwrap() > 0);
}

#[test]
fn the_piper_source_is_pinned() {
    let text = read("packaging/piper/source.lock");
    let line = text.lines().find(|l| !l.trim().is_empty() && !l.starts_with('#')).unwrap();
    let f: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(f[0], "piper_tts-1.8.0.tar.gz");
    assert!(f[1].starts_with("https://files.pythonhosted.org/") && f[1].ends_with(f[0]));
    assert_eq!(f[2], "830588aded347df579c91a32703e0fc2a3685d84f1e3533b14f2de69135d4904");
    assert!(f[3].parse::<u64>().unwrap() > 0);
}

#[test]
fn release_workflow_does_not_sign_in_ci() {
    let text = read(".github/workflows/release.yml");
    assert!(!text.to_lowercase().contains("minisign"), "podpis tylko u właściciela (R4)");
    assert!(!text.contains("secrets.MINISIGN"));
    assert!(text.contains("--draft"));
    assert!(text.contains("gh release create"));
    // Tylko po wypchnięciu znacznika v*, nigdy z gałęzi.
    let on = text.split("\non:\n").nth(1).unwrap().split("\n\n").next().unwrap();
    assert_eq!(on.trim(), "push:\n    tags: ['v*']");
    assert!(!text.contains("branches"));
    for job in ["make_manifest", "build-image.sh", "aars-os-list.json", "source.lock"] {
        assert!(text.contains(job), "release.yml bez {job}");
    }
}

#[test]
fn workflow_actions_are_pinned_to_commits() {
    let text = read(".github/workflows/release.yml");
    let uses: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim().trim_start_matches("- ").strip_prefix("uses: "))
        .collect();
    assert!(uses.len() >= 5);
    for action in uses {
        let (_, rev) = action.split_once('@').unwrap_or_else(|| panic!("bez wersji: {action}"));
        assert!(is_hex(rev.trim(), 40), "akcja nieprzypięta do commita: {action}");
    }
}

#[test]
fn the_signing_script_uses_the_versioned_trusted_comment() {
    let text = read("scripts/sign-release.sh");
    assert!(text.contains("-t \"aars $VERSION\""));
    assert!(text.contains("minisign -V -p \"$PUB\""));
    assert!(text.contains("--draft=false"));
}

#[cfg(unix)]
#[test]
fn shell_scripts_parse_and_are_executable() {
    use std::os::unix::fs::PermissionsExt;

    for script in ["packaging/image/build-image.sh", "scripts/sign-release.sh"] {
        let path = repo(script);
        let status = std::process::Command::new("bash").arg("-n").arg(&path).status().unwrap();
        assert!(status.success(), "bash -n {script}");
        let text = read(script);
        assert!(text.starts_with("#!/usr/bin/env bash\n"), "{script}");
        assert!(text.contains("set -euo pipefail"), "{script}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "{script}: brak bitu x");
    }
}

#[test]
fn the_image_script_verifies_and_always_cleans_up() {
    let text = read("packaging/image/build-image.sh");
    for needle in [
        "trap cleanup EXIT",
        "check_sha256 \"$ARCHIVE\" \"$ARCHIVE_SHA256\"",
        "check_sha256 \"$IMG\" \"$IMAGE_SHA256\"",
        "--require-hashes",
        "voices.lock",
        "systemctl enable aars.service aars-reset-admin.service",
        "zerofree",
        "\"init_format\": \"cloudinit-rpi\"",
    ] {
        assert!(text.contains(needle), "build-image.sh bez {needle}");
    }
}
