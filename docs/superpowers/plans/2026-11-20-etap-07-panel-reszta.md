# Etap 7: Panel — komunikaty z harmonogramem, kolejka, dzienniki, ustawienia, użytkownicy

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Operator układa w panelu komunikaty cykliczne i jednorazowe, odsłuchuje je przed nadaniem i zarządza kolejką; admin ustawia stację i konta; każdy widzi dziennik nadań i audytu.

**Architecture:** Nowe strony panelu żyją w osobnych modułach `aars-web` (jeden plik na stronę), każdy z własnym szablonem Askama. `aars-core` dalej nie wie o HTTP (spec D6): panel rozmawia z nim przez `MessageRepo`, `QueueRepo`, `UserRepo` i tabelę `settings`. Podgląd audio idzie przez nowy trait `PreviewRenderer` w `aars-web`, który binarka implementuje tym samym `AudioCache` + `Synthesizer`, co nadanie.

**Tech Stack:** axum 0.8, askama 0.16, HTMX 2.0.4 (zwendorowany), croner 4 (walidacja i następne odpalenie wzorca), hound (WAV w pamięci), sqlx/SQLite.

**Spec:** `docs/superpowers/specs/2026-10-13-aars-design.md` (§4.4, §5, §6, §8)

## Global Constraints

- Role (spec §8): **viewer** czyta; **operator** nadaje, planuje i anuluje; **admin** dodatkowo zmienia konfigurację sprzętu i użytkowników. Kill switch dostępny operatorowi.
- Minimalne role stron (spec §8): `/messages` operator, `/queue` operator, `/settings` admin, `/users` admin, `/logs` viewer.
- Podgląd audio przechodzi tym samym pipeline'em co nadanie, włącznie z normalizacją (spec §8). Ścieżka: `GET /messages/{id}/preview.wav`; dla tekstu z formularza `POST /preview`.
- CSRF tokenem per sesja na każdym formularzu zmieniającym stan (spec §8); HTMX wysyła go nagłówkiem `x-csrf-token` (jak w etapie 6).
- Pierwszy start wymusza zmianę hasła konta startowego przy pierwszym logowaniu (spec §8).
- Harmonogram i polityka spóźnień jak w spec §6. Scheduler liczy w UTC (`DateTime<Utc>`) i tak zostaje.
- **Każdy czas pokazany w panelu ma oba zapisy: UTC i lokalny** (decyzja użytkownika), w formacie `18:00 UTC (19:00 lokalnie)`, z datą gdy nie jest dzisiejsza: `2026-12-01 18:00 UTC (19:00 lokalnie)`. Czas lokalny = strefa systemowa urządzenia (`chrono::Local`). Chwilę „at” operator wpisuje w czasie lokalnym, formularz pokazuje obok odpowiednik UTC; wzorzec cron wpisuje się w UTC (pole podpisane „cron, UTC”) i obok widać najbliższe nadanie w obu zapisach.
- Edycja definicji nie przepisuje przeszłości: zadania już w kolejce zostają, jakie były (spec §5).
- `tx_log`: `queue_item_id, started_at, ended_at, duration_ms, ptt_backend, result ∈ {ok, aborted, failed, timeout}, note` (spec §5).
- Awaria panelu nie dotyka nadawania (spec §10); każdy błąd bazy w handlerze kończy się logiem i kodem 500, nie paniką.
- Teksty panelu po polsku; priorytety wyświetlane jako **Alarmowy / Wysoki / Zwykły / Niski**.
- TDD: test przed implementacją, commit po każdym zielonym cyklu. Testy panelu: `oneshot` na routerze z SQLite w pamięci (helpery `login`, `logged_in` z etapu 6).
- Zakończenie: `cargo test --workspace` zielone, `cargo clippy --workspace --all-targets -- -D warnings` czyste.

## Review Focus

- **Wzorzec cron z błędem** (np. `0 25 * * *`) — formularz ma odmówić zapisu z komunikatem, a nie zapisać definicję, przy której scheduler przy każdym tiku loguje błąd. Test w Task 3.
- **Chwila „at” w przeszłości** — zapis ma zostać odrzucony („ta chwila już minęła”), bo scheduler i tak by ją pominął po `misfire_grace_s` i operator nie wiedziałby dlaczego. Test w Task 3.
- **Admin odbiera sobie uprawnienia** (wyłącza własne konto albo zmienia sobie rolę) — odmowa, żeby stacja nie została bez admina. Test w Task 8.
- **Nieprawidłowe wartości ustawień** (pusty limit TX, `0`, tekst zamiast liczby, okno ciszy `25:00-06:00`) — odmowa z komunikatem, bez zapisu, bo zła wartość wychodzi dopiero po restarcie stacji. Test w Task 7.
- **Podgląd bez skonfigurowanej syntezy** (testy, brak modelu Piper) — `503` z czytelnym tekstem, a nie panika ani pusty plik. Test w Task 4.

---

### Task 1: Wspólna rama stron i polskie etykiety priorytetów

**Files:**
- Create: `crates/aars-web/src/chrome.rs`
- Modify: `crates/aars-web/src/lib.rs` (struktury stron, `queue_row`, `pub mod chrome`)
- Modify: `crates/aars-web/templates/base.html`, `dashboard.html`, `compose.html`, `queue_fragment.html`

**Interfaces:**
- Produces: `aars_web::chrome::Chrome { username: String, role: String, csrf: String, can_operate: bool, is_admin: bool }`, `Chrome::from(&CurrentUser)`; `aars_web::chrome::both_times<Tz: TimeZone>(at: DateTime<Utc>, local: &Tz, today: NaiveDate) -> String` (format z Global Constraints; data dopisywana, gdy lokalna data `at` ≠ `today`) i wygodny wrapper `both_times_now(at) -> String` na `chrono::Local`; `aars_web::chrome::priority_label(Priority) -> &'static str`; `aars_web::chrome::priority_value(Priority) -> &'static str` (`"emergency" | "high" | "normal" | "low"`, odwrotność istniejącego `parse_priority`, które przenosi się do `chrome.rs` jako `pub fn parse_priority`).
- Każdy szablon strony ma pole `chrome: Chrome`; `base.html` czyta `chrome.*`. Nawigacja: Pulpit, Nadaj, Komunikaty, Kolejka (operator), Dzienniki (każdy), Ustawienia, Użytkownicy (admin).

- [ ] **Step 1: Testy**

W `chrome.rs`:
```rust
#[test]
fn priorities_have_polish_labels() {
    assert_eq!(priority_label(Priority::Emergency), "Alarmowy");
    assert_eq!(priority_label(Priority::High), "Wysoki");
    assert_eq!(priority_label(Priority::Normal), "Zwykły");
    assert_eq!(priority_label(Priority::Low), "Niski");
}

#[test]
fn priority_value_round_trips_through_parse() {
    for p in [Priority::Emergency, Priority::High, Priority::Normal, Priority::Low] {
        assert_eq!(parse_priority(priority_value(p)), Some(p));
    }
}

#[test]
fn times_are_shown_in_utc_and_local() {
    let warsaw_winter = chrono::FixedOffset::east_opt(3600).unwrap();
    let at = Utc.with_ymd_and_hms(2026, 11, 20, 18, 0, 0).unwrap();
    let today = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
    assert_eq!(both_times(at, &warsaw_winter, today), "18:00 UTC (19:00 lokalnie)");
    let tomorrow = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();
    assert_eq!(both_times(at, &warsaw_winter, tomorrow), "2026-11-20 18:00 UTC (19:00 lokalnie)");
}
```
Kolejka na pulpicie pokazuje czas „od” przez `both_times_now`. W `lib.rs` (testy routera): `the_queue_shows_polish_priority_names` — po `enqueue` z `Priority::High` fragment `/fragments/queue` zawiera `Wysoki` i nie zawiera `High`. `an_admin_sees_admin_links_and_an_operator_does_not` — pulpit admina zawiera `href="/settings"` i `href="/users"`, pulpit operatora nie.

- [ ] **Step 2: Uruchom — FAIL** (`cargo test -p aars-web`)
- [ ] **Step 3: Implementacja** — `Chrome`, etykiety, przeniesienie `parse_priority`, przepięcie istniejących stron na `chrome`.
- [ ] **Step 4: Uruchom — PASS** (wszystkie testy `aars-web`, także z etapu 6)
- [ ] **Step 5: Commit** — `feat(web): wspólna rama stron, nawigacja według roli, polskie priorytety`

---

### Task 2: Zmiana hasła i jej wymuszenie

**Files:**
- Create: `crates/aars-web/src/account.rs`, `crates/aars-web/templates/password.html`
- Modify: `crates/aars-web/src/auth.rs` (wymuszenie), `crates/aars-web/src/lib.rs` (trasy)

**Interfaces:**
- Consumes: `UserRepo::authenticate`, `UserRepo::set_password` (czyści `must_change_password`), `User::must_change_password`.
- Produces: trasy `GET/POST /account/password` (każda zalogowana rola). Dopóki `must_change_password`, każda inna trasa poza `/account/password`, `/logout`, `/assets/*` odpowiada `303` na `/account/password`.

- [ ] **Step 1: Testy** (`account.rs`, przez router):
  - `a_user_who_must_change_the_password_is_sent_to_the_change_form` — konto z `must_change_password = 1` (utwórz przez `UserRepo::create`, potem `UPDATE users SET must_change_password = 1`), `GET /` → `303`, `Location: /account/password`.
  - `changing_the_password_lifts_the_redirect` — `POST /account/password` z `current`, `new`, `confirm`, `csrf` → `303` na `/`; potem `GET /` → `200`; logowanie starym hasłem → `401`.
  - `a_wrong_current_password_is_refused` → `400`, hasło bez zmian.
  - `a_new_password_shorter_than_10_characters_is_refused` → `400`.
  - `mismatched_confirmation_is_refused` → `400`.
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja** — sprawdzenie w ekstraktorze `CurrentUser` (nowy wariant `AuthRejection::MustChangePassword` → `303`), formularz z trzema polami, minimalna długość 10 znaków, wpis do `audit_log` akcja `password_change`.
- [ ] **Step 4: PASS**
- [ ] **Step 5: Commit** — `feat(web): zmiana hasła i wymuszenie jej po pierwszym logowaniu`

---

### Task 3: Definicje komunikatów z harmonogramem (`/messages`)

**Files:**
- Modify: `crates/aars-core/src/message.rs` (`update`)
- Create: `crates/aars-web/src/messages.rs`, `crates/aars-web/templates/messages.html`, `crates/aars-web/templates/message_form.html`
- Modify: `crates/aars-web/src/lib.rs` (trasy)

**Interfaces:**
- Consumes: `MessageRepo::{create, get, list, set_enabled, delete}`, `QueueRepo::enqueue`, `EnqueueRequest`, `chrome::*`.
- Produces:
  - `MessageRepo::update(&self, id: i64, new: NewMessage) -> Result<u64, DbError>` (zmienia definicję, nie dotyka `queue_items`).
  - `aars_web::messages::parse_schedule<Tz: TimeZone>(kind: &str, at: &str, cron: &str, now: DateTime<Utc>, local: &Tz) -> Result<ScheduleKind, String>` — `kind ∈ {immediate, at, cron}`; `at` w formacie `datetime-local` (`%Y-%m-%dT%H:%M`) jest **czasem lokalnym** w strefie `local`, przeliczanym na UTC (chwila nieistniejąca przy zmianie czasu → `Err`), odrzucany gdy `<= now` („ta chwila już minęła”); handler podaje `&chrono::Local`; `cron` sprawdzany przez `croner::Cron::from_str`, błąd zawiera treść błędu crona.
  - `aars_web::messages::next_fire(schedule: &ScheduleKind, now: DateTime<Utc>) -> Option<DateTime<Utc>>` — do kolumny „Następne nadanie”.
  - Trasy (operator): `GET /messages`, `GET /messages/new`, `POST /messages`, `GET /messages/{id}/edit`, `POST /messages/{id}`, `POST /messages/{id}/enabled`, `POST /messages/{id}/delete`, `POST /messages/{id}/send` (wkłada definicję do kolejki teraz, z jej priorytetem, przerywaniem i powtórzeniami, `message_id = Some(id)`).
- Pola formularza: `title`, `text`, `priority`, `interrupting`, `repeat` (1–20), `every` (s), `schedule_kind`, `at`, `cron`, `csrf`. Lista pokazuje tytuł, priorytet (etykieta PL), harmonogram (`cron 0 18 * * * (UTC)` / `jednorazowo` / `na żądanie`), następne nadanie przez `both_times_now`, włączony/wyłączony. Formularz przy polu „at” (czas lokalny) i przy polu cron pokazuje na żywo odpowiednik UTC / najbliższe nadanie w obu zapisach (mały skrypt inline albo HTMX `GET /messages/next?kind=&at=&cron=` zwracający tekst z `both_times_now` — wybierz prostsze).

- [ ] **Step 1: Testy**
  - `message.rs`: `update_changes_the_definition_but_not_queued_items` — utwórz definicję, `enqueue` z `message_id`, `update` z nowym tytułem i cronem; `get` zwraca nowe wartości, `pending` ma starą treść.
  - `messages.rs` (jednostkowe): `parse_schedule("cron", "", "0 18 * * *", now)` → `Cron("0 18 * * *")`; `parse_schedule("cron", "", "0 25 * * *", now)` → `Err` zawierające `0 25 * * *`; `parse_schedule("at", "2026-12-01T19:00", "", now=2026-11-20, local=UTC+1)` → `At(2026-12-01T18:00:00Z)` (czas lokalny przeliczony na UTC); ta sama chwila przy `now=2026-12-02` → `Err("ta chwila już minęła")`; `parse_schedule("immediate", …)` → `Immediate`; nieznany `kind` → `Err`. `next_fire(Cron("0 18 * * *"), 2026-11-20T10:00Z)` → `2026-11-20T18:00Z`; `next_fire(Immediate, …)` → `None`.
  - Router: `an_operator_creates_a_cron_message_and_sees_it_listed` (POST → `303` na `/messages`, lista zawiera tytuł i `0 18 * * *`); `an_invalid_cron_is_refused_and_nothing_is_saved` (`400`, `MessageRepo::list` pusta); `an_operator_can_disable_and_delete_a_message`; `send_now_puts_the_definition_on_the_queue` (po `POST /messages/{id}/send` `pending` ma `repeat` elementów z `message_id = Some(id)`); `a_viewer_cannot_open_messages` (`403`); `an_empty_message_text_is_refused` (`400`).
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja** — `update` w `MessageRepo` (te same kolumny co `create`), moduł `messages.rs`, szablony; każda zmiana zapisuje `audit_log` (`message_create`, `message_update`, `message_enabled`, `message_delete`, `message_send`) z tytułem w `detail`.
- [ ] **Step 4: PASS**
- [ ] **Step 5: Commit** — `feat(web): definicje komunikatów z harmonogramem w panelu`

---

### Task 4: Podgląd audio

**Files:**
- Create: `crates/aars-web/src/preview.rs`
- Modify: `crates/aars-web/src/state.rs` (pole `preview`), `crates/aars-web/src/lib.rs` (trasy), `templates/compose.html`, `templates/message_form.html`, `templates/messages.html` (przycisk „Odsłuchaj” + `<audio>`)
- Modify: `crates/aars-audio/src/wav.rs` (`wav_bytes_mono`)
- Modify: `crates/aars/src/main.rs` (wpięcie implementacji)

**Interfaces:**
- Produces:
  - `aars_web::preview::PreviewRenderer` — `fn render_wav<'a>(&'a self, text: &'a str) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>` (bez `async_trait`, ten sam wzorzec co `RenderFuture` w dispatcherze); `Send + Sync`.
  - `AppState.preview: Option<Arc<dyn PreviewRenderer>>`; `AppState::with_preview(self, Arc<dyn PreviewRenderer>) -> Self`. `for_tests` ustawia `None`.
  - `aars_audio::wav::wav_bytes_mono(pcm: &Pcm) -> Result<Vec<u8>, SynthError>` (16 bit, mono, `pcm.sample_rate`).
  - Trasy (operator): `GET /messages/{id}/preview.wav`, `POST /preview` (pole `text`, `csrf`) → `200`, `Content-Type: audio/wav`; brak renderera → `503` z tekstem „podgląd niedostępny: synteza nie jest skonfigurowana”; pusty tekst → `400`; błąd syntezy → `502` z treścią błędu.
  - W binarce: `PiperPreview { cache: AudioCache, synth: Arc<dyn Synthesizer> }` w `crates/aars/src/render.rs`, `cache.render(..)` → `wav_bytes_mono`. Bez identyfikacji stacji — podgląd to treść, nie nadanie.

- [ ] **Step 1: Testy**
  - `wav.rs`: `wav_bytes_round_trip` — 480 próbek → bajty zaczynają się od `RIFF`, zapis do pliku tymczasowego i `read_wav_mono` daje te same próbki.
  - `preview.rs` z fałszywym rendererem zwracającym `b"RIFF-fake"`: `the_preview_returns_wav_audio` (`200`, `audio/wav`, ciało `RIFF-fake`); `the_preview_without_a_renderer_says_why` (`503`, tekst zawiera „podgląd niedostępny”); `a_viewer_cannot_preview` (`403`); `a_preview_of_a_missing_message_is_404`.
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja** — trait, pole stanu, trasy, przyciski w szablonach (HTMX nie jest potrzebny: `<audio src="/messages/{{id}}/preview.wav">`, dla formularza `fetch` POST w małym skrypcie inline, który podstawia `blob:` do `<audio>`), wpięcie w `main.rs`.
- [ ] **Step 4: PASS**; ręcznie: `cargo run -p aars -- run`, na `/compose` „Odsłuchaj” gra znormalizowany tekst.
- [ ] **Step 5: Commit** — `feat(web): podgląd audio komunikatu tym samym torem co nadanie`

---

### Task 5: Kolejka (`/queue`) z anulowaniem grupy

**Files:**
- Create: `crates/aars-web/src/queue_page.rs`, `crates/aars-web/templates/queue.html`
- Modify: `crates/aars-web/templates/queue_fragment.html`, `crates/aars-web/src/lib.rs`

**Interfaces:**
- Consumes: `QueueRepo::{pending, cancel, cancel_group}`, `QueueItem.repeat_group`, `QueueItem.repeat_index`.
- Produces: `GET /queue` (operator) — pełna lista (limit 200) z kolumnami #, priorytet, od (`both_times_now`), treść, powtórzenie `n/N` w grupie; przyciski „Anuluj” i „Anuluj serię” (tylko gdy `repeat_group` jest ustawione). `POST /queue/group/{group}/cancel` (operator, CSRF w nagłówku) odpowiada fragmentem kolejki. `QueueRow` dostaje `group: Option<String>` i `repeat_label: String`.

- [ ] **Step 1: Testy**: `cancelling_a_group_removes_all_remaining_repeats` (enqueue `repeat_count: 3`, `POST /queue/group/{g}/cancel` → `pending` puste); `cancelling_one_repeat_keeps_the_others` (2 zostają); `the_queue_page_shows_repeat_position` (zawiera `1/3`); `a_viewer_cannot_open_the_queue_page` (`403`); `group_cancel_without_csrf_is_refused` (`403`).
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja** (wpis `audit_log` `queue_cancel_group`)
- [ ] **Step 4: PASS**
- [ ] **Step 5: Commit** — `feat(web): strona kolejki z anulowaniem całej serii`

---

### Task 6: Dziennik nadań i audytu (`/logs`)

**Files:**
- Modify: `crates/aars-core/src/dispatcher.rs` (zapis `tx_log` w `record_outcome`), `DispatcherConfig` (`ptt_backend: &'static str`)
- Create: `crates/aars-core/src/txlog.rs`; `crates/aars-web/src/logs.rs`, `crates/aars-web/templates/logs.html`
- Modify: `crates/aars/src/main.rs` (nazwa backendu PTT w `DispatcherConfig`)

**Interfaces:**
- Produces:
  - `aars_core::txlog::TxLogRepo::new(pool)`, `record(&self, entry: TxLogEntry) -> Result<(), DbError>`, `recent(&self, limit: i64) -> Result<Vec<TxLogRow>, DbError>`; `TxLogEntry { queue_item_id: i64, started_at: DateTime<Utc>, ended_at: DateTime<Utc>, ptt_backend: String, result: &'static str, note: Option<String> }`; `TxLogRow` ma te pola plus `duration_ms: i64` i `text: Option<String>` (treść z `queue_items` przez `LEFT JOIN`).
  - Dispatcher: `TxOutcome::Ok → "ok"`, `Aborted → "aborted"`, `Failed(err)` → `"timeout"`, gdy błąd pochodzi z watchdoga twardego limitu (sprawdź, jakim tekstem watchdog kończy nadanie w `sequencer.rs`/`watchdog.rs` i dopasuj po nim), w pozostałych przypadkach `"failed"`; `note` = treść błędu. `started_at = ended_at - played`.
  - `GET /logs` (viewer): dwie tabele — ostatnie 100 nadań (czas przez `both_times_now`, wynik po polsku: „nadane / przerwane / błąd / limit czasu”, długość s, treść) i ostatnie 100 wpisów audytu (czas przez `both_times_now`, użytkownik po `LEFT JOIN users`, akcja, szczegóły).

- [ ] **Step 1: Testy**
  - `dispatcher.rs` (harness z etapu 4): `a_finished_transmission_is_written_to_the_tx_log` (wynik `ok`, `ptt_backend` z konfiguracji, `duration_ms > 0`); `an_interrupted_transmission_is_logged_as_aborted`.
  - `txlog.rs`: `recent_returns_newest_first_with_the_message_text`.
  - `logs.rs`: `a_viewer_sees_transmissions_and_audit_entries` (po `compose` jako operator i ręcznym wpisie `tx_log`, viewer widzi „nadane” i akcję `compose`); `anonymous_visitors_are_sent_to_login`.
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars-core -p aars-web`)
- [ ] **Step 5: Commit** — `feat: dziennik nadań zapisywany przez dyspozytora i strona dzienników`

---

### Task 7: Ustawienia stacji (`/settings`) i okna ciszy

**Files:**
- Create: `crates/aars-core/src/settings.rs`, `crates/aars-web/src/settings_page.rs`, `crates/aars-web/templates/settings.html`
- Modify: `crates/aars-web/src/state.rs` (`hardware: Vec<(String, String)>`), `crates/aars/src/main.rs` (okna ciszy w pętli `run`, opis sprzętu do stanu)

**Interfaces:**
- Produces:
  - `aars_core::settings::StationSettings { callsign: String, id_mode: IdMode, id_interval_s: u64, tx_max_duration_s: u64, misfire_grace_s: u64, quiet_hours: Option<QuietHours>, quiet_min_priority: Priority }`; `SettingsRepo::new(pool)`, `load(&self) -> Result<StationSettings, DbError>`, `save(&self, s: &StationSettings) -> Result<(), DbError>`.
  - `aars_core::settings::QuietHours { start: NaiveTime, end: NaiveTime }`, `QuietHours::parse(&str) -> Result<QuietHours, String>` dla `"HH:MM-HH:MM"` (**czas lokalny** — operator myśli o ciszy nocnej po swojemu; okno może przechodzić przez północ), `QuietHours::contains(&self, t: NaiveTime) -> bool`, zapis jako ten sam tekst; pusty tekst = brak okna.
  - Walidacja formularza: `callsign` 3–10 znaków `[A-Z0-9/-]` po zamianie na wielkie litery albo pusty (ostrzeżenie „nadania bez identyfikacji”); `id_interval_s` 60–3600; `tx_max_duration_s` 10–600; `misfire_grace_s` 0–86400; `id_mode ∈ {cw, voice}`; `quiet_min_priority` jak w `parse_priority`.
  - `GET/POST /settings` (admin). Strona mówi wprost: znak, tryb i odstęp identyfikacji, limit TX i polityka spóźnień **wchodzą po restarcie stacji**; okno ciszy działa od razu. Pod formularzem tabela tylko do odczytu `hardware` (urządzenie audio, częstotliwość próbkowania, backend PTT, model głosu) z `aars.toml` — zmiana sprzętu to edycja pliku, nie panel.
  - W `main.rs` zadanie co 30 s: `SettingsRepo::load` → `interlocks.set_quiet(quiet_hours.contains(chrono::Local::now().time()), quiet_min_priority)`; strona ustawień pokazuje okno także w UTC (np. `22:00–06:00 lokalnie = 21:00–05:00 UTC`).

- [ ] **Step 1: Testy**
  - `settings.rs`: `quiet_hours_cross_midnight` (`22:00-06:00` zawiera `23:30` i `05:59`, nie zawiera `06:00` ani `12:00`); `quiet_hours_reject_bad_input` (`25:00-06:00`, `22:00`, `abc` → `Err`); `settings_round_trip_through_the_database`.
  - `settings_page.rs`: `an_admin_saves_the_callsign_uppercased` (`sp7tq` → w bazie `SP7TQ`); `out_of_range_values_are_refused_and_nothing_changes` (`tx_max_duration_s=0` → `400`, w bazie dalej `180`); `an_operator_cannot_open_settings` (`403`); `the_page_lists_the_hardware_from_the_config_file`.
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja** (wpis `audit_log` `settings` z listą zmienionych kluczy)
- [ ] **Step 4: PASS**
- [ ] **Step 5: Commit** — `feat: ustawienia stacji w panelu i okna ciszy`

---

### Task 8: Użytkownicy (`/users`)

**Files:**
- Modify: `crates/aars-core/src/user.rs` (`set_role`, `reset_password`)
- Create: `crates/aars-web/src/users.rs`, `crates/aars-web/templates/users.html`

**Interfaces:**
- Produces:
  - `UserRepo::set_role(&self, id: i64, role: Role) -> Result<(), DbError>`; `UserRepo::reset_password(&self, id: i64) -> Result<String, AuthError>` — losowe hasło 20 znaków, `must_change_password = 1`, zrywa sesje użytkownika.
  - Trasy (admin): `GET /users`; `POST /users` (`username`, `role`) — tworzy konto z losowym hasłem, ustawia `must_change_password`, strona po zapisie pokazuje hasło **jeden raz**; `POST /users/{id}/role`; `POST /users/{id}/disabled`; `POST /users/{id}/reset`; `POST /users/{id}/delete`.
  - Ochrona: admin nie może wyłączyć, usunąć ani odebrać sobie roli admin (`400` „nie możesz odebrać uprawnień sam sobie”); nazwa użytkownika `[a-z0-9_.-]{3,32}`, duplikat → `400`.

- [ ] **Step 1: Testy**
  - `user.rs`: `reset_password_forces_a_change_and_ends_sessions`; `set_role_changes_what_the_user_can_do`.
  - `users.rs`: `an_admin_creates_an_operator_who_must_change_the_password` (hasło z odpowiedzi pozwala się zalogować, `GET /` → `303` na `/account/password`); `an_admin_cannot_demote_or_disable_themselves`; `a_duplicate_username_is_refused`; `an_operator_cannot_manage_users` (`403`).
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja** (audyt: `user_create`, `user_role`, `user_disabled`, `user_reset`, `user_delete`)
- [ ] **Step 4: PASS**
- [ ] **Step 5: Commit** — `feat(web): zarządzanie użytkownikami`

---

### Task 9: Macierz rola × endpoint i kryterium wyjścia

**Files:**
- Create: `crates/aars-web/tests/role_matrix.rs` (feature `testing`, jak `AppState::for_tests`)
- Create: `crates/aars/tests/schedule_from_panel.rs` (albo test w `crates/aars-web` z `Scheduler` z `aars-core` jako dev-dependency, jeśli tak prościej)
- Modify: `README.md` (sekcja „Panel”: strony i role), `docs/superpowers/plans/README.md` (etap 7 ✓)

**Interfaces:**
- Consumes: wszystkie trasy z Task 1–8.

- [ ] **Step 1: Testy**
  - `role_matrix.rs`: jedna tabela `(metoda, ścieżka, minimalna rola)` obejmująca wszystkie trasy GET z Task 1–8 i etapu 6 (dla tras z `{id}` przygotuj rekord). Dla każdej trasy i każdej roli (`anon`, `viewer`, `operator`, `admin`): `anon` → `303` na `/login`; rola niższa niż minimalna → `403`; minimalna i wyższa → `200` (lub `303` dla przekierowań po POST). POST-y sprawdzane osobno: bez tokenu CSRF → `403` dla każdej roli.
  - `schedule_from_panel.rs`: operator tworzy przez router definicję `cron` `* * * * *`; `Scheduler::tick(now)` z `MessageRepo`/`QueueRepo` na tej samej puli wkłada do kolejki jedno zadanie z tą treścią i `message_id`; drugi `tick` w tej samej minucie nie dubluje.
- [ ] **Step 2: FAIL / PASS** — macierz powinna przejść od razu, jeśli Task 1–8 są poprawne; każda czerwona komórka to błąd w odpowiednim zadaniu, nie w teście.
- [ ] **Step 3: Pełna weryfikacja**: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`; ręcznie `cargo run -p aars -- run`: utwórz komunikat cron na najbliższą minutę UTC, sprawdź, że trafia do kolejki i do `/logs`.
- [ ] **Step 4: Commit** — `test(web): macierz rola × endpoint; harmonogram z panelu do kolejki`

---

## Kryterium wyjścia z etapu

- Harmonogram konfigurowalny z panelu: definicja cron utworzona w `/messages` trafia do kolejki przy najbliższym odpaleniu i nie dubluje się (Task 9).
- Macierz rola × endpoint przechodzi dla wszystkich stron (Task 9).
- Podgląd audio działa z `/compose` i `/messages` tym samym torem co nadanie (Task 4).
- `/logs` pokazuje faktyczne nadania z `tx_log` i wpisy audytu (Task 6).
- Konto startowe musi zmienić hasło przy pierwszym logowaniu (Task 2).
- `cargo test --workspace` zielone, `clippy -D warnings` czyste.
