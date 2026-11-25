//! Sprzęt stacji (admin): PTT, audio, głos, adres panelu, restart i testy
//! (etap 9, R2, R8–R12).

use aars_core::config::{PttBackendKind, SerialPttLine};
use aars_core::dispatcher::{StationTest, TEST_PTT_RANGE, TestOutcome};
use aars_core::hardware::probe::{HostLists, check_against_host, friendly_voice_name, host_lists, resolve_model};
use aars_core::hardware::{FieldError, HardwareRepo, HardwareSettings, SAMPLE_RATES};
use aars_core::interlock::RefusalReason;
use aars_core::settings::{SettingsRepo, StationSettings};
use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::audit;
use crate::auth::{AuthRejection, CurrentUser, require_csrf, require_role};
use crate::chrome::Chrome;
use crate::render::{db_error, page};
use crate::state::AppState;

/// Odpowiedź, gdy panel działa bez stacji (restart i testy).
const NO_STATION: &str = "stacja nie działa w tym procesie";
const VOX_NOTE: &str = "Przy VOX oba testy są wyłączone: test PTT nie ma czego sprawdzić, \
                        a dźwięk testowy poszedłby na antenę. Właściwy test to krótkie nadanie z";
/// Treść błędu dyspozytora, gdy odpowiedź nie przyszła w czasie (porzucone żądanie).
const NO_REPLY: &str = "brak odpowiedzi stacji";

pub struct Field {
    name: &'static str,
    label: &'static str,
    /// `text`, `number`, `select`, `radio` albo `checkbox`.
    kind: &'static str,
    value: String,
    hint: String,
    error: Option<String>,
    /// Dla `select` i `radio`: (wartość, etykieta).
    options: Vec<(String, String)>,
    note: Option<String>,
}

#[derive(Template)]
#[template(path = "hardware.html")]
struct HardwarePage {
    chrome: Chrome,
    origin: String,
    apply_error: Option<String>,
    pending: Vec<String>,
    has_station: bool,
    fields: Vec<Field>,
    warnings: Vec<String>,
    notice: Option<String>,
    form_failed: bool,
    result: Option<(&'static str, String)>,
    tests_available: bool,
    vox: bool,
    vox_note: &'static str,
    default_seconds: u64,
}

/// Wartości formularza tak, jak wpisał je użytkownik.
#[derive(Debug, Default, Clone)]
struct FormInput {
    bind: String,
    audio_device: String,
    sample_rate: String,
    gain_db: String,
    ptt_backend: String,
    tx_tail_ms: String,
    gpio_chip: String,
    gpio_line: String,
    gpio_active_low: bool,
    gpio_inhibit_line: String,
    serial_port: String,
    serial_line: String,
    voice_model: String,
}

impl FormInput {
    fn from_settings(s: &HardwareSettings) -> Self {
        let pairs = s.to_pairs();
        let get = |k: &str| pairs[k].clone();
        Self {
            bind: get("server.bind"),
            audio_device: get("audio.device"),
            sample_rate: get("audio.sample_rate"),
            gain_db: get("audio.gain_db"),
            ptt_backend: get("ptt.backend"),
            tx_tail_ms: get("ptt.tx_tail_ms"),
            gpio_chip: get("ptt.gpio.chip"),
            gpio_line: get("ptt.gpio.line"),
            gpio_active_low: s.gpio_active_low,
            gpio_inhibit_line: get("ptt.gpio.inhibit_line"),
            serial_port: get("ptt.serial.port"),
            serial_line: get("ptt.serial.line"),
            voice_model: get("tts.model"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct HardwareForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    bind: String,
    #[serde(default)]
    audio_device: String,
    #[serde(default)]
    sample_rate: String,
    #[serde(default)]
    gain_db: String,
    #[serde(default)]
    ptt_backend: String,
    #[serde(default)]
    tx_tail_ms: String,
    #[serde(default)]
    gpio_chip: String,
    #[serde(default)]
    gpio_line: String,
    #[serde(default)]
    gpio_active_low: String,
    #[serde(default)]
    gpio_inhibit_line: String,
    #[serde(default)]
    serial_port: String,
    #[serde(default)]
    serial_line: String,
    #[serde(default)]
    voice_model: String,
}

impl HardwareForm {
    fn input(&self) -> FormInput {
        FormInput {
            bind: self.bind.clone(),
            audio_device: self.audio_device.clone(),
            sample_rate: self.sample_rate.clone(),
            gain_db: self.gain_db.clone(),
            ptt_backend: self.ptt_backend.clone(),
            tx_tail_ms: self.tx_tail_ms.clone(),
            gpio_chip: self.gpio_chip.clone(),
            gpio_line: self.gpio_line.clone(),
            gpio_active_low: self.gpio_active_low == "1",
            gpio_inhibit_line: self.gpio_inhibit_line.clone(),
            serial_port: self.serial_port.clone(),
            serial_line: self.serial_line.clone(),
            voice_model: self.voice_model.clone(),
        }
    }
}

fn field_error(field: &'static str, message: &str) -> FieldError {
    FieldError { field, message: message.to_owned() }
}

/// Tekst formularza na typy; błędy czytelne dla człowieka, bez zgadywania.
fn parse_form(i: &FormInput) -> Result<HardwareSettings, Vec<FieldError>> {
    let mut errors = Vec::new();
    let mut number = |field: &'static str, text: &str, what: &str| -> Option<u64> {
        match text.trim().parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                errors.push(field_error(field, &format!("{what}: podaj liczbę całkowitą")));
                None
            }
        }
    };
    let sample_rate = number("audio.sample_rate", &i.sample_rate, "częstotliwość próbkowania");
    let tx_tail_ms = number("ptt.tx_tail_ms", &i.tx_tail_ms, "opóźnienie PTT");
    let gpio_line = number("ptt.gpio.line", &i.gpio_line, "linia GPIO");
    let inhibit = match i.gpio_inhibit_line.trim() {
        "" => Some(None),
        text => number("ptt.gpio.inhibit_line", text, "linia blokująca").map(Some),
    };
    let gain_db = match i.gain_db.trim().replace(',', ".").parse::<f32>() {
        Ok(g) if g.is_finite() => Some(g),
        _ => {
            errors.push(field_error("audio.gain_db", "poziom wyjścia: podaj liczbę, np. -6 albo -4,5"));
            None
        }
    };
    let ptt_backend = match i.ptt_backend.as_str() {
        "vox" => Some(PttBackendKind::Vox),
        "gpio" => Some(PttBackendKind::Gpio),
        "serial" => Some(PttBackendKind::Serial),
        _ => {
            errors.push(field_error("ptt.backend", "sterowanie PTT: VOX, GPIO albo port szeregowy"));
            None
        }
    };
    let serial_line = match i.serial_line.as_str() {
        "dtr" => Some(SerialPttLine::Dtr),
        "rts" => Some(SerialPttLine::Rts),
        _ => {
            errors.push(field_error("ptt.serial.line", "linia portu szeregowego: DTR albo RTS"));
            None
        }
    };
    let narrow = |v: Option<u64>| v.and_then(|n| u32::try_from(n).ok());
    let (Some(sample_rate), Some(tx_tail_ms), Some(gpio_line), Some(gpio_inhibit_line), Some(gain_db), Some(ptt_backend), Some(serial_line)) = (
        narrow(sample_rate),
        tx_tail_ms,
        narrow(gpio_line),
        inhibit.map(|o| o.and_then(|n| u32::try_from(n).ok())),
        gain_db,
        ptt_backend,
        serial_line,
    ) else {
        if errors.is_empty() {
            errors.push(field_error("ptt.gpio.line", "liczba poza zakresem"));
        }
        return Err(errors);
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    let settings = HardwareSettings {
        bind: i.bind.trim().to_owned(),
        audio_device: i.audio_device.trim().to_owned(),
        sample_rate,
        gain_db,
        ptt_backend,
        tx_tail_ms,
        gpio_chip: i.gpio_chip.trim().to_owned(),
        gpio_line,
        gpio_active_low: i.gpio_active_low,
        gpio_inhibit_line,
        serial_port: i.serial_port.trim().to_owned(),
        serial_line,
        voice_model: i.voice_model.trim().to_owned(),
    };
    settings.validate()?;
    Ok(settings)
}

/// Klucze stacji, które wchodzą po restarcie (R3).
fn station_diff(running: &StationSettings, stored: &StationSettings) -> Vec<String> {
    let mut keys = Vec::new();
    let mut note = |differs: bool, key: &str| {
        if differs {
            keys.push(key.to_owned());
        }
    };
    note(running.callsign != stored.callsign, "callsign");
    note(running.id_mode != stored.id_mode, "id_mode");
    note(running.id_interval_s != stored.id_interval_s, "id_interval_s");
    note(running.tx_max_duration_s != stored.tx_max_duration_s, "tx_max_duration_s");
    note(running.misfire_grace_s != stored.misfire_grace_s, "misfire_grace_s");
    keys
}

/// Co zapisano, a jeszcze nie działa. Pusta, gdy panel nie ma stacji.
pub fn pending_restart(
    state: &AppState,
    stored_hw: &HardwareSettings,
    stored_station: &StationSettings,
) -> Vec<String> {
    let Some(station) = &state.station else {
        return Vec::new();
    };
    let mut out = match station.running_hardware() {
        Some(running) => running.diff(stored_hw),
        None => HardwareSettings::from_config(&state.base_config).diff(stored_hw),
    };
    out.extend(station_diff(&station.running_station(), stored_station));
    out
}

/// Sprzęt zapisany w panelu nałożony na plik; ostrzeżenia o nieczytelnych
/// kluczach idą do logu (strona pokazuje wartości z pliku).
async fn stored_hardware(state: &AppState) -> Result<(HardwareSettings, bool), aars_core::error::DbError> {
    let overrides = HardwareRepo::new(state.pool.clone()).overrides().await?;
    let (hw, warnings) = HardwareSettings::from_pairs(&state.base_config, &overrides);
    for w in warnings {
        tracing::warn!(%w, "zapisane ustawienie sprzętu nieczytelne");
    }
    Ok((hw, overrides.is_empty()))
}

/// Dla strony ustawień: lista „czeka na restart” przy podanych ustawieniach stacji.
pub async fn pending_for(state: &AppState, station: &StationSettings) -> Vec<String> {
    match stored_hardware(state).await {
        Ok((hw, _)) => pending_restart(state, &hw, station),
        Err(err) => {
            tracing::error!(%err, "odczyt ustawień sprzętu zawiódł");
            Vec::new()
        }
    }
}

async fn lists(state: &AppState) -> Option<HostLists> {
    let probe = state.probe.clone()?;
    let dir = state.base_config.tts.voices_dir.clone();
    tokio::task::spawn_blocking(move || host_lists(probe.as_ref(), &dir))
        .await
        .map_err(|err| tracing::error!(%err, "listowanie urządzeń zawiodło"))
        .ok()
}

fn opts(items: &[String]) -> Vec<(String, String)> {
    items.iter().map(|i| (i.clone(), i.clone())).collect()
}

/// Lista rozwijana, a gdy listy brak albo zawiodła — pole tekstowe z notką.
/// Bieżąca wartość zawsze jest na liście, żeby nie zniknęła po błędzie.
fn choice(
    mut options: Vec<(String, String)>,
    failed: Option<String>,
    unavailable: &str,
    current: &str,
) -> (&'static str, Vec<(String, String)>, Option<String>) {
    match failed {
        Some(why) => ("text", Vec::new(), Some(format!("Lista urządzeń niedostępna: {why}"))),
        None if unavailable.is_empty() => {
            if !current.is_empty() && !options.iter().any(|(v, _)| v == current) {
                options.push((current.to_owned(), current.to_owned()));
            }
            ("select", options, None)
        }
        None => ("text", Vec::new(), Some(unavailable.to_owned())),
    }
}

fn build_fields(
    input: &FormInput,
    errors: &[FieldError],
    file: &HardwareSettings,
    lists: Option<&HostLists>,
) -> Vec<Field> {
    let file_pairs = file.to_pairs();
    let hint = |key: &str| {
        let v = file_pairs.get(key).cloned().unwrap_or_default();
        format!("w pliku: {}", if v.is_empty() { "brak" } else { &v })
    };
    let err_of = |key: &str| {
        let msgs: Vec<&str> = errors.iter().filter(|e| e.field == key).map(|e| e.message.as_str()).collect();
        (!msgs.is_empty()).then(|| msgs.join("; "))
    };
    let mut fields = Vec::new();
    let mut add = |name: &'static str,
                   label: &'static str,
                   key: &str,
                   kind: &'static str,
                   value: &str,
                   options: Vec<(String, String)>,
                   note: Option<String>| {
        fields.push(Field {
            name,
            label,
            kind,
            value: value.to_owned(),
            hint: hint(key),
            error: err_of(key),
            options,
            note,
        });
    };
    let no_list = "Brak listy urządzeń na tym komputerze — wpisz wartość ręcznie.";

    add("bind", "Adres panelu (adres:port, port od 1024)", "server.bind", "text", &input.bind, Vec::new(), None);

    let (kind, mut options, note) = match lists {
        Some(l) => {
            let mut o = vec![("default".to_owned(), "default".to_owned())];
            if let Ok(devices) = &l.audio {
                o.extend(opts(devices).into_iter().filter(|(v, _)| v != "default"));
            }
            choice(o, l.audio.as_ref().err().cloned(), "", &input.audio_device)
        }
        None => choice(Vec::new(), None, no_list, &input.audio_device),
    };
    if kind == "select" && options.is_empty() {
        options.push(("default".to_owned(), "default".to_owned()));
    }
    add("audio_device", "Urządzenie audio (wyjście)", "audio.device", kind, &input.audio_device, options, note);

    let rates: Vec<(String, String)> = SAMPLE_RATES
        .iter()
        .map(|r| (r.to_string(), format!("{r} Hz")))
        .collect();
    add("sample_rate", "Częstotliwość próbkowania", "audio.sample_rate", "select", &input.sample_rate, rates, None);
    add("gain_db", "Poziom wyjścia (dBFS, −30…0; przecinek dozwolony)", "audio.gain_db", "text", &input.gain_db, Vec::new(), None);

    let backends = vec![
        ("vox".to_owned(), "VOX".to_owned()),
        ("gpio".to_owned(), "GPIO".to_owned()),
        ("serial".to_owned(), "port szeregowy".to_owned()),
    ];
    add("ptt_backend", "Sterowanie PTT", "ptt.backend", "radio", &input.ptt_backend, backends, None);
    add("tx_tail_ms", "Opóźnienie PTT (ms, 0–2000)", "ptt.tx_tail_ms", "number", &input.tx_tail_ms, Vec::new(), None);

    let (kind, options, note) = match lists {
        Some(l) => {
            let o: Vec<(String, String)> = l
                .gpio
                .as_ref()
                .map(|chips| {
                    chips
                        .iter()
                        .map(|c| (c.path.clone(), format!("{} ({}, {} linii)", c.path, c.label, c.lines)))
                        .collect()
                })
                .unwrap_or_default();
            choice(o, l.gpio.as_ref().err().cloned(), "", &input.gpio_chip)
        }
        None => choice(Vec::new(), None, no_list, &input.gpio_chip),
    };
    add("gpio_chip", "Układ GPIO", "ptt.gpio.chip", kind, &input.gpio_chip, options, note);
    add("gpio_line", "Linia GPIO PTT", "ptt.gpio.line", "number", &input.gpio_line, Vec::new(), None);
    add(
        "gpio_active_low",
        "Linia PTT aktywna stanem niskim",
        "ptt.gpio.active_low",
        "checkbox",
        if input.gpio_active_low { "1" } else { "" },
        Vec::new(),
        None,
    );
    add(
        "gpio_inhibit_line",
        "Linia blokująca nadawanie (puste = brak)",
        "ptt.gpio.inhibit_line",
        "number",
        &input.gpio_inhibit_line,
        Vec::new(),
        None,
    );

    let (kind, options, note) = match lists {
        Some(l) => choice(
            l.serial.as_ref().map(|p| opts(p)).unwrap_or_default(),
            l.serial.as_ref().err().cloned(),
            "",
            &input.serial_port,
        ),
        None => choice(Vec::new(), None, no_list, &input.serial_port),
    };
    add("serial_port", "Port szeregowy", "ptt.serial.port", kind, &input.serial_port, options, note);
    let serial_lines = vec![("dtr".to_owned(), "DTR".to_owned()), ("rts".to_owned(), "RTS".to_owned())];
    add("serial_line", "Linia portu szeregowego", "ptt.serial.line", "select", &input.serial_line, serial_lines, None);

    let (kind, options, note) = match lists {
        // Bez żadnego głosu w katalogu lista byłaby pusta i nie dałoby się
        // wpisać modelu spoza niego (R8) — wtedy zwykłe pole.
        Some(l) if l.voices.is_empty() => choice(
            Vec::new(),
            None,
            "W katalogu głosów nie ma modeli — wpisz ścieżkę modelu .onnx ręcznie.",
            &input.voice_model,
        ),
        Some(l) => {
            let o = l.voices.iter().map(|v| (v.path.clone(), friendly_voice_name(&v.name))).collect();
            choice(o, None, "", &input.voice_model)
        }
        None => choice(Vec::new(), None, "Brak listy głosów — wpisz ścieżkę modelu .onnx ręcznie.", &input.voice_model),
    };
    add("voice_model", "Głos (model Piper)", "tts.model", kind, &input.voice_model, options, note);
    fields
}

#[derive(Default)]
struct Ctx {
    input: Option<FormInput>,
    errors: Vec<FieldError>,
    notice: Option<String>,
    result: Option<(&'static str, String)>,
}

fn server_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "błąd odczytu lub zapisu ustawień sprzętu").into_response()
}

async fn show(state: &AppState, current: &CurrentUser, status: StatusCode, ctx: Ctx) -> Response {
    let (stored, _) = match stored_hardware(state).await {
        Ok(v) => v,
        Err(err) => return db_error("odczyt ustawień sprzętu zawiódł", &err),
    };
    let station_settings = match SettingsRepo::new(state.pool.clone()).load().await {
        Ok(s) => s,
        Err(err) => return db_error("odczyt ustawień stacji zawiódł", &err),
    };
    let lists = lists(state).await;
    let mut warnings = Vec::new();
    if let Some(l) = &lists {
        let check = check_against_host(&stored, l);
        warnings.extend(check.warnings);
        warnings.extend(
            check
                .errors
                .into_iter()
                .map(|e| format!("zapisane ustawienie nie pasuje do tego komputera: {}", e.message)),
        );
    }
    // Zapisany model zniknął (np. darkman po aktualizacji): stacja gra Gosią.
    let resolved = resolve_model(&stored.voice_model, &state.base_config.tts.voices_dir);
    if let Some(missing) = resolved.replaced {
        warnings.push(format!(
            "model głosu {missing} nie istnieje — stacja używa głosu Gosia ({}); wybierz głos i zapisz",
            resolved.path
        ));
    }
    let form_failed = !ctx.errors.is_empty();
    let input = ctx.input.unwrap_or_else(|| FormInput::from_settings(&stored));
    let file = HardwareSettings::from_config(&state.base_config);
    let fields = build_fields(&input, &ctx.errors, &file, lists.as_ref());

    let (origin, apply_error) = match &state.station {
        Some(station) => {
            let status = station.apply_status();
            (status.origin.label().to_owned(), status.error)
        }
        None => ("brak stacji w tym procesie".to_owned(), None),
    };
    let vox = state
        .station
        .as_ref()
        .and_then(|s| s.running_hardware())
        .is_some_and(|hw| hw.ptt_backend == PttBackendKind::Vox);
    let has_station = state.station.is_some();
    page_response(
        status,
        &HardwarePage {
            chrome: Chrome::load(state, current).await,
            origin,
            apply_error,
            pending: pending_restart(state, &stored, &station_settings),
            has_station,
            fields,
            warnings,
            notice: ctx.notice,
            form_failed,
            result: ctx.result,
            tests_available: has_station && !vox,
            vox,
            vox_note: VOX_NOTE,
            default_seconds: 2,
        },
    )
}

fn page_response(status: StatusCode, template: &HardwarePage) -> Response {
    let mut response = page(template);
    if response.status() == StatusCode::OK {
        *response.status_mut() = status;
    }
    response
}

#[derive(Debug, Default, Deserialize)]
pub struct ShownQuery {
    #[serde(default)]
    saved: String,
    #[serde(default)]
    restart: String,
}

pub async fn page_view(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<ShownQuery>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, aars_core::user::Role::Admin)?;
    let mut notice = None;
    if query.saved == "1" {
        notice = Some("Zapisano. Zmiany wejdą w życie po restarcie stacji.".to_owned());
    } else if query.restart == "1" {
        let mut text = "Restart zlecony. Stacja dokończy bieżące nadanie i uruchomi się ponownie \
                        — odśwież stronę za kilka sekund."
            .to_owned();
        if let Ok((stored, _)) = stored_hardware(&state).await {
            let running_bind = state
                .station
                .as_ref()
                .and_then(|s| s.running_hardware())
                .map_or_else(|| state.base_config.server.bind.clone(), |hw| hw.bind);
            if stored.bind != running_bind {
                text.push_str(&format!(" Panel będzie pod adresem {}.", stored.bind));
            }
        }
        notice = Some(text);
    }
    Ok(show(&state, &current, StatusCode::OK, Ctx { notice, ..Ctx::default() }).await)
}

pub async fn save(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<HardwareForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, aars_core::user::Role::Admin)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;

    let input = form.input();
    let refuse = |errors: Vec<FieldError>| Ctx { input: Some(input.clone()), errors, ..Ctx::default() };

    let new = match parse_form(&input) {
        Ok(new) => new,
        Err(errors) => {
            return Ok(show(&state, &current, StatusCode::BAD_REQUEST, refuse(errors)).await);
        }
    };
    if let Some(l) = lists(&state).await {
        let check = check_against_host(&new, &l);
        if !check.errors.is_empty() {
            return Ok(show(&state, &current, StatusCode::BAD_REQUEST, refuse(check.errors)).await);
        }
    }

    let (stored, no_overrides) = match stored_hardware(&state).await {
        Ok(v) => v,
        Err(err) => return Ok(db_error("odczyt ustawień sprzętu zawiódł", &err)),
    };
    let changes = stored.diff(&new);
    // Bez różnic i bez zapisanych kluczy nie przypinamy wartości z pliku:
    // późniejsza edycja aars.toml nadal by działała.
    if !(changes.is_empty() && no_overrides)
        && let Err(err) = HardwareRepo::new(state.pool.clone()).save(&new).await
    {
        tracing::error!(%err, "zapis ustawień sprzętu zawiódł");
        return Ok(server_error());
    }
    if !changes.is_empty() {
        tracing::warn!(user = %current.user.username, "zmiana sprzętu z panelu");
        audit(&state, current.user.id, "hardware", &changes.join("; ")).await;
    }
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, "/settings/hardware?saved=1")]).into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct CsrfOnly {
    #[serde(default)]
    csrf: String,
}

pub async fn restart(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<CsrfOnly>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, aars_core::user::Role::Admin)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;
    let Some(station) = &state.station else {
        return Ok((StatusCode::SERVICE_UNAVAILABLE, NO_STATION).into_response());
    };
    station.request_restart();
    tracing::warn!(user = %current.user.username, "restart stacji zlecony z panelu");
    audit(&state, current.user.id, "station_restart", "zlecony").await;
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, "/settings/hardware?restart=1")]).into_response())
}

// --- Testy sprzętu (Task 9) ---

fn seconds_text(d: std::time::Duration) -> String {
    format!("{:.1}", d.as_secs_f64()).replace('.', ",")
}

fn refusal_text(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::TxDisabled => "nadawanie wyłączone (wyłącznik)",
        RefusalReason::Poisoned => "blokada watchdoga — zdejmij ją na pulpicie",
        RefusalReason::Inhibited => "blokada sprzętowa",
        RefusalReason::QuietHours => "okno ciszy",
        RefusalReason::ChannelBusy => "kanał zajęty",
    }
}

/// Wynik testu po polsku: klasa CSS i tekst (R10–R12).
pub fn outcome_text(test: &StationTest, outcome: &TestOutcome, device: &str) -> (&'static str, String) {
    match outcome {
        TestOutcome::Done { played, note } => match test {
            StationTest::Ptt { .. } => (
                "badge on",
                format!(
                    "Test PTT: nadajnik włączony na {} s i wyłączony. Wpis w dzienniku nadań.",
                    seconds_text(*played)
                ),
            ),
            StationTest::Audio { .. } => {
                let mut text = format!(
                    "Test audio: odtworzono {} s na urządzeniu {device} przy wyłączonym PTT.",
                    seconds_text(*played)
                );
                if let Some(note) = note {
                    text.push(' ');
                    text.push_str(note);
                }
                ("badge on", text)
            }
        },
        TestOutcome::Refused(reason) => ("error", format!("Odmowa: {}", refusal_text(*reason))),
        TestOutcome::Busy => (
            "error",
            "Stacja właśnie nadaje albo trwa inny test — spróbuj za chwilę.".to_owned(),
        ),
        TestOutcome::Unsupported(message) => ("error", (*message).to_owned()),
        TestOutcome::Failed(why) if why == NO_REPLY => (
            "error",
            format!(
                "Test nie powiódł się: {why}. Żądanie wygasło i zostanie porzucone — jeśli \
                 stacja jeszcze go nie rozpoczęła, radio nie zostanie włączone."
            ),
        ),
        TestOutcome::Failed(why) => ("error", format!("Test nie powiódł się: {why}")),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct PttTestForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    confirm: String,
    #[serde(default)]
    seconds: String,
}

async fn refuse_test(state: &AppState, current: &CurrentUser, message: &str) -> Response {
    show(
        state,
        current,
        StatusCode::BAD_REQUEST,
        Ctx { result: Some(("error", message.to_owned())), ..Ctx::default() },
    )
    .await
}

async fn finish_test(
    state: &AppState,
    current: &CurrentUser,
    test: StationTest,
    audit_action: &str,
    audit_prefix: &str,
) -> Response {
    let Some(station) = &state.station else {
        return (StatusCode::SERVICE_UNAVAILABLE, NO_STATION).into_response();
    };
    let device = station
        .running_hardware()
        .map_or_else(|| state.base_config.audio.device.clone(), |hw| hw.audio_device);
    let outcome = station.run_test(test.clone()).await;
    let (css, text) = outcome_text(&test, &outcome, &device);
    tracing::warn!(user = %current.user.username, action = audit_action, %text, "test sprzętu z panelu");
    audit(state, current.user.id, audit_action, &format!("{audit_prefix}{text}")).await;
    show(state, current, StatusCode::OK, Ctx { result: Some((css, text)), ..Ctx::default() }).await
}

pub async fn test_ptt(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<PttTestForm>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, aars_core::user::Role::Admin)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;
    if form.confirm != "tak" {
        return Ok(refuse_test(&state, &current, "Zaznacz potwierdzenie — test PTT włącza nadajnik.").await);
    }
    let seconds = match form.seconds.trim().parse::<u64>() {
        Ok(s) if TEST_PTT_RANGE.contains(&s) => s,
        _ => return Ok(refuse_test(&state, &current, "Czas testu: 1–5 s").await),
    };
    if state.station.is_none() {
        return Ok((StatusCode::SERVICE_UNAVAILABLE, NO_STATION).into_response());
    }
    let test = StationTest::Ptt { seconds, by: current.user.username.clone() };
    Ok(finish_test(&state, &current, test, "test_ptt", &format!("{seconds} s: ")).await)
}

pub async fn test_audio(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<CsrfOnly>,
) -> Result<Response, AuthRejection> {
    require_role(&current.user, aars_core::user::Role::Admin)?;
    require_csrf(&current.csrf_token, Some(&form.csrf))?;
    let test = StationTest::Audio { by: current.user.username.clone() };
    Ok(finish_test(&state, &current, test, "test_audio", "").await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testing::FakeStation;
    use crate::tests::login;
    use crate::router;
    use aars_core::hardware::probe::{FakeProbe, GpioChipInfo};
    use aars_core::interlock::RefusalReason;
    use aars_core::user::{Role, UserRepo};
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn probe() -> FakeProbe {
        FakeProbe {
            audio: Ok(vec!["hw:1,0".to_owned(), "USB Audio".to_owned()]),
            serial: Ok(vec!["/dev/ttyUSB0".to_owned()]),
            gpio: Ok(vec![GpioChipInfo {
                path: "/dev/gpiochip0".into(),
                label: "pinctrl".into(),
                lines: 28,
            }]),
        }
    }

    struct Env {
        app: Router,
        cookie: String,
        csrf: String,
        state: AppState,
        _voices: tempfile::TempDir,
    }

    async fn env_with(
        role: Role,
        probe: FakeProbe,
        station: Option<Arc<FakeStation>>,
    ) -> Env {
        let voices = tempfile::tempdir().unwrap();
        for f in ["pl_PL-test.onnx", "pl_PL-test.onnx.json"] {
            std::fs::write(voices.path().join(f), "x").unwrap();
        }
        let mut config = aars_core::config::Config::default();
        config.tts.voices_dir = voices.path().to_path_buf();
        config.tts.model = voices.path().join("pl_PL-test.onnx").to_string_lossy().into_owned();
        config.audio.device = "default".to_owned();
        let mut state = AppState::for_tests(aars_core::db::test_pool().await)
            .with_base_config(Arc::new(config.clone()))
            .with_probe(Arc::new(probe));
        if let Some(station) = station {
            state = state.with_station(station);
        }
        UserRepo::new(state.pool.clone()).create("u", "haslo", role).await.unwrap();
        let app = router(state.clone());
        let (cookie, csrf) = login(&app, "u", "haslo").await;
        Env { app, cookie, csrf, state, _voices: voices }
    }

    async fn env(role: Role) -> Env {
        env_with(role, probe(), None).await
    }

    fn fake(hw: Option<HardwareSettings>, edit: impl FnOnce(&mut FakeStation)) -> Arc<FakeStation> {
        let mut station = FakeStation::new(hw);
        edit(&mut station);
        Arc::new(station)
    }

    fn status(origin: aars_core::hardware::ConfigOrigin, error: &str) -> aars_core::hardware::ApplyStatus {
        aars_core::hardware::ApplyStatus { origin, error: Some(error.to_owned()) }
    }

    fn encode(v: &str) -> String {
        v.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    async fn send(env: &Env, method: &str, uri: &str, body: String) -> (StatusCode, String, Option<String>) {
        let mut builder = Request::builder().method(method).uri(uri).header(header::COOKIE, &env.cookie);
        if method == "POST" {
            builder = builder.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        let response = env.app.clone().oneshot(builder.body(Body::from(body)).unwrap()).await.unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned(), location)
    }

    async fn get(env: &Env, uri: &str) -> (StatusCode, String) {
        let (s, b, _) = send(env, "GET", uri, String::new()).await;
        (s, b)
    }

    /// Formularz z wartościami domyślnymi, z nadpisaniami.
    async fn post_hw(env: &Env, over: &[(&str, &str)]) -> (StatusCode, String, Option<String>) {
        let hw = HardwareSettings::from_config(&env.state.base_config);
        let i = FormInput::from_settings(&hw);
        let mut fields: Vec<(String, String)> = vec![
            ("csrf".into(), env.csrf.clone()),
            ("bind".into(), i.bind),
            ("audio_device".into(), i.audio_device),
            ("sample_rate".into(), i.sample_rate),
            ("gain_db".into(), i.gain_db),
            ("ptt_backend".into(), i.ptt_backend),
            ("tx_tail_ms".into(), i.tx_tail_ms),
            ("gpio_chip".into(), "/dev/gpiochip0".into()),
            ("gpio_line".into(), i.gpio_line),
            ("gpio_inhibit_line".into(), i.gpio_inhibit_line),
            ("serial_port".into(), i.serial_port),
            ("serial_line".into(), i.serial_line),
            ("voice_model".into(), i.voice_model),
        ];
        for (k, v) in over {
            match fields.iter_mut().find(|(key, _)| key == k) {
                Some(slot) => slot.1 = (*v).to_owned(),
                None => fields.push(((*k).to_owned(), (*v).to_owned())),
            }
        }
        let body = fields
            .iter()
            .map(|(k, v)| format!("{k}={}", encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        send(env, "POST", "/settings/hardware", body).await
    }

    async fn post_simple(env: &Env, uri: &str, extra: &str) -> (StatusCode, String) {
        let (s, b, _) = send(env, "POST", uri, format!("csrf={}{extra}", env.csrf)).await;
        (s, b)
    }

    async fn overrides(env: &Env) -> std::collections::BTreeMap<String, String> {
        HardwareRepo::new(env.state.pool.clone()).overrides().await.unwrap()
    }

    async fn audits(env: &Env, action: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = ?")
            .bind(action)
            .fetch_all(&env.state.pool)
            .await
            .unwrap()
    }

    #[test]
    fn without_any_installed_voice_the_model_is_a_text_field() {
        let lists = HostLists {
            audio: Ok(vec![]),
            serial: Ok(vec![]),
            gpio: Ok(vec![]),
            voices: vec![],
        };
        let file = HardwareSettings::from_config(&aars_core::config::Config::default());
        let input = FormInput {
            voice_model: "/opt/glosy/pl_PL-inny.onnx".to_owned(),
            ..FormInput::default()
        };
        let fields = build_fields(&input, &[], &file, Some(&lists));
        let voice = fields.iter().find(|f| f.name == "voice_model").unwrap();
        assert_eq!(voice.kind, "text", "pusta lista nie może zablokować wpisania modelu (R8)");
        assert!(voice.note.is_some());
    }

    #[tokio::test]
    async fn the_page_lists_host_devices_and_voices() {
        let env = env(Role::Admin).await;
        let (status, html) = get(&env, "/settings/hardware").await;
        assert_eq!(status, StatusCode::OK);
        for needle in ["hw:1,0", "/dev/ttyUSB0", "gpiochip0", "pl_PL-test", "w pliku:"] {
            assert!(html.contains(needle), "brak {needle}: {html}");
        }
    }

    #[tokio::test]
    async fn the_voice_list_uses_friendly_names_for_the_bundled_voices() {
        let env = env(Role::Admin).await;
        for f in [
            "pl_PL-gosia-medium.onnx",
            "pl_PL-gosia-medium.onnx.json",
            "pl_PL-mc_speech-medium.onnx",
            "pl_PL-mc_speech-medium.onnx.json",
        ] {
            std::fs::write(env._voices.path().join(f), "x").unwrap();
        }
        let (_, html) = get(&env, "/settings/hardware").await;
        assert!(html.contains("Gosia (kobiecy)"), "{html}");
        assert!(html.contains("MC Speech (męski)"), "{html}");
        assert!(html.contains("pl_PL-test"), "nieznany głos zostaje pod nazwą pliku");
        assert!(!html.contains("stacja używa głosu Gosia"), "model istnieje, bez ostrzeżenia");
    }

    #[tokio::test]
    async fn a_vanished_model_is_reported_as_replaced_by_gosia() {
        let env = env(Role::Admin).await;
        for f in ["pl_PL-gosia-medium.onnx", "pl_PL-gosia-medium.onnx.json"] {
            std::fs::write(env._voices.path().join(f), "x").unwrap();
        }
        let darkman = env._voices.path().join("pl_PL-darkman-medium.onnx");
        sqlx::query("INSERT INTO settings (key, value) VALUES ('hw:tts.model', ?)")
            .bind(darkman.to_string_lossy().as_ref())
            .execute(&env.state.pool)
            .await
            .unwrap();
        let (_, html) = get(&env, "/settings/hardware").await;
        assert!(html.contains("pl_PL-darkman-medium.onnx nie istnieje"), "{html}");
        assert!(html.contains("stacja używa głosu Gosia"), "{html}");
    }

    #[tokio::test]
    async fn an_admin_saves_a_gpio_setup_and_it_is_stored_with_an_audit_entry() {
        let env = env(Role::Admin).await;
        let (status, _, location) =
            post_hw(&env, &[("ptt_backend", "gpio"), ("gpio_line", "17")]).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.as_deref(), Some("/settings/hardware?saved=1"));
        let stored: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'hw:ptt.backend'")
            .fetch_one(&env.state.pool)
            .await
            .unwrap();
        assert_eq!(stored, "gpio");
        let line: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'hw:ptt.gpio.line'")
            .fetch_one(&env.state.pool)
            .await
            .unwrap();
        assert_eq!(line, "17");
        let details = audits(&env, "hardware").await;
        assert!(details.iter().any(|d| d.contains("ptt.backend: vox → gpio")), "{details:?}");

        let (_, html) = get(&env, "/settings/hardware?saved=1").await;
        assert!(html.contains("Zapisano. Zmiany wejdą w życie po restarcie stacji."), "{html}");
    }

    #[tokio::test]
    async fn saving_unchanged_values_writes_no_audit_entry() {
        let env = env(Role::Admin).await;
        let (status, _, _) = post_hw(&env, &[]).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(audits(&env, "hardware").await.is_empty());
        assert!(overrides(&env).await.is_empty(), "wartości z pliku nie są przypinane");
    }

    #[tokio::test]
    async fn an_unknown_audio_device_is_refused_and_nothing_is_stored() {
        let env = env(Role::Admin).await;
        let (status, html, _) = post_hw(&env, &[("audio_device", "hw:9,0")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(html.contains("hw:9,0") && html.contains("nie istnieje"), "{html}");
        assert!(overrides(&env).await.is_empty());
    }

    #[tokio::test]
    async fn a_gpio_line_outside_the_chip_is_refused() {
        let env = env(Role::Admin).await;
        let (status, html, _) = post_hw(&env, &[("ptt_backend", "gpio"), ("gpio_line", "40")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(html.contains("poza zakresem"), "{html}");
        assert!(overrides(&env).await.is_empty());
    }

    #[tokio::test]
    async fn when_the_host_cannot_list_serial_ports_the_port_is_accepted_with_a_warning() {
        let mut p = probe();
        p.serial = Err("brak uprawnień".to_owned());
        let env = env_with(Role::Admin, p, None).await;
        let (status, _, _) =
            post_hw(&env, &[("ptt_backend", "serial"), ("serial_port", "/dev/ttyUSB7")]).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(overrides(&env).await["ptt.serial.port"], "/dev/ttyUSB7");
        let (_, html) = get(&env, "/settings/hardware?saved=1").await;
        assert!(html.contains("nie sprawdzono — brak uprawnień"), "{html}");
    }

    #[tokio::test]
    async fn a_low_port_is_refused() {
        let env = env(Role::Admin).await;
        let (status, html, _) = post_hw(&env, &[("bind", "0.0.0.0:80")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(html.contains("wymaga uprawnień roota"), "{html}");
        assert!(overrides(&env).await.is_empty());
    }

    #[tokio::test]
    async fn a_decimal_comma_in_gain_is_accepted() {
        let env = env(Role::Admin).await;
        let (status, _, _) = post_hw(&env, &[("gain_db", "-4,5")]).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(overrides(&env).await["audio.gain_db"], "-4.5");
    }

    #[tokio::test]
    async fn a_missing_field_is_a_validation_error_not_a_422() {
        let env = env(Role::Admin).await;
        let (status, _, _) = send(&env, "POST", "/settings/hardware", format!("csrf={}", env.csrf)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn the_page_shows_the_apply_error_and_the_origin() {
        let station = fake(None, |s| {
            s.status = status(
                aars_core::hardware::ConfigOrigin::LastGood,
                "nie znaleziono urządzenia hw:9,0",
            );
        });
        let env = env_with(Role::Admin, probe(), Some(station)).await;
        let (_, html) = get(&env, "/settings/hardware").await;
        assert!(html.contains("nie znaleziono urządzenia hw:9,0"), "{html}");
        assert!(html.contains("ostatnia działająca konfiguracja"), "{html}");
    }

    #[tokio::test]
    async fn pending_restart_lists_changed_keys() {
        let state = AppState::for_tests(aars_core::db::test_pool().await);
        let mut running_hw = HardwareSettings::from_config(&state.base_config);
        running_hw.ptt_backend = PttBackendKind::Vox;
        let station = FakeStation::new(Some(running_hw.clone()));
        let mut stored_station = station.station.clone();
        let state = state.with_station(Arc::new(station));
        let mut stored = running_hw.clone();
        stored.ptt_backend = PttBackendKind::Gpio;
        assert_eq!(
            pending_restart(&state, &stored, &stored_station),
            vec!["ptt.backend: vox → gpio".to_owned()]
        );
        stored_station.callsign = "SP7TQ".to_owned();
        assert_eq!(pending_restart(&state, &running_hw, &stored_station), vec!["callsign".to_owned()]);

        let none = AppState::for_tests(aars_core::db::test_pool().await);
        assert!(pending_restart(&none, &stored, &stored_station).is_empty());
    }

    #[tokio::test]
    async fn restart_is_requested_once_and_audited() {
        let station = Arc::new(FakeStation::new(None));
        let env = env_with(Role::Admin, probe(), Some(station.clone())).await;
        let (status, _, location) =
            send(&env, "POST", "/station/restart", format!("csrf={}", env.csrf)).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.as_deref(), Some("/settings/hardware?restart=1"));
        assert_eq!(station.restarts(), 1);
        assert_eq!(audits(&env, "station_restart").await.len(), 1);
        let (_, html) = get(&env, "/settings/hardware?restart=1").await;
        assert!(
            html.contains("Restart zlecony. Stacja dokończy bieżące nadanie i uruchomi się ponownie"),
            "{html}"
        );
    }

    #[tokio::test]
    async fn restart_without_a_station_is_503() {
        let env = env(Role::Admin).await;
        let (status, body) = post_simple(&env, "/station/restart", "").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("stacja nie działa w tym procesie"));
    }

    #[tokio::test]
    async fn an_operator_cannot_open_hardware() {
        let env = env(Role::Operator).await;
        assert_eq!(get(&env, "/settings/hardware").await.0, StatusCode::FORBIDDEN);
        let (status, _, _) = post_hw(&env, &[("ptt_backend", "gpio")]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(overrides(&env).await.is_empty());
    }

    #[tokio::test]
    async fn the_dashboard_shows_that_transmitting_is_unavailable() {
        for role in [Role::Viewer, Role::Operator] {
            let station = fake(None, |s| {
                s.status = status(aars_core::hardware::ConfigOrigin::None, "brak PTT");
            });
            let env = env_with(role, probe(), Some(station)).await;
            let (_, html) = get(&env, "/").await;
            assert!(html.contains("Nadawanie niedostępne: brak PTT"), "{html}");
            assert!(!html.contains("Ustawienia sprzętu"), "tylko admin widzi link");
            assert_eq!(html.contains("action=\"/tx-enabled\""), role == Role::Operator);
        }
        let station = fake(None, |s| {
            s.status = status(aars_core::hardware::ConfigOrigin::None, "brak PTT");
        });
        let env = env_with(Role::Admin, probe(), Some(station)).await;
        let (_, html) = get(&env, "/").await;
        assert!(html.contains("Ustawienia sprzętu"), "{html}");
    }

    #[tokio::test]
    async fn the_dashboard_tells_an_admin_about_the_last_good_fallback() {
        let station = fake(None, |s| {
            s.status = status(aars_core::hardware::ConfigOrigin::LastGood, "port zniknął");
        });
        let env = env_with(Role::Admin, probe(), Some(station)).await;
        let (_, html) = get(&env, "/").await;
        assert!(html.contains("Działa ostatnia działająca konfiguracja") && html.contains("port zniknął"));
    }

    // --- Task 9 ---

    async fn station_env(
        role: Role,
        hw: Option<HardwareSettings>,
        outcome: TestOutcome,
    ) -> (Env, Arc<FakeStation>) {
        let station = fake(hw, |s| *s.outcome.lock().unwrap() = outcome);
        (env_with(role, probe(), Some(station.clone())).await, station)
    }

    #[tokio::test]
    async fn test_ptt_needs_the_confirmation() {
        let (env, station) = station_env(Role::Admin, None, TestOutcome::Busy).await;
        let (status, body) = post_simple(&env, "/settings/hardware/test-ptt", "&seconds=2").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("Zaznacz potwierdzenie — test PTT włącza nadajnik."), "{body}");
        assert!(audits(&env, "test_ptt").await.is_empty());
        assert!(station.tests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_ptt_duration_is_bounded() {
        let (env, station) = station_env(Role::Admin, None, TestOutcome::Busy).await;
        for seconds in ["0", "6", "x", ""] {
            let (status, body) =
                post_simple(&env, "/settings/hardware/test-ptt", &format!("&confirm=tak&seconds={seconds}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{seconds}");
            assert!(body.contains("Czas testu: 1–5 s"));
        }
        assert!(audits(&env, "test_ptt").await.is_empty());
        assert!(station.tests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_confirmed_test_ptt_reaches_the_station_and_is_audited() {
        let (env, station) = station_env(
            Role::Admin,
            None,
            TestOutcome::Done { played: std::time::Duration::from_secs(2), note: None },
        )
        .await;
        let (status, body) =
            post_simple(&env, "/settings/hardware/test-ptt", "&confirm=tak&seconds=2").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nadajnik włączony na 2,0 s"), "{body}");
        assert_eq!(
            *station.tests.lock().unwrap(),
            vec![StationTest::Ptt { seconds: 2, by: "u".to_owned() }]
        );
        let details = audits(&env, "test_ptt").await;
        assert_eq!(details.len(), 1);
        assert!(details[0].starts_with("2 s: Test PTT"), "{details:?}");
    }

    #[tokio::test]
    async fn a_refusal_is_shown_with_its_reason() {
        let (env, _) = station_env(Role::Admin, None, TestOutcome::Refused(RefusalReason::TxDisabled)).await;
        let (_, body) = post_simple(&env, "/settings/hardware/test-ptt", "&confirm=tak&seconds=2").await;
        assert!(body.contains("Odmowa: nadawanie wyłączone (wyłącznik)"), "{body}");
    }

    #[tokio::test]
    async fn test_audio_says_the_ptt_stayed_off() {
        let (env, _) = station_env(Role::Admin, None, TestOutcome::Done {
            played: std::time::Duration::from_millis(1600),
            note: Some("Synteza zawiodła — zagrano sam ton.".to_owned()),
        })
        .await;
        let (status, body) = post_simple(&env, "/settings/hardware/test-audio", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("przy wyłączonym PTT"), "{body}");
        assert!(body.contains("zagrano sam ton"), "{body}");
        assert_eq!(audits(&env, "test_audio").await.len(), 1);
    }

    #[tokio::test]
    async fn an_abandoned_test_is_shown_as_dropped() {
        let (env, _) = station_env(Role::Admin, None, TestOutcome::Failed("brak odpowiedzi stacji".to_owned())).await;
        let (_, body) = post_simple(&env, "/settings/hardware/test-audio", "").await;
        assert!(body.contains("brak odpowiedzi stacji") && body.contains("zostanie porzucone"), "{body}");
    }

    #[tokio::test]
    async fn vox_greys_out_both_tests_and_points_to_compose() {
        let mut hw = HardwareSettings::from_config(&AppState::for_tests(aars_core::db::test_pool().await).base_config);
        hw.ptt_backend = PttBackendKind::Vox;
        let (env, _) = station_env(Role::Admin, Some(hw), TestOutcome::Busy).await;
        let (_, html) = get(&env, "/settings/hardware").await;
        assert_eq!(html.matches("<fieldset disabled").count(), 2, "{html}");
        assert!(html.contains("href=\"/compose\""), "{html}");
    }

    #[tokio::test]
    async fn tests_without_a_station_are_503() {
        let env = env(Role::Admin).await;
        let (s, _) = post_simple(&env, "/settings/hardware/test-ptt", "&confirm=tak&seconds=2").await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        let (s, _) = post_simple(&env, "/settings/hardware/test-audio", "").await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn an_operator_cannot_run_tests() {
        let (env, station) = station_env(Role::Operator, None, TestOutcome::Busy).await;
        let (s, _) = post_simple(&env, "/settings/hardware/test-ptt", "&confirm=tak&seconds=2").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = post_simple(&env, "/settings/hardware/test-audio", "").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(audits(&env, "test_ptt").await.is_empty());
        assert!(station.tests.lock().unwrap().is_empty());
    }

    #[test]
    fn outcome_text_covers_every_variant() {
        use std::time::Duration;
        let ptt = StationTest::Ptt { seconds: 2, by: "u".into() };
        let audio = StationTest::Audio { by: "u".into() };
        let done = |ms| TestOutcome::Done { played: Duration::from_millis(ms), note: None };
        let cases: Vec<(&StationTest, TestOutcome, &str)> = vec![
            (&ptt, done(2000), "Test PTT: nadajnik włączony na 2,0 s i wyłączony. Wpis w dzienniku nadań."),
            (&audio, done(1600), "Test audio: odtworzono 1,6 s na urządzeniu hw:1,0 przy wyłączonym PTT."),
            (
                &audio,
                TestOutcome::Done { played: Duration::from_millis(500), note: Some("tylko ton".into()) },
                "Test audio: odtworzono 0,5 s na urządzeniu hw:1,0 przy wyłączonym PTT. tylko ton",
            ),
            (&ptt, TestOutcome::Refused(RefusalReason::TxDisabled), "Odmowa: nadawanie wyłączone (wyłącznik)"),
            (&ptt, TestOutcome::Refused(RefusalReason::Poisoned), "Odmowa: blokada watchdoga — zdejmij ją na pulpicie"),
            (&ptt, TestOutcome::Refused(RefusalReason::Inhibited), "Odmowa: blokada sprzętowa"),
            (&ptt, TestOutcome::Refused(RefusalReason::QuietHours), "Odmowa: okno ciszy"),
            (&ptt, TestOutcome::Refused(RefusalReason::ChannelBusy), "Odmowa: kanał zajęty"),
            (&ptt, TestOutcome::Busy, "Stacja właśnie nadaje albo trwa inny test — spróbuj za chwilę."),
            (&ptt, TestOutcome::Unsupported("VOX…"), "VOX…"),
            (&ptt, TestOutcome::Failed("awaria".into()), "Test nie powiódł się: awaria"),
        ];
        for (test, outcome, want) in cases {
            assert_eq!(outcome_text(test, &outcome, "hw:1,0").1, want);
        }
        assert_eq!(outcome_text(&ptt, &done(1), "x").0, "badge on");
        assert_eq!(outcome_text(&ptt, &TestOutcome::Busy, "x").0, "error");
    }
}
