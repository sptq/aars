//! Reset kont admina plikiem z partycji boot (R13, bez importu hasła z
//! Raspberry Pi Imager, który na razie odpada).
//!
//! Właściciel kładzie pusty plik `aars-reset-admin` (albo
//! `aars-reset-admin.txt`, przegląd końcowy M-3) na partycji boot.
//! `aars-reset-admin.service` (przed `aars.service`) zakłada jako użytkownik
//! `aars` plik o tej samej nazwie obok bazy, a jako root usuwa plik z
//! partycji boot. Bazę rusza wyłącznie ten proces (użytkownik `aars`), więc
//! nie powstają pliki `-wal`/`-shm` roota.

use std::path::{Path, PathBuf};

use aars_core::user::UserRepo;
use sqlx::SqlitePool;

/// Nazwa pliku prośby o reset: na partycji boot i obok bazy.
pub const RESET_FLAG_NAME: &str = "aars-reset-admin";

/// Gdzie usługa szuka prośby o reset: katalog bazy (`/var/lib/aars`).
pub fn reset_flag_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join(RESET_FLAG_NAME)
}

/// Gdy jest prośba `flag`: wyłącza wszystkie konta admina, kończy ich
/// sesje, zapisuje audyt `admin_reset` i usuwa prośbę. Zwraca liczbę
/// wyłączonych kont albo `None`, gdy prośby nie ma.
pub async fn reset_admins(flag: &Path, pool: &SqlitePool) -> Result<Option<u64>, String> {
    match std::fs::symlink_metadata(flag) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("prośba o reset {}: {e}", flag.display())),
    }
    let disabled = UserRepo::new(pool.clone())
        .disable_all_admins()
        .await
        .map_err(|e| format!("reset kont admina: {e}"))?;
    let detail = format!(
        "wyłączone konta admina: {disabled} (plik {RESET_FLAG_NAME} z partycji boot) — kreator /setup otwarty"
    );
    sqlx::query("INSERT INTO audit_log (user_id, action, detail) VALUES (NULL, 'admin_reset', ?)")
        .bind(&detail)
        .execute(pool)
        .await
        .map_err(|e| format!("audyt resetu: {e}"))?;
    // Prośba znika dopiero po zapisie: przerwany reset powtórzy się przy
    // następnym starcie, a powtórka niczego nie psuje.
    std::fs::remove_file(flag).map_err(|e| format!("usuwanie {}: {e}", flag.display()))?;
    Ok(Some(disabled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::user::Role;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = aars_core::db::connect(&dir.path().join("aars.db")).await.unwrap();
        aars_core::db::migrate(&pool).await.unwrap();
        (dir, pool)
    }

    #[test]
    fn the_flag_lies_next_to_the_database() {
        assert_eq!(
            reset_flag_path(Path::new("/var/lib/aars/aars.db")),
            PathBuf::from("/var/lib/aars/aars-reset-admin")
        );
        assert_eq!(
            reset_flag_path(Path::new("aars.db")),
            PathBuf::from("./aars-reset-admin")
        );
    }

    #[tokio::test]
    async fn reset_disables_admins_ends_sessions_and_reopens_the_wizard() {
        let (dir, pool) = pool().await;
        let repo = UserRepo::new(pool.clone());
        let admin = repo.create_first_admin("szef", "dlugie-haslo-1").await.unwrap();
        let operator = repo.create("op", "dlugie-haslo-2", Role::Operator).await.unwrap();
        let session = repo.start_session(admin, None).await.unwrap().id;
        let flag = reset_flag_path(&dir.path().join("aars.db"));
        std::fs::write(&flag, "").unwrap();

        assert_eq!(reset_admins(&flag, &pool).await.unwrap(), Some(1));
        assert!(!repo.has_enabled_admin().await.unwrap());
        assert!(repo.session_user(&session).await.unwrap().is_none());
        assert!(!repo.by_id(operator).await.unwrap().unwrap().disabled);
        assert!(!flag.exists(), "prośba usunięta");
        let audits: Vec<String> = sqlx::query_scalar(
            "SELECT detail FROM audit_log WHERE action = 'admin_reset' AND user_id IS NULL",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(audits.len(), 1, "{audits:?}");
        assert!(audits[0].contains("kreator /setup"), "{audits:?}");
    }

    #[tokio::test]
    async fn without_the_flag_nothing_changes() {
        let (dir, pool) = pool().await;
        let repo = UserRepo::new(pool.clone());
        repo.create_first_admin("szef", "dlugie-haslo-1").await.unwrap();
        let flag = reset_flag_path(&dir.path().join("aars.db"));

        assert_eq!(reset_admins(&flag, &pool).await.unwrap(), None);
        assert!(repo.has_enabled_admin().await.unwrap());
        let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(audits, 0);
    }
}
