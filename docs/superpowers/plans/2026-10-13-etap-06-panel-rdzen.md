# Etap 6: Panel — uwierzytelnianie, role, dashboard, compose, kolejka

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Operator loguje się w przeglądarce, widzi stan stacji na żywo, wpisuje własny komunikat i wysyła go na pasmo, a kolejkę może przejrzeć i anulować.

**Architecture:** `aars-web` wystawia router Axum; `aars-core` nadal nie wie o HTTP (spec D6), więc panel rozmawia z nim przez `QueueRepo`, `MessageRepo` i `Interlocks`. Szablony Askama renderują się po stronie serwera, HTMX odświeża fragmenty, SSE podaje status TX. Wszystkie assety są wkompilowane w binarkę — stacja ostrzegawcza musi działać bez internetu.

**Tech Stack:** axum 0.8, askama 0.16 (`Template::render() -> Result<String>`, bez integracji z axum — renderujemy przez własny helper), argon2 0.6, rust-embed 8, HTMX 2.0.4 zwendorowany w `assets/vendor/`.

**Spec:** `docs/superpowers/specs/2026-10-13-aars-design.md`

## Global Constraints

- Role: **viewer** czyta; **operator** nadaje, planuje i anuluje; **admin** dodatkowo zmienia konfigurację sprzętu i użytkowników (spec §8).
- Kill switch jest dostępny operatorowi — zatrzymanie nadawania nie może wymagać admina (spec §8).
- Hasła Argon2id; sesja w cookie `HttpOnly` + `SameSite=Lax`, `Secure` gdy skonfigurowany HTTPS; sesje w bazie, by wylogowanie działało natychmiast (spec §5, §8).
- Ochrona CSRF tokenem per sesja na każdym formularzu zmieniającym stan; rate limiting na `/login` (spec §8). Token żyje w nowej kolumnie `sessions.csrf_token` (migracja `0002_session_csrf.sql`) — dopisane po napisaniu planu, bo pierwotne zadania wymieniały ten wymóg w ograniczeniach, a nie realizowały go w krokach.
- Pierwszy start bez użytkowników tworzy konto admina z hasłem wypisanym na konsolę i wymusza zmianę przy pierwszym logowaniu (spec §8).
- Podgląd audio przechodzi tym samym pipeline'em co nadanie, włącznie z normalizacją (spec §8).
- Assety i szablony wkompilowane przez `rust-embed`; zero zależności sieciowych w czasie pracy (spec §8, §12).
- Awaria panelu nie dotyka nadawania (spec §10) — panel tylko wkłada zadania do kolejki.
- Testy panelu: `oneshot` na routerze z SQLite w pamięci; macierz rola × endpoint, CSRF, cykl życia sesji (spec §11).
- TDD: test przed implementacją, commit po każdym zielonym cyklu.

---

### Task 1: Użytkownicy, hasła i sesje

**Files:**
- Create: `crates/aars-core/src/user.rs`
- Modify: `crates/aars-core/src/lib.rs`
- Modify: `crates/aars-core/Cargo.toml` (dodaj `argon2`, `rand`)
- Modify: `Cargo.toml` (dodaj `argon2`, `rand` do `workspace.dependencies`)

**Interfaces:**
- Consumes: `aars_core::error::DbError`
- Produces: `aars_core::user::{UserRepo, User, Role, Session, AuthError}`; metody `create`, `authenticate`, `by_id`, `list`, `set_password`, `set_disabled`, `delete`, `start_session`, `session_user`, `end_session`, `purge_expired_sessions`, `ensure_bootstrap_admin`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-core/src/user.rs`:

```rust
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
        let user = repo.session_user(&session.id).await.unwrap().expect("sesja musi działać");
        assert_eq!(user.id, id);

        repo.end_session(&session.id).await.unwrap();
        assert!(
            repo.session_user(&session.id).await.unwrap().is_none(),
            "wylogowanie musi działać natychmiast"
        );
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
    async fn session_ids_are_unpredictable() {
        let repo = UserRepo::new(test_pool().await);
        let id = repo.create("operator", "haslo", Role::Operator).await.unwrap();

        let mut ids = std::collections::HashSet::new();
        for _ in 0..20 {
            let session = repo.start_session(id, None).await.unwrap();
            assert!(session.id.len() >= 32, "identyfikator sesji za krótki: {}", session.id);
            assert!(ids.insert(session.id), "identyfikator sesji się powtórzył");
        }
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
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-core user`
Expected: FAIL — `cannot find type UserRepo`

- [ ] **Step 3: Zaimplementuj `UserRepo`**

`Cargo.toml` workspace — dopisz do `[workspace.dependencies]`:

```toml
argon2 = "0.6"
rand = "0.9"
```

`crates/aars-core/Cargo.toml` — dopisz do `[dependencies]`:

```toml
argon2.workspace = true
rand.workspace = true
```

`crates/aars-core/src/user.rs` — nad blokiem testów:

```rust
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, password_hash::rand_core::OsRng};
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
}

pub struct UserRepo {
    pool: SqlitePool,
}

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

/// Losowy token o 256 bitach entropii, zapisany szesnastkowo.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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

    pub async fn create(
        &self,
        username: &str,
        password: &str,
        role: Role,
    ) -> Result<i64, AuthError> {
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
        let parsed =
            PasswordHash::new(&stored).map_err(|e| AuthError::Hash(e.to_string()))?;
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
        sqlx::query(
            "UPDATE users SET password_hash = ?, must_change_password = 0 WHERE id = ?",
        )
        .bind(hash)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| AuthError::Db(DbError::Sqlx(e)))?;
        Ok(())
    }

    /// Wyłącza konto i od razu zrywa jego sesje — wyłączenie, które działa
    /// dopiero po wygaśnięciu sesji, nie jest wyłączeniem.
    pub async fn set_disabled(&self, id: i64, disabled: bool) -> Result<(), DbError> {
        sqlx::query("UPDATE users SET disabled = ? WHERE id = ?")
            .bind(i64::from(disabled))
            .bind(id)
            .execute(&self.pool)
            .await?;

        if disabled {
            sqlx::query("DELETE FROM sessions WHERE user_id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn delete(&self, id: i64) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn start_session(
        &self,
        user_id: i64,
        user_agent: Option<&str>,
    ) -> Result<Session, DbError> {
        let id = random_token();
        let expires_at = Utc::now() + Duration::hours(SESSION_LIFETIME_HOURS);

        sqlx::query(
            "INSERT INTO sessions (id, user_id, expires_at, user_agent) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(iso(expires_at))
        .bind(user_agent)
        .execute(&self.pool)
        .await?;

        Ok(Session { id, user_id, expires_at })
    }

    /// Użytkownik stojący za sesją — albo `None`, gdy sesja wygasła, została
    /// zakończona, albo konto jest wyłączone.
    pub async fn session_user(&self, session_id: &str) -> Result<Option<User>, DbError> {
        let row = sqlx::query(&format!(
            "SELECT u.id, u.username, u.role, u.disabled, u.must_change_password
               FROM sessions s JOIN users u ON u.id = s.user_id
              WHERE s.id = ? AND s.expires_at > ? AND u.disabled = 0"
        ))
        .bind(session_id)
        .bind(iso(Utc::now()))
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.as_ref().map(row_to_user))
    }

    pub async fn end_session(&self, session_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
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

        let password = random_token()[..20].to_owned();
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
```

`crates/aars-core/src/lib.rs` — dopisz `pub mod user;`

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-core user`
Expected: PASS — dziewięć testów

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/aars-core
git commit -m "feat(core): użytkownicy z rolami, hasła Argon2id i sesje w bazie"
```

---

### Task 2: Router panelu z logowaniem i rolami

**Files:**
- Create: `crates/aars-web/Cargo.toml`
- Create: `crates/aars-web/src/lib.rs`
- Create: `crates/aars-web/src/state.rs`
- Create: `crates/aars-web/src/auth.rs`
- Create: `crates/aars-web/src/render.rs`
- Create: `crates/aars-web/src/assets.rs`
- Create: `crates/aars-web/templates/base.html`
- Create: `crates/aars-web/templates/login.html`
- Create: `assets/app.css`
- Modify: `Cargo.toml` (member `crates/aars-web`, zależności axum/askama/rust-embed/tower)

**Interfaces:**
- Consumes: `aars_core::user::{UserRepo, Role, User}`, `aars_core::queue::QueueRepo`, `aars_core::interlock::InMemoryInterlocks`
- Produces: `aars_web::{router, AppState}`; `aars_web::auth::{CurrentUser, require_role}`; `aars_web::state::AppState::new(...)`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

`crates/aars-web/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use aars_core::user::{Role, UserRepo};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    async fn state() -> AppState {
        let pool = aars_core::db::test_pool().await;
        AppState::for_tests(pool)
    }

    async fn login(app: &axum::Router, username: &str, password: &str) -> Option<String> {
        let body = format!("username={username}&password={password}");
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or_default().to_owned())
    }

    #[tokio::test]
    async fn the_dashboard_redirects_an_anonymous_visitor_to_the_login_page() {
        let app = router(state().await);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap().to_str().unwrap(),
            "/login"
        );
    }

    #[tokio::test]
    async fn the_login_page_is_served_without_a_session() {
        let app = router(state().await);

        let response = app
            .oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_correct_login_sets_an_httponly_session_cookie() {
        let state = state().await;
        UserRepo::new(state.pool.clone())
            .create("operator", "haslo-operatora", Role::Operator)
            .await
            .unwrap();
        let app = router(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("username=operator&password=haslo-operatora"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
    }

    #[tokio::test]
    async fn a_wrong_password_does_not_set_a_session() {
        let state = state().await;
        UserRepo::new(state.pool.clone())
            .create("operator", "haslo-operatora", Role::Operator)
            .await
            .unwrap();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("username=operator&password=zle"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn a_logged_in_viewer_sees_the_dashboard() {
        let state = state().await;
        UserRepo::new(state.pool.clone())
            .create("widz", "haslo-widza", Role::Viewer)
            .await
            .unwrap();
        let app = router(state);
        let cookie = login(&app, "widz", "haslo-widza").await.expect("cookie");

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_viewer_cannot_reach_operator_endpoints() {
        let state = state().await;
        UserRepo::new(state.pool.clone())
            .create("widz", "haslo-widza", Role::Viewer)
            .await
            .unwrap();
        let app = router(state);
        let cookie = login(&app, "widz", "haslo-widza").await.expect("cookie");

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/compose")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn logging_out_invalidates_the_cookie_immediately() {
        let state = state().await;
        UserRepo::new(state.pool.clone())
            .create("operator", "haslo", Role::Operator)
            .await
            .unwrap();
        let app = router(state);
        let cookie = login(&app, "operator", "haslo").await.expect("cookie");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/logout")
                    .header(header::COOKIE, cookie.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "sesja musi być unieważniona");
    }

    #[tokio::test]
    async fn vendored_assets_are_served_from_the_binary() {
        let app = router(state().await);

        for path in ["/assets/vendor/htmx.min.js", "/assets/app.css"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "brak assetu {path}");
        }
    }
}
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-web`
Expected: FAIL — brak pakietu `aars-web`

- [ ] **Step 3: Zaimplementuj panel**

`Cargo.toml` workspace — dopisz `"crates/aars-web"` do `members` oraz do `[workspace.dependencies]`:

```toml
axum = "0.8"
askama = "0.16"
rust-embed = "8"
tower = "0.5"
mime_guess = "2"
```

`crates/aars-web/Cargo.toml`:

```toml
[package]
name = "aars-web"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
aars-core = { path = "../aars-core" }
anyhow.workspace = true
askama.workspace = true
axum.workspace = true
chrono.workspace = true
mime_guess.workspace = true
rust-embed.workspace = true
serde.workspace = true
sqlx.workspace = true
tokio.workspace = true
tracing.workspace = true

[dev-dependencies]
aars-core = { path = "../aars-core", features = ["testing"] }
tokio = { workspace = true, features = ["rt", "macros"] }
tower.workspace = true
```

`crates/aars-web/src/assets.rs`:

```rust
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

/// Assety wkompilowane w binarkę: stacja ostrzegawcza musi działać bez
/// internetu, więc HTMX jest zwendorowany, nie ciągnięty z CDN.
#[derive(Embed)]
#[folder = "../../assets/"]
pub struct Assets;

pub async fn serve(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    match Assets::get(&path) {
        None => (StatusCode::NOT_FOUND, "nie ma takiego pliku").into_response(),
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_owned()),
                    // Assety są wersjonowane razem z binarką, więc mogą
                    // leżeć w cache przeglądarki.
                    (header::CACHE_CONTROL, "public, max-age=3600".to_owned()),
                ],
                file.data,
            )
                .into_response()
        }
    }
}
```

`crates/aars-web/src/render.rs`:

```rust
use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

/// Askama 0.16 nie ma integracji z axum — renderujemy sami. Błąd szablonu
/// to błąd programisty, więc zwracamy 500 i zapisujemy go w logu.
pub fn page<T: Template>(template: &T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            tracing::error!(%err, "renderowanie szablonu zawiodło");
            (StatusCode::INTERNAL_SERVER_ERROR, "błąd szablonu").into_response()
        }
    }
}
```

`crates/aars-web/src/state.rs`:

```rust
use std::sync::Arc;

use aars_core::interlock::InMemoryInterlocks;
use aars_core::queue::QueueRepo;
use sqlx::SqlitePool;

/// Wszystko, czego panel potrzebuje od rdzenia. Panel nie trzyma uchwytu do
/// sekwencera — tylko wkłada zadania do kolejki (spec §10).
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub queue: Arc<QueueRepo>,
    pub interlocks: Arc<InMemoryInterlocks>,
    /// Czy cookie sesji ma mieć flagę `Secure`.
    pub secure_cookies: bool,
}

impl AppState {
    pub fn new(
        pool: SqlitePool,
        queue: Arc<QueueRepo>,
        interlocks: Arc<InMemoryInterlocks>,
        secure_cookies: bool,
    ) -> Self {
        Self { pool, queue, interlocks, secure_cookies }
    }

    /// Stan na potrzeby testów: własna kolejka, interlocki przepuszczające.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_tests(pool: SqlitePool) -> Self {
        Self {
            queue: Arc::new(QueueRepo::new(pool.clone())),
            interlocks: Arc::new(InMemoryInterlocks::permissive()),
            pool,
            secure_cookies: false,
        }
    }
}
```

`crates/aars-web/src/auth.rs`:

```rust
use aars_core::user::{Role, User, UserRepo};
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};

use crate::state::AppState;

pub const SESSION_COOKIE: &str = "aars_session";

/// Użytkownik stojący za żądaniem. Brak sesji kończy się przekierowaniem na
/// stronę logowania, nie pustą stroną.
pub struct CurrentUser(pub User);

/// Odpowiedź dla żądania bez sesji albo bez uprawnień.
pub enum AuthRejection {
    Anonymous,
    Forbidden,
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        match self {
            Self::Anonymous => Redirect::to("/login").into_response(),
            Self::Forbidden => {
                (StatusCode::FORBIDDEN, "brak uprawnień do tej operacji").into_response()
            }
        }
    }
}

fn session_id_from(parts: &Parts) -> Option<String> {
    let header = parts.headers.get(header::COOKIE)?.to_str().ok()?;
    header.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == SESSION_COOKIE).then(|| value.to_owned())
    })
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session_id = session_id_from(parts).ok_or(AuthRejection::Anonymous)?;
        let user = UserRepo::new(state.pool.clone())
            .session_user(&session_id)
            .await
            .map_err(|err| {
                tracing::error!(%err, "odczyt sesji zawiódł");
                AuthRejection::Anonymous
            })?
            .ok_or(AuthRejection::Anonymous)?;
        Ok(CurrentUser(user))
    }
}

/// Sprawdza, czy użytkownik ma co najmniej wskazaną rolę.
pub fn require_role(user: &User, required: Role) -> Result<(), AuthRejection> {
    if user.role.can(required) {
        Ok(())
    } else {
        tracing::warn!(
            user = %user.username,
            role = user.role.as_str(),
            required = required.as_str(),
            "odmowa dostępu"
        );
        Err(AuthRejection::Forbidden)
    }
}

/// Buduje wartość nagłówka `Set-Cookie` dla sesji.
pub fn session_cookie(session_id: &str, secure: bool) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE}={session_id}; Path=/; HttpOnly; SameSite=Lax; Max-Age=43200"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn clear_cookie(secure: bool) -> String {
    let mut cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}
```

`crates/aars-web/src/lib.rs` — nad blokiem testów:

```rust
pub mod assets;
pub mod auth;
pub mod render;
pub mod state;

use aars_core::user::{AuthError, Role, UserRepo};
use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;

use crate::auth::{CurrentUser, require_role};
use crate::render::page;
pub use crate::state::AppState;

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    username: String,
    role: String,
    can_operate: bool,
    tx_enabled: bool,
    poisoned: bool,
    pending: Vec<QueueRow>,
}

#[derive(Template)]
#[template(path = "compose.html")]
struct ComposePage {
    username: String,
    role: String,
    can_operate: bool,
}

struct QueueRow {
    id: i64,
    priority: String,
    not_before: String,
    text: String,
    interrupting: bool,
}

#[derive(Debug, Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login_page() -> Response {
    page(&LoginPage { error: None })
}

async fn login_submit(
    State(state): State<AppState>,
    Form(form): Form<LoginForm>,
) -> Response {
    let repo = UserRepo::new(state.pool.clone());

    match repo.authenticate(&form.username, &form.password).await {
        Ok(user) => match repo.start_session(user.id, None).await {
            Ok(session) => (
                StatusCode::SEE_OTHER,
                [
                    (header::LOCATION, "/".to_owned()),
                    (
                        header::SET_COOKIE,
                        auth::session_cookie(&session.id, state.secure_cookies),
                    ),
                ],
            )
                .into_response(),
            Err(err) => {
                tracing::error!(%err, "utworzenie sesji zawiodło");
                (StatusCode::INTERNAL_SERVER_ERROR, "błąd sesji").into_response()
            }
        },
        Err(AuthError::InvalidCredentials | AuthError::Disabled) => {
            tracing::warn!(username = %form.username, "nieudane logowanie");
            (
                StatusCode::UNAUTHORIZED,
                page(&LoginPage {
                    error: Some("Niepoprawna nazwa użytkownika lub hasło.".to_owned()),
                }),
            )
                .into_response()
        }
        Err(err) => {
            tracing::error!(%err, "błąd uwierzytelniania");
            (StatusCode::INTERNAL_SERVER_ERROR, "błąd uwierzytelniania").into_response()
        }
    }
}

async fn logout(
    State(state): State<AppState>,
    CurrentUser(_user): CurrentUser,
    headers: header::HeaderMap,
) -> Response {
    if let Some(cookie) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok())
        && let Some(session_id) = cookie.split(';').find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == auth::SESSION_COOKIE).then(|| value.to_owned())
        })
    {
        let _ = UserRepo::new(state.pool.clone()).end_session(&session_id).await;
    }

    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/login".to_owned()),
            (header::SET_COOKIE, auth::clear_cookie(state.secure_cookies)),
        ],
    )
        .into_response()
}

async fn dashboard(State(state): State<AppState>, CurrentUser(user): CurrentUser) -> Response {
    let interlocks = aars_core::interlock::Interlocks::snapshot(state.interlocks.as_ref());
    let pending = match state.queue.pending(20).await {
        Ok(items) => items,
        Err(err) => {
            tracing::error!(%err, "odczyt kolejki dla panelu zawiódł");
            Vec::new()
        }
    };

    page(&DashboardPage {
        username: user.username.clone(),
        role: user.role.as_str().to_owned(),
        can_operate: user.role.can(Role::Operator),
        tx_enabled: interlocks.tx_enabled,
        poisoned: interlocks.poisoned,
        pending: pending.into_iter().map(queue_row).collect(),
    })
}

fn queue_row(item: aars_core::queue::QueueItem) -> QueueRow {
    QueueRow {
        id: item.id,
        priority: format!("{:?}", item.priority),
        not_before: item.not_before.format("%H:%M:%S").to_string(),
        text: item.rendered_text,
        interrupting: item.interrupting,
    }
}

async fn compose_page(CurrentUser(user): CurrentUser) -> Result<Response, auth::AuthRejection> {
    require_role(&user, Role::Operator)?;
    Ok(page(&ComposePage {
        username: user.username.clone(),
        role: user.role.as_str().to_owned(),
        can_operate: true,
    }))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .route("/compose", get(compose_page))
        .route("/assets/{*path}", get(assets::serve))
        .with_state(state)
}
```

`assets/app.css`:

```css
:root {
  --bg: #11161c;
  --panel: #19202a;
  --line: #2a3542;
  --text: #e6edf3;
  --muted: #8b98a8;
  --accent: #4da3ff;
  --warn: #ffb224;
  --bad: #ff5c5c;
  --good: #3fcf8e;
}

* { box-sizing: border-box; }

body {
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif;
}

header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  padding: 0.75rem 1.25rem;
  background: var(--panel);
  border-bottom: 1px solid var(--line);
}

header h1 { font-size: 1.05rem; margin: 0; letter-spacing: 0.04em; }
header nav { display: flex; gap: 1rem; align-items: center; }
header nav a { color: var(--muted); text-decoration: none; }
header nav a:hover { color: var(--text); }

main { padding: 1.25rem; max-width: 72rem; margin: 0 auto; }

.card {
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 8px;
  padding: 1rem 1.25rem;
  margin-bottom: 1rem;
}

.card h2 { margin: 0 0 0.75rem; font-size: 0.95rem; color: var(--muted);
            text-transform: uppercase; letter-spacing: 0.08em; }

table { width: 100%; border-collapse: collapse; }
th, td { text-align: left; padding: 0.4rem 0.5rem; border-bottom: 1px solid var(--line); }
th { color: var(--muted); font-weight: 600; font-size: 0.85rem; }
tr:last-child td { border-bottom: none; }

.badge {
  display: inline-block; padding: 0.1rem 0.45rem; border-radius: 4px;
  font-size: 0.78rem; font-weight: 600;
}
.badge.on { background: rgba(63, 207, 142, 0.15); color: var(--good); }
.badge.off { background: rgba(255, 92, 92, 0.15); color: var(--bad); }
.badge.warn { background: rgba(255, 178, 36, 0.15); color: var(--warn); }

label { display: block; margin-bottom: 0.3rem; color: var(--muted); font-size: 0.85rem; }

input, select, textarea, button {
  font: inherit; color: var(--text); background: #0e1319;
  border: 1px solid var(--line); border-radius: 6px; padding: 0.45rem 0.6rem;
}
textarea { width: 100%; min-height: 6rem; resize: vertical; }
.row { display: flex; gap: 1rem; flex-wrap: wrap; margin-bottom: 0.9rem; }
.row > div { flex: 1 1 10rem; }

button {
  background: var(--accent); border-color: var(--accent); color: #06131f;
  font-weight: 600; cursor: pointer;
}
button:hover { filter: brightness(1.08); }
button.ghost { background: transparent; color: var(--text); border-color: var(--line); }
button.danger { background: var(--bad); border-color: var(--bad); color: #1b0606; }

.error { color: var(--bad); margin-bottom: 0.8rem; }
.login { max-width: 22rem; margin: 6rem auto; }
```

`crates/aars-web/templates/base.html`:

```html
<!doctype html>
<html lang="pl">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{% block title %}AARS{% endblock %}</title>
  <link rel="stylesheet" href="/assets/app.css">
  <script src="/assets/vendor/htmx.min.js" defer></script>
</head>
<body>
{% block chrome %}
<header>
  <h1>AARS — stacja ostrzegawcza</h1>
  <nav>
    <a href="/">Pulpit</a>
    {% if can_operate %}<a href="/compose">Nadaj</a>{% endif %}
    <span class="badge warn">{{ username }} · {{ role }}</span>
    <form method="post" action="/logout" style="margin:0">
      <button class="ghost" type="submit">Wyloguj</button>
    </form>
  </nav>
</header>
{% endblock %}
<main>{% block content %}{% endblock %}</main>
</body>
</html>
```

`crates/aars-web/templates/login.html`:

```html
<!doctype html>
<html lang="pl">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>AARS — logowanie</title>
  <link rel="stylesheet" href="/assets/app.css">
</head>
<body>
<main>
  <div class="card login">
    <h2>Logowanie</h2>
    {% if let Some(message) = error %}<p class="error">{{ message }}</p>{% endif %}
    <form method="post" action="/login">
      <div style="margin-bottom:0.8rem">
        <label for="username">Użytkownik</label>
        <input id="username" name="username" autocomplete="username" autofocus required>
      </div>
      <div style="margin-bottom:1rem">
        <label for="password">Hasło</label>
        <input id="password" name="password" type="password"
               autocomplete="current-password" required>
      </div>
      <button type="submit">Wejdź</button>
    </form>
  </div>
</main>
</body>
</html>
```

`crates/aars-web/templates/dashboard.html`:

```html
{% extends "base.html" %}
{% block title %}AARS — pulpit{% endblock %}
{% block content %}
<div class="card">
  <h2>Stan nadawania</h2>
  <p>
    Nadawanie:
    {% if tx_enabled %}<span class="badge on">włączone</span>
    {% else %}<span class="badge off">wyłączone</span>{% endif %}
    {% if poisoned %}<span class="badge off">watchdog — wymaga reset</span>{% endif %}
  </p>
</div>

<div class="card">
  <h2>Kolejka</h2>
  {% if pending.is_empty() %}
    <p style="color:var(--muted)">Kolejka pusta.</p>
  {% else %}
  <table>
    <thead><tr><th>#</th><th>Priorytet</th><th>Od</th><th>Treść</th></tr></thead>
    <tbody>
      {% for item in pending %}
      <tr>
        <td>{{ item.id }}</td>
        <td>{{ item.priority }}{% if item.interrupting %} ·
            <span class="badge warn">przerywa</span>{% endif %}</td>
        <td>{{ item.not_before }}</td>
        <td>{{ item.text }}</td>
      </tr>
      {% endfor %}
    </tbody>
  </table>
  {% endif %}
</div>
{% endblock %}
```

`crates/aars-web/templates/compose.html`:

```html
{% extends "base.html" %}
{% block title %}AARS — nadaj komunikat{% endblock %}
{% block content %}
<div class="card">
  <h2>Nadaj komunikat</h2>
  <form method="post" action="/compose">
    <div style="margin-bottom:0.9rem">
      <label for="text">Treść</label>
      <textarea id="text" name="text" required
                placeholder="Uwaga, ostrzeżenie meteorologiczne..."></textarea>
    </div>
    <div class="row">
      <div>
        <label for="priority">Priorytet</label>
        <select id="priority" name="priority">
          <option value="normal" selected>Zwykły</option>
          <option value="low">Niski</option>
          <option value="high">Wysoki</option>
          <option value="emergency">Alarmowy</option>
        </select>
      </div>
      <div>
        <label for="repeat">Powtórzeń</label>
        <input id="repeat" name="repeat" type="number" value="1" min="1" max="20">
      </div>
      <div>
        <label for="every">Odstęp (s)</label>
        <input id="every" name="every" type="number" value="600" min="0">
      </div>
    </div>
    <label style="display:flex;gap:0.5rem;align-items:center;margin-bottom:1rem">
      <input type="checkbox" name="interrupting" value="1" style="width:auto">
      Przerwij trwające nadanie o niższym priorytecie
    </label>
    <button type="submit">Do kolejki</button>
  </form>
</div>
{% endblock %}
```

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test -p aars-web`
Expected: PASS — osiem testów

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml assets crates/aars-web
git commit -m "feat(web): router panelu z logowaniem, rolami i wkompilowanymi assetami"
```

---

### Task 3: Nadawanie z panelu, kill switch i status na żywo

**Files:**
- Modify: `crates/aars-web/src/lib.rs`
- Create: `crates/aars-web/templates/queue_fragment.html`
- Modify: `crates/aars-web/templates/dashboard.html`
- Modify: `crates/aars/src/main.rs` (serwer HTTP w `aars run`)
- Modify: `crates/aars/Cargo.toml`

**Interfaces:**
- Consumes: `aars_core::queue::EnqueueRequest`, `aars_core::interlock::InMemoryInterlocks`
- Produces: `POST /compose`, `POST /tx-enabled`, `POST /queue/{id}/cancel`, `GET /fragments/queue`; `aars run` nasłuchuje na `server.bind`

- [ ] **Step 1: Napisz testy, które mają nie przejść**

Dopisz do bloku testów w `crates/aars-web/src/lib.rs`:

```rust
    async fn logged_in(role: Role) -> (axum::Router, String, AppState) {
        let state = state().await;
        UserRepo::new(state.pool.clone()).create("u", "haslo", role).await.unwrap();
        let app = router(state.clone());
        let cookie = login(&app, "u", "haslo").await.expect("cookie");
        (app, cookie, state)
    }

    #[tokio::test]
    async fn an_operator_can_put_a_message_on_the_queue() {
        let (app, cookie, state) = logged_in(Role::Operator).await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/compose")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "text=uwaga+ostrzezenie&priority=high&repeat=2&every=60&interrupting=1",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let pending = state.queue.pending(10).await.unwrap();
        assert_eq!(pending.len(), 2, "powtórzenia muszą się rozwinąć");
        assert_eq!(pending[0].rendered_text, "uwaga ostrzezenie");
        assert_eq!(pending[0].priority, aars_core::priority::Priority::High);
        assert!(pending[0].interrupting);
    }

    #[tokio::test]
    async fn a_viewer_cannot_put_anything_on_the_queue() {
        let (app, cookie, state) = logged_in(Role::Viewer).await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/compose")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("text=proba&priority=normal&repeat=1&every=0"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.queue.pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_message_is_rejected_instead_of_queuing_silence() {
        let (app, cookie, state) = logged_in(Role::Operator).await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/compose")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("text=+++&priority=normal&repeat=1&every=0"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(state.queue.pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_operator_can_flip_the_kill_switch() {
        let (app, cookie, state) = logged_in(Role::Operator).await;
        assert!(aars_core::interlock::Interlocks::snapshot(state.interlocks.as_ref()).tx_enabled);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tx-enabled")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("enabled=0"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            !aars_core::interlock::Interlocks::snapshot(state.interlocks.as_ref()).tx_enabled,
            "zatrzymanie nadawania musi działać od razu"
        );

        let stored: String =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'tx_enabled'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(stored, "false", "stan kill switcha musi przetrwać restart");
    }

    #[tokio::test]
    async fn an_operator_can_cancel_a_queued_item() {
        let (app, cookie, state) = logged_in(Role::Operator).await;
        let ids = state
            .queue
            .enqueue(aars_core::queue::EnqueueRequest {
                message_id: None,
                rendered_text: "do anulowania".to_owned(),
                priority: aars_core::priority::Priority::Low,
                interrupting: false,
                not_before: None,
                expires_at: None,
                repeat_count: 1,
                repeat_interval: std::time::Duration::ZERO,
                created_by: None,
            })
            .await
            .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/queue/{}/cancel", ids[0]))
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(response.status().is_success(), "{:?}", response.status());
        assert!(state.queue.pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_queue_fragment_is_served_for_live_updates() {
        let (app, cookie, state) = logged_in(Role::Viewer).await;
        state
            .queue
            .enqueue(aars_core::queue::EnqueueRequest {
                message_id: None,
                rendered_text: "w kolejce".to_owned(),
                priority: aars_core::priority::Priority::Normal,
                interrupting: false,
                not_before: None,
                expires_at: None,
                repeat_count: 1,
                repeat_interval: std::time::Duration::ZERO,
                created_by: None,
            })
            .await
            .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/fragments/queue")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("w kolejce"), "{html}");
        assert!(!html.contains("<html"), "fragment nie może być całą stroną");
    }
```

- [ ] **Step 2: Uruchom testy i potwierdź, że nie przechodzą**

Run: `cargo test -p aars-web`
Expected: FAIL — brak tras `/compose` (POST), `/tx-enabled`, `/queue/{id}/cancel`, `/fragments/queue`

- [ ] **Step 3: Zaimplementuj trasy**

`crates/aars-web/templates/queue_fragment.html`:

```html
{% if pending.is_empty() %}
<p style="color:var(--muted)">Kolejka pusta.</p>
{% else %}
<table>
  <thead><tr><th>#</th><th>Priorytet</th><th>Od</th><th>Treść</th>
    {% if can_operate %}<th></th>{% endif %}</tr></thead>
  <tbody>
    {% for item in pending %}
    <tr>
      <td>{{ item.id }}</td>
      <td>{{ item.priority }}{% if item.interrupting %} ·
          <span class="badge warn">przerywa</span>{% endif %}</td>
      <td>{{ item.not_before }}</td>
      <td>{{ item.text }}</td>
      {% if can_operate %}
      <td><button class="ghost" hx-post="/queue/{{ item.id }}/cancel"
                  hx-target="#queue" hx-swap="innerHTML">Anuluj</button></td>
      {% endif %}
    </tr>
    {% endfor %}
  </tbody>
</table>
{% endif %}
```

W `crates/aars-web/templates/dashboard.html` zamień zawartość karty „Kolejka" na
kontener odświeżany przez HTMX oraz dodaj przełącznik nadawania:

```html
<div class="card">
  <h2>Stan nadawania</h2>
  <p>
    Nadawanie:
    {% if tx_enabled %}<span class="badge on">włączone</span>
    {% else %}<span class="badge off">wyłączone</span>{% endif %}
    {% if poisoned %}<span class="badge off">watchdog — wymaga reset</span>{% endif %}
  </p>
  {% if can_operate %}
  <form method="post" action="/tx-enabled" style="margin-top:0.6rem">
    <input type="hidden" name="enabled" value="{% if tx_enabled %}0{% else %}1{% endif %}">
    <button type="submit" class="{% if tx_enabled %}danger{% endif %}">
      {% if tx_enabled %}Zatrzymaj nadawanie{% else %}Włącz nadawanie{% endif %}
    </button>
  </form>
  {% endif %}
</div>

<div class="card">
  <h2>Kolejka</h2>
  <div id="queue" hx-get="/fragments/queue" hx-trigger="load, every 3s"
       hx-swap="innerHTML">{% include "queue_fragment.html" %}</div>
</div>
```

W `crates/aars-web/src/lib.rs` dopisz szablon fragmentu, formularze i trasy:

```rust
#[derive(Template)]
#[template(path = "queue_fragment.html")]
struct QueueFragment {
    pending: Vec<QueueRow>,
    can_operate: bool,
}

#[derive(Debug, Deserialize)]
struct ComposeForm {
    text: String,
    priority: String,
    #[serde(default = "one")]
    repeat: u32,
    #[serde(default)]
    every: u64,
    #[serde(default)]
    interrupting: Option<String>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
struct TxEnabledForm {
    enabled: String,
}

fn parse_priority(value: &str) -> Option<aars_core::priority::Priority> {
    use aars_core::priority::Priority;
    match value {
        "emergency" => Some(Priority::Emergency),
        "high" => Some(Priority::High),
        "normal" => Some(Priority::Normal),
        "low" => Some(Priority::Low),
        _ => None,
    }
}

async fn queue_rows(state: &AppState) -> Vec<QueueRow> {
    match state.queue.pending(50).await {
        Ok(items) => items.into_iter().map(queue_row).collect(),
        Err(err) => {
            tracing::error!(%err, "odczyt kolejki zawiódł");
            Vec::new()
        }
    }
}

async fn compose_submit(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(form): Form<ComposeForm>,
) -> Result<Response, auth::AuthRejection> {
    require_role(&user, Role::Operator)?;

    let text = form.text.trim().to_owned();
    if text.is_empty() {
        // Puste nadanie to wejście na pasmo z ciszą — odmawiamy.
        return Ok((StatusCode::BAD_REQUEST, "treść komunikatu nie może być pusta")
            .into_response());
    }

    let Some(priority) = parse_priority(&form.priority) else {
        return Ok((StatusCode::BAD_REQUEST, "nieznany priorytet").into_response());
    };

    let request = aars_core::queue::EnqueueRequest {
        message_id: None,
        rendered_text: text,
        priority,
        interrupting: form.interrupting.is_some(),
        not_before: None,
        expires_at: None,
        repeat_count: form.repeat.clamp(1, 20),
        repeat_interval: std::time::Duration::from_secs(form.every),
        created_by: Some(user.id),
    };

    match state.queue.enqueue(request).await {
        Ok(ids) => {
            tracing::info!(user = %user.username, count = ids.len(), "komunikat dodany z panelu");
            audit(&state, user.id, "compose", &format!("{} zadań", ids.len())).await;
            Ok(Redirect::to("/").into_response())
        }
        Err(err) => {
            tracing::error!(%err, "dodanie komunikatu z panelu zawiodło");
            Ok((StatusCode::INTERNAL_SERVER_ERROR, "błąd kolejki").into_response())
        }
    }
}

async fn set_tx_enabled(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(form): Form<TxEnabledForm>,
) -> Result<Response, auth::AuthRejection> {
    // Kill switch jest dostępny operatorowi: zatrzymanie nadawania nie może
    // wymagać admina (spec §8).
    require_role(&user, Role::Operator)?;

    let enabled = matches!(form.enabled.as_str(), "1" | "true" | "on");
    state.interlocks.set_tx_enabled(enabled);

    // Stan musi przetrwać restart, więc ląduje też w `settings`.
    if let Err(err) = sqlx::query(
        "UPDATE settings SET value = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
          WHERE key = 'tx_enabled'",
    )
    .bind(if enabled { "true" } else { "false" })
    .execute(&state.pool)
    .await
    {
        tracing::error!(%err, "zapis stanu nadawania zawiódł");
    }

    tracing::warn!(user = %user.username, enabled, "zmiana stanu nadawania z panelu");
    audit(&state, user.id, "tx_enabled", if enabled { "włączone" } else { "wyłączone" }).await;

    Ok(Redirect::to("/").into_response())
}

async fn cancel_item(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Response, auth::AuthRejection> {
    require_role(&user, Role::Operator)?;

    match state.queue.cancel(id).await {
        Ok(count) => {
            tracing::info!(user = %user.username, id, count, "anulowanie zadania z panelu");
            audit(&state, user.id, "queue_cancel", &format!("zadanie {id}")).await;
        }
        Err(err) => tracing::error!(%err, id, "anulowanie zadania zawiodło"),
    }

    // Odpowiadamy fragmentem, więc HTMX podmienia tabelę bez przeładowania.
    Ok(page(&QueueFragment {
        pending: queue_rows(&state).await,
        can_operate: true,
    }))
}

async fn queue_fragment(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Response {
    page(&QueueFragment {
        pending: queue_rows(&state).await,
        can_operate: user.role.can(Role::Operator),
    })
}

/// Zapis do dziennika audytu: kto kazał i co (spec §5).
async fn audit(state: &AppState, user_id: i64, action: &str, detail: &str) {
    if let Err(err) = sqlx::query("INSERT INTO audit_log (user_id, action, detail) VALUES (?, ?, ?)")
        .bind(user_id)
        .bind(action)
        .bind(detail)
        .execute(&state.pool)
        .await
    {
        tracing::error!(%err, "zapis do dziennika audytu zawiódł");
    }
}
```

oraz rozszerz `router`:

```rust
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .route("/compose", get(compose_page).post(compose_submit))
        .route("/tx-enabled", post(set_tx_enabled))
        .route("/queue/{id}/cancel", post(cancel_item))
        .route("/fragments/queue", get(queue_fragment))
        .route("/healthz", get(|| async { "ok" }))
        .route("/assets/{*path}", get(assets::serve))
        .with_state(state)
}
```

W `crates/aars/src/main.rs`, w gałęzi `Run`, po utworzeniu `queue` i `stack`
uruchom serwer HTTP i wypisz hasło konta startowego:

```rust
            let users = aars_core::user::UserRepo::new(pool.clone());
            if let Some(password) =
                users.ensure_bootstrap_admin().await.context("konto startowe")?
            {
                // Jedyny moment, w którym to hasło jest widoczne.
                println!("== konto startowe: admin / {password}");
                println!("== zmień je po pierwszym logowaniu");
            }

            let app = aars_web::router(aars_web::AppState::new(
                pool.clone(),
                queue.clone(),
                stack.interlocks.clone(),
                false,
            ));
            let listener = tokio::net::TcpListener::bind(&config.server.bind)
                .await
                .with_context(|| format!("nasłuch na {}", config.server.bind))?;
            tracing::info!(bind = %config.server.bind, "panel dostępny");
            let server_shutdown = shutdown.clone();
            let server_task = tokio::spawn(async move {
                let graceful = axum::serve(listener, app).with_graceful_shutdown(async move {
                    while !server_shutdown.is_cancelled() {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                });
                if let Err(err) = graceful.await {
                    tracing::error!(%err, "serwer panelu zakończył się błędem");
                }
            });
```

Przy zatrzymaniu dołóż `let _ = server_task.await;`.

`crates/aars/Cargo.toml` — dopisz do `[dependencies]`:

```toml
aars-web = { path = "../aars-web" }
axum.workspace = true
```

- [ ] **Step 4: Uruchom testy i potwierdź, że przechodzą**

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 5: Sprawdź panel w przeglądarce**

```bash
export AARS_STORAGE__DB_PATH=/tmp/aars-web/a.db
cargo run -p aars -- run
# Hasło konta startowego wypisuje się na konsoli; wejdź na http://127.0.0.1:8080
```
Expected: logowanie działa, pulpit pokazuje stan nadawania i kolejkę
odświeżaną co 3 s, operator może nadać komunikat i zatrzymać nadawanie

- [ ] **Step 6: Commit**

```bash
git add crates/aars-web crates/aars
git commit -m "feat(web): nadawanie z panelu, kill switch i kolejka na żywo"
```

---

## Kryterium wyjścia z etapu

- `cargo test --workspace` zielone; `cargo clippy --workspace --all-targets -- -D warnings` bez zastrzeżeń.
- Anonimowy gość jest przekierowany na `/login`; poprawne logowanie ustawia cookie `HttpOnly` + `SameSite=Lax`; wylogowanie unieważnia sesję natychmiast.
- Macierz rola × endpoint: viewer czyta pulpit, ale nie dosięga `/compose`; operator nadaje, anuluje i przestawia kill switch.
- Wyłączenie konta zrywa jego otwarte sesje.
- Kill switch przestawiony z panelu działa od razu i przetrwa restart (zapis w `settings`).
- Pusta treść komunikatu jest odrzucana, zamiast wchodzić na pasmo z ciszą.
- Assety, w tym HTMX, są serwowane z binarki — panel działa bez internetu.
