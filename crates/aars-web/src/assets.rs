use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

/// Assety wkompilowane w binarkę: stacja ostrzegawcza musi działać bez
/// internetu, więc HTMX jest zwendorowany, nie ciągnięty z CDN.
#[derive(Embed)]
#[folder = "../../assets/"]
pub struct Assets;

/// Odcisk wszystkich assetów (z sum SHA-256 wkompilowanych plików), liczony
/// raz. Szablony dokładają go do adresów (`/assets/app.css?v=…`), więc po
/// aktualizacji stacji przeglądarka pobiera nowe pliki od razu, zamiast
/// trzymać stare z cache.
pub fn stamp() -> &'static str {
    static STAMP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STAMP.get_or_init(|| {
        let mut names: Vec<_> = Assets::iter().collect();
        names.sort();
        // FNV-1a po nazwach i sumach: stabilny między uruchomieniami.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for b in bytes {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
        };
        for name in names {
            eat(name.as_bytes());
            if let Some(file) = Assets::get(&name) {
                eat(&file.metadata.sha256_hash());
            }
        }
        format!("{hash:016x}")[..10].to_owned()
    })
}

#[derive(serde::Deserialize)]
pub struct Version {
    v: Option<String>,
}

pub async fn serve(
    axum::extract::Path(path): axum::extract::Path<String>,
    axum::extract::Query(version): axum::extract::Query<Version>,
) -> Response {
    match Assets::get(&path) {
        None => (StatusCode::NOT_FOUND, "nie ma takiego pliku").into_response(),
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            // Adres z bieżącym odciskiem (`?v=`) nigdy nie zmieni treści — może
            // leżeć w cache rok. Bez odcisku (albo ze starym) jak dawniej:
            // godzina, granice powiatów (743 KB) dzień.
            let cache = if version.v.as_deref() == Some(stamp()) {
                "public, max-age=31536000, immutable"
            } else if path == "powiaty.geojson" {
                "public, max-age=86400"
            } else {
                "public, max-age=3600"
            };
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
    async fn a_versioned_address_is_cached_for_good_and_an_old_one_is_not() {
        let stamp = super::stamp();
        assert_eq!(stamp.len(), 10);
        assert_eq!(stamp, super::stamp(), "odcisk liczony raz, stały");
        let (status, mime, cache) = fetch(&format!("/assets/app.css?v={stamp}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(mime.starts_with("text/css"), "{mime}");
        assert_eq!(cache, "public, max-age=31536000, immutable");
        // stary odcisk (po aktualizacji) — zwykła godzina, nie rok
        let (_, _, cache) = fetch("/assets/app.css?v=0000000000").await;
        assert_eq!(cache, "public, max-age=3600");
    }

    #[tokio::test]
    async fn pages_link_assets_with_the_current_stamp() {
        let app = router(AppState::for_tests(aars_core::db::test_pool().await));
        let response = app
            .oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let html = String::from_utf8_lossy(&body);
        let stamp = super::stamp();
        assert!(html.contains(&format!("href=\"/assets/app.css?v={stamp}\"")), "{html}");
        assert!(html.contains(&format!("src=\"/assets/theme.js?v={stamp}\"")), "{html}");
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

    #[tokio::test]
    async fn the_theme_script_and_the_icon_sprite_are_embedded() {
        let (status, mime) = content_type("/assets/theme.js").await;
        assert_eq!(status, StatusCode::OK);
        assert!(mime.contains("javascript"), "{mime}");
        let (status, mime) = content_type("/assets/icons.svg").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(mime, "image/svg+xml");
        let script = String::from_utf8(super::Assets::get("theme.js").unwrap().data.into_owned()).unwrap();
        for needle in ["aars-theme", "try", "data-theme-toggle"] {
            assert!(script.contains(needle), "theme.js bez {needle}");
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

#[cfg(test)]
mod demo_tests {
    #[test]
    fn the_demo_does_not_touch_hardware() {
        let root = env!("CARGO_MANIFEST_DIR");
        let source = std::fs::read_to_string(format!("{root}/examples/panel_demo.rs")).unwrap();
        for banned in [
            "with_station",
            "with_probe",
            "with_update",
            // `with_preview` jest dozwolone: lokalny Piper składa tylko plik
            // WAV dla przeglądarki, bez karty dźwiękowej i bez PTT.
            "serial",
            "gpio",
            "cpal",
        ] {
            assert!(!source.contains(banned), "przykład używa {banned}");
        }
        assert!(source.contains("127.0.0.1"));
        let bin = std::fs::read_to_string(format!("{root}/../aars/Cargo.toml")).unwrap();
        assert!(!bin.contains("panel_demo"));
    }
}
