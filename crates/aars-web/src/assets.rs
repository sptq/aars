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
