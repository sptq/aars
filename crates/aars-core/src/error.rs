use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("plik konfiguracyjny nie istnieje: {path}")]
    NotFound { path: PathBuf },
    #[error("niepoprawna konfiguracja: {0}")]
    Invalid(#[from] figment::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("błąd bazy danych: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("błąd migracji: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("błąd wejścia/wyjścia: {0}")]
    Io(#[from] std::io::Error),
}
