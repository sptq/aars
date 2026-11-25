use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

/// Błąd bazy przy odczycie strony: zapis w logu i 500. Pusta tabela
/// udawałaby „nic nie czeka”, a to na stacji ostrzegawczej kłamstwo.
pub fn db_error(what: &str, err: &dyn std::fmt::Display) -> Response {
    tracing::error!(%err, "{what}");
    (StatusCode::INTERNAL_SERVER_ERROR, "błąd bazy danych").into_response()
}

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

/// Odpowiedzi dla formularzy wysyłanych przez htmx (etap 13, D): zamiast
/// przekierowania karta strony i powiadomienie w nagłówku `HX-Trigger`.
/// Bez JS formularze idą zwykłym `POST` i dostają to, co dotąd.
pub mod htmx {
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::response::Response;

    /// Formularz z `hx-post` prosi o fragment strony. Nawigacja `hx-boost`
    /// i odtworzenie historii też niosą `HX-Request`, ale podmieniają całe
    /// `<body>`, więc dostają pełną stronę.
    pub fn is_htmx(headers: &HeaderMap) -> bool {
        let on = |name: &str| headers.get(name).is_some_and(|v| v.as_bytes() == b"true");
        on("HX-Request") && !on("HX-Boosted") && !on("HX-History-Restore-Request")
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Kind {
        Ok,
        Error,
    }

    impl Kind {
        fn as_str(self) -> &'static str {
            match self {
                Kind::Ok => "ok",
                Kind::Error => "error",
            }
        }
    }

    /// Tekst jako napis JSON z samym ASCII: nagłówek HTTP przeglądarka
    /// czyta jako Latin-1, więc polskie litery idą jako `\uXXXX`.
    fn json_string(text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 2);
        out.push('"');
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                ' '..='~' => out.push(c),
                _ => {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        out.push_str(&format!("\\u{unit:04x}"));
                    }
                }
            }
        }
        out.push('"');
        out
    }

    /// Dokłada powiadomienie (`HX-Trigger: {"toast":{…}}`); `assets/panel.js`
    /// pokazuje je w regionie `aria-live`.
    pub fn toast(mut response: Response, kind: Kind, text: &str) -> Response {
        let value = format!(
            "{{\"toast\":{{\"kind\":\"{}\",\"text\":{}}}}}",
            kind.as_str(),
            json_string(text)
        );
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().insert("HX-Trigger", value);
        }
        response
    }

    /// Udany zapis: karta i „Zapisano” (albo inny tekst). Gdy samo
    /// odświeżenie strony po zapisie zawiodło (np. 500 z bazy), powiadomienie
    /// mówi o tym zamiast udawać, że wszystko jest w porządku.
    pub fn ok(response: Response, text: &str) -> Response {
        if response.status().is_success() {
            toast(response, Kind::Ok, text)
        } else {
            toast(response, Kind::Error, NOT_REFRESHED)
        }
    }

    /// Akcja wykonana, ale widok po niej się nie złożył.
    pub const NOT_REFRESHED: &str =
        "Wykonano, ale nie udało się odświeżyć widoku — przeładuj stronę.";

    /// Odrzucony formularz: karta z błędem i wpisanymi wartościami. Strona
    /// z błędem walidacji (400) idzie jako 422, który htmx wstawia w kartę;
    /// inne statusy (500, 503) zostają, jakie są.
    pub fn refused(mut response: Response, text: &str) -> Response {
        let html = response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|v| v.as_bytes().starts_with(b"text/html"));
        if html && response.status() == StatusCode::BAD_REQUEST {
            *response.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
        }
        toast(response, Kind::Error, text)
    }

    /// Powiadomienie, gdy błędne pola są oznaczone w karcie (`aria-invalid`);
    /// formularze z jednym komunikatem błędu pokazują w powiadomieniu jego.
    pub const FIX_FIELDS: &str = "Popraw zaznaczone pola";

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_toast_header_is_ascii_json() {
            let response = toast(Response::default(), Kind::Error, "zła \"wartość\"\n");
            let value = response.headers().get("HX-Trigger").unwrap().to_str().unwrap();
            assert_eq!(
                value,
                r#"{"toast":{"kind":"error","text":"z\u0142a \"warto\u015b\u0107\"\u000a"}}"#
            );
            let parsed: serde_json::Value = serde_json::from_str(value).unwrap();
            assert_eq!(parsed["toast"]["text"], "zła \"wartość\"\n");
        }

        #[test]
        fn a_failed_refresh_after_a_save_is_not_reported_as_saved() {
            let failed = super::super::db_error("test", &"x");
            let response = ok(failed, "Zapisano");
            let value = response.headers().get("HX-Trigger").unwrap().to_str().unwrap();
            assert!(value.contains("\"kind\":\"error\""), "{value}");
            assert!(!value.contains("Zapisano"), "{value}");
        }

        #[test]
        fn boosted_and_history_requests_are_not_fragments() {
            let mut headers = HeaderMap::new();
            assert!(!is_htmx(&headers));
            headers.insert("HX-Request", HeaderValue::from_static("true"));
            assert!(is_htmx(&headers));
            headers.insert("HX-Boosted", HeaderValue::from_static("true"));
            assert!(!is_htmx(&headers));
            headers.remove("HX-Boosted");
            headers.insert("HX-History-Restore-Request", HeaderValue::from_static("true"));
            assert!(!is_htmx(&headers));
        }
    }
}
