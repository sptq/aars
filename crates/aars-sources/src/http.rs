//! Pobieranie z limitem rozmiaru. Błędy sieci to wartości, nie panika.

use std::time::Duration;

use crate::source::SourceError;

/// User-Agent dla MET Norway (wymaga identyfikacji aplikacji i kontaktu); bez danych osobowych.
pub const MET_USER_AGENT: &str = concat!("aars/", env!("CARGO_PKG_VERSION"), " github.com/sptq/aars");

/// Klient HTTP źródeł: limit czasu i User-Agent `aars/{wersja}`, nic więcej.
pub fn client(timeout: Duration) -> Result<reqwest::Client, SourceError> {
    client_with_agent(timeout, concat!("aars/", env!("CARGO_PKG_VERSION")))
}

/// Jak `client`, z własnym User-Agentem.
pub fn client_with_agent(timeout: Duration, agent: &str) -> Result<reqwest::Client, SourceError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(agent)
        .build()
        .map_err(|e| SourceError::Http(describe(&e)))
}

/// Podpowiedź przy błędzie certyfikatu: na Pi bez zegara RTC to zwykle zły czas.
const CLOCK_HINT: &str = "sprawdź zegar stacji (zła data psuje certyfikaty)";
const DNS_HINT: &str = "sprawdź internet i DNS na stacji";

/// Błąd sieci po polsku, z serwerem i przyczyną z łańcucha błędów: z samego
/// „error sending request” operator nie wyczyta, co poszło nie tak.
pub fn describe(err: &reqwest::Error) -> String {
    let what = if err.is_timeout() {
        "serwer nie odpowiedział w wyznaczonym czasie"
    } else if err.is_connect() {
        "nie można połączyć się z serwerem"
    } else if err.is_body() || err.is_decode() {
        "przerwane pobieranie odpowiedzi"
    } else if err.is_builder() {
        "nie można przygotować zapytania"
    } else {
        "błąd połączenia"
    };
    let mut text = what.to_owned();
    if let Some(host) = err.url().and_then(|u| u.host_str()) {
        text.push_str(&format!(" ({host})"));
    }
    let mut causes: Vec<String> = Vec::new();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !cause_text.is_empty() && !causes.iter().any(|c| c.contains(&cause_text)) {
            causes.push(cause_text);
        }
        source = cause.source();
    }
    if !causes.is_empty() {
        text.push_str(": ");
        text.push_str(&causes.join(": "));
    }
    let lower = text.to_lowercase();
    if lower.contains("certificate") {
        text.push_str(&format!(" — {CLOCK_HINT}"));
    } else if lower.contains("dns") || lower.contains("lookup") {
        text.push_str(&format!(" — {DNS_HINT}"));
    }
    text
}

/// Pobiera `url` (może mieć zapytanie); więcej niż `max` bajtów to `TooLarge` (sprawdzane z
/// `Content-Length` i po sumie kawałków).
pub async fn get_limited(
    client: &reqwest::Client,
    url: &str,
    max: usize,
) -> Result<Vec<u8>, SourceError> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| SourceError::Http(describe(&e)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(SourceError::Status(status.as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        return Err(SourceError::TooLarge(max));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| SourceError::Http(describe(&e)))?
    {
        if body.len().saturating_add(chunk.len()) > max {
            return Err(SourceError::TooLarge(max));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_refused_connection_names_the_server_in_polish() {
        // Port, na którym nikt nie słucha.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let c = client(Duration::from_secs(5)).unwrap();
        let err = get_limited(&c, &format!("http://127.0.0.1:{port}/x"), 10).await.unwrap_err();
        let text = err.to_string();
        assert!(text.starts_with("nie można połączyć się z serwerem (127.0.0.1)"), "{text}");
        assert!(!text.contains("error sending request"), "{text}");
    }

    #[tokio::test]
    async fn a_slow_server_is_reported_as_a_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
            .mount(&server)
            .await;
        let c = client(Duration::from_millis(200)).unwrap();
        let err = get_limited(&c, &server.uri(), 10).await.unwrap_err();
        assert!(
            err.to_string().starts_with("serwer nie odpowiedział w wyznaczonym czasie (127.0.0.1)"),
            "{err}"
        );
    }
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn the_client_sends_the_aars_user_agent_and_keeps_the_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api"))
            .and(query_param("page", "1"))
            .and(header("user-agent", concat!("aars/", env!("CARGO_PKG_VERSION"))))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let c = client(Duration::from_secs(5)).unwrap();
        let body = get_limited(&c, &format!("{}/api?page=1", server.uri()), 10).await.unwrap();
        assert_eq!(body, b"ok");
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_too_large() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("0123456789"))
            .mount(&server)
            .await;
        let c = client(Duration::from_secs(5)).unwrap();
        let err = get_limited(&c, &server.uri(), 5).await.unwrap_err();
        assert!(matches!(err, SourceError::TooLarge(5)));
    }
}
