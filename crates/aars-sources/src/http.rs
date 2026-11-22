//! Pobieranie z limitem rozmiaru. Błędy sieci to wartości, nie panika.

use std::time::Duration;

use crate::source::SourceError;

/// Klient HTTP źródeł: limit czasu i User-Agent `aars/{wersja}`, nic więcej.
pub fn client(timeout: Duration) -> Result<reqwest::Client, SourceError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("aars/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| SourceError::Http(e.to_string()))
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
        .map_err(|e| SourceError::Http(e.to_string()))?;
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
        .map_err(|e| SourceError::Http(e.to_string()))?
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
