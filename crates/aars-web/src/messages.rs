//! Definicje komunikatów z harmonogramem: lista, formularz, włączanie,
//! usuwanie i nadanie na żądanie.

use std::str::FromStr;
use std::time::Duration;

use aars_core::message::{
    MAX_REPEAT_INTERVAL_S, Message, MessageRepo, NewMessage, REPEAT_INTERVAL_ERROR, ScheduleKind,
};
use aars_core::queue::EnqueueRequest;
use aars_core::user::Role;
use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use croner::Cron;
use serde::Deserialize;

use crate::audit;
use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::chrome::{Chrome, both_times_now, parse_priority, priority_label, priority_value};
use crate::render::page;
use crate::state::AppState;

const AT_FORMAT: &str = "%Y-%m-%dT%H:%M";

/// Składa harmonogram z pól formularza. `at` jest czasem lokalnym w strefie
/// `local` i trafia do bazy jako UTC; `cron` liczy się w UTC.
pub fn parse_schedule<Tz: TimeZone>(
    kind: &str,
    at: &str,
    cron: &str,
    now: DateTime<Utc>,
    local: &Tz,
) -> Result<ScheduleKind, String> {
    match kind {
        "immediate" => Ok(ScheduleKind::Immediate),
        "at" => {
            let naive = NaiveDateTime::parse_from_str(at.trim(), AT_FORMAT)
                .map_err(|_| "podaj datę i godzinę jednorazowego nadania".to_owned())?;
            // Przy zmianie czasu lokalna chwila bywa nieistniejąca albo
            // dwuznaczna; nie zgadujemy, którą operator miał na myśli.
            let at = local
                .from_local_datetime(&naive)
                .single()
                .ok_or("ta chwila nie istnieje lub jest dwuznaczna (zmiana czasu)")?
                .with_timezone(&Utc);
            if at <= now {
                return Err("ta chwila już minęła".to_owned());
            }
            Ok(ScheduleKind::At(at))
        }
        "cron" => {
            let expr = cron.trim();
            Cron::from_str(expr).map_err(|e| format!("wzorzec „{expr}”: {e}"))?;
            Ok(ScheduleKind::Cron(expr.to_owned()))
        }
        other => Err(format!("nieznany rodzaj harmonogramu „{other}”")),
    }
}

/// Najbliższe nadanie definicji po `now`; `None` dla „na żądanie”, minionego
/// `at` i niepoprawnego crona.
pub fn next_fire(schedule: &ScheduleKind, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match schedule {
        ScheduleKind::Immediate => None,
        ScheduleKind::At(at) => (*at > now).then_some(*at),
        ScheduleKind::Cron(expr) => Cron::from_str(expr)
            .ok()?
            .find_next_occurrence(&now, false)
            .ok(),
    }
}

fn schedule_label(schedule: &ScheduleKind) -> String {
    match schedule {
        ScheduleKind::Immediate => "na żądanie".to_owned(),
        ScheduleKind::At(_) => "jednorazowo".to_owned(),
        ScheduleKind::Cron(expr) => format!("cron {expr} (UTC)"),
    }
}

struct MessageRow {
    id: i64,
    title: String,
    priority: String,
    schedule: String,
    next: String,
    enabled: bool,
}

fn message_row(message: &Message, now: DateTime<Utc>) -> MessageRow {
    MessageRow {
        id: message.id,
        title: message.title.clone(),
        priority: priority_label(message.priority).to_owned(),
        schedule: schedule_label(&message.schedule),
        // Wyłączona definicja nie odpali, więc nie ma „następnego nadania”.
        next: next_fire(&message.schedule, now)
            .filter(|_| message.enabled)
            .map(both_times_now)
            .unwrap_or_else(|| "—".to_owned()),
        enabled: message.enabled,
    }
}

#[derive(Template)]
#[template(path = "messages.html")]
struct MessagesPage {
    chrome: Chrome,
    rows: Vec<MessageRow>,
}

#[derive(Template)]
#[template(path = "message_form.html")]
struct MessageFormPage {
    chrome: Chrome,
    /// `None` przy nowej definicji.
    id: Option<i64>,
    title: String,
    text: String,
    priority: &'static str,
    interrupting: bool,
    repeat: u32,
    every: u64,
    kind: &'static str,
    at: String,
    cron: String,
}

fn form_page(chrome: Chrome, id: Option<i64>, message: Option<&Message>) -> MessageFormPage {
    let (kind, at, cron) = match message.map(|m| &m.schedule) {
        Some(ScheduleKind::At(at)) => (
            "at",
            at.with_timezone(&Local).format(AT_FORMAT).to_string(),
            String::new(),
        ),
        Some(ScheduleKind::Cron(expr)) => ("cron", String::new(), expr.clone()),
        _ => ("immediate", String::new(), String::new()),
    };
    MessageFormPage {
        chrome,
        id,
        title: message.map(|m| m.title.clone()).unwrap_or_default(),
        text: message.map(|m| m.body_template.clone()).unwrap_or_default(),
        priority: message.map_or("normal", |m| priority_value(m.priority)),
        interrupting: message.is_some_and(|m| m.interrupting),
        repeat: message.map_or(1, |m| m.repeat_count),
        every: message.map_or(600, |m| m.repeat_interval.as_secs()),
        kind,
        at,
        cron,
    }
}

#[derive(Debug, Deserialize)]
pub struct MessageForm {
    csrf: String,
    title: String,
    text: String,
    priority: String,
    #[serde(default)]
    interrupting: Option<String>,
    #[serde(default = "one")]
    repeat: u32,
    #[serde(default)]
    every: u64,
    schedule_kind: String,
    #[serde(default)]
    at: String,
    #[serde(default)]
    cron: String,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct EnabledForm {
    csrf: String,
    enabled: String,
}

fn repo(state: &AppState) -> MessageRepo {
    MessageRepo::new(state.pool.clone())
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, message.to_owned()).into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "nie ma takiego komunikatu").into_response()
}

fn server_error(what: &str, err: &dyn std::fmt::Display) -> Response {
    tracing::error!(%err, "{what}");
    (StatusCode::INTERNAL_SERVER_ERROR, "błąd bazy danych").into_response()
}

/// Waliduje formularz i składa definicję; błąd to komunikat dla operatora.
fn build_message(form: &MessageForm, user_id: i64) -> Result<NewMessage, String> {
    let title = form.title.trim().to_owned();
    if title.is_empty() {
        return Err("tytuł nie może być pusty".to_owned());
    }
    let text = form.text.trim().to_owned();
    if text.is_empty() {
        // Puste nadanie to wejście na pasmo z ciszą — odmawiamy.
        return Err("treść komunikatu nie może być pusta".to_owned());
    }
    let priority = parse_priority(&form.priority).ok_or("nieznany priorytet")?;
    if form.every > MAX_REPEAT_INTERVAL_S {
        return Err(REPEAT_INTERVAL_ERROR.to_owned());
    }
    let schedule = parse_schedule(
        &form.schedule_kind,
        &form.at,
        &form.cron,
        Utc::now(),
        &Local,
    )?;

    Ok(NewMessage {
        title,
        body_template: text,
        priority,
        interrupting: form.interrupting.is_some(),
        repeat_count: form.repeat.clamp(1, 20),
        repeat_interval: Duration::from_secs(form.every),
        schedule,
        valid_from: None,
        valid_until: None,
        source_id: None,
        created_by: Some(user_id),
    })
}

pub async fn list(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    let messages = match repo(&state).list().await {
        Ok(messages) => messages,
        Err(err) => return Ok(server_error("odczyt definicji zawiódł", &err)),
    };
    let now = Utc::now();
    Ok(page(&MessagesPage {
        chrome: Chrome::load(&state, &current).await,
        rows: messages.iter().map(|m| message_row(m, now)).collect(),
    }))
}

pub async fn new_page(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    Ok(page(&form_page(Chrome::load(&state, &current).await, None, None)))
}

pub async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<MessageForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let new = match build_message(&form, current.user.id) {
        Ok(new) => new,
        Err(message) => return Ok(bad_request(&message)),
    };
    let title = new.title.clone();
    match repo(&state).create(new).await {
        Ok(_) => {
            audit(&state, current.user.id, "message_create", &title).await;
            Ok(Redirect::to("/messages").into_response())
        }
        Err(err) => Ok(server_error("zapis definicji zawiódł", &err)),
    }
}

pub async fn edit_page(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    match repo(&state).get(id).await {
        Ok(Some(message)) => Ok(page(&form_page(
            Chrome::load(&state, &current).await,
            Some(id),
            Some(&message),
        ))),
        Ok(None) => Ok(not_found()),
        Err(err) => Ok(server_error("odczyt definicji zawiódł", &err)),
    }
}

pub async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<MessageForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let new = match build_message(&form, current.user.id) {
        Ok(new) => new,
        Err(message) => return Ok(bad_request(&message)),
    };
    let title = new.title.clone();
    match repo(&state).update(id, new).await {
        Ok(0) => Ok(not_found()),
        Ok(_) => {
            audit(&state, current.user.id, "message_update", &title).await;
            Ok(Redirect::to("/messages").into_response())
        }
        Err(err) => Ok(server_error("zmiana definicji zawiodła", &err)),
    }
}

pub async fn set_enabled(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<EnabledForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let enabled = matches!(form.enabled.as_str(), "1" | "true" | "on");
    let message = match repo(&state).get(id).await {
        Ok(Some(message)) => message,
        Ok(None) => return Ok(not_found()),
        Err(err) => return Ok(server_error("odczyt definicji zawiódł", &err)),
    };
    if let Err(err) = repo(&state).set_enabled(id, enabled).await {
        return Ok(server_error("zmiana stanu definicji zawiodła", &err));
    }
    let detail = format!(
        "{} — {}",
        message.title,
        if enabled { "włączony" } else { "wyłączony" }
    );
    audit(&state, current.user.id, "message_enabled", &detail).await;
    Ok(Redirect::to("/messages").into_response())
}

pub async fn delete(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let title = match repo(&state).get(id).await {
        Ok(Some(message)) => message.title,
        Ok(None) => return Ok(not_found()),
        Err(err) => return Ok(server_error("odczyt definicji zawiódł", &err)),
    };
    if let Err(err) = repo(&state).delete(id).await {
        return Ok(server_error("usunięcie definicji zawiodło", &err));
    }
    audit(&state, current.user.id, "message_delete", &title).await;
    Ok(Redirect::to("/messages").into_response())
}

/// Nadanie na żądanie: definicja trafia do kolejki teraz, z własnym
/// priorytetem, przerywaniem i powtórzeniami.
pub async fn send(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let message = match repo(&state).get(id).await {
        Ok(Some(message)) => message,
        Ok(None) => return Ok(not_found()),
        Err(err) => return Ok(server_error("odczyt definicji zawiódł", &err)),
    };
    let request = EnqueueRequest {
        message_id: Some(id),
        rendered_text: message.body_template.clone(),
        priority: message.priority,
        interrupting: message.interrupting,
        not_before: None,
        expires_at: message.valid_until,
        repeat_count: message.repeat_count,
        repeat_interval: message.repeat_interval,
        created_by: Some(current.user.id),
    };
    match state.queue.enqueue(request).await {
        Ok(_) => {
            audit(&state, current.user.id, "message_send", &message.title).await;
            Ok(Redirect::to("/queue").into_response())
        }
        Err(err) => Ok(server_error("dodanie do kolejki zawiodło", &err)),
    }
}

#[derive(Debug, Deserialize)]
pub struct NextQuery {
    #[serde(default)]
    schedule_kind: String,
    #[serde(default)]
    at: String,
    #[serde(default)]
    cron: String,
}

/// Podpowiedź pod formularzem: odpowiednik UTC i najbliższe nadanie.
pub async fn next_hint(
    current: CurrentUser,
    Query(query): Query<NextQuery>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    let now = Utc::now();
    let text = match parse_schedule(&query.schedule_kind, &query.at, &query.cron, now, &Local) {
        Ok(ScheduleKind::Immediate) => "Nadawane tylko na żądanie.".to_owned(),
        Ok(ScheduleKind::At(at)) => format!("Nadanie: {}", both_times_now(at)),
        Ok(schedule @ ScheduleKind::Cron(_)) => match next_fire(&schedule, now) {
            Some(at) => format!("Najbliższe nadanie: {}", both_times_now(at)),
            None => "Ten wzorzec nie ma przyszłego nadania.".to_owned(),
        },
        Err(err) => err,
    };
    Ok(text.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn at(iso: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn utc_plus_one() -> FixedOffset {
        FixedOffset::east_opt(3600).unwrap()
    }

    #[test]
    fn a_valid_cron_is_accepted() {
        let now = at("2026-11-20T10:00:00Z");
        assert_eq!(
            parse_schedule("cron", "", "0 18 * * *", now, &Utc),
            Ok(ScheduleKind::Cron("0 18 * * *".to_owned()))
        );
    }

    #[test]
    fn an_invalid_cron_is_refused_with_the_pattern_in_the_message() {
        let now = at("2026-11-20T10:00:00Z");
        let err = parse_schedule("cron", "", "0 25 * * *", now, &Utc).unwrap_err();
        assert!(err.contains("0 25 * * *"), "{err}");
    }

    #[test]
    fn a_local_at_time_is_converted_to_utc() {
        let now = at("2026-11-20T10:00:00Z");
        assert_eq!(
            parse_schedule("at", "2026-12-01T19:00", "", now, &utc_plus_one()),
            Ok(ScheduleKind::At(at("2026-12-01T18:00:00Z")))
        );
    }

    #[test]
    fn an_at_time_in_the_past_is_refused() {
        let now = at("2026-12-02T10:00:00Z");
        assert_eq!(
            parse_schedule("at", "2026-12-01T19:00", "", now, &utc_plus_one()),
            Err("ta chwila już minęła".to_owned())
        );
    }

    #[test]
    fn a_malformed_at_time_is_refused() {
        let now = at("2026-11-20T10:00:00Z");
        assert!(parse_schedule("at", "jutro", "", now, &Utc).is_err());
    }

    #[test]
    fn immediate_needs_nothing_and_unknown_kinds_are_refused() {
        let now = at("2026-11-20T10:00:00Z");
        assert_eq!(
            parse_schedule("immediate", "", "", now, &Utc),
            Ok(ScheduleKind::Immediate)
        );
        assert!(parse_schedule("co-tydzien", "", "", now, &Utc).is_err());
    }

    #[test]
    fn a_disabled_definition_shows_no_next_fire() {
        let now = at("2026-11-20T10:00:00Z");
        let mut message = Message {
            id: 1,
            title: "biuletyn".to_owned(),
            body_template: "treść".to_owned(),
            priority: aars_core::priority::Priority::Normal,
            interrupting: false,
            repeat_count: 1,
            repeat_interval: Duration::ZERO,
            schedule: ScheduleKind::Cron("0 18 * * *".to_owned()),
            valid_from: None,
            valid_until: None,
            source_id: None,
            enabled: true,
        };
        assert_ne!(message_row(&message, now).next, "—");
        message.enabled = false;
        assert_eq!(message_row(&message, now).next, "—");
    }

    #[test]
    fn next_fire_follows_the_cron_in_utc() {
        let now = at("2026-11-20T10:00:00Z");
        assert_eq!(
            next_fire(&ScheduleKind::Cron("0 18 * * *".to_owned()), now),
            Some(at("2026-11-20T18:00:00Z"))
        );
    }

    #[test]
    fn next_fire_is_none_for_on_demand_and_past_one_shots() {
        let now = at("2026-11-20T10:00:00Z");
        assert_eq!(next_fire(&ScheduleKind::Immediate, now), None);
        assert_eq!(
            next_fire(&ScheduleKind::At(at("2026-11-19T10:00:00Z")), now),
            None
        );
        assert_eq!(
            next_fire(&ScheduleKind::At(at("2026-11-21T10:00:00Z")), now),
            Some(at("2026-11-21T10:00:00Z"))
        );
    }
}
