//! Strona „Moduły” (etap 9, R13–R15; przebudowa w etapie 13): po lewej
//! lista modułów ze stanem i przełącznikiem, po prawej szczegóły wybranego
//! (`/modules?m=<id>`) z ustawieniami generowanymi ze schematu. Operator
//! widzi stan i ustawienia do odczytu. Biuletyn ma własny edytor
//! (`/bulletin`). Panel nie zna crate'a źródeł — źródło budzi
//! `AppState::source_refresh`.

use std::collections::BTreeMap;

use aars_core::error::DbError;
use aars_core::geo::PositionRepo;
use aars_core::module::{
    ModuleDescriptor, ModuleError, ModuleKind, ModuleRepo, SettingKind, Toggle,
};
use aars_core::settings::SettingsRepo;
use aars_core::source_state::SourceStateRepo;
use aars_core::user::Role;
use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;

use crate::audit;
use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::chrome::Chrome;
use crate::render::{db_error, htmx, page};
use crate::sources_page::{health, health_needs};
use crate::state::AppState;

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
    /// Błąd walidacji tego pola (pokazany przy nim, pole z `aria-invalid`).
    pub(crate) error: Option<String>,
    /// Wartość do odczytu (operator): etykieta wyboru, „—” dla pustej.
    pub(crate) display: String,
}

pub(crate) struct ModuleView {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) description: &'static str,
    pub(crate) css: &'static str,
    pub(crate) state: String,
    /// Krótki stan do listy („włączony”, „zawsze”, „planowany”).
    pub(crate) short_state: &'static str,
    pub(crate) switchable: bool,
    pub(crate) enabled: bool,
    pub(crate) is_source: bool,
    /// Plakietka zdrowia źródła z etapu 8: (klasa CSS, tekst).
    pub(crate) health: Option<(&'static str, String)>,
    pub(crate) see_also: Option<(&'static str, &'static str)>,
    pub(crate) fields: Vec<FieldView>,
    /// Źródło z obszarem liczonym z pozycji: zdanie nad polem obszaru.
    pub(crate) auto_area_note: Option<String>,
    /// Tylko `auto-id`: bieżący znak (pusty = brak).
    pub(crate) ident: Option<String>,
    pub(crate) error: Option<String>,
    /// Tylko `bulletin`: skrót ustawień; ustawia się je w edytorze `/bulletin`.
    pub(crate) bulletin: Option<BulletinSummary>,
}

pub(crate) struct BulletinSummary {
    /// „co 10 minut” / „o stałych porach: 07:00, 19:00”.
    pub(crate) when: String,
    pub(crate) next: String,
    /// (włączone, wszystkie) klocki układu.
    pub(crate) blocks_on: usize,
    pub(crate) blocks_all: usize,
}

/// Pozycja listy modułów (lewa kolumna).
pub(crate) struct ListItem {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) css: &'static str,
    pub(crate) short_state: &'static str,
    pub(crate) state: String,
    pub(crate) health: Option<(&'static str, String)>,
    pub(crate) switchable: bool,
    pub(crate) enabled: bool,
}

pub(crate) struct ListGroup {
    pub(crate) title: &'static str,
    pub(crate) items: Vec<ListItem>,
}

#[derive(Template)]
#[template(path = "modules.html")]
struct ModulesPage {
    chrome: Chrome,
    groups: Vec<ListGroup>,
    selected: &'static str,
    detail: ModuleView,
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
        SettingKind::BulletinLayout => "layout",
    }
}

fn field_view(spec: &aars_core::module::SettingSpec, value: String) -> FieldView {
    let (max_len, min, max, options) = match spec.kind {
        SettingKind::Text { max_len } => (max_len, 0, 0, Vec::new()),
        SettingKind::Integer { min, max } => (0, min, max, Vec::new()),
        SettingKind::Area => (0, 0, 0, Vec::new()),
        SettingKind::Choice(options) | SettingKind::List(options) => (0, 0, 0, options.to_vec()),
        SettingKind::Decimal { .. } | SettingKind::TimeList { .. } | SettingKind::BulletinLayout => {
            (0, 0, 0, Vec::new())
        }
    };
    let (min_text, max_text) = match spec.kind {
        SettingKind::Decimal { min, max } => (
            format!("{min:.1}").replace('.', ","),
            format!("{max:.1}").replace('.', ","),
        ),
        _ => (String::new(), String::new()),
    };
    // Kanonicznie „3.0”; w formularzu po polsku „3,0” (walidacja przyjmuje oba).
    // Pusty układ biuletynu to układ domyślny — pokazany, żeby było co edytować.
    let value = match spec.kind {
        SettingKind::Decimal { .. } => value.replace('.', ","),
        SettingKind::BulletinLayout if value.trim().is_empty() => {
            aars_core::bulletin_layout::Layout::default_layout().to_json()
        }
        _ => value,
    };
    let display = match spec.kind {
        SettingKind::Choice(options) => options
            .iter()
            .find(|(v, _)| *v == value)
            .map_or_else(|| value.clone(), |(_, l)| (*l).to_owned()),
        SettingKind::List(options) => {
            let chosen: Vec<&str> = value.split(',').map(str::trim).collect();
            options
                .iter()
                .filter(|(v, _)| chosen.contains(v))
                .map(|(_, l)| *l)
                .collect::<Vec<_>>()
                .join(", ")
        }
        _ => value.clone(),
    };
    let display = if display.trim().is_empty() { "—".to_owned() } else { display };
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
        error: None,
        display,
    }
}

async fn module_view(
    state: &AppState,
    m: &ModuleDescriptor,
    callsign: &str,
) -> Result<ModuleView, DbError> {
    let repo = ModuleRepo::new(state.pool.clone());
    let enabled = repo.is_enabled(m).await?;
    let (css, short_state, state_text) = match m.toggle {
        Toggle::Switchable if enabled => ("on", "włączony", "włączony".to_owned()),
        Toggle::Switchable => ("off", "wyłączony", "wyłączony".to_owned()),
        Toggle::AlwaysOn(reason) => ("on", "zawsze", format!("zawsze włączony — {reason}")),
        Toggle::Planned(when) => ("warn", "planowany", format!("planowany — {when}")),
    };
    let mut health_badge = None;
    let mut auto_area_note = None;
    if m.kind == ModuleKind::Source {
        let position = PositionRepo::new(state.pool.clone()).load().await.ok().flatten();
        let sources = SourceStateRepo::new(state.pool.clone());
        let status = sources
            .statuses()
            .await?
            .into_iter()
            .find(|s| s.source_id == m.id);
        if let (Some(status), Some(cfg)) = (status, sources.config(m.id).await?) {
            let needs = health_needs(m.needs, m.auto_area.is_some(), cfg.area.is_empty());
            health_badge = Some(health(&status, &cfg, needs, position.is_some(), Utc::now()));
            auto_area_note = m.auto_area.map(|g| {
                aars_core::area_auto::auto_area_note(position.as_ref(), g, !cfg.area.is_empty())
            });
        }
    }
    let health = health_badge;
    let values = repo.settings(m).await?;
    let bulletin = if m.kind == ModuleKind::Bulletin {
        Some(bulletin_summary(state, enabled, &values).await?)
    } else {
        None
    };
    // Ustawienia biuletynu (tryb, pory, układ) są w edytorze `/bulletin`.
    let fields = if bulletin.is_some() {
        Vec::new()
    } else {
        m.settings
            .iter()
            .map(|spec| field_view(spec, values.get(spec.key).cloned().unwrap_or_default()))
            .collect()
    };
    Ok(ModuleView {
        id: m.id,
        name: m.name,
        kind: m.kind.label(),
        description: m.description,
        css,
        state: state_text,
        short_state,
        switchable: m.toggle == Toggle::Switchable,
        enabled,
        is_source: m.kind == ModuleKind::Source,
        health,
        see_also: m.see_also,
        fields,
        auto_area_note,
        ident: (m.kind == ModuleKind::Ident).then(|| callsign.to_owned()),
        error: None,
        bulletin,
    })
}

async fn bulletin_summary(
    state: &AppState,
    enabled: bool,
    values: &BTreeMap<String, String>,
) -> Result<BulletinSummary, DbError> {
    use aars_core::bulletin_layout as bl;
    let when = match bl::cycle_interval(values) {
        Some(n) => format!("co {n} minut, o pełnych wielokrotnościach od północy"),
        None => format!(
            "o stałych porach: {}",
            values.get("times").map(String::as_str).unwrap_or("").replace(',', ", ")
        ),
    };
    let next = if enabled {
        let warsaw = Utc::now().with_timezone(&chrono_tz::Europe::Warsaw).time();
        crate::status::load_next_bulletin(state, warsaw).await?.label()
    } else {
        "moduł wyłączony".to_owned()
    };
    let layout = bl::load(&state.pool).await?;
    Ok(BulletinSummary {
        when,
        next,
        blocks_on: layout.blocks.iter().filter(|b| b.enabled).count(),
        blocks_all: layout.blocks.len(),
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

/// Strona modułów ze statusem `status` i szczegółami modułu `id` (nieznany
/// → pierwszy); `edit` zmienia jego kartę.
async fn page_with(
    state: &AppState,
    current: &CurrentUser,
    id: &str,
    status: StatusCode,
    edit: impl FnOnce(&mut ModuleView),
) -> Response {
    let views = match load_views(state).await {
        Ok(views) => views,
        Err(err) => return db_error("odczyt modułów zawiódł", &err),
    };
    let Some(at) = views.iter().position(|v| v.id == id).or((!views.is_empty()).then_some(0)) else {
        return db_error("odczyt modułów zawiódł", &"pusty rejestr modułów");
    };
    let mut builtin = Vec::new();
    let mut sources = Vec::new();
    let mut detail = None;
    for (i, view) in views.into_iter().enumerate() {
        let item = ListItem {
            id: view.id,
            name: view.name,
            css: view.css,
            short_state: view.short_state,
            state: view.state.clone(),
            // Wyłączone źródło ma zdrowie „wyłączone” — to już mówi stan.
            health: view.health.clone().filter(|_| view.enabled),
            switchable: view.switchable,
            enabled: view.enabled,
        };
        if view.is_source { sources.push(item) } else { builtin.push(item) }
        if i == at {
            detail = Some(view);
        }
    }
    let Some(mut detail) = detail else {
        return db_error("odczyt modułów zawiódł", &"brak modułu");
    };
    edit(&mut detail);
    let mut groups = vec![ListGroup { title: "Stacja", items: builtin }];
    if !sources.is_empty() {
        groups.push(ListGroup { title: "Źródła danych", items: sources });
    }
    (
        status,
        page(&ModulesPage {
            chrome: Chrome::load(state, current).await,
            groups,
            selected: detail.id,
            detail,
        }),
    )
        .into_response()
}

/// Strona z błędem w karcie modułu `id`; `entered` zastępuje wartości pól.
/// Błąd pola `field` stoi przy nim, inny — u góry karty. Formularz htmx
/// dostaje kartę z kodem 422 i powiadomienie z treścią błędu.
async fn page_with_error(
    state: &AppState,
    current: &CurrentUser,
    id: &str,
    message: String,
    entered: Option<&BTreeMap<String, String>>,
    field: Option<&str>,
) -> Response {
    let toast = message.clone();
    let response = page_with(state, current, id, StatusCode::BAD_REQUEST, |view| {
        if let Some(entered) = entered {
            for field in &mut view.fields {
                if let Some(value) = entered.get(field.key) {
                    field.value = value.clone();
                }
            }
        }
        match view.fields.iter_mut().find(|f| Some(f.key) == field) {
            Some(f) => f.error = Some(message),
            None => view.error = Some(message),
        }
    })
    .await;
    if current.htmx {
        htmx::refused(response, &toast)
    } else {
        response
    }
}

/// Po udanej akcji: formularz htmx dostaje świeżą stronę ze szczegółami
/// modułu `id` (htmx bierze z niej listę i kartę) i powiadomienie, zwykły
/// formularz przekierowanie jak dotąd.
async fn done(
    state: AppState,
    current: CurrentUser,
    id: &str,
    toast: &str,
) -> Result<Response, AuthRejection> {
    if current.htmx {
        let response = page_with(&state, &current, id, StatusCode::OK, |_| {}).await;
        return Ok(htmx::ok(response, toast));
    }
    Ok(see_other(format!("/modules?m={id}")))
}

#[derive(Debug, Default, Deserialize)]
pub struct Selection {
    #[serde(default)]
    m: Option<String>,
}

/// `GET /modules?m=<id>` (operator: stan i ustawienia do odczytu; admin:
/// także przełączniki i formularze). Bez `m` — pierwszy moduł.
pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(selection): Query<Selection>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    let id = selection.m.unwrap_or_default();
    Ok(page_with(&state, &current, &id, StatusCode::OK, |_| {}).await)
}

#[derive(Debug, Deserialize)]
pub struct EnabledForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    enabled: String,
    /// Moduł, którego szczegóły są otwarte (przełącznik na liście).
    #[serde(default)]
    m: Option<String>,
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
            return Ok(page_with_error(&state, &current, m.id, err.to_string(), None, None).await);
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
    let toast = if enabled { "Włączono" } else { "Wyłączono" };
    let shown = form
        .m
        .as_deref()
        .and_then(|m| state.modules.get(m))
        .map_or(m.id, |shown| shown.id);
    done(state, current, shown, toast).await
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
    let values = match m.validate_fields(&input) {
        Ok(values) => values,
        Err((key, message)) => {
            return Ok(
                page_with_error(&state, &current, m.id, message, Some(&input), Some(key)).await,
            );
        }
    };
    let changed: Vec<&str> = m
        .settings
        .iter()
        .filter(|spec| stored.get(spec.key) != values.get(spec.key))
        .map(|spec| spec.key)
        .collect();
    if changed.is_empty() {
        return done(state, current, m.id, "Bez zmian").await;
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
    done(state, current, m.id, "Zapisano").await
}

#[cfg(test)]
mod tests {
    use crate::AppState;
    use crate::router;
    use crate::tests::{login, state};
    use aars_core::settings::SettingsRepo;
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
    async fn an_operator_sees_the_list_and_details_without_forms() {
        let (app, cookie, _, _) = app_as(Role::Operator, state().await).await;
        let (status, html) = get(&app, &cookie, "/modules").await;
        assert_eq!(status, StatusCode::OK);
        for needle in [
            "Harmonogram",
            "Ostrzeżenia meteorologiczne IMGW",
            "Biuletyn",
            "id=\"module-list\"",
            "id=\"module-detail\"",
            "href=\"/modules?m=imgw-meteo\"",
            "hx-push-url=\"true\"",
        ] {
            assert!(html.contains(needle), "brak {needle}: {html}");
        }
        // Kolejność rejestru: wbudowane, potem źródła (osobna grupa).
        let scheduler = html.find("Harmonogram").unwrap();
        let group = html.find(">Źródła danych<").unwrap();
        let source = html.find("Ostrzeżenia meteorologiczne IMGW").unwrap();
        assert!(scheduler < group && group < source, "{html}");
        // Szczegóły źródła: stan, link, ustawienia do odczytu — bez formularzy.
        let (_, html) = get(&app, &cookie, "/modules?m=imgw-meteo").await;
        for needle in [
            "źródło danych",
            "href=\"/sources\"",
            "Interwał odpytywania",
            "<dl class=\"facts\">",
            "aria-current=\"page\"",
        ] {
            assert!(html.contains(needle), "brak {needle}: {html}");
        }
        assert_eq!(html.matches("action=\"/modules/").count(), 0, "{html}");
        assert!(!html.contains("role=\"switch\""), "{html}");
        let (_, html) = get(&app, &cookie, "/modules?m=auto-id").await;
        assert!(html.contains("zawsze włączony — identyfikacja stacji jest obowiązkowa"), "{html}");
    }

    #[tokio::test]
    async fn an_admin_sees_toggles_and_forms_generated_from_the_schema() {
        let (app, cookie, _, _) = app_as(Role::Admin, state().await).await;
        let (_, html) = get(&app, &cookie, "/modules?m=imgw-meteo").await;
        for needle in [
            "action=\"/modules/scheduler/enabled\"",
            "action=\"/modules/imgw-meteo/enabled\"",
            "action=\"/modules/imgw-meteo/settings\"",
            "action=\"/modules/bulletin/enabled\"",
            "role=\"switch\" aria-checked=\"false\"",
            "name=\"m\" value=\"imgw-meteo\"",
            "<textarea",
            "name=\"area\"",
            "name=\"interval_s\" type=\"number\" min=\"60\" max=\"3600\" value=\"300\"",
            "id=\"imgw-meteo\"",
        ] {
            assert!(html.contains(needle), "brak {needle}: {html}");
        }
        for absent in [
            "action=\"/modules/auto-id/enabled\"",
            "action=\"/modules/scheduler/settings\"",
        ] {
            assert!(!html.contains(absent), "{absent}: {html}");
        }
        let (_, html) = get(&app, &cookie, "/modules?m=auto-id").await;
        assert!(html.contains("Uwaga: bez znaku wywoławczego stacja nadaje bez identyfikacji."), "{html}");
        // Nieznany moduł w adresie: pierwszy z listy, nie błąd.
        let (status, _) = get(&app, &cookie, "/modules?m=nie-ma").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_bulletin_details_point_to_the_editor() {
        let (app, cookie, _, _) = app_as(Role::Admin, state().await).await;
        let (_, html) = get(&app, &cookie, "/modules?m=bulletin").await;
        assert!(html.contains("href=\"/bulletin\""), "{html}");
        assert!(html.contains("Otwórz edytor biuletynu"), "{html}");
        assert!(html.contains("co 10 minut"), "{html}");
        // Ustawienia biuletynu są w edytorze, nie tutaj.
        assert!(!html.contains("action=\"/modules/bulletin/settings\""), "{html}");
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
        assert_eq!(location.as_deref(), Some("/modules?m=scheduler"));
        assert_eq!(
            setting(&state, "module:scheduler:enabled").await.as_deref(),
            Some("false")
        );
        assert_eq!(
            audit_details(&state, "module_enabled").await,
            vec!["scheduler: wyłączony".to_owned()]
        );
        let (_, html) = get(&app, &cookie, "/modules?m=scheduler").await;
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
        let (_, html) = get(&app, &cookie, "/bulletin").await;
        assert!(html.contains("Pory nadania"), "{html}");
        assert!(html.contains("value=\"07:00,19:00\""), "{html}");
        assert!(html.contains("moduł włączony"), "{html}");
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
        assert_eq!(location.as_deref(), Some("/modules?m=imgw-meteo"));
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
    async fn the_bulletin_layout_is_validated_and_stored_canonically() {
        use aars_core::bulletin_layout::Layout;
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        aars_core::module::ModuleRepo::new(state.pool.clone())
            .register(&state.modules)
            .await
            .unwrap();
        let (_, html) = get(&app, &cookie, "/bulletin").await;
        assert!(html.contains("name=\"layout\""), "{html}");
        assert!(html.contains("&#34;pogoda_teraz&#34;"), "{html}");
        assert!(html.contains("Co określony czas"), "{html}");

        let kv = SettingsRepo::new(state.pool.clone());
        let json = r#"{"bloki":[{"rodzaj":"wody"}]}"#;
        let body = format!("layout={}&mode=pory", urlencode(json));
        let (status, _, _) = post(&app, &cookie, &csrf, "/modules/bulletin/settings", &body).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let stored = kv.kv_get("module:bulletin:layout").await.unwrap().unwrap();
        assert_eq!(stored, Layout::parse(json).unwrap().to_json());
        assert_eq!(kv.kv_get("module:bulletin:mode").await.unwrap().as_deref(), Some("pory"));

        let bad = r#"{"bloki":[{"rodzaj":"wstep","tekst":"Tu {stacja}."}]}"#;
        let body = format!("layout={}", urlencode(bad));
        let (status, _, html) = post(&app, &cookie, &csrf, "/modules/bulletin/settings", &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(html.contains("nieznana zmienna"), "{html}");
        assert_eq!(kv.kv_get("module:bulletin:layout").await.unwrap().unwrap(), stored);
    }

    fn urlencode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
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
            auto_area: None,
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

    async fn auto_area_state(position: Option<(&str, &str)>) -> AppState {
        use aars_core::area_auto::AreaGranularity;
        use aars_core::module::{ModuleDescriptor, ModuleRegistry, sample_source};
        let mut st = state().await;
        st.modules = Arc::new(ModuleRegistry::new(vec![ModuleDescriptor {
            auto_area: Some(AreaGranularity::County),
            ..sample_source("imgw-meteo")
        }]));
        if let Some((lat, lon)) = position {
            let p = aars_core::geo::parse_position(lat, lon, "").unwrap();
            aars_core::geo::PositionRepo::new(st.pool.clone()).save(p.as_ref()).await.unwrap();
        }
        st
    }

    #[tokio::test]
    async fn the_area_field_shows_the_automatic_county() {
        let st = auto_area_state(Some(("51.38", "20.28"))).await;
        let (app, cookie, _, _) = app_as(Role::Admin, st).await;
        let (_, html) = get(&app, &cookie, "/modules").await;
        assert!(html.contains("Automatycznie: powiat opoczyński (1007)"), "{html}");
        assert!(html.contains("Nadpisz obszar (zostaw puste, żeby liczyć z pozycji)"), "{html}");
        let (_, html) = get(&app, &cookie, "/sources").await;
        assert!(html.contains("Automatycznie: powiat opoczyński (1007)"), "{html}");
    }

    #[tokio::test]
    async fn a_manual_area_is_not_presented_as_automatic() {
        let st = auto_area_state(Some(("51.38", "20.28"))).await;
        SourceStateRepo::new(st.pool.clone())
            .save_config("imgw-meteo", "1061 Łódź", 300)
            .await
            .unwrap();
        let (app, cookie, _, _) = app_as(Role::Admin, st).await;
        let (_, html) = get(&app, &cookie, "/modules").await;
        assert!(html.contains("Obszar ręczny. Z pozycji byłby: powiat opoczyński (1007)."), "{html}");
        assert!(!html.contains("Automatycznie:"), "{html}");
    }

    #[tokio::test]
    async fn the_area_field_asks_for_a_position_when_there_is_none() {
        let st = auto_area_state(None).await;
        let (app, cookie, _, _) = app_as(Role::Admin, st).await;
        let (_, html) = get(&app, &cookie, "/modules").await;
        assert!(html.contains("Brak pozycji stacji — ustaw ją w Ustawieniach"), "{html}");
        assert!(!html.contains("brak obszaru"), "{html}");
    }

    #[tokio::test]
    async fn a_viewer_cannot_open_modules() {
        let (app, cookie, _, _) = app_as(Role::Viewer, state().await).await;
        assert_eq!(
            get(&app, &cookie, "/modules").await.0,
            StatusCode::FORBIDDEN
        );
    }
}
