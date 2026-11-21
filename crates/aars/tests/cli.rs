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

#[test]
fn say_dry_run_prints_the_normalized_text_without_synthesizing() {
    let out = aars()
        .args(["say", "--dry-run", "wiatr 12 m/s o 18:00"])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("wiatr dwanaście metrów na sekundę o godzinie osiemnastej"),
        "stdout: {stdout}"
    );
}

#[test]
fn tx_refuses_when_the_kill_switch_is_off() {
    let dir = std::env::temp_dir().join(format!("aars-tx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("tx.db");

    // Stacja z wyłączonym nadawaniem musi odmówić, nie nadać.
    let migrate = aars().env("AARS_STORAGE__DB_PATH", &db).arg("migrate").output().unwrap();
    assert!(migrate.status.success(), "{}", String::from_utf8_lossy(&migrate.stderr));

    let set = std::process::Command::new("sqlite3")
        .arg(&db)
        .arg("UPDATE settings SET value = 'false' WHERE key = 'tx_enabled';")
        .status()
        .unwrap();
    assert!(set.success());

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args(["tx", "--dry-run", "test"])
        .output()
        .unwrap();

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("TxDisabled") || combined.contains("nadawanie wyłączone"),
        "{combined}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn queue_add_then_list_shows_the_item_in_priority_order() {
    let dir = std::env::temp_dir().join(format!("aars-queue-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("queue.db");

    let migrate = aars().env("AARS_STORAGE__DB_PATH", &db).arg("migrate").output().unwrap();
    assert!(migrate.status.success(), "{}", String::from_utf8_lossy(&migrate.stderr));

    for (text, priority) in [("zwykly", "normal"), ("alarm", "emergency")] {
        let out = aars()
            .env("AARS_STORAGE__DB_PATH", &db)
            .args(["queue", "add", "--priority", priority, text])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args(["queue", "list"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let stdout = String::from_utf8_lossy(&out.stdout);
    let alarm = stdout.find("alarm").expect("alarm w wyniku");
    let zwykly = stdout.find("zwykly").expect("zwykly w wyniku");
    assert!(alarm < zwykly, "pilniejsze musi być wyżej:\n{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn queue_add_with_repeats_creates_a_cancellable_group() {
    let dir = std::env::temp_dir().join(format!("aars-group-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("group.db");

    assert!(
        aars().env("AARS_STORAGE__DB_PATH", &db).arg("migrate").output().unwrap().status.success()
    );

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args(["queue", "add", "--repeat", "3", "--every", "60", "ostrzeżenie"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let group = String::from_utf8(
        std::process::Command::new("sqlite3")
            .arg(&db)
            .arg("SELECT DISTINCT repeat_group FROM queue_items;")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    assert!(!group.is_empty(), "powtórzenia muszą mieć grupę");

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args(["queue", "cancel", "--group", &group])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("anulowano 3"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn message_add_then_list_shows_the_schedule() {
    let dir = std::env::temp_dir().join(format!("aars-msg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("msg.db");

    assert!(
        aars().env("AARS_STORAGE__DB_PATH", &db).arg("migrate").output().unwrap().status.success()
    );

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args([
            "message", "add", "--title", "biuletyn", "--cron", "0 18 * * *", "--priority", "high",
            "prognoza na dziś",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args(["message", "list"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("biuletyn"), "{stdout}");
    assert!(stdout.contains("0 18 * * *"), "{stdout}");
    assert!(stdout.contains("High"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn message_add_rejects_a_broken_cron_expression_up_front() {
    let dir = std::env::temp_dir().join(format!("aars-badcron-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("bad.db");

    assert!(
        aars().env("AARS_STORAGE__DB_PATH", &db).arg("migrate").output().unwrap().status.success()
    );

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .args(["message", "add", "--title", "zly", "--cron", "to nie cron", "treść"])
        .output()
        .unwrap();

    assert!(!out.status.success(), "zepsuty wzorzec nie może wejść do bazy");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cron"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn source_preview_without_an_area_says_so() {
    let dir = std::env::temp_dir().join(format!("aars-src-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("src.db");
    let _ = std::fs::remove_file(&db);

    // Adres wskazuje port, na którym nic nie słucha: test nie dotyka sieci.
    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .env("AARS_SOURCES__IMGW_METEO_URL", "http://127.0.0.1:9/")
        .args(["source", "preview"])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("brak obszaru"), "stdout: {stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn source_preview_reports_a_malformed_area_as_invalid() {
    let dir = std::env::temp_dir().join(format!("aars-src-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("src.db");
    let _ = std::fs::remove_file(&db);

    let migrate = aars().env("AARS_STORAGE__DB_PATH", &db).arg("migrate").output().unwrap();
    assert!(migrate.status.success());
    let set = std::process::Command::new("sqlite3")
        .arg(&db)
        .arg("UPDATE settings SET value = 'xx' WHERE key = 'source:imgw-meteo:area';")
        .status()
        .unwrap();
    assert!(set.success());

    let out = aars()
        .env("AARS_STORAGE__DB_PATH", &db)
        .env("AARS_SOURCES__IMGW_METEO_URL", "http://127.0.0.1:9/")
        .args(["source", "preview"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("nieprawidłowy obszar"), "stdout: {stdout}");
    assert!(!stdout.contains("brak obszaru"), "stdout: {stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn hardware_reset_clears_panel_overrides() {
    let dir = std::env::temp_dir().join(format!("aars-hw-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("hw.db");
    let _ = std::fs::remove_file(&db);

    let migrate = aars().env("AARS_STORAGE__DB_PATH", &db).arg("migrate").output().unwrap();
    assert!(migrate.status.success(), "{}", String::from_utf8_lossy(&migrate.stderr));

    let set = std::process::Command::new("sqlite3")
        .arg(&db)
        .arg(
            "INSERT INTO settings (key, value) VALUES ('hw:audio.device', 'hw:1,0'), \
             ('hw_good:audio.device', 'hw:1,0'), ('hw_state:origin', 'panel');",
        )
        .status()
        .unwrap();
    assert!(set.success());

    let show = aars().env("AARS_STORAGE__DB_PATH", &db).args(["hardware", "show"]).output().unwrap();
    assert!(show.status.success(), "{}", String::from_utf8_lossy(&show.stderr));
    let stdout = String::from_utf8_lossy(&show.stdout);
    assert!(stdout.contains("audio.device = hw:1,0  (panel)"), "{stdout}");

    let out = aars().env("AARS_STORAGE__DB_PATH", &db).args(["hardware", "reset"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("aars.toml"), "{stdout}");
    assert!(stdout.contains("usunięto 3"), "{stdout}");

    let left = std::process::Command::new("sqlite3")
        .arg(&db)
        .arg("SELECT COUNT(*) FROM settings WHERE key LIKE 'hw%:%';")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&left.stdout).trim(), "0");

    std::fs::remove_dir_all(&dir).ok();
}

/// `systemctl stop` wysyła SIGTERM: stacja ma się zatrzymać tak jak po
/// Ctrl-C (R10e), z kodem 0 i w ograniczonym czasie.
#[cfg(unix)]
#[test]
fn sigterm_stops_the_station_like_ctrl_c() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("aars.toml");
    // PTT na nieistniejącym porcie pada przy budowie stosu, zanim ktokolwiek
    // otworzy kartę: stacja idzie w tryb bez nadawania i nic nie gra.
    std::fs::write(
        &config,
        format!(
            "[server]\nbind = \"127.0.0.1:0\"\n\n\
             [storage]\ndb_path = {db:?}\ncache_dir = {cache:?}\ncache_max_bytes = 1048576\n\n\
             [ptt]\nbackend = \"serial\"\n\n\
             [ptt.serial]\nport = \"/dev/aars-test-nie-ma\"\nline = \"dtr\"\n\n\
             [sources]\nimgw_meteo_url = \"http://127.0.0.1:9/x\"\n",
            db = dir.path().join("aars.db").to_string_lossy(),
            cache = dir.path().join("cache").to_string_lossy(),
        ),
    )
    .unwrap();

    let mut child = aars()
        .arg("--config")
        .arg(&config)
        .arg("run")
        .env_remove("NOTIFY_SOCKET")
        .env_remove("AARS_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Logi idą na stdout (tracing), błędy CLI na stderr: czytamy oba.
    let (tx, rx) = mpsc::channel::<String>();
    let mut readers = Vec::new();
    for stream in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    drop(tx);

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let started = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                let hit = line.contains("stacja uruchomiona");
                seen.push(line);
                if hit {
                    break true;
                }
            }
            Err(_) => break false,
        }
    };
    if !started {
        let _ = child.kill();
        let _ = child.wait();
        panic!("stacja nie wstała w 10 s: {seen:#?}");
    }

    let pid = child.id().to_string();
    let killed = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(killed.success());

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("SIGTERM nie zatrzymał stacji w 5 s");
    };
    for reader in readers {
        let _ = reader.join();
    }
    seen.extend(rx.try_iter());
    assert!(status.success(), "kod wyjścia {status:?}: {seen:#?}");
    assert!(
        seen.iter().any(|l| l.contains("zatrzymuję stację")),
        "SIGTERM idzie tą samą drogą co Ctrl-C: {seen:#?}"
    );
}

#[test]
fn the_version_flag_prints_what_the_smoke_test_expects() {
    let out = aars().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("aars {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn update_status_and_guard_work_without_an_installation() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        aars()
            .env("AARS_STORAGE__DB_PATH", dir.path().join("aars.db"))
            .env("AARS_UPDATE__INSTALL_ROOT", dir.path().join("opt"))
            .env("AARS_UPDATE__GUARD_DIR", dir.path().join("guard"))
            .env("AARS_UPDATE__STATE_DIR", dir.path().join("state"))
            .args(args)
            .output()
            .unwrap()
    };

    let out = run(&["update", "status"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("poprzednia: brak"), "{stdout}");
    assert!(stdout.contains("próba:      brak"), "{stdout}");

    // Strażnik zawsze kończy się kodem 0 (R8), nawet bez katalogów.
    let out = run(&["update", "guard"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("nic do zrobienia"));

    // Bez pliku blokady stan usługi jest nieznany: odmowa bez --force (M-5).
    let out = run(&["update", "rollback"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nie wiadomo, czy usługa aars działa"));
    let out = run(&["update", "rollback", "--force"]);
    assert!(!out.status.success(), "powrót bez poprzedniej wersji to błąd");
    assert!(String::from_utf8_lossy(&out.stderr).contains("brak poprzedniej wersji"));
}
