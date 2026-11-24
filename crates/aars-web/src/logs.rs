//! Dziennik nadań i dziennik audytu.

use aars_core::txlog::{TxLogRepo, TxLogRow};
use askama::Template;
use axum::extract::State;
use axum::response::Response;
use chrono::{DateTime, Utc};

use crate::auth::{AuthRejection, CurrentUser};
use crate::chrome::{Chrome, audit_action_label, both_times_now};
use crate::render::{db_error, page};
use crate::state::AppState;

#[derive(Template)]
#[template(path = "logs.html")]
struct LogsPage {
    chrome: Chrome,
    transmissions: Vec<TxRow>,
    audit: Vec<AuditRow>,
}

pub(crate) struct TxRow {
    pub(crate) at: String,
    pub(crate) result: String,
    pub(crate) seconds: String,
    pub(crate) text: String,
    /// Wiersz bez zadania z kolejki (test PTT): `text` to notka.
    pub(crate) without_item: bool,
}

struct AuditRow {
    at: String,
    user: String,
    action: String,
    detail: String,
}

/// Czas, użytkownik (po `LEFT JOIN`), akcja, szczegóły.
type AuditDbRow = (String, Option<String>, String, Option<String>);

/// Ile ostatnich wpisów pokazuje każda z tabel.
const LIMIT: i64 = 100;

fn result_label(result: &str) -> &str {
    match result {
        "ok" => "nadane",
        "aborted" => "przerwane",
        "failed" => "błąd",
        "timeout" => "limit czasu",
        other => other,
    }
}

pub(crate) fn tx_row(row: TxLogRow) -> TxRow {
    let without_item = row.queue_item_id.is_none();
    // Nadanie spoza kolejki (test PTT) nie ma treści: pokazujemy notkę.
    let text = if without_item { row.note } else { row.text };
    TxRow {
        at: both_times_now(row.started_at),
        result: result_label(&row.result).to_owned(),
        seconds: format!("{:.1}", row.duration_ms as f64 / 1000.0),
        text: aars_audio::pause::display(&text.unwrap_or_default()),
        without_item,
    }
}

async fn transmissions(state: &AppState) -> Result<Vec<TxRow>, Response> {
    match TxLogRepo::new(state.pool.clone()).recent(LIMIT).await {
        Ok(rows) => Ok(rows.into_iter().map(tx_row).collect()),
        Err(err) => Err(db_error("odczyt dziennika nadań zawiódł", &err)),
    }
}

async fn audit_rows(state: &AppState) -> Result<Vec<AuditRow>, Response> {
    let rows: Result<Vec<AuditDbRow>, _> = sqlx::query_as(
        "SELECT a.at, u.username, a.action, a.detail \
         FROM audit_log a LEFT JOIN users u ON u.id = a.user_id \
         ORDER BY a.at DESC, a.id DESC LIMIT ?",
    )
    .bind(LIMIT)
    .fetch_all(&state.pool)
    .await;

    match rows {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|(at, user, action, detail)| AuditRow {
                at: DateTime::parse_from_rfc3339(&at)
                    .map(|dt| both_times_now(dt.with_timezone(&Utc)))
                    .unwrap_or(at),
                user: user.unwrap_or_else(|| "—".to_owned()),
                action: audit_action_label(&action).to_owned(),
                detail: detail.unwrap_or_default(),
            })
            .collect()),
        Err(err) => Err(db_error("odczyt dziennika audytu zawiódł", &err)),
    }
}

pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    let transmissions = match transmissions(&state).await {
        Ok(rows) => rows,
        Err(response) => return Ok(response),
    };
    let audit = match audit_rows(&state).await {
        Ok(rows) => rows,
        Err(response) => return Ok(response),
    };
    Ok(page(&LogsPage {
        chrome: Chrome::load(&state, &current).await,
        transmissions,
        audit,
    }))
}
