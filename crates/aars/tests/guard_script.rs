//! `packaging/aars-update-guard`: w trakcie próby woła poprzednią binarkę,
//! w pozostałych przypadkach bieżącą (R8).
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../packaging/aars-update-guard"
);

/// Binarka-atrapa: dopisuje swoją nazwę i argumenty do `calls`.
fn fake_binary(root: &Path, link: &str) {
    let dir = root.join("opt/aars").join(link);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("aars");
    let log = root.join("calls");
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho \"{link} $*\" >> '{}'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn run(root: &Path) -> String {
    let _ = std::fs::remove_file(root.join("calls"));
    let out = Command::new("sh")
        .arg(SCRIPT)
        .args(["--config", "/etc/aars/aars.toml"])
        .env("AARS_UPDATE_PREFIX", root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read_to_string(root.join("calls")).unwrap()
}

#[test]
fn the_script_runs_previous_during_a_trial_and_current_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fake_binary(root, "current");
    fake_binary(root, "previous");

    assert_eq!(
        run(root),
        "current update guard --config /etc/aars/aars.toml\n"
    );

    std::fs::create_dir_all(root.join("var/lib/aars-update")).unwrap();
    std::fs::write(root.join("var/lib/aars-update/trial.json"), "{}").unwrap();
    assert_eq!(
        run(root),
        "previous update guard --config /etc/aars/aars.toml\n"
    );

    // Próba bez poprzedniej binarki: zostaje bieżąca.
    std::fs::remove_dir_all(root.join("opt/aars/previous")).unwrap();
    assert_eq!(
        run(root),
        "current update guard --config /etc/aars/aars.toml\n"
    );
}

#[test]
fn the_script_is_executable() {
    let mode = std::fs::metadata(SCRIPT).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "skrypt musi być wykonywalny");
}
