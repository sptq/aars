use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use chrono::{DateTime, Duration, Utc};
use rand::Rng;
use sqlx::{Row, SqlitePool};

use crate::error::DbError;

/// Jak długo żyje sesja bez odnowienia.
const SESSION_LIFETIME_HOURS: i64 = 12;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("niepoprawna nazwa użytkownika lub hasło")]
    InvalidCredentials,
    #[error("konto wyłączone")]
    Disabled,
    #[error("błąd bazy danych: {0}")]
    Db(#[from] DbError),
    #[error("błąd haszowania hasła: {0}")]
    Hash(String),
}

/// Zmiana konta, która mogłaby zostawić stację bez administratora.
#[derive(Debug, thiserror::Error)]
pub enum UserChangeError {
    #[error("stacja musi mieć co najmniej jednego aktywnego administratora")]
    LastAdmin,
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for UserChangeError {
    fn from(err: sqlx::Error) -> Self {
        Self::Db(DbError::Sqlx(err))
    }
}

/// Warunek SQL: konto `id_param` nie jest aktywnym adminem albo istnieje inny
/// aktywny admin. Sprawdzany w tym samym zapytaniu co zmiana, więc dwie
/// równoległe degradacje nie zostawią stacji bez admina.
fn keeps_an_admin(id_param: &str) -> String {
    format!(
        "(NOT (role = 'admin' AND disabled = 0)
          OR EXISTS (SELECT 1 FROM users o
                      WHERE o.id <> {id_param} AND o.role = 'admin' AND o.disabled = 0))"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Operator => "operator",
            Self::Admin => "admin",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "viewer" => Some(Self::Viewer),
            "operator" => Some(Self::Operator),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// Ranga uprawnień; wyższa liczba to więcej wolno.
    fn rank(self) -> u8 {
        match self {
            Self::Viewer => 0,
            Self::Operator => 1,
            Self::Admin => 2,
        }
    }

    /// Czy ta rola wystarcza tam, gdzie wymagana jest `required`.
    pub fn can(self, required: Role) -> bool {
        self.rank() >= required.rank()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub role: Role,
    pub disabled: bool,
    pub must_change_password: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub user_id: i64,
    pub expires_at: DateTime<Utc>,
    /// Token wymagany na każdym formularzu zmieniającym stan (spec §8).
    pub csrf_token: String,
}

pub struct UserRepo {
    pool: SqlitePool,
}

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn hash_password(password: &str) -> Result<String, AuthError> {
    // argon2 0.6 sam losuje sól (feature `getrandom`) i zwraca ciąg PHC.
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

/// Losowy token o 256 bitach entropii, zapisany szesnastkowo.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Losowe hasło tymczasowe: 20 znaków szesnastkowych (80 bitów).
fn random_password() -> String {
    random_token()[..20].to_owned()
}

fn row_to_user(row: &sqlx::sqlite::SqliteRow) -> User {
    let role: String = row.get("role");
    User {
        id: row.get("id"),
        username: row.get("username"),
        role: Role::parse(&role).unwrap_or(Role::Viewer),
        disabled: row.get::<i64, _>("disabled") != 0,
        must_change_password: row.get::<i64, _>("must_change_password") != 0,
    }
}

const USER_COLUMNS: &str = "id, username, role, disabled, must_change_password";

impl UserRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn create(&self, username: &str, password: &str, role: Role) -> Result<i64, AuthError> {
        let hash = hash_password(password)?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO users (username, password_hash, role) VALUES (?, ?, ?) RETURNING id",
        )
        .bind(username)
        .bind(hash)
        .bind(role.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AuthError::Db(DbError::Sqlx(e)))?;
        Ok(id)
    }

    pub async fn authenticate(&self, username: &str, password: &str) -> Result<User, AuthError> {
        let row = sqlx::query(&format!(
            "SELECT {USER_COLUMNS}, password_hash FROM users WHERE username = ?"
        ))
        .bind(username)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AuthError::Db(DbError::Sqlx(e)))?;

        // Nieistniejący użytkownik i złe hasło dają ten sam błąd: panel nie
        // podpowiada, które konta istnieją.
        let Some(row) = row else {
            return Err(AuthError::InvalidCredentials);
        };

        let stored: String = row.get("password_hash");
        let parsed = PasswordHash::new(&stored).map_err(|e| AuthError::Hash(e.to_string()))?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|_| AuthError::InvalidCredentials)?;

        let user = row_to_user(&row);
        if user.disabled {
            return Err(AuthError::Disabled);
        }
        Ok(user)
    }

    pub async fn by_id(&self, id: i64) -> Result<Option<User>, DbError> {
        let row = sqlx::query(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_user))
    }

    pub async fn list(&self) -> Result<Vec<User>, DbError> {
        let rows = sqlx::query(&format!("SELECT {USER_COLUMNS} FROM users ORDER BY username"))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_user).collect())
    }

    pub async fn set_password(&self, id: i64, password: &str) -> Result<(), AuthError> {
        let hash = hash_password(password)?;
        sqlx::query("UPDATE users SET password_hash = ?, must_change_password = 0 WHERE id = ?")
            .bind(hash)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| AuthError::Db(DbError::Sqlx(e)))?;
        Ok(())
    }

    /// Wyłącza konto i od razu zrywa jego sesje — wyłączenie, które działa
    /// dopiero po wygaśnięciu sesji, nie jest wyłączeniem.
    ///
    /// Odmawia ([`UserChangeError::LastAdmin`]) wyłączenia ostatniego aktywnego admina.
    pub async fn set_disabled(&self, id: i64, disabled: bool) -> Result<(), UserChangeError> {
        let result = sqlx::query(&format!(
            "UPDATE users SET disabled = ?1 WHERE id = ?2 AND (?1 = 0 OR {})",
            keeps_an_admin("?2")
        ))
        .bind(i64::from(disabled))
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return self.refused_or_missing(id).await;
        }

        if disabled {
            sqlx::query("DELETE FROM sessions WHERE user_id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Zmienia rolę konta. Odmawia degradacji ostatniego aktywnego admina.
    pub async fn set_role(&self, id: i64, role: Role) -> Result<(), UserChangeError> {
        let result = sqlx::query(&format!(
            "UPDATE users SET role = ?1 WHERE id = ?2 AND (?1 = 'admin' OR {})",
            keeps_an_admin("?2")
        ))
        .bind(role.as_str())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return self.refused_or_missing(id).await;
        }
        Ok(())
    }

    /// Zapytanie strzeżone nic nie zmieniło: brak konta to nie błąd,
    /// istniejące konto znaczy, że zadziałała ochrona ostatniego admina.
    async fn refused_or_missing(&self, id: i64) -> Result<(), UserChangeError> {
        match self.by_id(id).await? {
            Some(_) => Err(UserChangeError::LastAdmin),
            None => Ok(()),
        }
    }

    /// Losowe hasło tymczasowe: 20 znaków, `must_change_password = 1`,
    /// sesje konta zerwane. Zwraca hasło, żeby admin przekazał je użytkownikowi.
    pub async fn reset_password(&self, id: i64) -> Result<String, AuthError> {
        let password = random_password();
        let hash = hash_password(&password)?;
        sqlx::query("UPDATE users SET password_hash = ?, must_change_password = 1 WHERE id = ?")
            .bind(hash)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| AuthError::Db(DbError::Sqlx(e)))?;
        self.end_all_sessions(id).await?;
        Ok(password)
    }

    /// Tworzy konto z losowym hasłem tymczasowym (do zmiany przy pierwszym logowaniu).
    pub async fn create_with_temporary_password(
        &self,
        username: &str,
        role: Role,
    ) -> Result<(i64, String), AuthError> {
        let password = random_password();
        let hash = hash_password(&password)?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO users (username, password_hash, role, must_change_password)
             VALUES (?, ?, ?, 1) RETURNING id",
        )
        .bind(username)
        .bind(hash)
        .bind(role.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AuthError::Db(DbError::Sqlx(e)))?;
        Ok((id, password))
    }

    /// Usuwa konto; ostatniego aktywnego admina — nie.
    pub async fn delete(&self, id: i64) -> Result<u64, UserChangeError> {
        let result = sqlx::query(&format!(
            "DELETE FROM users WHERE id = ?1 AND {}",
            keeps_an_admin("?1")
        ))
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            self.refused_or_missing(id).await?;
        }
        Ok(result.rows_affected())
    }

    pub async fn start_session(
        &self,
        user_id: i64,
        user_agent: Option<&str>,
    ) -> Result<Session, DbError> {
        let id = random_token();
        let csrf_token = random_token();
        let expires_at = Utc::now() + Duration::hours(SESSION_LIFETIME_HOURS);

        sqlx::query(
            "INSERT INTO sessions (id, user_id, expires_at, user_agent, csrf_token)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(iso(expires_at))
        .bind(user_agent)
        .bind(&csrf_token)
        .execute(&self.pool)
        .await?;

        Ok(Session { id, user_id, expires_at, csrf_token })
    }

    /// Użytkownik i token CSRF stojące za sesją — albo `None`, gdy sesja
    /// wygasła, została zakończona, albo konto jest wyłączone.
    pub async fn session_user(&self, session_id: &str) -> Result<Option<(User, String)>, DbError> {
        let row = sqlx::query(
            "SELECT u.id, u.username, u.role, u.disabled, u.must_change_password, s.csrf_token
               FROM sessions s JOIN users u ON u.id = s.user_id
              WHERE s.id = ? AND s.expires_at > ? AND u.disabled = 0",
        )
        .bind(session_id)
        .bind(iso(Utc::now()))
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.as_ref().map(|row| (row_to_user(row), row.get::<String, _>("csrf_token"))))
    }

    pub async fn end_session(&self, session_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Kończy wszystkie sesje użytkownika (np. po zmianie hasła).
    pub async fn end_all_sessions(&self, user_id: i64) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM sessions WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn purge_expired_sessions(&self) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
            .bind(iso(Utc::now()))
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Przy pierwszym starcie tworzy konto `admin` z losowym hasłem i zwraca
    /// je, żeby binarka mogła je wypisać na konsolę. Gdy jakikolwiek
    /// użytkownik już istnieje, nie robi nic.
    pub async fn ensure_bootstrap_admin(&self) -> Result<Option<String>, AuthError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AuthError::Db(DbError::Sqlx(e)))?;
        if count > 0 {
            return Ok(None);
        }

        let password = random_password();
        let hash = hash_password(&password)?;
        sqlx::query(
            "INSERT INTO users (username, password_hash, role, must_change_password)
             VALUES ('admin', ?, 'admin', 1)",
        )
        .bind(hash)
        .execute(&self.pool)
        .await
        .map_err(|e| AuthError::Db(DbError::Sqlx(e)))?;

        Ok(Some(password))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    #[tokio::test]
    async fn a_password_is_never_stored_in_clear_text() {
        let repo = UserRepo::new(test_pool().await);
        repo.create("operator", "tajne-haslo", Role::Operator).await.unwrap();

        let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE username = ?")
            .bind("operator")
            .fetch_one(repo.pool())
            .await
            .unwrap();

        assert!(!hash.contains("tajne-haslo"));
        assert!(hash.starts_with("$argon2id$"), "oczekiwano Argon2id, jest {hash}");
    }

    #[tokio::test]
    async fn authentication_accepts_the_right_password_and_rejects_the_wrong_one() {
        let repo = UserRepo::new(test_pool().await);
        repo.create("operator", "tajne-haslo", Role::Operator).await.unwrap();

        let user = repo.authenticate("operator", "tajne-haslo").await.unwrap();
        assert_eq!(user.role, Role::Operator);

        assert!(matches!(
            repo.authenticate("operator", "zle-haslo").await,
            Err(AuthError::InvalidCredentials)
        ));
        assert!(matches!(
            repo.authenticate("nie-ma-takiego", "cokolwiek").await,
            Err(AuthError::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn a_disabled_account_cannot_log_in_even_with_the_right_password() {
        let repo = UserRepo::new(test_pool().await);
        let id = repo.create("operator", "tajne-haslo", Role::Operator).await.unwrap();
        repo.set_disabled(id, true).await.unwrap();

        assert!(matches!(
            repo.authenticate("operator", "tajne-haslo").await,
            Err(AuthError::Disabled)
        ));
    }

    #[tokio::test]
    async fn a_session_resolves_to_its_user_until_it_is_ended() {
        let repo = UserRepo::new(test_pool().await);
        let id = repo.create("operator", "haslo", Role::Operator).await.unwrap();

        let session = repo.start_session(id, Some("test-agent")).await.unwrap();
        let (user, _csrf) =
            repo.session_user(&session.id).await.unwrap().expect("sesja musi działać");
        assert_eq!(user.id, id);

        repo.end_session(&session.id).await.unwrap();
        assert!(
            repo.session_user(&session.id).await.unwrap().is_none(),
            "wylogowanie musi działać natychmiast"
        );
    }

    #[tokio::test]
    async fn ending_all_sessions_leaves_other_users_alone() {
        let repo = UserRepo::new(test_pool().await);
        let a = repo.create("a", "haslo", Role::Operator).await.unwrap();
        let b = repo.create("b", "haslo", Role::Operator).await.unwrap();
        let a1 = repo.start_session(a, None).await.unwrap();
        let a2 = repo.start_session(a, None).await.unwrap();
        let b1 = repo.start_session(b, None).await.unwrap();

        assert_eq!(repo.end_all_sessions(a).await.unwrap(), 2);

        assert!(repo.session_user(&a1.id).await.unwrap().is_none());
        assert!(repo.session_user(&a2.id).await.unwrap().is_none());
        assert!(repo.session_user(&b1.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn disabling_an_account_invalidates_its_live_sessions() {
        let repo = UserRepo::new(test_pool().await);
        let id = repo.create("operator", "haslo", Role::Operator).await.unwrap();
        let session = repo.start_session(id, None).await.unwrap();

        repo.set_disabled(id, true).await.unwrap();

        assert!(
            repo.session_user(&session.id).await.unwrap().is_none(),
            "konto wyłączone nie może dalej działać z otwartą sesją"
        );
    }

    #[tokio::test]
    async fn an_expired_session_does_not_resolve_and_gets_purged() {
        let repo = UserRepo::new(test_pool().await);
        let id = repo.create("operator", "haslo", Role::Operator).await.unwrap();
        let session = repo.start_session(id, None).await.unwrap();

        sqlx::query("UPDATE sessions SET expires_at = '2000-01-01T00:00:00Z' WHERE id = ?")
            .bind(&session.id)
            .execute(repo.pool())
            .await
            .unwrap();

        assert!(repo.session_user(&session.id).await.unwrap().is_none());
        assert_eq!(repo.purge_expired_sessions().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn session_ids_and_csrf_tokens_are_unpredictable_and_distinct() {
        let repo = UserRepo::new(test_pool().await);
        let id = repo.create("operator", "haslo", Role::Operator).await.unwrap();

        let mut seen = std::collections::HashSet::new();
        for _ in 0..20 {
            let session = repo.start_session(id, None).await.unwrap();
            assert!(session.id.len() >= 32, "identyfikator sesji za krótki: {}", session.id);
            assert!(session.csrf_token.len() >= 32, "token CSRF za krótki");
            assert_ne!(
                session.id, session.csrf_token,
                "token CSRF nie może być identyfikatorem sesji"
            );
            assert!(seen.insert(session.id.clone()), "identyfikator sesji się powtórzył");
            assert!(seen.insert(session.csrf_token), "token CSRF się powtórzył");
        }
    }

    #[tokio::test]
    async fn the_csrf_token_comes_back_with_the_session() {
        let repo = UserRepo::new(test_pool().await);
        let id = repo.create("operator", "haslo", Role::Operator).await.unwrap();
        let session = repo.start_session(id, None).await.unwrap();

        let (_user, csrf) = repo.session_user(&session.id).await.unwrap().unwrap();
        assert_eq!(csrf, session.csrf_token);
    }

    #[tokio::test]
    async fn roles_compare_by_capability_not_alphabetically() {
        assert!(Role::Admin.can(Role::Operator));
        assert!(Role::Admin.can(Role::Viewer));
        assert!(Role::Operator.can(Role::Viewer));
        assert!(!Role::Operator.can(Role::Admin));
        assert!(!Role::Viewer.can(Role::Operator));
        assert!(Role::Viewer.can(Role::Viewer));
    }

    #[tokio::test]
    async fn the_bootstrap_admin_is_created_only_when_there_are_no_users() {
        let repo = UserRepo::new(test_pool().await);

        let password = repo.ensure_bootstrap_admin().await.unwrap().expect("pierwszy start");
        assert!(password.len() >= 16, "hasło startowe musi być mocne: {password}");

        let user = repo.authenticate("admin", &password).await.unwrap();
        assert_eq!(user.role, Role::Admin);
        assert!(user.must_change_password, "hasło startowe wymaga zmiany");

        assert!(
            repo.ensure_bootstrap_admin().await.unwrap().is_none(),
            "drugi start nie może nadpisać konta"
        );
    }

    #[tokio::test]
    async fn reset_password_forces_a_change_and_ends_sessions() {
        let repo = UserRepo::new(test_pool().await);
        let id = repo.create("operator", "stare-haslo", Role::Operator).await.unwrap();
        let session = repo.start_session(id, None).await.unwrap();

        let fresh = repo.reset_password(id).await.unwrap();

        assert_eq!(fresh.len(), 20);
        assert!(repo.session_user(&session.id).await.unwrap().is_none());
        assert!(matches!(
            repo.authenticate("operator", "stare-haslo").await,
            Err(AuthError::InvalidCredentials)
        ));
        let user = repo.authenticate("operator", &fresh).await.unwrap();
        assert!(user.must_change_password);
    }

    #[tokio::test]
    async fn set_role_changes_what_the_user_can_do() {
        let repo = UserRepo::new(test_pool().await);
        repo.create("admin", "haslo", Role::Admin).await.unwrap();
        let id = repo.create("operator", "haslo", Role::Viewer).await.unwrap();
        repo.set_role(id, Role::Admin).await.unwrap();
        assert_eq!(repo.by_id(id).await.unwrap().unwrap().role, Role::Admin);
        repo.set_role(id, Role::Operator).await.unwrap();
        assert_eq!(repo.by_id(id).await.unwrap().unwrap().role, Role::Operator);
    }

    #[tokio::test]
    async fn the_last_enabled_admin_cannot_be_demoted_disabled_or_deleted() {
        let repo = UserRepo::new(test_pool().await);
        let a = repo.create("a", "haslo", Role::Admin).await.unwrap();
        let b = repo.create("b", "haslo", Role::Admin).await.unwrap();

        // Dwóch adminów: A degraduje B — wolno.
        repo.set_role(b, Role::Operator).await.unwrap();

        // B (już w bazie nie-admin, ale np. ze starym widokiem) próbuje zdjąć A:
        // A jest ostatnim aktywnym adminem, więc baza odmawia za każdym razem.
        assert!(repo.set_role(a, Role::Viewer).await.is_err());
        assert!(repo.set_disabled(a, true).await.is_err());
        assert!(repo.delete(a).await.is_err());

        let admin = repo.by_id(a).await.unwrap().unwrap();
        assert_eq!((admin.role, admin.disabled), (Role::Admin, false));

        // Wyłączony admin się nie liczy.
        let c = repo.create("c", "haslo", Role::Admin).await.unwrap();
        repo.set_disabled(c, true).await.unwrap();
        assert!(repo.set_role(a, Role::Operator).await.is_err());
        // Ale można go wyłączyć/usunąć, a gdy jest drugi aktywny — także A.
        repo.set_disabled(c, false).await.unwrap();
        repo.set_disabled(a, true).await.unwrap();
        assert!(repo.delete(c).await.is_err(), "c jest teraz ostatnim aktywnym adminem");
        // Nieaktywnego admina można usunąć.
        assert_eq!(repo.delete(a).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_temporary_password_account_must_change_it_and_a_duplicate_is_refused() {
        let repo = UserRepo::new(test_pool().await);
        let (id, password) =
            repo.create_with_temporary_password("nowy", Role::Operator).await.unwrap();
        assert_eq!(password.len(), 20);
        let user = repo.authenticate("nowy", &password).await.unwrap();
        assert_eq!(user.id, id);
        assert!(user.must_change_password);
        assert!(repo.create_with_temporary_password("nowy", Role::Viewer).await.is_err());
    }
}
