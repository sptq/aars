//! Pełna kolejka z anulowaniem pojedynczego wystąpienia lub całej serii.

use aars_core::user::Role;
use askama::Template;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::Response;

use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::chrome::Chrome;
use crate::render::htmx::{Kind, toast};
use crate::render::page;
use crate::state::AppState;
use crate::{QueueRow, audit, queue_fragment_response, queue_rows};

#[derive(Template)]
#[template(path = "queue.html")]
struct QueuePage {
    chrome: Chrome,
    pending: Vec<QueueRow>,
    full: bool,
}

pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    let pending = match queue_rows(&state, 200).await {
        Ok(rows) => rows,
        Err(err) => return Ok(crate::render::db_error("odczyt kolejki zawiódł", &err)),
    };
    Ok(page(&QueuePage {
        chrome: Chrome::load(&state, &current).await,
        pending,
        full: true,
    }))
}

pub async fn cancel_group(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: header::HeaderMap,
    Path(group): Path<String>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, headers.get("x-csrf-token").and_then(|v| v.to_str().ok()))?;

    let outcome = match state.queue.cancel_group(&group).await {
        Ok(count) => {
            tracing::info!(user = %current.user.username, %group, count, "anulowanie serii z panelu");
            audit(
                &state,
                current.user.id,
                "queue_cancel_group",
                &format!("seria {group}, anulowano {count}"),
            )
            .await;
            (Kind::Ok, format!("Anulowano powtórzenia serii: {count}"))
        }
        Err(err) => {
            tracing::error!(%err, %group, "anulowanie serii zawiodło");
            (Kind::Error, "Anulowanie serii zawiodło — szczegóły w dzienniku stacji".to_owned())
        }
    };

    let response = queue_fragment_response(&state, &current, true).await;
    Ok(toast(response, outcome.0, &outcome.1))
}
