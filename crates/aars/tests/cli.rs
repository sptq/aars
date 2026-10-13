use std::process::Command;

fn aars() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aars"))
}

#[test]
fn migrate_creates_the_database_file() {
    let dir = std::env::temp_dir().join(format!("aars-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("test.db");
    let _ = std::fs::remove_file(&db);

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .arg("migrate")
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(db.exists(), "migrate musi utworzyć plik bazy");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn info_prints_the_effective_configuration() {
    let out = aars().env("AARS_SERVER__BIND", "127.0.0.1:1234").arg("info").output().unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("127.0.0.1:1234"), "info musi pokazać efektywną konfigurację: {stdout}");
}

#[test]
fn a_missing_explicit_config_file_fails_loudly() {
    let out = aars().args(["--config", "/nie/ma/takiego.toml", "info"]).output().unwrap();

    assert!(!out.status.success(), "literówka w ścieżce nie może być milcząco zignorowana");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nie istnieje"), "stderr: {stderr}");
}
