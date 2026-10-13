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
