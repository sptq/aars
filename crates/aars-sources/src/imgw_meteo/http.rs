//! Pobieranie z limitem rozmiaru. Błędy sieci to wartości, nie panika.

use crate::source::SourceError;

/// Pobiera `url`; więcej niż `max` bajtów to `TooLarge` (sprawdzane z
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
