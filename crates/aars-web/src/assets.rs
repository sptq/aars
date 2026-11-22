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
            // Assety są wersjonowane razem z binarką, więc mogą leżeć w cache
            // przeglądarki; granice powiatów (743 KB) zmieniają się rzadko.
            let cache = if path == "powiaty.geojson" { "public, max-age=86400" } else { "public, max-age=3600" };
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_owned()),
                    (header::CACHE_CONTROL, cache.to_owned()),
                ],
                file.data,
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::router;
    use crate::state::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    async fn content_type(path: &str) -> (StatusCode, String) {
        let (status, mime, _) = fetch(path).await;
        (status, mime)
    }

    /// Status, `Content-Type` i `Cache-Control` assetu.
    async fn fetch(path: &str) -> (StatusCode, String, String) {
        let app = router(AppState::for_tests(aars_core::db::test_pool().await));
        let response = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let header_text = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        let mime = header_text(header::CONTENT_TYPE);
        let cache = header_text(header::CACHE_CONTROL);
        (response.status(), mime, cache)
    }

    #[tokio::test]
    async fn the_county_borders_are_cached_for_a_day() {
        let (status, mime, cache) = fetch("/assets/powiaty.geojson").await;
        assert_eq!(status, StatusCode::OK);
        assert!(mime.contains("json"), "{mime}");
        assert_eq!(cache, "public, max-age=86400");
        let (status, _, cache) = fetch("/assets/station-map.js").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cache, "public, max-age=3600");
    }

    #[tokio::test]
    async fn leaflet_and_the_map_script_are_embedded() {
        for path in [
            "/assets/vendor/leaflet/leaflet.js",
            "/assets/station-map.js",
            "/assets/warnings-map.js",
        ] {
            let (status, mime) = content_type(path).await;
            assert_eq!(status, StatusCode::OK, "brak assetu {path}");
            assert!(mime.contains("javascript"), "{path}: {mime}");
        }
        let (status, mime) = content_type("/assets/vendor/leaflet/leaflet.css").await;
        assert_eq!(status, StatusCode::OK);
        assert!(mime.contains("css"), "{mime}");
        for image in ["marker-icon.png", "marker-icon-2x.png", "marker-shadow.png"] {
            let (status, mime) =
                content_type(&format!("/assets/vendor/leaflet/images/{image}")).await;
            assert_eq!(status, StatusCode::OK, "brak {image}");
            assert_eq!(mime, "image/png", "{image}");
        }
    }

    /// Spec D5: panel działa bez internetu, więc szablony nie ładują skryptów
    /// ani stylów z innych serwerów (podkład mapy to obrazki, nie kod).
    #[test]
    fn no_template_loads_scripts_or_styles_from_another_origin() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            for bad in [
                "<script src=\"http",
                "<script src=\"//",
                "<link rel=\"stylesheet\" href=\"http",
                "<link rel=\"stylesheet\" href=\"//",
            ] {
                assert!(!text.contains(bad), "{} ładuje {bad}…", path.display());
            }
            checked += 1;
        }
        assert!(checked > 5, "nie znaleziono szablonów w {}", dir.display());
    }
}
