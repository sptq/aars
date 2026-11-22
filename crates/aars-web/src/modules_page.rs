//! Strona „Moduły” (etap 9, R13–R15): stan każdego modułu z rejestru, a dla
//! administratora włączanie i formularz ustawień generowany ze schematu.
//! Operator widzi tylko stan. Panel nie zna crate'a źródeł — źródło budzi
//! `AppState::source_refresh`.

use std::collections::BTreeMap;

use aars_core::error::DbError;
use aars_core::geo::PositionRepo;
use aars_core::module::{
    BULLETIN, ModuleDescriptor, ModuleError, ModuleKind, ModuleRepo, SettingKind, Toggle,
};
use aars_core::settings::SettingsRepo;
use aars_core::source_state::SourceStateRepo;
use aars_core::user::Role;
use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;

use crate::audit;
use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::chrome::Chrome;
use crate::render::{db_error, page};
use crate::sources_page::health;
use crate::state::{AppState, BulletinPreviewView};

/// Pole formularza ustawień wygenerowane z `SettingSpec`.
pub(crate) struct FieldView {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) help: &'static str,
    /// `text` | `integer` | `area` | `choice` | `decimal` | `list`
    pub(crate) kind: &'static str,
    pub(crate) value: String,
    pub(crate) max_len: usize,
    pub(crate) min: i64,
    pub(crate) max: i64,
    /// Granice `decimal` już z przecinkiem, np. `1,0`.
    pub(crate) min_text: String,
    pub(crate) max_text: String,
    /// Dla `list`: (wartość, etykieta, zaznaczona).
    pub(crate) checks: Vec<(&'static str, &'static str, bool)>,
    /// (wartość, etykieta)
    pub(crate) options: Vec<(&'static str, &'static str)>,
}

pub(crate) struct ModuleView {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) description: &'static str,
    pub(crate) css: &'static str,
    pub(crate) state: String,
    pub(crate) switchable: bool,
    pub(crate) enabled: bool,
    pub(crate) is_source: bool,
    /// Plakietka zdrowia źródła z etapu 8: (klasa CSS, tekst).
    pub(crate) health: Option<(&'static str, String)>,
    pub(crate) see_also: Option<(&'static str, &'static str)>,
    pub(crate) fields: Vec<FieldView>,
    /// Tylko `auto-id`: bieżący znak (pusty = brak).
    pub(crate) ident: Option<String>,
    pub(crate) error: Option<String>,
    /// Tylko `bulletin`, po `POST /modules/bulletin/preview`.
    pub(crate) bulletin_preview: Option<BulletinPreviewView>,
}

#[derive(Template)]
#[template(path = "modules.html")]
struct ModulesPage {
    chrome: Chrome,
    modules: Vec<ModuleView>,
}

fn kind_name(kind: SettingKind) -> &'static str {
    match kind {
        SettingKind::Text { .. } => "text",
        SettingKind::Integer { .. } => "integer",
        SettingKind::Area => "area",
        SettingKind::Choice(_) => "choice",
        SettingKind::Decimal { .. } => "decimal",
        SettingKind::List(_) => "list",
        SettingKind::TimeList { .. } => "timelist",
    }
}

fn field_view(spec: &aars_core::module::SettingSpec, value: String) -> FieldView {
    let (max_len, min, max, options) = match spec.kind {
        SettingKind::Text { max_len } => (max_len, 0, 0, Vec::new()),
        SettingKind::Integer { min, max } => (0, min, max, Vec::new()),
        SettingKind::Area => (0, 0, 0, Vec::new()),
        SettingKind::Choice(options) | SettingKind::List(options) => (0, 0, 0, options.to_vec()),
        SettingKind::Decimal { .. } | SettingKind::TimeList { .. } => (0, 0, 0, Vec::new()),
    };
    let (min_text, max_text) = match spec.kind {
        SettingKind::Decimal { min, max } => (
            format!("{min:.1}").replace('.', ","),
            format!("{max:.1}").replace('.', ","),
        ),
        _ => (String::new(), String::new()),
    };
    // Kanonicznie „3.0”; w formularzu po polsku „3,0” (walidacja przyjmuje oba).
    let value = if matches!(spec.kind, SettingKind::Decimal { .. }) {
        value.replace('.', ",")
    } else {
        value
    };
    let checks = if matches!(spec.kind, SettingKind::List(_)) {
        let chosen: Vec<&str> = value.split(',').map(str::trim).collect();
        options.iter().map(|(v, l)| (*v, *l, chosen.contains(v))).collect()
    } else {
        Vec::new()
    };
    FieldView {
        key: spec.key,
        label: spec.label,
        help: spec.help,
        kind: kind_name(spec.kind),
        value,
        max_len,
        min,
        max,
        min_text,
        max_text,
        checks,
        options,
    }
}

async fn module_view(
    state: &AppState,
    m: &ModuleDescriptor,
    callsign: &str,
) -> Result<ModuleView, DbError> {
    let repo = ModuleRepo::new(state.pool.clone());
    let enabled = repo.is_enabled(m).await?;
    let (css, state_text) = match m.toggle {
        Toggle::Switchable if enabled => ("on", "włączony".to_owned()),
        Toggle::Switchable => ("off", "wyłączony".to_owned()),
        Toggle::AlwaysOn(reason) => ("on", format!("zawsze włączony — {reason}")),
        Toggle::Planned(when) => ("warn", format!("planowany — {when}")),
    };
    let health = if m.kind == ModuleKind::Source {
        let sources = SourceStateRepo::new(state.pool.clone());
        let status = sources
            .statuses()
            .await?
            .into_iter()
            .find(|s| s.source_id == m.id);
        match (status, sources.config(m.id).await?) {
            (Some(status), Some(cfg)) => {
                let has_position = matches!(
                    PositionRepo::new(state.pool.clone()).load().await,
                    Ok(Some(_))
                );
                Some(health(&status, &cfg, m.needs, has_position, Utc::now()))
            }
            _ => None,
        }
    } else {
        None
    };
    let values = repo.settings(m).await?;
    let fields = m
        .settings
        .iter()
        .map(|spec| field_view(spec, values.get(spec.key).cloned().unwrap_or_default()))
        .collect();
    Ok(ModuleView {
        id: m.id,
        name: m.name,
        kind: m.kind.label(),
        description: m.description,
        css,
        state: state_text,
        switchable: m.toggle == Toggle::Switchable,
        enabled,
        is_source: m.kind == ModuleKind::Source,
        health,
        see_also: m.see_also,
        fields,
        ident: (m.kind == ModuleKind::Ident).then(|| callsign.to_owned()),
        error: None,
        bulletin_preview: None,
    })
}

async fn load_views(state: &AppState) -> Result<Vec<ModuleView>, DbError> {
    let callsign = SettingsRepo::new(state.pool.clone()).load().await?.callsign;
    let mut views = Vec::new();
    for m in state.modules.iter() {
        views.push(module_view(state, m, &callsign).await?);
    }
    Ok(views)
}

/// Strona modułów ze statusem `status`; `edit` zmienia kartę modułu `id`.
async fn page_with(
    state: &AppState,
    current: &CurrentUser,
    id: &str,
    status: StatusCode,
    edit: impl FnOnce(&mut ModuleView),
) -> Response {
    let mut modules = match load_views(state).await {
        Ok(views) => views,
        Err(err) => return db_error("odczyt modułów zawiódł", &err),
    };
    if let Some(view) = modules.iter_mut().find(|v| v.id == id) {
        edit(view);
    }
    (
        status,
        page(&ModulesPage {
            chrome: Chrome::from(current),
            modules,
        }),
    )
        .into_response()
}

/// Strona z błędem w karcie modułu `id`; `entered` zastępuje wartości pól.
async fn page_with_error(
    state: &AppState,
    current: &CurrentUser,
    id: &str,
    message: String,
    entered: Option<&BTreeMap<String, String>>,
) -> Response {
    page_with(state, current, id, StatusCode::BAD_REQUEST, |view| {
        if let Some(entered) = entered {
            for field in &mut view.fields {
                if let Some(value) = entered.get(field.key) {
                    field.value = value.clone();
                }
            }
        }
        view.error = Some(message);
    })
    .await
}

/// `GET /modules` (operator: stan; admin: także przełączniki i ustawienia).
pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    match load_views(&state).await {
        Ok(modules) => Ok(page(&ModulesPage {
            chrome: Chrome::from(&current),
            modules,
        })),
        Err(err) => Ok(db_error("odczyt modułów zawiódł", &err)),
    }
}

#[derive(Debug, Deserialize)]
pub struct EnabledForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    enabled: String,
}

fn unknown_module() -> Response {
    (StatusCode::NOT_FOUND, "nie ma takiego modułu").into_response()
}

fn see_other(location: String) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response()
}

/// `POST /modules/{id}/enabled` (admin, R15). Włączone źródło jest od razu
/// budzone; harmonogram zauważa zmianę przy następnym tiku (R3).
pub async fn set_enabled(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<String>,
    Form(form): Form<EnabledForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Admin)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let Some(m) = state.modules.get(&id).copied() else {
        return Ok(unknown_module());
    };
    let enabled = matches!(form.enabled.as_str(), "1" | "true" | "on");
    match ModuleRepo::new(state.pool.clone())
        .set_enabled(&m, enabled)
        .await
    {
        Ok(()) => {}
        Err(err @ (ModuleError::AlwaysOn(_) | ModuleError::Planned(_))) => {
            return Ok(page_with_error(&state, &current, m.id, err.to_string(), None).await);
        }
        Err(ModuleError::Db(err)) => return Ok(db_error("zmiana stanu modułu zawiodła", &err)),
    }
    // Włączenie modułu zawsze włączonego niczego nie zmienia, więc nie ma wpisu.
    if m.toggle == Toggle::Switchable {
        audit(
            &state,
            current.user.id,
            "module_enabled",
            &format!(
                "{}: {}",
                m.id,
                if enabled { "włączony" } else { "wyłączony" }
            ),
        )
        .await;
    }
    if enabled
        && m.kind == ModuleKind::Source
        && let Some(refresh) = &state.source_refresh
    {
        refresh(m.id);
    }
    Ok(see_other("/modules".to_owned()))
}

/// `POST /modules/{id}/settings` (admin). Brane są tylko klucze ze schematu;
/// brak pola znaczy „bez zmiany”.
pub async fn save_settings(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<String>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Admin)?;
    // Pola wyboru listy powtarzają nazwę; powtórzone klucze łączymy przecinkiem.
    let mut form: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in pairs {
        form.entry(key)
            .and_modify(|joined| {
                joined.push(',');
                joined.push_str(&value);
            })
            .or_insert(value);
    }
    require_csrf(&current.csrf_token, form.get("csrf").map(String::as_str))?;

    let Some(m) = state.modules.get(&id).copied() else {
        return Ok(unknown_module());
    };
    if m.settings.is_empty() {
        return Ok(unknown_module());
    }
    let repo = ModuleRepo::new(state.pool.clone());
    let stored = match repo.settings(&m).await {
        Ok(stored) => stored,
        Err(err) => return Ok(db_error("odczyt ustawień modułu zawiódł", &err)),
    };
    let mut input = stored.clone();
    for spec in m.settings {
        if let Some(value) = form.get(spec.key) {
            input.insert(spec.key.to_owned(), value.clone());
        }
    }
    let values = match m.validate(&input) {
        Ok(values) => values,
        Err(message) => {
            return Ok(page_with_error(&state, &current, m.id, message, Some(&input)).await);
        }
    };
    let changed: Vec<&str> = m
        .settings
        .iter()
        .filter(|spec| stored.get(spec.key) != values.get(spec.key))
        .map(|spec| spec.key)
        .collect();
    if changed.is_empty() {
        return Ok(see_other(format!("/modules#{}", m.id)));
    }
    if let Err(err) = repo.save_settings(&m, &values).await {
        return Ok(db_error("zapis ustawień modułu zawiódł", &err));
    }
    audit(
        &state,
        current.user.id,
        "module_settings",
        &format!("{}: {}", m.id, changed.join(", ")),
    )
    .await;
    if m.kind == ModuleKind::Source
        && let Some(refresh) = &state.source_refresh
    {
        refresh(m.id);
    }
    Ok(see_other(format!("/modules#{}", m.id)))
}

async fn bulletin_message(
    state: &AppState,
    current: &CurrentUser,
    status: StatusCode,
    message: String,
) -> Response {
    page_with(state, current, BULLETIN.id, status, |card| {
        card.error = Some(message);
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    #[serde(default)]
    csrf: String,
}

/// `POST /modules/bulletin/preview` (operator): biuletyn złożony na teraz,
/// bez kolejki. Jeden podgląd naraz — każdy pyta sieć o wszystkie części.
pub async fn preview_bulletin(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;
    let Some(preview) = state.bulletin_preview.clone() else {
        return Ok(bulletin_message(
            &state,
            &current,
            StatusCode::SERVICE_UNAVAILABLE,
            "biuletyn nie wystartował — zobacz dziennik systemowy (journalctl -u aars)".to_owned(),
        )
        .await);
    };
    let Ok(_permit) = state.bulletin_preview_gate.clone().try_acquire_owned() else {
        return Ok(bulletin_message(
            &state,
            &current,
            StatusCode::SERVICE_UNAVAILABLE,
            "podgląd biuletynu już trwa, spróbuj za chwilę".to_owned(),
        )
        .await);
    };
    match preview().await {
        Ok(view) => Ok(
            page_with(&state, &current, BULLETIN.id, StatusCode::OK, |card| {
                card.bulletin_preview = Some(view)
            })
            .await,
        ),
        Err(e) => Ok(bulletin_message(
            &state,
            &current,
            StatusCode::BAD_GATEWAY,
            format!("podgląd biuletynu zawiódł: {e}"),
        )
        .await),
    }
}

#[cfg(test)]
mod tests {
    use crate::AppState;
    use crate::router;
    use crate::state::{BulletinPreview, BulletinPreviewView};
    use crate::tests::{login, state};
    use aars_core::source_state::SourceStateRepo;
    use aars_core::user::{Role, UserRepo};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    async fn app_as(role: Role, state: AppState) -> (Router, String, String, AppState) {
        UserRepo::new(state.pool.clone())
            .create("u", "haslo", role)
            .await
            .unwrap();
        let app = router(state.clone());
        let (cookie, csrf) = login(&app, "u", "haslo").await;
        (app, cookie, csrf, state)
    }

    async fn get(app: &Router, cookie: &str, path: &str) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn post(
        app: &Router,
        cookie: &str,
        csrf: &str,
        path: &str,
        fields: &str,
    ) -> (StatusCode, Option<String>, String) {
        let body = format!("csrf={csrf}&{fields}");
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, location, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn encode(text: &str) -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    async fn audit_details(state: &AppState, action: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = ? ORDER BY id")
            .bind(action)
            .fetch_all(&state.pool)
            .await
            .unwrap()
    }

    async fn setting(state: &AppState, key: &str) -> Option<String> {
        sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&state.pool)
            .await
            .unwrap()
    }

    async fn imgw(state: &AppState) -> aars_core::source_state::SourceConfig {
        SourceStateRepo::new(state.pool.clone())
            .config("imgw-meteo")
            .await
            .unwrap()
            .unwrap()
    }

    fn counting_refresh(state: AppState) -> (AppState, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let state = state.with_source_refresh(Arc::new(move |id| {
            assert_eq!(id, "imgw-meteo");
            seen.fetch_add(1, Ordering::SeqCst);
            true
        }));
        (state, calls)
    }

    #[tokio::test]
    async fn an_operator_sees_modules_without_forms() {
        let (app, cookie, _, _) = app_as(Role::Operator, state().await).await;
        let (status, html) = get(&app, &cookie, "/modules").await;
        assert_eq!(status, StatusCode::OK);
        for needle in [
            "Harmonogram",
            "Ostrzeżenia meteorologiczne IMGW",
            "Biuletyn",
            "zawsze włączony — identyfikacja stacji jest obowiązkowa",
            "źródło danych",
            "href=\"/sources\"",
            "href=\"/settings\"",
        ] {
            assert!(html.contains(needle), "brak {needle}: {html}");
        }
        // Operator nie dostaje przełączników ani ustawień — tylko podgląd biuletynu.
        assert_eq!(html.matches("action=\"/modules/").count(), 1, "{html}");
        assert!(
            html.contains("action=\"/modules/bulletin/preview\""),
            "{html}"
        );
        // Kolejność rejestru: wbudowane, potem źródło.
        let scheduler = html.find("Harmonogram").unwrap();
        let source = html.find("Ostrzeżenia meteorologiczne IMGW").unwrap();
        assert!(scheduler < source, "{html}");
    }

    #[tokio::test]
    async fn an_admin_sees_toggles_and_forms_generated_from_the_schema() {
        let (app, cookie, _, _) = app_as(Role::Admin, state().await).await;
        let (_, html) = get(&app, &cookie, "/modules").await;
        for needle in [
            "action=\"/modules/scheduler/enabled\"",
            "action=\"/modules/imgw-meteo/enabled\"",
            "action=\"/modules/imgw-meteo/settings\"",
            "action=\"/modules/bulletin/enabled\"",
            "<textarea",
            "name=\"area\"",
            "name=\"interval_s\" type=\"number\" min=\"60\" max=\"3600\" value=\"300\"",
            "id=\"imgw-meteo\"",
            "Uwaga: bez znaku wywoławczego stacja nadaje bez identyfikacji.",
        ] {
            assert!(html.contains(needle), "brak {needle}: {html}");
        }
        for absent in [
            "action=\"/modules/auto-id/enabled\"",
            "action=\"/modules/scheduler/settings\"",
        ] {
            assert!(!html.contains(absent), "{absent}: {html}");
        }
    }

    #[tokio::test]
    async fn the_ident_card_shows_the_callsign() {
        let (app, cookie, _, state) = app_as(Role::Operator, state().await).await;
        sqlx::query("UPDATE settings SET value = 'SP7TQ' WHERE key = 'callsign'")
            .execute(&state.pool)
            .await
            .unwrap();
        let (_, html) = get(&app, &cookie, "/modules").await;
        assert!(html.contains("SP7TQ"), "{html}");
        assert!(!html.contains("bez znaku wywoławczego"), "{html}");
    }

    #[tokio::test]
    async fn an_admin_disables_the_scheduler_and_it_is_audited() {
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        let (status, location, _) = post(
            &app,
            &cookie,
            &csrf,
            "/modules/scheduler/enabled",
            "enabled=0",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.as_deref(), Some("/modules"));
        assert_eq!(
            setting(&state, "module:scheduler:enabled").await.as_deref(),
            Some("false")
        );
        assert_eq!(
            audit_details(&state, "module_enabled").await,
            vec!["scheduler: wyłączony".to_owned()]
        );
        let (_, html) = get(&app, &cookie, "/modules").await;
        assert!(html.contains("wyłączony"), "{html}");
    }

    #[tokio::test]
    async fn auto_id_cannot_be_disabled() {
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        let (status, _, body) = post(
            &app,
            &cookie,
            &csrf,
            "/modules/auto-id/enabled",
            "enabled=0",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("obowiązkowa"), "{body}");
        assert!(audit_details(&state, "module_enabled").await.is_empty());
    }

    #[tokio::test]
    async fn the_bulletin_can_be_enabled() {
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        let (status, _, _) = post(&app, &cookie, &csrf, "/modules/bulletin/enabled", "enabled=1").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(audit_details(&state, "module_enabled").await, vec!["bulletin: włączony".to_owned()]);
        let (_, html) = get(&app, &cookie, "/modules").await;
        assert!(html.contains("Pory nadania"), "{html}");
        assert!(html.contains("value=\"07:00,19:00\""), "{html}");
    }

    #[tokio::test]
    async fn enabling_a_source_module_writes_source_state_and_wakes_it() {
        let (state, calls) = counting_refresh(state().await);
        let (app, cookie, csrf, state) = app_as(Role::Admin, state).await;
        let (status, _, _) = post(
            &app,
            &cookie,
            &csrf,
            "/modules/imgw-meteo/enabled",
            "enabled=1",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(imgw(&state).await.enabled);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            audit_details(&state, "module_enabled").await,
            vec!["imgw-meteo: włączony".to_owned()]
        );
        // Wyłączenie nie budzi źródła.
        post(
            &app,
            &cookie,
            &csrf,
            "/modules/imgw-meteo/enabled",
            "enabled=0",
        )
        .await;
        assert!(!imgw(&state).await.enabled);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_admin_saves_imgw_area_through_the_module_form() {
        let (state, calls) = counting_refresh(state().await);
        let (app, cookie, csrf, state) = app_as(Role::Admin, state).await;
        let fields = format!("area={}&interval_s=600", encode("1061 Łódź\n10"));
        let (status, location, body) = post(
            &app,
            &cookie,
            &csrf,
            "/modules/imgw-meteo/settings",
            &fields,
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        assert_eq!(location.as_deref(), Some("/modules#imgw-meteo"));
        let cfg = imgw(&state).await;
        assert_eq!(cfg.area.len(), 2);
        assert_eq!(cfg.interval_s, 600);
        assert_eq!(
            audit_details(&state, "module_settings").await,
            vec!["imgw-meteo: area, interval_s".to_owned()]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_missing_field_keeps_its_current_value() {
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        post(
            &app,
            &cookie,
            &csrf,
            "/modules/imgw-meteo/settings",
            "interval_s=900",
        )
        .await;
        post(
            &app,
            &cookie,
            &csrf,
            "/modules/imgw-meteo/settings",
            "area=10",
        )
        .await;
        let cfg = imgw(&state).await;
        assert_eq!(cfg.interval_s, 900);
        assert_eq!(cfg.area.len(), 1);
        assert_eq!(
            audit_details(&state, "module_settings").await,
            vec![
                "imgw-meteo: interval_s".to_owned(),
                "imgw-meteo: area".to_owned()
            ]
        );
        // Bez zmian nie ma wpisu.
        post(
            &app,
            &cookie,
            &csrf,
            "/modules/imgw-meteo/settings",
            "area=10",
        )
        .await;
        assert_eq!(audit_details(&state, "module_settings").await.len(), 2);
    }

    #[tokio::test]
    async fn a_county_without_a_name_is_refused_and_nothing_changes() {
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        let (status, _, body) = post(
            &app,
            &cookie,
            &csrf,
            "/modules/imgw-meteo/settings",
            "area=1061&interval_s=600",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("1061"), "wpisana wartość zostaje: {body}");
        let cfg = imgw(&state).await;
        assert_eq!(cfg.area_text, "");
        assert_eq!(cfg.interval_s, 300);
        assert!(audit_details(&state, "module_settings").await.is_empty());
    }

    #[tokio::test]
    async fn an_interval_out_of_range_is_refused() {
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        let fields = format!("area={}&interval_s=30", encode("10"));
        let (status, _, body) = post(
            &app,
            &cookie,
            &csrf,
            "/modules/imgw-meteo/settings",
            &fields,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("60–3600"), "{body}");
        let cfg = imgw(&state).await;
        assert_eq!(cfg.area_text, "");
        assert_eq!(cfg.interval_s, 300);
    }

    #[tokio::test]
    async fn an_operator_cannot_change_module_settings() {
        let (state, calls) = counting_refresh(state().await);
        let (app, cookie, csrf, state) = app_as(Role::Operator, state).await;
        let fields = format!("area={}&interval_s=600", encode("10"));
        let (status, _, _) = post(
            &app,
            &cookie,
            &csrf,
            "/modules/imgw-meteo/settings",
            &fields,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = post(
            &app,
            &cookie,
            &csrf,
            "/modules/scheduler/enabled",
            "enabled=0",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(imgw(&state).await.area_text, "");
        assert_eq!(
            setting(&state, "module:scheduler:enabled").await.as_deref(),
            Some("true")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_wrong_csrf_token_changes_nothing() {
        let (app, cookie, _, state) = app_as(Role::Admin, state().await).await;
        let (status, _, _) = post(
            &app,
            &cookie,
            "zly",
            "/modules/scheduler/enabled",
            "enabled=0",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = post(
            &app,
            &cookie,
            "zly",
            "/modules/imgw-meteo/settings",
            "area=10",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            setting(&state, "module:scheduler:enabled").await.as_deref(),
            Some("true")
        );
        assert_eq!(imgw(&state).await.area_text, "");
    }

    #[tokio::test]
    async fn an_unknown_module_is_404() {
        let (app, cookie, csrf, _) = app_as(Role::Admin, state().await).await;
        for path in ["/modules/nie-ma/enabled", "/modules/nie-ma/settings"] {
            let (status, _, _) = post(&app, &cookie, &csrf, path, "enabled=1&area=10").await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
        // Moduł bez ustawień nie ma formularza.
        let (status, _, _) = post(&app, &cookie, &csrf, "/modules/scheduler/settings", "x=1").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn list_checkboxes_and_decimal_fields_round_trip_through_the_form() {
        use aars_core::module::{ModuleDescriptor, ModuleKind, ModuleRegistry, Needs, SettingKind, SettingSpec, Toggle};
        const OPTS: &[(&str, &str)] = &[("a", "Alfa"), ("b", "Beta"), ("c", "Gamma")];
        const SPECS: &[SettingSpec] = &[
            SettingSpec { key: "cats", label: "Kategorie", help: "", kind: SettingKind::List(OPTS), default: "a" },
            SettingSpec { key: "mag", label: "Magnituda", help: "", kind: SettingKind::Decimal { min: 1.0, max: 9.0 }, default: "3.0" },
        ];
        let mut st = state().await;
        st.modules = Arc::new(ModuleRegistry::new(vec![ModuleDescriptor {
            id: "x-lista",
            name: "Lista",
            description: "d",
            kind: ModuleKind::Scheduler,
            toggle: Toggle::Switchable,
            default_enabled: false,
            needs: Needs::Nothing,
            settings: SPECS,
            see_also: None,
        }]));
        let (app, cookie, csrf, state) = app_as(Role::Admin, st).await;
        let (_, html) = get(&app, &cookie, "/modules").await;
        assert!(html.contains("type=\"checkbox\" name=\"cats\" value=\"a\" checked"), "{html}");
        assert!(html.contains("name=\"cats\" value=\"b\" >"), "{html}");
        assert!(html.contains("inputmode=\"decimal\""), "{html}");
        assert!(html.contains("(1,0–9,0)"), "{html}");
        let (status, _, body) =
            post(&app, &cookie, &csrf, "/modules/x-lista/settings", "cats=c&cats=a&mag=4%2C5").await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        assert_eq!(setting(&state, "module:x-lista:cats").await.as_deref(), Some("a,c"));
        assert_eq!(setting(&state, "module:x-lista:mag").await.as_deref(), Some("4.5"));
        // Formularz pokazuje przecinek dziesiętny, tak jak opis zakresu.
        let (_, html) = get(&app, &cookie, "/modules").await;
        assert!(html.contains("inputmode=\"decimal\" value=\"4,5\""), "{html}");
        let (status, _, body) =
            post(&app, &cookie, &csrf, "/modules/x-lista/settings", "mag=3").await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        // Odznaczenie wszystkich pól listy: pole nie przychodzi, więc bez zmian.
        assert_eq!(setting(&state, "module:x-lista:cats").await.as_deref(), Some("a,c"));
    }

    #[tokio::test]
    async fn a_viewer_cannot_open_modules() {
        let (app, cookie, _, _) = app_as(Role::Viewer, state().await).await;
        assert_eq!(
            get(&app, &cookie, "/modules").await.0,
            StatusCode::FORBIDDEN
        );
    }

    fn preview_of(view: BulletinPreviewView) -> BulletinPreview {
        Arc::new(move || {
            let view = view.clone();
            Box::pin(async move { Ok(view) })
        })
    }

    fn sample_view() -> BulletinPreviewView {
        BulletinPreviewView {
            text: Some("Biuletyn stacji SP9XYZ. 07:00. Koniec biuletynu.".to_owned()),
            parts: vec![(
                "pogoda".to_owned(),
                "pominięta — brak pozycji stacji".to_owned(),
            )],
            skipped_reason: None,
        }
    }

    #[tokio::test]
    async fn an_operator_previews_the_bulletin() {
        let state = state()
            .await
            .with_bulletin_preview(preview_of(sample_view()));
        let (app, cookie, csrf, _) = app_as(Role::Operator, state).await;
        let (_, html) = get(&app, &cookie, "/modules").await;
        assert!(
            html.contains("action=\"/modules/bulletin/preview\""),
            "{html}"
        );
        assert!(html.contains("Podgląd biuletynu"), "{html}");

        let (status, _, html) = post(&app, &cookie, &csrf, "/modules/bulletin/preview", "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        for needle in [
            "Biuletyn stacji SP9XYZ. 07:00. Koniec biuletynu.",
            "pogoda",
            "pominięta — brak pozycji stacji",
            "Odsłuchaj",
        ] {
            assert!(html.contains(needle), "brak {needle}: {html}");
        }
    }

    #[tokio::test]
    async fn a_bulletin_with_nothing_to_say_shows_the_reason() {
        let view = BulletinPreviewView {
            text: None,
            parts: vec![("propagacja".to_owned(), "błąd — x".to_owned())],
            skipped_reason: Some("brak danych".to_owned()),
        };
        let state = state().await.with_bulletin_preview(preview_of(view));
        let (app, cookie, csrf, _) = app_as(Role::Operator, state).await;
        let (status, _, html) = post(&app, &cookie, &csrf, "/modules/bulletin/preview", "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(html.contains("Nic do nadania: brak danych"), "{html}");
        assert!(html.contains("propagacja: błąd — x"), "{html}");
        assert!(!html.contains("Odsłuchaj"), "{html}");
    }

    #[tokio::test]
    async fn the_bulletin_preview_without_a_callback_is_unavailable() {
        let (app, cookie, csrf, _) = app_as(Role::Operator, state().await).await;
        let (status, _, html) = post(&app, &cookie, &csrf, "/modules/bulletin/preview", "").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(html.contains("biuletyn nie wystartował — zobacz dziennik systemowy (journalctl -u aars)"), "{html}");
    }

    #[tokio::test]
    async fn one_bulletin_preview_at_a_time() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let preview: BulletinPreview = {
            let (entered, release, calls) = (entered.clone(), release.clone(), calls.clone());
            Arc::new(move || {
                let (entered, release) = (entered.clone(), release.clone());
                let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                Box::pin(async move {
                    if first {
                        entered.notify_one();
                        release.notified().await;
                    }
                    Ok(sample_view())
                })
            })
        };
        let state = state().await.with_bulletin_preview(preview);
        let (app, cookie, csrf, _) = app_as(Role::Operator, state).await;

        let slow = {
            let (app, cookie, csrf) = (app.clone(), cookie.clone(), csrf.clone());
            tokio::spawn(async move {
                post(&app, &cookie, &csrf, "/modules/bulletin/preview", "").await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .expect("pierwszy podgląd musi ruszyć");

        let (status, _, html) = post(&app, &cookie, &csrf, "/modules/bulletin/preview", "").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            html.contains("podgląd biuletynu już trwa, spróbuj za chwilę"),
            "{html}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "drugi podgląd nie składa biuletynu"
        );

        release.notify_one();
        let (status, _, _) = slow.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = post(&app, &cookie, &csrf, "/modules/bulletin/preview", "").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_bulletin_preview_error_is_shown() {
        let preview: BulletinPreview = Arc::new(|| Box::pin(async { Err("x".to_owned()) }));
        let state = state().await.with_bulletin_preview(preview);
        let (app, cookie, csrf, _) = app_as(Role::Operator, state).await;
        let (status, _, html) = post(&app, &cookie, &csrf, "/modules/bulletin/preview", "").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(html.contains("podgląd biuletynu zawiódł: x"), "{html}");
    }

    #[tokio::test]
    async fn a_viewer_cannot_preview_the_bulletin() {
        let state = state()
            .await
            .with_bulletin_preview(preview_of(sample_view()));
        let (app, cookie, csrf, _) = app_as(Role::Viewer, state).await;
        let (status, _, _) = post(&app, &cookie, &csrf, "/modules/bulletin/preview", "").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
