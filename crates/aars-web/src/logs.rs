//! Dziennik nadań i dziennik audytu.

use aars_core::txlog::{TxLogRepo, TxLogRow};
use askama::Template;
use axum::extract::State;
use axum::response::Response;
use chrono::{DateTime, Utc};

use crate::auth::{AuthRejection, CurrentUser};
use crate::chrome::{Chrome, both_times_now};
use crate::render::page;
use crate::state::AppState;

#[derive(Template)]
#[template(path = "logs.html")]
struct LogsPage {
    chrome: Chrome,
    transmissions: Vec<TxRow>,
    audit: Vec<AuditRow>,
}

struct TxRow {
    at: String,
    result: String,
    seconds: String,
    text: String,
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

fn tx_row(row: TxLogRow) -> TxRow {
    TxRow {
        at: both_times_now(row.started_at),
        result: result_label(&row.result).to_owned(),
        seconds: format!("{:.1}", row.duration_ms as f64 / 1000.0),
        text: row.text.unwrap_or_default(),
    }
}

async fn transmissions(state: &AppState) -> Vec<TxRow> {
    match TxLogRepo::new(state.pool.clone()).recent(LIMIT).await {
        Ok(rows) => rows.into_iter().map(tx_row).collect(),
        Err(err) => {
            tracing::error!(%err, "odczyt dziennika nadań zawiódł");
            Vec::new()
        }
    }
}

async fn audit_rows(state: &AppState) -> Vec<AuditRow> {
    let rows: Result<Vec<AuditDbRow>, _> = sqlx::query_as(
        "SELECT a.at, u.username, a.action, a.detail \
         FROM audit_log a LEFT JOIN users u ON u.id = a.user_id \
         ORDER BY a.at DESC, a.id DESC LIMIT ?",
    )
    .bind(LIMIT)
    .fetch_all(&state.pool)
    .await;

    match rows {
        Ok(rows) => rows
            .into_iter()
            .map(|(at, user, action, detail)| AuditRow {
                at: DateTime::parse_from_rfc3339(&at)
                    .map(|dt| both_times_now(dt.with_timezone(&Utc)))
                    .unwrap_or(at),
                user: user.unwrap_or_else(|| "—".to_owned()),
                action,
                detail: detail.unwrap_or_default(),
            })
            .collect(),
        Err(err) => {
            tracing::error!(%err, "odczyt dziennika audytu zawiódł");
            Vec::new()
        }
    }
}

pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    Ok(page(&LogsPage {
        chrome: Chrome::from(&current),
        transmissions: transmissions(&state).await,
        audit: audit_rows(&state).await,
    }))
}
