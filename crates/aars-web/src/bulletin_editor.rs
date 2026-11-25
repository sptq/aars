//! Edytor biuletynu z klocków (etap 13, 4a): kolejność, pauzy przed i po,
//! kiedy klocek jest nadawany, własne teksty, tryb i pory nadawania, oś
//! czasu, podgląd i odsłuch niezapisanego układu, nadanie na żądanie.
//!
//! Serwer renderuje układ do odczytu (bez JS) i formularz z polem `layout`
//! (JSON). `assets/bulletin-editor.js` zamienia go w edytor kart, a stan
//! trzyma w tym samym polu, więc zapis idzie zawsze tą samą drogą
//! i tą samą walidacją (`ModuleDescriptor::validate_fields`,
//! `Layout::parse_located`). Operator ogląda, słucha i nadaje; zmienia
//! administrator.

use std::collections::BTreeMap;

use aars_core::bulletin_layout::{
    self as bl, BlockKind, Layout, LayoutError, When, render_template,
};
use aars_core::error::DbError;
use aars_core::module::{BULLETIN, BULLETIN_MODES, ModuleRepo};
use aars_core::priority::Priority;
use aars_core::queue::EnqueueRequest;
use aars_core::settings::SettingsRepo;
use aars_core::user::Role;
use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{NaiveDateTime, Utc};
use serde::Serialize;

use crate::audit;
use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::chrome::Chrome;
use crate::render::{db_error, htmx, page};
use crate::state::{AppState, BulletinPreviewView};

/// Klocek do odczytu (bez JS, operator) i do oznaczenia błędu.
pub(crate) struct BlockView {
    pub(crate) n: usize,
    pub(crate) kind: &'static str,
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
    pub(crate) enabled: bool,
    pub(crate) before: String,
    pub(crate) after: String,
    pub(crate) always: bool,
    pub(crate) when: String,
    pub(crate) text: String,
    /// Tekst po wstawieniu zmiennych na teraz.
    pub(crate) rendered: String,
    pub(crate) say_when_empty: bool,
    pub(crate) is_warnings: bool,
    pub(crate) error: Option<String>,
}

/// Wiersz wyniku podglądu: klocek i co się z nim stało.
pub(crate) struct PartRow {
    pub(crate) label: String,
    pub(crate) outcome: String,
    /// `on` (w biuletynie) | `off` (pominięty) | `err` (błąd) | `warn` (limit)
    pub(crate) css: &'static str,
}

#[derive(Default)]
pub(crate) struct PreviewCard {
    /// Surowy tekst (ze znacznikami pauz) do odsłuchu.
    pub(crate) text: Option<String>,
    pub(crate) shown: Option<String>,
    pub(crate) parts: Vec<PartRow>,
    pub(crate) skipped: Option<String>,
    /// Długości klocków jako JSON (`[123, null, …]`) dla osi czasu.
    pub(crate) chars_json: String,
    /// Podgląd z niezapisanego układu.
    pub(crate) unsaved: bool,
    pub(crate) seconds: u64,
}

#[derive(Template)]
#[template(path = "bulletin_editor.html")]
struct EditorPage {
    chrome: Chrome,
    module_enabled: bool,
    can_edit: bool,
    mode: String,
    interval_min: String,
    times: String,
    modes: &'static [(&'static str, &'static str)],
    min_interval: u32,
    max_interval: u32,
    /// Pole formularza z błędem (`mode`, `interval_min`, `times`, `layout`).
    error_field: &'static str,
    error: Option<String>,
    blocks: Vec<BlockView>,
    layout_json: String,
    default_json: String,
    catalog_json: String,
    vars: &'static [&'static str],
    callsign: String,
    tx_max_s: u64,
    limit_s: u64,
    estimate_s: u64,
    next: String,
    /// Formularz pokazuje wpisany, niezapisany układ (np. po błędzie).
    dirty: bool,
    preview: Option<PreviewCard>,
    preview_error: Option<String>,
    send_error: Option<String>,
    /// Bez JS: pytanie „przywrócić układ domyślny?” przed zapisem.
    confirm_reset: bool,
    /// Teraz w Warszawie (`YYYY-MM-DDTHH:MM:SS`) — JS liczy „Usłyszysz”
    /// i pory cyklu w czasie stacji, nie przeglądarki.
    now_warsaw: String,
}

#[derive(Serialize)]
struct CatalogEntry {
    id: &'static str,
    label: &'static str,
    desc: &'static str,
    text: bool,
    warn: bool,
    protected: bool,
    typical: usize,
}

fn catalog_json() -> String {
    let entries: Vec<CatalogEntry> = BlockKind::ALL
        .into_iter()
        .map(|k| CatalogEntry {
            id: k.id(),
            label: k.label(),
            desc: k.description(),
            text: k.is_text(),
            warn: k == BlockKind::Ostrzezenia,
            protected: k.is_protected(),
            typical: k.typical_chars(),
        })
        .collect();
    serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_owned())
}

/// Sekundy po polsku z jednym miejscem po przecinku: 700 → „0,7 s”.
fn seconds(ms: u32) -> String {
    let text = format!("{:.1}", f64::from(ms) / 1000.0).replace('.', ",");
    format!("{text} s")
}

fn local_now() -> NaiveDateTime {
    Utc::now()
        .with_timezone(&chrono_tz::Europe::Warsaw)
        .naive_local()
}

fn block_views(layout: &Layout, callsign: &str, error: Option<&LayoutError>) -> Vec<BlockView> {
    let now = local_now();
    layout
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| BlockView {
            n: i + 1,
            kind: b.kind.id(),
            label: b.kind.label(),
            description: b.kind.description(),
            enabled: b.enabled,
            before: seconds(b.pause_before_ms),
            after: seconds(b.pause_after_ms),
            always: b.when == When::Zawsze,
            when: b.when.describe(),
            text: b.text.clone(),
            rendered: if b.kind.is_text() {
                render_template(&b.text, callsign, now)
            } else {
                String::new()
            },
            say_when_empty: b.say_when_empty,
            is_warnings: b.kind == BlockKind::Ostrzezenia,
            error: error
                .filter(|e| e.block == Some(i))
                .map(|e| e.detail.clone()),
        })
        .collect()
}

/// Szacowany czas układu w sekundach (10 znaków ≈ 1 s, pauzy jak
/// w kompozytorze: po poprzednim plus przed następnym, najwyżej 10 s).
/// Klocki z danymi liczone typową długością; „kiedy” pominięte (najdłuższy
/// cykl). To samo liczy oś czasu w `bulletin-editor.js`.
fn estimate_s(layout: &Layout, callsign: &str) -> u64 {
    let now = local_now();
    let mut chars = 0usize;
    let mut pause_ms = 0u64;
    let mut prev_after: Option<u32> = None;
    for b in layout.blocks.iter().filter(|b| b.enabled) {
        let len = if b.kind.is_text() {
            render_template(&b.text, callsign, now).chars().count()
        } else {
            b.kind.typical_chars()
        };
        if let Some(after) = prev_after {
            pause_ms += u64::from((after + b.pause_before_ms).min(bl::MAX_PAUSE_MS));
        }
        chars += len + 1;
        prev_after = Some(b.pause_after_ms);
    }
    (chars as u64 / 10) + pause_ms / 1000
}

fn preview_card(view: BulletinPreviewView, unsaved: bool) -> PreviewCard {
    let parts = view
        .parts
        .iter()
        .enumerate()
        .map(|(i, (label, outcome))| {
            let included = view.part_chars.get(i).copied().flatten().is_some();
            let css = if included {
                "on"
            } else if outcome.starts_with("błąd") {
                "err"
            } else if outcome.contains("limit czasu") {
                "warn"
            } else {
                "off"
            };
            PartRow {
                label: label.clone(),
                outcome: outcome.clone(),
                css,
            }
        })
        .collect();
    let chars: Vec<String> = view
        .part_chars
        .iter()
        .map(|c| c.map_or_else(|| "null".to_owned(), |n| n.to_string()))
        .collect();
    let seconds = view.text.as_deref().map_or(0, |t| {
        let pauses: u64 = aars_core::pause::split(t)
            .iter()
            .map(|s| match s {
                aars_core::pause::Segment::Pause(d) => d.as_millis() as u64,
                aars_core::pause::Segment::Text(_) => 0,
            })
            .sum();
        aars_core::pause::strip_markers(t).chars().count() as u64 / 10 + pauses / 1000
    });
    PreviewCard {
        shown: view.display_text(),
        text: view.text,
        parts,
        skipped: view.skipped_reason,
        chars_json: format!("[{}]", chars.join(",")),
        unsaved,
        seconds,
    }
}

/// Zapisany stan edytora.
async fn load(state: &AppState, current: &CurrentUser) -> Result<EditorPage, DbError> {
    let repo = ModuleRepo::new(state.pool.clone());
    let module_enabled = repo.is_enabled(&BULLETIN).await?;
    let values = repo.settings(&BULLETIN).await?;
    let station = SettingsRepo::new(state.pool.clone()).load().await?;
    let layout = bl::load(&state.pool).await?;
    let next = if module_enabled {
        let warsaw = Utc::now().with_timezone(&chrono_tz::Europe::Warsaw).time();
        crate::status::load_next_bulletin(state, warsaw)
            .await?
            .label()
    } else {
        "moduł wyłączony".to_owned()
    };
    let get = |k: &str| values.get(k).cloned().unwrap_or_default();
    let limit_chars = bl::length_limit(station.tx_max_duration_s);
    Ok(EditorPage {
        chrome: Chrome::load(state, current).await,
        module_enabled,
        can_edit: current.user.role.can(Role::Admin),
        mode: if bl::cycle_interval(&values).is_some() {
            bl::MODE_CYCLE.to_owned()
        } else {
            bl::MODE_TIMES.to_owned()
        },
        interval_min: get("interval_min"),
        times: get("times"),
        modes: BULLETIN_MODES,
        min_interval: bl::MIN_INTERVAL_MIN,
        max_interval: bl::MAX_INTERVAL_MIN,
        error_field: "",
        error: None,
        blocks: block_views(&layout, &station.callsign, None),
        layout_json: layout.to_json(),
        default_json: Layout::default_layout().to_json(),
        catalog_json: catalog_json(),
        vars: bl::TEMPLATE_VARS,
        estimate_s: estimate_s(&layout, &station.callsign),
        callsign: station.callsign,
        tx_max_s: station.tx_max_duration_s,
        limit_s: (limit_chars / 10) as u64,
        next,
        dirty: false,
        preview: None,
        preview_error: None,
        send_error: None,
        confirm_reset: false,
        now_warsaw: local_now().format("%Y-%m-%dT%H:%M:%S").to_string(),
    })
}

async fn render_with(
    state: &AppState,
    current: &CurrentUser,
    status: StatusCode,
    edit: impl FnOnce(&mut EditorPage),
) -> Response {
    match load(state, current).await {
        Ok(mut view) => {
            edit(&mut view);
            (status, page(&view)).into_response()
        }
        Err(err) => db_error("odczyt edytora biuletynu zawiódł", &err),
    }
}

/// `GET /bulletin` (operator: ogląda i słucha; admin: edytuje).
pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    Ok(render_with(&state, &current, StatusCode::OK, |_| {}).await)
}

/// Pola formularza (powtórzone klucze: pierwszy wygrywa).
fn fields(pairs: Vec<(String, String)>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in pairs {
        out.entry(k).or_insert(v);
    }
    out
}

const FORM_KEYS: [&str; 4] = ["mode", "interval_min", "times", "layout"];

/// Górna granica pola `layout` przed odczytem JSON. Najdłuższy poprawny
/// układ (24 klocki po 500 znaków, z wcięciami) to około 20 KiB.
pub const MAX_LAYOUT_BYTES: usize = 32 * 1024;

fn too_long() -> String {
    format!(
        "układ biuletynu: najwyżej {} KiB zapisu",
        MAX_LAYOUT_BYTES / 1024
    )
}

/// `POST /bulletin/layout` (admin): tryb, interwał, pory i układ (JSON)
/// razem, przez walidację opisu modułu. Błąd układu stoi przy klocku,
/// którego dotyczy; wpisane wartości zostają. Bez JS „Przywróć domyślny”
/// ma dwa kroki: `reset=1` pokazuje pytanie (nic nie zapisuje),
/// `reset=confirm` zapisuje układ domyślny (z JS edytor wczytuje domyślny
/// u siebie i zapisuje się zwykłym „Zapisz”).
pub async fn save(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Admin)?;
    let mut form = fields(pairs);
    require_csrf(&current.csrf_token, form.get("csrf").map(String::as_str))?;
    match form.get("reset").map(String::as_str) {
        Some("1") => {
            return Ok(render_with(&state, &current, StatusCode::OK, |view| {
                view.confirm_reset = true;
            })
            .await);
        }
        Some("confirm") => {
            form.insert("layout".to_owned(), String::new());
        }
        _ => {}
    }
    if form
        .get("layout")
        .is_some_and(|l| l.len() > MAX_LAYOUT_BYTES)
    {
        let message = too_long();
        let toast = message.clone();
        // Bez odsyłania wpisanego tekstu i bez rysowania jego klocków.
        let response = render_with(&state, &current, StatusCode::BAD_REQUEST, |view| {
            view.error_field = "layout";
            view.error = Some(message);
        })
        .await;
        return Ok(if current.htmx {
            htmx::refused(response, &toast)
        } else {
            response
        });
    }
    let repo = ModuleRepo::new(state.pool.clone());
    let stored = match repo.settings(&BULLETIN).await {
        Ok(stored) => stored,
        Err(err) => return Ok(db_error("odczyt ustawień biuletynu zawiódł", &err)),
    };
    let mut input = stored.clone();
    for key in FORM_KEYS {
        if let Some(value) = form.get(key) {
            input.insert(key.to_owned(), value.clone());
        }
    }
    let values = match BULLETIN.validate_fields(&input) {
        Ok(values) => values,
        Err((key, message)) => {
            return Ok(refused(&state, &current, &input, key, message).await);
        }
    };
    let changed: Vec<&str> = BULLETIN
        .settings
        .iter()
        .filter(|spec| stored.get(spec.key) != values.get(spec.key))
        .map(|spec| spec.key)
        .collect();
    let toast = if changed.is_empty() {
        "Bez zmian"
    } else {
        if let Err(err) = repo.save_settings(&BULLETIN, &values).await {
            return Ok(db_error("zapis ustawień biuletynu zawiódł", &err));
        }
        audit(
            &state,
            current.user.id,
            "module_settings",
            &format!("bulletin: {}", changed.join(", ")),
        )
        .await;
        "Zapisano układ biuletynu"
    };
    if current.htmx {
        let response = render_with(&state, &current, StatusCode::OK, |_| {}).await;
        return Ok(htmx::ok(response, toast));
    }
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, "/bulletin")]).into_response())
}

/// Formularz z błędem: wpisane pola zostają, błąd układu przy klocku
/// (gdy JSON da się odczytać, edytor pokazuje wpisany układ).
async fn refused(
    state: &AppState,
    current: &CurrentUser,
    input: &BTreeMap<String, String>,
    key: &'static str,
    message: String,
) -> Response {
    let toast = message.clone();
    let raw = input.get("layout").cloned().unwrap_or_default();
    let response = render_with(state, current, StatusCode::BAD_REQUEST, |view| {
        let get = |k: &str| input.get(k).cloned().unwrap_or_default();
        view.mode = get("mode");
        view.interval_min = get("interval_min");
        view.times = get("times");
        view.error_field = key;
        view.dirty = true;
        if key == "layout" && !raw.trim().is_empty() {
            view.layout_json = raw.clone();
            let located = Layout::parse_located(&raw).err();
            if let Ok(entered) = Layout::parse_unchecked(&raw)
                && entered.blocks.len() <= bl::MAX_BLOCKS
            {
                view.blocks = block_views(&entered, &view.callsign, located.as_ref());
                view.estimate_s = estimate_s(&entered, &view.callsign);
                // Błąd przy klocku — u góry tylko, gdy dotyczy całego układu.
                if located.as_ref().is_some_and(|e| e.block.is_some()) {
                    view.error_field = "block";
                }
            }
        }
        view.error = Some(message);
    })
    .await;
    if current.htmx {
        htmx::refused(response, &toast)
    } else {
        response
    }
}

/// Układ do podglądu z pola `layout`: brak pola — zapisany (`None`),
/// puste — domyślny, inaczej wpisany (sprawdzony).
fn preview_layout(form: &BTreeMap<String, String>) -> Result<Option<Layout>, LayoutError> {
    match form.get("layout") {
        None => Ok(None),
        Some(raw) if raw.len() > MAX_LAYOUT_BYTES => Err(LayoutError {
            block: None,
            message: too_long(),
            detail: too_long(),
        }),
        Some(raw) if raw.trim().is_empty() => Ok(Some(Layout::default_layout())),
        Some(raw) => Layout::parse_located(raw).map(Some),
    }
}

async fn preview_failed(
    state: &AppState,
    current: &CurrentUser,
    status: StatusCode,
    message: String,
) -> Response {
    let toast = message.clone();
    let response = render_with(state, current, status, |view| {
        view.preview_error = Some(message);
    })
    .await;
    if current.htmx {
        htmx::refused(response, &toast)
    } else {
        response
    }
}

const NOT_STARTED: &str =
    "biuletyn nie wystartował — zobacz dziennik systemowy (journalctl -u aars)";

/// `POST /bulletin/preview` (operator): biuletyn złożony na teraz z układu
/// z formularza (niezapisanego) albo zapisanego; bez kolejki i bez zapisu.
/// Jeden podgląd naraz — każdy pyta sieć o wszystkie części.
pub async fn preview(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    let form = fields(pairs);
    require_csrf(&current.csrf_token, form.get("csrf").map(String::as_str))?;
    let layout = match preview_layout(&form) {
        Ok(layout) => layout,
        Err(e) => {
            return Ok(preview_failed(&state, &current, StatusCode::BAD_REQUEST, e.message).await);
        }
    };
    let Some(compose) = state.bulletin_preview.clone() else {
        return Ok(preview_failed(
            &state,
            &current,
            StatusCode::SERVICE_UNAVAILABLE,
            NOT_STARTED.to_owned(),
        )
        .await);
    };
    let Ok(_permit) = state.bulletin_preview_gate.clone().try_acquire_owned() else {
        return Ok(preview_failed(
            &state,
            &current,
            StatusCode::SERVICE_UNAVAILABLE,
            "podgląd biuletynu już trwa, spróbuj za chwilę".to_owned(),
        )
        .await);
    };
    let unsaved = layout.is_some();
    match compose(layout).await {
        Ok(view) => {
            let response = render_with(&state, &current, StatusCode::OK, |page| {
                page.preview = Some(preview_card(view, unsaved));
            })
            .await;
            Ok(if current.htmx {
                htmx::ok(response, "Podgląd gotowy")
            } else {
                response
            })
        }
        Err(e) => Ok(preview_failed(
            &state,
            &current,
            StatusCode::BAD_GATEWAY,
            format!("podgląd biuletynu zawiódł: {e}"),
        )
        .await),
    }
}

/// Jak długo ręcznie zlecony biuletyn czeka w kolejce (jak pora z harmonogramu).
const SEND_EXPIRY: chrono::TimeDelta = chrono::TimeDelta::minutes(15);

async fn send_failed(
    state: &AppState,
    current: &CurrentUser,
    status: StatusCode,
    message: String,
    preview: Option<PreviewCard>,
) -> Response {
    let toast = message.clone();
    let response = render_with(state, current, status, |view| {
        view.send_error = Some(message);
        view.preview = preview;
    })
    .await;
    if current.htmx {
        htmx::refused(response, &toast)
    } else {
        response
    }
}

/// `POST /bulletin/send` (operator, jawny przycisk „Nadaj teraz”): biuletyn
/// z ZAPISANEGO układu, złożony ze świeżych danych, idzie do kolejki jak
/// pora z harmonogramu (priorytet Zwykły, ważny 15 min). Pole `layout`
/// z formularza jest ignorowane — nadaje się to, co zapisane. Gdy nie ma
/// czego nadać, nic nie trafia do kolejki, a karta podaje powód.
pub async fn send(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, Role::Operator)?;
    let form = fields(pairs);
    require_csrf(&current.csrf_token, form.get("csrf").map(String::as_str))?;
    let Some(compose) = state.bulletin_preview.clone() else {
        return Ok(send_failed(
            &state,
            &current,
            StatusCode::SERVICE_UNAVAILABLE,
            NOT_STARTED.to_owned(),
            None,
        )
        .await);
    };
    let Ok(_permit) = state.bulletin_preview_gate.clone().try_acquire_owned() else {
        return Ok(send_failed(
            &state,
            &current,
            StatusCode::SERVICE_UNAVAILABLE,
            "biuletyn jest właśnie składany, spróbuj za chwilę".to_owned(),
            None,
        )
        .await);
    };
    let view = match compose(None).await {
        Ok(view) => view,
        Err(e) => {
            return Ok(send_failed(
                &state,
                &current,
                StatusCode::BAD_GATEWAY,
                format!("składanie biuletynu zawiodło: {e}"),
                None,
            )
            .await);
        }
    };
    let Some(text) = view.text.clone() else {
        let reason = view
            .skipped_reason
            .clone()
            .unwrap_or_else(|| "brak danych".to_owned());
        return Ok(send_failed(
            &state,
            &current,
            StatusCode::OK,
            format!("Nic do nadania: {reason}"),
            Some(preview_card(view, false)),
        )
        .await);
    };
    let now = Utc::now();
    let request = EnqueueRequest {
        message_id: None,
        rendered_text: text,
        priority: Priority::Normal,
        interrupting: false,
        not_before: Some(now),
        expires_at: Some(now + SEND_EXPIRY),
        repeat_count: 1,
        repeat_interval: std::time::Duration::ZERO,
        created_by: Some(current.user.id),
    };
    match state.queue.enqueue(request).await {
        Ok(_) => {
            audit(
                &state,
                current.user.id,
                "bulletin_send",
                "biuletyn na żądanie",
            )
            .await;
            if current.htmx {
                let response = render_with(&state, &current, StatusCode::OK, |page| {
                    page.preview = Some(preview_card(view, false));
                })
                .await;
                return Ok(htmx::ok(response, "Biuletyn dodany do kolejki"));
            }
            Ok(Redirect::to("/queue").into_response())
        }
        Err(err) => Ok(db_error("dodanie biuletynu do kolejki zawiodło", &err)),
    }
}

#[cfg(test)]
mod tests {
    use crate::AppState;
    use crate::router;
    use crate::state::{BulletinPreview, BulletinPreviewView};
    use crate::tests::{login, state};
    use aars_core::bulletin_layout::{LAYOUT_KEY, Layout};
    use aars_core::settings::SettingsRepo;
    use aars_core::user::{Role, UserRepo};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
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

    struct Reply {
        status: StatusCode,
        location: Option<String>,
        trigger: Option<String>,
        body: String,
    }

    async fn send(app: &Router, cookie: &str, path: &str, body: String, htmx: bool) -> Reply {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if htmx {
            request = request.header("HX-Request", "true");
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let (status, location, trigger) = {
            let header_text = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            (
                response.status(),
                header_text("location"),
                header_text("hx-trigger"),
            )
        };
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        Reply {
            status,
            location,
            trigger,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn post(app: &Router, cookie: &str, csrf: &str, path: &str, fields: &str) -> Reply {
        send(app, cookie, path, format!("csrf={csrf}&{fields}"), false).await
    }

    async fn hx_post(app: &Router, cookie: &str, csrf: &str, path: &str, fields: &str) -> Reply {
        send(app, cookie, path, format!("csrf={csrf}&{fields}"), true).await
    }

    fn urlencode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    async fn stored_layout(state: &AppState) -> Option<String> {
        SettingsRepo::new(state.pool.clone())
            .kv_get(LAYOUT_KEY)
            .await
            .unwrap()
    }

    fn preview_of(view: BulletinPreviewView) -> BulletinPreview {
        Arc::new(move |_| {
            let view = view.clone();
            Box::pin(async move { Ok(view) })
        })
    }

    /// Podgląd, który zapamiętuje, z jakim układem go wołano.
    fn recording(view: BulletinPreviewView) -> (BulletinPreview, Arc<Mutex<Vec<Option<Layout>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let preview: BulletinPreview = Arc::new(move |layout| {
            log.lock().unwrap().push(layout);
            let view = view.clone();
            Box::pin(async move { Ok(view) })
        });
        (preview, seen)
    }

    fn sample_view() -> BulletinPreviewView {
        BulletinPreviewView {
            text: Some("Biuletyn stacji SP9XYZ. [[pauza:700]] 07:00. Koniec biuletynu.".to_owned()),
            parts: vec![
                ("Wstęp".to_owned(), "w biuletynie (24 znaków)".to_owned()),
                (
                    "Pogoda bieżąca".to_owned(),
                    "pominięta — brak pozycji stacji".to_owned(),
                ),
            ],
            skipped_reason: None,
            part_chars: vec![Some(24), None],
        }
    }

    #[tokio::test]
    async fn a_viewer_has_no_access_to_the_editor() {
        let state = state()
            .await
            .with_bulletin_preview(preview_of(sample_view()));
        let (app, cookie, csrf, _) = app_as(Role::Viewer, state).await;
        assert_eq!(
            get(&app, &cookie, "/bulletin").await.0,
            StatusCode::FORBIDDEN
        );
        for path in ["/bulletin/preview", "/bulletin/send", "/bulletin/layout"] {
            assert_eq!(
                post(&app, &cookie, &csrf, path, "").await.status,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn an_operator_sees_listens_and_sends_but_cannot_save() {
        let (app, cookie, csrf, state) = app_as(Role::Operator, state().await).await;
        let (status, html) = get(&app, &cookie, "/bulletin").await;
        assert_eq!(status, StatusCode::OK);
        for needle in [
            "Klocki w kolejności nadawania",
            "Obowiązujące ostrzeżenia",
            "Warunki propagacji KF",
            "co 3. cykl",
            "action=\"/bulletin/preview\"",
            "action=\"/bulletin/send\"",
            "Nadaj teraz",
            "id=\"bulletin-timeline\"",
            "Układ zmienia administrator",
            "aria-current=\"page\"",
        ] {
            assert!(html.contains(needle), "brak {needle}: {html}");
        }
        for absent in [
            "action=\"/bulletin/layout\"",
            "name=\"layout\"",
            "Zapisz",
            "<template id=\"blk-template\">",
            "name=\"mode\"",
        ] {
            assert!(!html.contains(absent), "{absent}: {html}");
        }
        let json = urlencode(r#"{"bloki":[{"rodzaj":"wody"}]}"#);
        let reply = post(
            &app,
            &cookie,
            &csrf,
            "/bulletin/layout",
            &format!("layout={json}"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(stored_layout(&state).await, None);
    }

    #[tokio::test]
    async fn an_admin_gets_the_full_editor() {
        let (app, cookie, _, _) = app_as(Role::Admin, state().await).await;
        let (_, html) = get(&app, &cookie, "/bulletin").await;
        for needle in [
            "action=\"/bulletin/layout\"",
            "data-editable=\"1\"",
            "<textarea id=\"bulletin-layout\" name=\"layout\"",
            "&#34;pogoda_teraz&#34;",
            "<template id=\"blk-template\">",
            "data-act=\"up\"",
            "data-act=\"down\"",
            "data-act=\"grip\"",
            "data-act=\"add-text\"",
            "Przywróć domyślny",
            "Zapisz",
            "name=\"mode\" value=\"cykl\" checked",
            "name=\"interval_min\"",
            "name=\"times\"",
            // katalog klocków dla JS: polskie nazwy i opisy
            "&#34;label&#34;:&#34;Wschód i zachód słońca&#34;",
            "{znak}",
        ] {
            assert!(html.contains(needle), "brak {needle}: {html}");
        }
        // Zapisz stoi w DOM przed „Przywróć domyślny”: Enter w polu zapisuje.
        assert!(
            html.find("class=\"savebar-save\"").unwrap() < html.find("name=\"reset\"").unwrap()
        );
    }

    #[tokio::test]
    async fn a_valid_layout_is_saved_with_the_mode_and_audited() {
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        let json = r#"{"bloki":[{"rodzaj":"wstep","tekst":"Tu {znak}.","pauza_po_ms":1200},{"rodzaj":"wody","kiedy":{"typ":"w_godzinach","od":"06:00","do":"22:00"}}]}"#;
        let fields = format!(
            "mode=cykl&interval_min=15&times=07%3A00&layout={}",
            urlencode(json)
        );
        let reply = post(&app, &cookie, &csrf, "/bulletin/layout", &fields).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        assert_eq!(reply.location.as_deref(), Some("/bulletin"));
        assert_eq!(
            stored_layout(&state).await.unwrap(),
            Layout::parse(json).unwrap().to_json()
        );
        let kv = SettingsRepo::new(state.pool.clone());
        assert_eq!(
            kv.kv_get("module:bulletin:interval_min")
                .await
                .unwrap()
                .as_deref(),
            Some("15")
        );
        let audit: Vec<String> =
            sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = 'module_settings'")
                .fetch_all(&state.pool)
                .await
                .unwrap();
        assert_eq!(audit, ["bulletin: interval_min, times, layout"]);

        // Zapisany układ wraca w edytorze: karty do odczytu i JSON w polu.
        let (_, html) = get(&app, &cookie, "/bulletin").await;
        assert!(html.contains("pauza po 1,2 s"), "{html}");
        assert!(html.contains("w godzinach 06:00–22:00"), "{html}");
        assert_eq!(html.matches("class=\"blk blk-static").count(), 2, "{html}");

        // htmx: karta formularza i powiadomienie; drugi raz — bez zmian.
        let reply = hx_post(&app, &cookie, &csrf, "/bulletin/layout", &fields).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains("id=\"bulletin-form\"") && !reply.body.contains("<html"));
        assert!(reply.trigger.unwrap().contains("Bez zmian"));
    }

    #[tokio::test]
    async fn an_error_in_a_block_is_shown_at_that_block_and_nothing_changes() {
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        let bad = r#"{"bloki":[{"rodzaj":"wody"},{"rodzaj":"tekst","tekst":"Tu {stacja}."}]}"#;
        let fields = format!("mode=cykl&interval_min=10&layout={}", urlencode(bad));
        let reply = post(&app, &cookie, &csrf, "/bulletin/layout", &fields).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        let html = reply.body;
        assert!(
            html.contains("class=\"blk blk-static k-tekst is-invalid\" id=\"blk-2\""),
            "{html}"
        );
        assert!(
            html.contains("<p class=\"error\" id=\"blk-2-error\">nieznana zmienna „{stacja}”"),
            "{html}"
        );
        assert!(html.contains("data-error-block=\"2\""), "{html}");
        // wpisany układ zostaje w polu, baza bez zmian
        assert!(html.contains("{stacja}"), "{html}");
        assert!(html.contains("data-dirty=\"1\""), "{html}");
        assert_eq!(stored_layout(&state).await, None);

        // htmx: 422 z kartą formularza i powiadomieniem o błędzie
        let reply = hx_post(&app, &cookie, &csrf, "/bulletin/layout", &fields).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(reply.body.contains("id=\"blk-2-error\""));
        let trigger = reply.trigger.unwrap();
        assert!(trigger.contains("\"kind\":\"error\""), "{trigger}");
    }

    #[tokio::test]
    async fn unreadable_json_keeps_the_entered_text_under_the_field() {
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        let reply = post(
            &app,
            &cookie,
            &csrf,
            "/bulletin/layout",
            "layout=%7Bzepsute",
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(
            reply.body.contains(
                "<p class=\"error\" id=\"bulletin-layout-error\">układ biuletynu: niepoprawny zapis"
            ),
            "{}",
            reply.body
        );
        assert!(
            reply.body.contains(">{zepsute</textarea>"),
            "{}",
            reply.body
        );
        assert!(
            reply
                .body
                .contains("aria-describedby=\"bulletin-layout-error\"")
        );
        assert_eq!(stored_layout(&state).await, None);
        // błąd interwału stoi przy polu interwału
        let reply = post(&app, &cookie, &csrf, "/bulletin/layout", "interval_min=3").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(reply.body.contains("value=\"3\" required aria-invalid=\"true\" aria-describedby=\"bulletin-interval-error\""), "{}", reply.body);
    }

    #[tokio::test]
    async fn restoring_the_default_stores_it_empty() {
        let (app, cookie, csrf, state) = app_as(Role::Admin, state().await).await;
        let json = urlencode(r#"{"bloki":[{"rodzaj":"wody"}]}"#);
        post(
            &app,
            &cookie,
            &csrf,
            "/bulletin/layout",
            &format!("layout={json}"),
        )
        .await;
        assert!(stored_layout(&state).await.is_some_and(|v| !v.is_empty()));
        let reply = post(
            &app,
            &cookie,
            &csrf,
            "/bulletin/layout",
            &format!("layout={json}&reset=1"),
        )
        .await;
        // Bez JS pierwszy krok tylko pyta i nic nie zapisuje.
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert!(
            reply.body.contains("Przywrócić układ domyślny?"),
            "{}",
            reply.body
        );
        assert!(
            reply.body.contains("name=\"reset\" value=\"confirm\""),
            "{}",
            reply.body
        );
        assert!(stored_layout(&state).await.is_some_and(|v| !v.is_empty()));
        let reply = post(&app, &cookie, &csrf, "/bulletin/layout", "reset=confirm").await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        assert_eq!(stored_layout(&state).await.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn an_oversized_layout_is_refused_before_parsing() {
        let (preview, seen) = recording(sample_view());
        let state = state().await.with_bulletin_preview(preview);
        let (app, cookie, csrf, state) = app_as(Role::Admin, state).await;
        // poprawny JSON z bardzo wieloma klockami
        let many = format!(
            "{{\"bloki\":[{}]}}",
            vec![r#"{"rodzaj":"tekst","tekst":"x"}"#; 2000].join(",")
        );
        assert!(many.len() > super::MAX_LAYOUT_BYTES);
        let fields = format!("layout={}", urlencode(&many));
        let reply = post(&app, &cookie, &csrf, "/bulletin/layout", &fields).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(
            reply.body.contains("najwyżej 32 KiB"),
            "{}",
            &reply.body[..2000.min(reply.body.len())]
        );
        assert!(reply.body.len() < 200_000, "bez rysowania 2000 klocków");
        assert_eq!(stored_layout(&state).await, None);
        let reply = post(&app, &cookie, &csrf, "/bulletin/preview", &fields).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(seen.lock().unwrap().is_empty());
        // ponad 24 klocki, ale w limicie bajtów: błąd bez rysowania wpisanych klocków
        let thirty = format!(
            "{{\"bloki\":[{}]}}",
            vec![r#"{"rodzaj":"tekst","tekst":"x"}"#; 30].join(",")
        );
        let reply = post(
            &app,
            &cookie,
            &csrf,
            "/bulletin/layout",
            &format!("layout={}", urlencode(&thirty)),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(reply.body.contains("najwyżej 24 klocków"), "{}", reply.body);
        assert!(reply.body.matches("class=\"blk blk-static").count() <= 24);
    }

    #[tokio::test]
    async fn an_htmx_save_also_refreshes_the_next_airing() {
        let (app, cookie, csrf, _) = app_as(Role::Admin, state().await).await;
        let (_, html) = get(&app, &cookie, "/bulletin").await;
        assert!(
            html.contains("hx-select-oob=\"#bulletin-status\""),
            "{html}"
        );
        let reply = hx_post(
            &app,
            &cookie,
            &csrf,
            "/bulletin/layout",
            "mode=pory&times=08%3A00",
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(
            reply.body.contains("id=\"bulletin-status\""),
            "{}",
            reply.body
        );
    }

    #[tokio::test]
    async fn the_preview_of_an_unsaved_layout_does_not_touch_the_database() {
        let (preview, seen) = recording(sample_view());
        let state = state().await.with_bulletin_preview(preview);
        let (app, cookie, csrf, state) = app_as(Role::Admin, state).await;
        let json = r#"{"bloki":[{"rodzaj":"wstep","tekst":"Tu {znak}."},{"rodzaj":"slonce"}]}"#;
        let reply = hx_post(
            &app,
            &cookie,
            &csrf,
            "/bulletin/preview",
            &format!("layout={}", urlencode(json)),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert!(reply.body.contains("id=\"bulletin-preview\""));
        assert!(
            reply.body.contains("z układu w edytorze (niezapisanego)"),
            "{}",
            reply.body
        );
        assert!(
            reply.body.contains("data-chars=\"[24,null]\""),
            "{}",
            reply.body
        );
        assert!(reply.trigger.unwrap().contains("Podgl"));
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [Some(Layout::parse(json).unwrap())]
        );
        assert_eq!(
            stored_layout(&state).await,
            None,
            "podgląd nic nie zapisuje"
        );
        assert!(
            state.queue.pending(10).await.unwrap().is_empty(),
            "ani nie nadaje"
        );

        // bez pola — zapisany układ; puste pole — domyślny
        post(&app, &cookie, &csrf, "/bulletin/preview", "").await;
        post(&app, &cookie, &csrf, "/bulletin/preview", "layout=").await;
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls[1], None);
        assert_eq!(calls[2], Some(Layout::default_layout()));

        // błędny układ: odmowa z treścią błędu, podgląd się nie składa
        let bad = urlencode(r#"{"bloki":[{"rodzaj":"tekst","tekst":""}]}"#);
        let reply = hx_post(
            &app,
            &cookie,
            &csrf,
            "/bulletin/preview",
            &format!("layout={bad}"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            reply.body.contains("klocek 1 (Własny tekst): wpisz tekst"),
            "{}",
            reply.body
        );
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn an_operator_previews_and_listens() {
        let state = state()
            .await
            .with_bulletin_preview(preview_of(sample_view()));
        let (app, cookie, csrf, _) = app_as(Role::Operator, state).await;
        let reply = post(&app, &cookie, &csrf, "/bulletin/preview", "").await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        for needle in [
            "Biuletyn stacji SP9XYZ. ⏸ 0,7 s 07:00. Koniec biuletynu.",
            "value=\"Biuletyn stacji SP9XYZ. [[pauza:700]] 07:00. Koniec biuletynu.\"",
            "data-preview-text=\"bulletin-text\"",
            "Odsłuchaj",
            "Pogoda bieżąca",
            "pominięta — brak pozycji stacji",
            "z zapisanego układu",
        ] {
            assert!(reply.body.contains(needle), "brak {needle}: {}", reply.body);
        }
    }

    #[tokio::test]
    async fn a_bulletin_with_nothing_to_say_shows_the_reason() {
        let view = BulletinPreviewView {
            text: None,
            parts: vec![("Warunki propagacji KF".to_owned(), "błąd — x".to_owned())],
            skipped_reason: Some("brak danych".to_owned()),
            part_chars: vec![None],
        };
        let state = state().await.with_bulletin_preview(preview_of(view));
        let (app, cookie, csrf, state) = app_as(Role::Operator, state).await;
        let reply = post(&app, &cookie, &csrf, "/bulletin/preview", "").await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert!(
            reply.body.contains("Nic do nadania: brak danych"),
            "{}",
            reply.body
        );
        assert!(reply.body.contains("błąd — x"), "{}", reply.body);
        assert!(!reply.body.contains("Odsłuchaj"), "{}", reply.body);
        // nadanie: nic nie idzie do kolejki, karta podaje powód
        let reply = post(&app, &cookie, &csrf, "/bulletin/send", "").await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert!(
            reply.body.contains("Nic do nadania: brak danych"),
            "{}",
            reply.body
        );
        assert!(state.queue.pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sending_now_queues_the_saved_layout_and_ignores_the_editor() {
        let (preview, seen) = recording(sample_view());
        let state = state().await.with_bulletin_preview(preview);
        let (app, cookie, csrf, state) = app_as(Role::Operator, state).await;
        let json = urlencode(r#"{"bloki":[{"rodzaj":"wody"}]}"#);
        let reply = post(
            &app,
            &cookie,
            &csrf,
            "/bulletin/send",
            &format!("layout={json}"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        assert_eq!(reply.location.as_deref(), Some("/queue"));
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [None],
            "nadaje się zapisany układ"
        );
        let pending = state.queue.pending(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        let item = &pending[0];
        assert_eq!(
            item.rendered_text,
            "Biuletyn stacji SP9XYZ. [[pauza:700]] 07:00. Koniec biuletynu."
        );
        assert_eq!(item.priority, aars_core::priority::Priority::Normal);
        assert!(!item.interrupting);
        let left = item.expires_at.expect("ważność") - chrono::Utc::now();
        assert!(
            left > chrono::TimeDelta::minutes(14) && left <= chrono::TimeDelta::minutes(15),
            "{left}"
        );
        let audit: Vec<String> =
            sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = 'bulletin_send'")
                .fetch_all(&state.pool)
                .await
                .unwrap();
        assert_eq!(audit, ["biuletyn na żądanie"]);
    }

    #[tokio::test]
    async fn sending_and_previewing_need_csrf() {
        let (preview, seen) = recording(sample_view());
        let state = state().await.with_bulletin_preview(preview);
        let (app, cookie, _, state) = app_as(Role::Admin, state).await;
        for path in ["/bulletin/send", "/bulletin/preview", "/bulletin/layout"] {
            let reply = post(&app, &cookie, "zly", path, "layout=").await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{path}");
        }
        assert!(seen.lock().unwrap().is_empty());
        assert!(state.queue.pending(10).await.unwrap().is_empty());
        assert_eq!(stored_layout(&state).await, None);
    }

    #[tokio::test]
    async fn without_a_callback_the_preview_is_unavailable() {
        let (app, cookie, csrf, _) = app_as(Role::Operator, state().await).await;
        let reply = post(&app, &cookie, &csrf, "/bulletin/preview", "").await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            reply.body.contains(
                "biuletyn nie wystartował — zobacz dziennik systemowy (journalctl -u aars)"
            ),
            "{}",
            reply.body
        );
        let reply = post(&app, &cookie, &csrf, "/bulletin/send", "").await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_preview_error_is_shown() {
        let preview: BulletinPreview = Arc::new(|_| Box::pin(async { Err("x".to_owned()) }));
        let state = state().await.with_bulletin_preview(preview);
        let (app, cookie, csrf, _) = app_as(Role::Operator, state).await;
        let reply = post(&app, &cookie, &csrf, "/bulletin/preview", "").await;
        assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
        assert!(
            reply.body.contains("podgląd biuletynu zawiódł: x"),
            "{}",
            reply.body
        );
    }

    #[tokio::test]
    async fn one_bulletin_preview_at_a_time() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let preview: BulletinPreview = {
            let (entered, release, calls) = (entered.clone(), release.clone(), calls.clone());
            Arc::new(move |_| {
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
                post(&app, &cookie, &csrf, "/bulletin/preview", "")
                    .await
                    .status
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .expect("pierwszy podgląd musi ruszyć");
        let reply = post(&app, &cookie, &csrf, "/bulletin/preview", "").await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            reply
                .body
                .contains("podgląd biuletynu już trwa, spróbuj za chwilę")
        );
        let reply = post(&app, &cookie, &csrf, "/bulletin/send", "").await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "drugi podgląd nie składa biuletynu"
        );
        release.notify_one();
        assert_eq!(slow.await.unwrap(), StatusCode::OK);
        assert_eq!(
            post(&app, &cookie, &csrf, "/bulletin/preview", "")
                .await
                .status,
            StatusCode::OK
        );
    }

    #[test]
    fn the_estimate_counts_text_typical_lengths_and_merged_pauses() {
        use aars_core::bulletin_layout::{Block, BlockKind};
        let layout = Layout {
            blocks: vec![
                Block {
                    text: "x".repeat(99),
                    pause_after_ms: 1500,
                    ..Block::new(BlockKind::Tekst)
                },
                Block {
                    pause_before_ms: 500,
                    ..Block::new(BlockKind::Slonce)
                },
                Block {
                    enabled: false,
                    ..Block::new(BlockKind::Prognoza)
                },
            ],
        };
        // (99 + 1) + (70 + 1) znaków ≈ 17 s, pauza 1,5 + 0,5 = 2 s
        assert_eq!(super::estimate_s(&layout, ""), 19);
        assert_eq!(super::seconds(700), "0,7 s");
    }
}
