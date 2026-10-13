use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("plik konfiguracyjny nie istnieje: {path}")]
    NotFound { path: PathBuf },
    #[error("niepoprawna konfiguracja: {0}")]
    Invalid(#[from] figment::Error),
}
