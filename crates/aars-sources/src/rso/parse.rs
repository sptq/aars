//! Parsowanie odpowiedzi RSO. Dane z sieci: bez `unwrap`, `expect` i indeksowania.

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;

use crate::source::SourceError;
use crate::speech::warsaw_to_utc;

#[derive(Debug, Clone, PartialEq)]
pub struct News {
    pub id: i64,
    pub title: String,
    pub shortcut: String,
    pub content: String,
    pub valid_until: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub province_slugs: Vec<String>,
}

/// Czas polski `YYYY-MM-DD HH:MM:SS`; brak albo śmieci to `None`.
fn local_time(v: &Value, key: &str) -> Option<DateTime<Utc>> {
    let s = v.get(key)?.as_str()?;
    let t = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    warsaw_to_utc(t)
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// `provinces` to obiekt kluczowany id (pusty bywa `[]`); bierzemy `slug_name`.
fn province_slugs(v: &Value) -> Vec<String> {
    let entries: Vec<&Value> = match v.get("provinces") {
        Some(Value::Object(map)) => map.values().collect(),
        Some(Value::Array(list)) => list.iter().collect(),
        _ => Vec::new(),
    };
    entries
        .into_iter()
        .filter_map(|p| p.get("slug_name")?.as_str())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_item(v: &Value) -> Option<News> {
    let id = match v.get("id")? {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    Some(News {
        id,
        title: text(v, "title"),
        shortcut: text(v, "shortcut"),
        content: text(v, "content"),
        valid_until: local_time(v, "valid_to"),
        updated_at: local_time(v, "updated_at"),
        province_slugs: province_slugs(v),
    })
}

/// Pozycje odpowiedzi i liczba pominiętych (bez czytelnego `id`).
pub fn parse_news(body: &[u8]) -> Result<(Vec<News>, usize), SourceError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    let list = value
        .get("newses")
        .and_then(Value::as_array)
        .ok_or_else(|| SourceError::Parse("brak listy komunikatów RSO".to_owned()))?;
    let items: Vec<News> = list.iter().filter_map(parse_item).collect();
    let skipped = list.len() - items.len();
    Ok((items, skipped))
}
