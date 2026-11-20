# Etap 8: `DataSource` i ostrzeżenia meteorologiczne IMGW

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stacja sama pobiera ostrzeżenia meteorologiczne IMGW dla skonfigurowanego obszaru i wkłada do kolejki dokładnie jeden polski komunikat na każde nowe, zmienione albo odwołane ostrzeżenie; operator widzi w panelu zdrowie źródła i aktywne ostrzeżenia.

**Architecture:** Nowy crate `aars-sources` (spec §3) z traitem `DataSource`, ogólnym `SourceRunner` (odpytywanie, backoff, porównanie z poprzednim stanem, wkładanie do kolejki) i źródłem `imgw_meteo` (parsowanie, filtr TERYT, tekst mowy). Stan źródła i śledzone ostrzeżenia żyją w SQLite (`source_state` + nowa tabela `source_items`), a repozytorium jest w `aars-core`, bo czyta je też panel — `aars-web` dalej nie zależy od `aars-sources` (spec §3, D6). Źródło produkuje tekst z cyframi i godzinami; słowa robi istniejąca normalizacja w syntezie (D8, §4.1), rozszerzona o daty i przypadki godzin.

**Tech Stack:** reqwest 0.13 (`default-features = false`, `rustls`), serde_json, chrono-tz 0.10 (`Europe/Warsaw`), wiremock 0.6 (tylko testy), axum 0.8 / askama 0.16 (panel), sqlx/SQLite.

**Spec:** `docs/superpowers/specs/2026-10-13-aars-design.md` (§3, §5 `source_state`, §5.1, §7, §8 `/sources` i zdrowie źródeł na `/`, §9, §10 awaria źródła, §11 źródła)

## Rzeczywiste API IMGW (sprawdzone 2026-11-20)

- `GET https://danepubliczne.imgw.pl/api/data/warningsmeteo` → `200`, `application/json; charset=utf-8`, bez autoryzacji, `access-control-allow-origin: *`.
- **Brak ostrzeżeń to obiekt, nie pusta tablica:** `{"message":"Brak ostrzeżeń meteorologicznych"}` (tak odpowiedziało API przy sprawdzeniu, 56 bajtów). Spec tego nie przewiduje.
- Są ostrzeżenia → tablica obiektów; **wszystkie wartości to napisy**: `id` (`"Sk20260722203344412"`), `nazwa_zdarzenia` (`"Burze"`), `stopien` (`"1"`), `prawdopodobienstwo` (`"80"`), `obowiazuje_od`, `obowiazuje_do`, `opublikowano` (`"2026-07-23 10:00:00"` — **czas polski bez strefy**; `opublikowano` 22:33 przy `id` z 20:33 w lipcu i 11:45 przy 10:45 w grudniu potwierdza `Europe/Warsaw`), `tresc`, `komentarz`, `biuro`, `teryt` (tablica **czterocyfrowych kodów powiatów TERYT**, np. `"1061"` = Łódź; kody miast na prawach powiatu `xx61`–`xx65` występują). Kodów województw w `teryt` nie ma — filtr województwa to prefiks dwóch cyfr.
- Fiksy w repo (prawdziwe odpowiedzi, niczego nie poprawiać):
  - `crates/aars-sources/tests/fixtures/imgw/warningsmeteo-2026-11-20-brak.json` — odpowiedź na żywo z dnia pisania planu.
  - `crates/aars-sources/tests/fixtures/imgw/warningsmeteo-2024-12-29.json` — 1 ostrzeżenie (Silny wiatr, st. 1, 70 %, 2024-12-30 02:00–10:00, powiaty `22xx`/`32xx`); kopia z Wayback Machine (`web.archive.org/web/20241229123941id_/…`).
  - `crates/aars-sources/tests/fixtures/imgw/warningsmeteo-2026-07-23.json` — 5 ostrzeżeń (3× Burze, 2× Silny deszcz z burzami, 260 kodów); Wayback `20260723130145id_`. Kod `1061` trafia **tylko** w `Sk20260722203344412` (Burze, st. 1, 80 %, 2026-07-23 10:00–22:00, `tresc` 297 znaków).
  - Warianty (stopień 2/3, zmienione godziny, uszkodzony wpis) testy robią w kodzie: wczytują fiks do `serde_json::Value` i podmieniają pola. Nie dodawać zmyślonych plików.

## Decyzje etapu 8

Spec nie ma sekcji decyzji per etap, więc decyzje są tylko tutaj.

- **R1 Trait inny niż w spec §7.** Bez `async_trait` (wzorzec `RenderFuture` z dyspozytora): `fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a>`. `fetch` zwraca **listę** pozycji (`SourceOutput { items, complete }`), bo jedno odpytanie IMGW to wiele ostrzeżeń; `dedup_key` ze spec rozpada się na `key` (tożsamość), `topic` (zjawisko) i `fingerprint` (zmiana ⇒ nowe zdarzenie). `poll_interval()` → `default_interval()`, bo interwał ustawia się z panelu. `Segment` i tryb biuletynu dochodzą w etapie 9 razem z kompozytorem; tu pozycja niesie gotowe teksty.
- **R2 Idempotencja w ogólnym `SourceRunner`, nie w źródle.** Źródło jest czystą funkcją odpowiedzi → pozycje; porównanie z bazą (nowe / zmienione / odwołane / wygasłe) robi runner, więc `imgw-hydro` w etapie 9 dostaje to za darmo.
- **R3 Odcisk = zjawisko + stopień + `od` + `do` + odczytywany obszar.** Zmiana treści opisowej czy prawdopodobieństwa nie wywołuje ponownego nadania; zmiana stopnia, ważności albo obszaru w naszym rejonie — tak.
- **R4 IMGW może wydać to samo ostrzeżenie pod nowym `id`.** Nowe `id` z odciskiem równym aktywnemu → ciche przejęcie. Nowe `id` tego samego zjawiska, gdy stare `id` zniknęło w tym samym odpytaniu → jeden komunikat „Zmiana ostrzeżenia”, nie „nowe” + „odwołanie”.
- **R5 Odwołanie po dwóch kolejnych udanych odpytaniach bez ostrzeżenia** i tylko przed `obowiazuje_do`; po terminie znika po cichu (wygasło). Chroni przed jednorazowym „mrugnięciem” API. Odpowiedź błędna albo niekompletna (`complete = false`) nie liczy się jako nieobecność.
- **R6 Priorytet ze stopnia:** 3 → `Emergency` + przerywa; 2 → `High`; 1 i nieznany → `Normal`; odwołanie → `Normal`, nie przerywa. `expires_at` = koniec ważności (nieaktualne ostrzeżenie nie idzie na antenę, spec §4.4/kolejka). Ostrzeżenie już wygasłe w chwili odpytania nie jest nadawane.
- **R7 Zmiana albo odwołanie, zanim poprzedni komunikat poszedł na antenę** (np. czeka w oknie ciszy) → stary element kolejki jest anulowany; odwołania ostrzeżenia, które nigdy nie zostało nadane, nie nadajemy.
- **R8 Obszar w panelu: linie `KOD [nazwa]`** (albo rozdzielone `;`). 2 cyfry = województwo (nazwa domyślna „województwo łódzkie” z wbudowanej tabeli 16 województw), 4 cyfry = powiat, **nazwa wymagana** (to ją słychać na antenie; tabeli 380 powiatów nie wbudowujemy). Pusty obszar = źródło nic nie pobiera („brak obszaru”).
- **R9 Zmiana obszaru nie generuje odwołań:** śledzone pozycje zapisane przy innym obszarze są porzucane po cichu (`dropped`).
- **R10 Godziny i daty po polsku przez normalizację:** `od 10:00` → „od godziny dziesiątej”, `o 18:00` → „o godzinie osiemnastej”, `23 lipca` → „dwudziestego trzeciego lipca”, `24:00` → „dwudziesta czwarta”. Dotychczasowe asercje „od godzina osiemnasta” były gramatycznie błędne i zmieniają się (także test CLI `say --dry-run`). Bez „dziś/jutro” — komunikat może przeczekać w kolejce do następnego dnia. Koniec `00:00` mówimy jako `24:00` poprzedniego dnia.
- **R11 Czas IMGW = `Europe/Warsaw` przez chrono-tz**, nie `chrono::Local` — stacja na RPi często chodzi w UTC. Niejednoznaczna chwila przy zmianie czasu → wcześniejsza; nieistniejąca → wpis pominięty (`complete = false`).
- **R12 Ustawienia źródła w bazie:** włączenie w `source_state.enabled` (kolumna już jest), obszar i interwał w `settings` pod kluczami `source:imgw-meteo:area` i `source:imgw-meteo:interval_s` (domyślnie `''` i `300`, zakres 60–3600 s). Działają od następnego cyklu, bez restartu. Źródło po migracji jest **wyłączone**. Adres API w `aars.toml` (`[sources] imgw_meteo_url`), bo to rozruch, nie operacja (spec §9).
- **R13 Backoff:** po `n` kolejnych porażkach czekanie `min(interval · 2^(n−1), max(interval, 1800 s))`. Źródło jest **przestarzałe**, gdy ostatni sukces był dawniej niż `max(3 · interval, 1800 s)` (albo go nie było, a próby są).
- **R14 Role:** `GET /sources` operator (spec §8); włączanie i „Odśwież teraz” operator; obszar i interwał **admin** (konfiguracja stacji, jak `/settings`). Zdrowie źródeł na pulpicie widzi każdy.
- **R15 HTTP:** limit czasu 20 s, odpowiedź do 2 MiB, `User-Agent: aars/<wersja>` bez danych stacji. Testy: wiremock na `127.0.0.1` — zero ruchu na zewnątrz. Jeżeli `reqwest` z `rustls` nie zbuduje się na macOS, zatrzymaj się i zgłoś (nie przechodzić na `native-tls`; cross-compile to etap 10).
- **R16 Kolejność zapisu:** najpierw `enqueue`, potem zapis stanu. Gdy zapis stanu zawiedzie, następny cykl nada ponownie — duplikat ostrzeżenia jest lepszy niż zgubione. Żadna transakcja bazy nie trwa przez `await` na HTTP.
- **R17 Pulpit (poprawka bezpieczeństwa z przeglądu etapu 7):** błąd odczytu kolejki albo stanu źródeł nie daje 500 — pulpit renderuje się z wyłącznikiem nadawania i notką o błędzie, zamiast tabeli (pusta tabela kłamałaby „nic nie czeka”).

## Global Constraints

- Kierunek zależności (spec §3): `aars` → {`aars-web`, `aars-sources`, `aars-audio`, `aars-ptt`} → `aars-core`. `aars-web` nie zależy od `aars-sources`.
- Źródła produkują tekst, nie audio (D8). Normalizacja zostaje w `aars-audio::text` i działa w syntezie (`PiperSynth`), więc źródło pisze cyfry i `HH:MM`.
- Awaria źródła izolowana: błąd do `source_state`, backoff, zero paniki, zero wpływu na dyspozytora i sekwencer (spec §7, §10). Żadne `unwrap`/`expect`/indeksowanie na danych z sieci.
- Role (spec §8): viewer czyta; operator nadaje, planuje, anuluje; admin konfiguruje. Kill switch dostępny operatorowi i **zawsze widoczny** na pulpicie.
- CSRF na każdym formularzu zmieniającym stan; pola formularzy źródła jako `String` z `#[serde(default)]`, żeby brak pola nie dawał 422 przed sprawdzeniem roli (macierz z etapu 7).
- Czasy w panelu w obu zapisach przez `chrome::both_times_now` (`18:00 UTC (20:00 lokalnie)`).
- Teksty panelu i komunikatów po polsku; priorytety jako Alarmowy / Wysoki / Zwykły / Niski.
- Żaden test nie dotyka sieci zewnętrznej, karty dźwiękowej ani GPIO (spec §11).
- TDD, commit po każdym zielonym cyklu. Zakończenie: `cargo test --workspace` zielone, `cargo clippy --workspace --all-targets -- -D warnings` czyste.

## Review Focus

- **API odpowiada `{"message":"Brak ostrzeżeń…"}`** — to pusta lista (sukces), nie błąd parsowania; jedno takie odpytanie nie odwołuje niczego, dwa z rzędu odwołują aktywne ostrzeżenie dokładnie raz. Testy w Task 5 i Task 4.
- **Odpowiedź zepsuta (HTML, `500`, ucięty JSON, jeden wpis bez `teryt`)** — żadne śledzone ostrzeżenie nie zostaje odwołane, błąd jest w `source_state.last_error`, licznik porażek rośnie; dobry wpis obok zepsutego nadal jest ogłaszany. Testy w Task 4, 5, 6.
- **Ostrzeżenie do `00:00` następnego dnia** — ma brzmieć „do godziny dwudziestej czwartej” z datą dnia, który się kończy, nie z datą następnego. Testy w Task 2 i Task 5.
- **Admin zmienia obszar** — ostrzeżenia spoza nowego obszaru nie są „odwoływane” na antenie. Test w Task 4.
- **IMGW wydaje ostrzeżenie ponownie pod nowym `id`** — bez zmian: cisza; z podniesionym stopniem: jeden komunikat „Zmiana ostrzeżenia” z nowym priorytetem, bez osobnego odwołania. Test w Task 4.

---

### Task 1: Pulpit pokazuje wyłącznik nadawania mimo błędu odczytu kolejki

**Files:**
- Modify: `crates/aars-web/src/lib.rs` (`queue_rows`, `dashboard`, `DashboardPage`, `queue_fragment_response`)
- Modify: `crates/aars-web/templates/dashboard.html`

**Interfaces:**
- Produces: `async fn queue_rows(state: &AppState, limit: i64) -> Result<Vec<QueueRow>, DbError>` (błąd już zalogowany przez `tracing::error!`); `DashboardPage.queue_error: Option<String>`. Fragment `/fragments/queue` i strona `/queue` dalej odpowiadają 500 przez `db_error` (HTMX przy błędzie nie podmienia treści, więc na pulpicie zostaje ostatnia dobra tabela).
- Tekst notki: `Nie udało się odczytać kolejki — szczegóły w dzienniku stacji. Wyłącznik nadawania działa.`

- [ ] **Step 1: Test** (w `lib.rs::tests`)
```rust
#[tokio::test]
async fn the_kill_switch_is_shown_even_when_the_queue_cannot_be_read() {
    let (app, cookie, _csrf, state) = logged_in(Role::Operator).await;
    sqlx::query("DROP TABLE queue_items").execute(&state.pool).await.unwrap();
    let response = app.oneshot(/* GET / z cookie */).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = /* ciało */;
    assert!(html.contains("action=\"/tx-enabled\""));
    assert!(html.contains("Nie udało się odczytać kolejki"));
    assert!(!html.contains("Kolejka pusta."), "pusta tabela kłamałaby, że nic nie czeka");
}
```
- [ ] **Step 2: Uruchom — FAIL** (`cargo test -p aars-web the_kill_switch_is_shown`; dziś 500)
- [ ] **Step 3: Implementacja** — `queue_rows` zwraca `DbError`; pulpit przy błędzie: `pending` puste, `queue_error = Some(..)`, szablon pokazuje notkę zamiast `{% include "queue_fragment.html" %}` (bez `hx-get`, żeby nie podmieniać notki co 3 s na 500). `queue_fragment_response` mapuje błąd przez `db_error`.
- [ ] **Step 4: Uruchom — PASS** (cały `cargo test -p aars-web`, łącznie z macierzą ról)
- [ ] **Step 5: Commit** — `fix(web): pulpit z wyłącznikiem nadawania także przy błędzie odczytu kolejki`

---

### Task 2: Normalizacja — godziny w przypadku zależnym i daty

**Files:**
- Modify: `crates/aars-audio/src/text/numbers.rs`, `crates/aars-audio/src/text/normalize.rs`
- Modify: `crates/aars/tests/cli.rs:54` (oczekiwany tekst)

**Interfaces:**
- Produces: `numbers::hour_ordinal_genitive(h: u8) -> &'static str` („dwunastej”, `0` i `24` → „dwudziestej czwartej”); `numbers::day_ordinal_genitive(d: u8) -> Option<&'static str>` (1–31, „pierwszego” … „trzydziestego pierwszego”); `normalize::MONTHS_GENITIVE: [&str; 12]` (`stycznia` … `grudnia`), publiczne, bo używa go `imgw_meteo::speech`.
- Reguły w `normalize` (kolejność: przed regułą „sama liczba”):
  - `HH:MM` poprzedzone słowem `od`/`do`/`po`/`około` (rdzeń bez interpunkcji, wielkość liter bez znaczenia) → `godziny <ordinal_genitive>`; poprzedzone `o` → `godzinie <ordinal_genitive>`; inaczej jak dotąd `godzina <ordinal>`. Minuty bez zmian (`trzydzieści`, `zero pięć`).
  - `24:00` jest poprawną godziną (tylko z minutami `00`) → „dwudziesta czwarta” / „dwudziestej czwartej”.
  - Liczba 1–31 bez części dziesiętnej, po której stoi token z rdzeniem z `MONTHS_GENITIVE` → `day_ordinal_genitive`; miesiąc zostaje z interpunkcją.

- [ ] **Step 1: Testy** (zmieniają istniejące asercje — R10)
```rust
#[test]
fn clock_times_take_the_case_their_preposition_needs() {
    assert_eq!(n("od 18:00"), "od godziny osiemnastej");
    assert_eq!(n("do 06:30"), "do godziny szóstej trzydzieści");
    assert_eq!(n("o 23:05"), "o godzinie dwudziestej trzeciej zero pięć");
    assert_eq!(n("Od 10:00"), "Od godziny dziesiątej");
    assert_eq!(n("18:00"), "godzina osiemnasta");
    assert_eq!(n("do 24:00"), "do godziny dwudziestej czwartej");
    assert_eq!(n("24:30"), "24:30", "po 24:00 nie ma minut");
}

#[test]
fn days_before_a_month_become_genitive_ordinals() {
    assert_eq!(n("23 lipca"), "dwudziestego trzeciego lipca");
    assert_eq!(n("1 stycznia,"), "pierwszego stycznia,");
    assert_eq!(n("30 grudnia od 02:00 do 10:00."),
        "trzydziestego grudnia od godziny drugiej do godziny dziesiątej.");
    assert_eq!(n("32 lipca"), "trzydzieści dwa lipca", "poza 1–31 zostaje liczebnik główny");
    assert_eq!(n("od 15 mm do 25 mm"), "od piętnaście milimetrów do dwadzieścia pięć milimetrów",
        "jednostki bez zmian");
}
```
  Plus w `numbers.rs`: `hour_genitive_ordinals` (1 → „pierwszej”, 12 → „dwunastej”, 22 → „dwudziestej drugiej”, 0 i 24 → „dwudziestej czwartej”) i `day_genitive_ordinals` (1, 2, 13, 20, 21, 31; 0 i 32 → `None`). Zaktualizuj `clock_times_become_spoken_hours` i linię `od 18:00 do 06:30,` w `punctuation_glued_to_a_token_does_not_block_normalization`; w `cli.rs` oczekuj `wiatr dwanaście metrów na sekundę o godzinie osiemnastej`.
- [ ] **Step 2: Uruchom — FAIL** (`cargo test -p aars-audio`)
- [ ] **Step 3: Implementacja** — tabele ordinalnych, kontekst poprzedniego rdzenia w pętli `normalize`, `spoken_time` z parametrem przypadku.
- [ ] **Step 4: Uruchom — PASS** (`cargo test -p aars-audio -p aars`)
- [ ] **Step 5: Commit** — `feat(audio): godziny w przypadku zależnym i daty dzienne po polsku`

---

### Task 3: Obszar TERYT i stan źródeł w bazie (`aars-core`)

**Files:**
- Create: `migrations/0003_sources.sql`
- Create: `crates/aars-core/src/area.rs`, `crates/aars-core/src/source_state.rs`
- Modify: `crates/aars-core/src/lib.rs`, `crates/aars-core/src/db.rs` (test listy tabel)

**Interfaces:**
- Migracja:
```sql
CREATE TABLE source_items (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id     TEXT    NOT NULL,
    item_key      TEXT    NOT NULL,
    topic         TEXT    NOT NULL,
    fingerprint   TEXT    NOT NULL,
    area          TEXT    NOT NULL,           -- tekst obszaru, przy którym śledzono (R9)
    summary       TEXT    NOT NULL,           -- wiersz do panelu
    cancel_text   TEXT    NOT NULL,
    priority      INTEGER NOT NULL CHECK (priority BETWEEN 0 AND 3),
    valid_from    TEXT,
    valid_until   TEXT,
    state         TEXT    NOT NULL CHECK (state IN ('active','cancelled','expired','replaced','dropped')),
    misses        INTEGER NOT NULL DEFAULT 0,
    first_seen_at TEXT    NOT NULL,
    last_seen_at  TEXT    NOT NULL,
    queue_item_id INTEGER REFERENCES queue_items(id) ON DELETE SET NULL,
    UNIQUE (source_id, item_key)
);
CREATE INDEX source_items_active_idx ON source_items(source_id, state);
INSERT INTO source_state (source_id, enabled) VALUES ('imgw-meteo', 0);
INSERT INTO settings (key, value) VALUES ('source:imgw-meteo:area', ''), ('source:imgw-meteo:interval_s', '300');
```
- `aars_core::area`: `AreaEntry { code: String, label: String }`, `AreaEntry::matches(&self, teryt: &str) -> bool` (2 cyfry → prefiks, 4 → równość); `parse_area(text: &str) -> Result<Vec<AreaEntry>, String>` (R8: linie albo `;`, pusty → `Ok(vec![])`, kod województwa z 16 parzystych `02`–`32`, powiat = prefiks województwa + `01`–`99`, powiat bez nazwy → `Err("powiat 1061: podaj nazwę do odczytu na antenie")`, duplikat → `Err`, najwyżej 40 wpisów); `voivodeship_name(code: &str) -> Option<&'static str>` (`"10"` → `"łódzkie"`); `area_text(entries: &[AreaEntry]) -> String` (postać kanoniczna, jedna linia na wpis — ją zapisujemy i nią porównujemy obszar w R9).
- `aars_core::source_state`:
  - `pub const INTERVAL_RANGE: RangeInclusive<u64> = 60..=3600;`
  - `SourceConfig { enabled: bool, area: Vec<AreaEntry>, area_text: String, interval_s: u64 }` (zepsuty tekst obszaru w bazie → pusty obszar + `tracing::error!`, nie błąd).
  - `SourceStatus { source_id: String, enabled: bool, last_run_at: Option<DateTime<Utc>>, last_success_at: Option<DateTime<Utc>>, consecutive_failures: i64, last_error: Option<String> }`; `SourceStatus::is_stale(&self, interval_s: u64, now: DateTime<Utc>) -> bool` (R13).
  - `TrackedState { Active, Cancelled, Expired, Replaced, Dropped }` z `as_str`/`parse`; `TrackedItem` = kolumny `source_items` (`priority: Priority`, czasy jako `DateTime<Utc>`); `NewTracked` = to samo bez `id`, `state`, `misses`.
  - `SourceStateRepo::new(pool)` z metodami: `config(&self, id: &str) -> Result<Option<SourceConfig>, DbError>` (`None` = brak wiersza w `source_state`, czyli nieznane źródło); `save_config(&self, id, area_text: &str, interval_s: u64)`; `set_enabled(&self, id, enabled: bool) -> Result<u64, DbError>`; `statuses(&self) -> Result<Vec<SourceStatus>, DbError>`; `record_success(&self, id, now)` (zeruje porażki i błąd); `record_failure(&self, id, now, error: &str) -> Result<i64, DbError>` (zwraca nowy licznik); `active_items(&self, id) -> Result<Vec<TrackedItem>, DbError>`; `upsert_active(&self, item: &NewTracked, now) -> Result<i64, DbError>` (po `UNIQUE` wraca do `active`, `misses = 0`); `touch(&self, row: i64, now)`; `set_misses(&self, row: i64, misses: i64)`; `set_state(&self, row: i64, state: TrackedState)`; `set_queue_item(&self, row: i64, queue_item_id: i64)`.

- [ ] **Step 1: Testy**
  - `area.rs`: `a_voivodeship_code_matches_every_county_inside_it` (`10` pasuje do `1061` i `1020`, nie do `1461`); `a_county_code_matches_only_itself`; `the_voivodeship_gets_its_name_by_default` (`parse_area("10")` → etykieta `województwo łódzkie`); `a_county_needs_a_spoken_name` (`"1061"` → `Err`, `"1061 Łódź"` → `Ok`); `bad_codes_are_refused` (`"11"`, `"1000 x"`, `"3401 x"`, `"abc"`, duplikat `"10\n10"` → `Err`); `entries_can_be_separated_by_lines_or_semicolons` (`"1061 Łódź; 1020 powiat zgierski"` → 2 wpisy, `area_text` daje dwie linie).
  - `source_state.rs`: `imgw_meteo_exists_disabled_after_migration`; `config_round_trips_area_and_interval`; `failures_count_up_and_a_success_resets_them`; `staleness_follows_three_intervals_or_half_an_hour` (interwał 300 s: sukces 29 min temu → nie, 31 min → tak; interwał 1200 s: 50 min → nie, 61 min → tak); `an_item_seen_again_after_cancellation_becomes_active`; `unknown_source_has_no_config`.
- [ ] **Step 2: FAIL** (`cargo test -p aars-core`)
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS**
- [ ] **Step 5: Commit** — `feat(core): obszar TERYT i stan źródeł danych w bazie`

---

### Task 4: Crate `aars-sources` — trait `DataSource` i `SourceRunner`

**Files:**
- Create: `crates/aars-sources/Cargo.toml`, `crates/aars-sources/src/lib.rs`, `crates/aars-sources/src/source.rs`, `crates/aars-sources/src/runner.rs`
- Modify: `Cargo.toml` (member, `reqwest`, `serde_json`, `chrono-tz`, `wiremock` w `[workspace.dependencies]`)

**Interfaces:**
- Consumes: `SourceStateRepo`, `SourceConfig`, `TrackedItem`, `NewTracked`, `TrackedState` (Task 3); `QueueRepo::{enqueue, cancel}`, `EnqueueRequest`; `CancelToken`.
- Produces (`aars_sources::source`):
```rust
pub type FetchFuture<'a> = Pin<Box<dyn Future<Output = Result<SourceOutput, SourceError>> + Send + 'a>>;
pub trait DataSource: Send + Sync {
    fn id(&self) -> &'static str;
    fn default_interval(&self) -> Duration;
    fn fetch<'a>(&'a self, ctx: &'a SourceCtx) -> FetchFuture<'a>;
}
pub struct SourceCtx { pub now: DateTime<Utc>, pub area: Vec<AreaEntry> }
pub struct SourceOutput { pub items: Vec<SourceItem>, pub complete: bool }
pub struct SourceItem {
    pub key: String, pub topic: String, pub fingerprint: String,
    pub text: String, pub change_text: String, pub cancel_text: String, pub summary: String,
    pub priority: Priority, pub interrupting: bool,
    pub valid_from: Option<DateTime<Utc>>, pub valid_until: Option<DateTime<Utc>>,
}
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("błąd połączenia: {0}")] Http(String),
    #[error("serwer odpowiedział kodem {0}")] Status(u16),
    #[error("odpowiedź większa niż {0} bajtów")] TooLarge(usize),
    #[error("nieczytelna odpowiedź: {0}")] Parse(String),
}
```
- Produces (`aars_sources::runner`): `SourceRunner::new(source: Arc<dyn DataSource>, state: SourceStateRepo, queue: Arc<QueueRepo>) -> Self`; `refresh_handle(&self) -> Arc<tokio::sync::Notify>`; `async fn cycle(&self, now: DateTime<Utc>) -> CycleReport` (nigdy nie zwraca błędu ani nie panikuje — błędy idą do `source_state` i logu); `async fn run(self, shutdown: CancelToken)`; `CycleReport { skipped: Option<&'static str> /* "wyłączone" | "brak obszaru" */, fetched: bool, announced: usize, changed: usize, cancelled: usize, error: Option<String> }`; `pub fn next_delay(interval: Duration, consecutive_failures: u32) -> Duration` (R13).
- Algorytm `cycle` (poza tym, co wynika z sygnatur):
```text
cfg = state.config(id); wyłączone albo pusty obszar → skipped, bez sieci, bez zapisu
out = source.fetch(ctx{now, cfg.area}); Err → record_failure, koniec (stan pozycji nietknięty)
active = state.active_items(id)
  pozycje z area ≠ cfg.area_text → Dropped (cisza, R9)
dla każdej item z out.items (valid_until ≤ now → pomiń):
  ten sam key w active:   fingerprint równy → touch;  inny → ogłoś change_text (R7: anuluj stary queue_item)
  nowy key:
    fingerprint równy jakiejkolwiek aktywnej/ogłoszonej w tym cyklu → śledź po cichu (R4)
    jest aktywna o tym samym topic, której key nie ma w out → ogłoś change_text, starą oznacz Replaced (R4)
    inaczej → ogłoś text
aktywne nieobecne w out i nie zastąpione:
  out.complete == false → nic
  valid_until ≤ now → Expired (cisza)
  misses+1 < 2 → set_misses;  inaczej → Cancelled; jeśli stary queue_item jeszcze czekał (cancel == 1) → cisza, inaczej ogłoś cancel_text z Normal (R5, R6, R7)
record_success
ogłoś = enqueue{rendered_text, priority, interrupting, expires_at: valid_until, repeat_count: 1, message_id: None, created_by: None}, potem upsert_active + set_queue_item (R16)
```
  `run`: `cycle(Utc::now())`, potem czekanie `next_delay(cfg.interval_s albo default_interval, porażki)` w kawałkach ≤ 1 s sprawdzających `shutdown`, przerywane przez `refresh_handle().notified()`. Wyłączone źródło sprawdza konfigurację co interwał (panel budzi je przez `notify` po włączeniu).

- [ ] **Step 1: Testy** (`runner.rs`, z `FakeSource`, który zwraca kolejne zaprogramowane `Result<SourceOutput, SourceError>` i liczy wywołania; pula `test_pool()`, źródło `imgw-meteo` włączone z obszarem `10`)
  - `a_new_item_is_queued_once_and_a_repeat_fetch_queues_nothing` — dwa cykle tej samej pozycji → `pending` ma 1 element z jej tekstem, priorytetem i `expires_at = valid_until`.
  - `a_changed_fingerprint_is_announced_with_the_change_text`.
  - `the_same_warning_under_a_new_key_is_adopted_silently` i `a_new_key_with_a_raised_level_is_one_change_not_new_plus_cancel` (Review Focus).
  - `a_missing_item_is_cancelled_after_two_successful_fetches_only` (1 cykl pusty → nic; 2 → jeden `cancel_text`, priorytet `Normal`; 3 → nic).
  - `a_failed_or_incomplete_fetch_never_cancels` (`Err(Parse)` ×3 i `complete=false` ×3 → brak odwołań, `consecutive_failures = 3` po błędach, `last_error` ustawiony); `a_new_item_in_an_incomplete_fetch_is_still_announced`.
  - `an_expired_item_disappears_silently` i `an_item_already_expired_is_not_announced`.
  - `changing_the_area_drops_tracked_items_without_cancelling` (Review Focus).
  - `a_cancellation_of_a_never_aired_warning_cancels_the_queued_one_instead`.
  - `a_disabled_source_or_an_empty_area_does_not_fetch` (licznik wywołań `FakeSource` = 0).
  - `backoff_doubles_up_to_the_ceiling` (`next_delay(300 s, 0|1)` = 300, `2` = 600, `3` = 1200, `5` = 1800; `next_delay(3600 s, 4)` = 3600).
  - `run_stops_on_shutdown_and_wakes_on_refresh` (czas rzeczywisty, interwał 60 s: po pierwszym `fetch` `notify_one` → drugi `fetch` w ≤ 2 s; `shutdown.cancel()` → `run` kończy się w ≤ 2 s; bez `tokio::time::pause`, który gryzie się z pulą sqlx).
- [ ] **Step 2: FAIL** (`cargo test -p aars-sources`)
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS**
- [ ] **Step 5: Commit** — `feat(sources): trait DataSource i runner z idempotencją, odwołaniami i backoffem`

---

### Task 5: `imgw_meteo` — parsowanie, filtr obszaru, tekst komunikatu

**Files:**
- Create: `crates/aars-sources/src/imgw_meteo/mod.rs`, `parse.rs`, `speech.rs`
- Modify: `crates/aars-sources/Cargo.toml` (`chrono-tz`, `serde_json`; dev-dependency `aars-audio` tylko do testów zgodności z normalizacją)
- Test: fiksy z sekcji „Rzeczywiste API IMGW”

**Interfaces:**
- Consumes: `AreaEntry`, `MONTHS_GENITIVE` — **uwaga:** `aars-sources` nie zależy od `aars-audio`; skopiuj 12 nazw miesięcy jako stałą w `speech.rs` (test pilnuje zgodności z normalizacją przez dev-dependency `aars-audio`).
- Produces:
  - `parse::MeteoWarning { id: String, event: String, level: Option<u8>, probability: Option<u8>, valid_from: NaiveDateTime, valid_until: NaiveDateTime, text: String, teryt: Vec<String> }`; `parse::parse_warnings(body: &[u8]) -> Result<ParsedWarnings, SourceError>`; `ParsedWarnings { warnings: Vec<MeteoWarning>, skipped: usize }`. Obiekt z kluczem `message` → pusto; tablica → wpis bez `id`/`nazwa_zdarzenia`/`obowiazuje_od`/`obowiazuje_do`/`teryt` albo z niepoprawną datą → `skipped += 1`; cokolwiek innego → `Err(Parse)`. `stopien` poza `1..=3` → `None`.
  - `speech::warsaw_to_utc(t: NaiveDateTime) -> Option<DateTime<Utc>>` (R11).
  - `speech::validity(from: NaiveDateTime, until: NaiveDateTime) -> String` — `until` równe północy → `24:00` dnia poprzedniego; ten sam dzień: `23 lipca od 10:00 do 22:00`; różne: `od 22:00 23 lipca do 06:00 24 lipca`.
  - `speech::announcement(w: &MeteoWarning, area: &str) -> String`, `speech::change(w, area) -> String` (= `"Zmiana ostrzeżenia. "` + announcement), `speech::cancellation(event: &str, area: &str) -> String`, `speech::level_priority(level: Option<u8>) -> (Priority, bool)` (R6). Dokładne brzmienie:
    - `Ostrzeżenie meteorologiczne {pierwszego|drugiego|trzeciego} stopnia: {event}. Obszar: {area}. Obowiązuje {validity}. Prawdopodobieństwo {p} %. {tresc} Źródło: IMGW.` — bez stopnia: `Ostrzeżenie meteorologiczne: {event}.`; bez prawdopodobieństwa zdanie znika; `tresc` przycięta do 400 znaków na ostatniej kropce (albo granicy słowa + `.`), pusta znika.
    - `Odwołanie ostrzeżenia meteorologicznego: {event}. Obszar: {area}. Źródło: IMGW.`
  - `imgw_meteo::to_output(parsed: ParsedWarnings, ctx: &SourceCtx) -> SourceOutput` (funkcja czysta; `fetch` z Task 6 to HTTP + `parse_warnings` + `to_output`): obszar odczytywany = etykiety pasujących wpisów w kolejności konfiguracji, złączone `", "`; `key = id`, `topic = event` małymi literami, `fingerprint = "{event}|{level}|{od}|{do}|{area}"`, `summary = "{event}, {n}. stopień"` (albo samo zjawisko), `complete = skipped == 0`; wpis bez pasującego kodu pomijany; nieistniejąca chwila → pominięty i `complete = false`.

- [ ] **Step 1: Testy**
  - `parse.rs`: `the_no_warnings_message_is_an_empty_list` (fiks `…-brak.json`); `the_july_fixture_has_five_warnings_with_county_codes` (5 wpisów, `Sk20260722203344412` ma `level == Some(1)`, `probability == Some(80)`, `valid_from == 2026-07-23 10:00`); `a_broken_entry_is_skipped_and_counted` (fiks z usuniętym `teryt` w jednym wpisie → 4 + `skipped = 1`); `html_and_truncated_json_are_parse_errors`.
  - `speech.rs`: `july_storm_for_lodz_reads_like_this` — z fiksu, obszar `Łódź`:
    ```text
    Ostrzeżenie meteorologiczne pierwszego stopnia: Burze. Obszar: Łódź. Obowiązuje 23 lipca od 10:00 do 22:00. Prawdopodobieństwo 80 %. Prognozowane są burze, … lokalnie. Źródło: IMGW.
    ```
    (pełna `tresc` z fiksu); `aars_audio::text::normalize` z tego tekstu zawiera `dwudziestego trzeciego lipca od godziny dziesiątej do godziny dwudziestej drugiej` i `osiemdziesiąt procent`.
    `a_warning_until_midnight_ends_at_24_of_the_same_day` (`2026-07-23 12:00` – `2026-07-24 00:00` → `23 lipca od 12:00 do 24:00`); `a_warning_over_two_days_names_both_dates`; `levels_map_to_priorities` (3 → `(Emergency, true)`, 2 → `(High, false)`, 1/`None` → `(Normal, false)`); `warsaw_time_is_converted_with_daylight_saving` (`2026-07-23 10:00` → `08:00Z`, `2024-12-30 02:00` → `01:00Z`, `2026-03-29 02:30` → `None`); `month_names_match_the_normalizer`.
  - `mod.rs`: `only_warnings_touching_the_area_are_returned` (fiks lipcowy: obszar `1061 Łódź` → 1 pozycja; `10` → 1; `22` → 3; `3064 Poznań` → 0); `two_area_entries_hit_by_one_warning_are_read_together` (`1061 Łódź` + `1020 powiat zgierski` → obszar `Łódź, powiat zgierski`, 1 pozycja); `the_fingerprint_ignores_text_but_not_level_or_validity`.
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars-sources`)
- [ ] **Step 5: Commit** — `feat(sources): imgw-meteo — parsowanie ostrzeżeń, filtr TERYT i polski tekst`

---

### Task 6: `imgw_meteo` — klient HTTP

**Files:**
- Create: `crates/aars-sources/src/imgw_meteo/http.rs`
- Modify: `crates/aars-sources/src/imgw_meteo/mod.rs`

**Interfaces:**
- Consumes: `parse_warnings`, `to_output` (Task 5), `DataSource`, `FetchFuture` (Task 4).
- Produces: `pub const DEFAULT_URL: &str = "https://danepubliczne.imgw.pl/api/data/warningsmeteo";`; `ImgwMeteo::new(url: &str) -> Result<ImgwMeteo, SourceError>` (klient `reqwest` z limitem 20 s, `User-Agent: aars/{CARGO_PKG_VERSION}`) i `ImgwMeteo::with_timeout(url: &str, timeout: Duration) -> Result<ImgwMeteo, SourceError>`; `impl DataSource for ImgwMeteo` (`id() = "imgw-meteo"`, `default_interval() = 300 s`, `fetch` = `get_limited` → `parse_warnings` → `to_output`); `http::get_limited(client: &reqwest::Client, url: &str, max: usize) -> Result<Vec<u8>, SourceError>` (status ≠ 2xx → `Status`, `Content-Length` > `max` albo suma kawałków `chunk()` > `max` → `TooLarge`, błąd/timeout → `Http`); `MAX_BODY: usize = 2 * 1024 * 1024`.

- [ ] **Step 1: Testy** (wiremock, `#[tokio::test]`)
  - `the_real_july_response_is_fetched_and_parsed` (serwer zwraca bajty fiksu → `fetch` z obszarem `1061 Łódź` daje 1 pozycję, `complete`).
  - `the_no_warnings_object_is_an_empty_successful_fetch`.
  - `a_server_error_is_a_status_error` (`503` → `SourceError::Status(503)`).
  - `an_html_maintenance_page_is_a_parse_error` (`200`, `text/html`).
  - `a_slow_server_times_out` (opóźnienie 3 s przy `ImgwMeteo::with_timeout(url, 1 s)` → `Err(Http)`).
  - `an_oversized_body_is_refused` (3 MiB → `TooLarge`).
  - `the_user_agent_names_only_the_program` (matcher nagłówka `user-agent` zaczyna się od `aars/`).
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS**
- [ ] **Step 5: Commit** — `feat(sources): klient HTTP IMGW z limitem czasu i rozmiaru`

---

### Task 7: Wpięcie źródła w stację

**Files:**
- Modify: `crates/aars-core/src/config.rs` (`SourcesConfig`), `aars.example.toml`
- Modify: `crates/aars/Cargo.toml`, `crates/aars/src/main.rs`, `crates/aars/src/cli.rs`
- Modify: `crates/aars-web/src/state.rs` (`source_refresh`)
- Test: `crates/aars-core/src/config.rs`, `crates/aars/tests/cli.rs`

**Interfaces:**
- Produces:
  - `Config.sources: SourcesConfig { imgw_meteo_url: String }`, domyślnie `https://danepubliczne.imgw.pl/api/data/warningsmeteo`, nadpisywalne `AARS_SOURCES__IMGW_METEO_URL`; stary `aars.toml` bez `[sources]` dalej się wczytuje.
  - `pub type SourceRefresh = Arc<dyn Fn(&str) -> bool + Send + Sync>;` w `aars_web::state`; `AppState.source_refresh: Option<SourceRefresh>` (`for_tests` → `None`), `AppState::with_source_refresh(self, SourceRefresh) -> Self`. Zwraca `false` dla nieznanego id.
  - `aars run`: `SourceRunner` dla `ImgwMeteo::new(&config.sources.imgw_meteo_url)` w osobnym zadaniu tokio na wspólnym `shutdown`; `refresh_handle` → `with_source_refresh`; przy zatrzymaniu `await` na zadaniu jak pozostałe.
  - `aars source preview` — jedno pobranie z obszarem z bazy, wypisuje dla każdej pozycji priorytet, `summary`, `text` i znormalizowany tekst; nic nie zapisuje i nie wkłada do kolejki (ręczna weryfikacja na żywym API).

- [ ] **Step 1: Testy** — `config.rs`: `a_config_without_sources_uses_the_imgw_default`, `the_imgw_url_can_be_overridden_from_the_environment`. `cli.rs`: `source_preview_without_an_area_says_so` (świeża baza → wyjście zawiera `brak obszaru`, kod 0, bez sieci).
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test --workspace`); ręcznie: `cargo run -p aars -- source preview` po ustawieniu obszaru `sqlite3 var/aars.db "UPDATE settings SET value='10' WHERE key='source:imgw-meteo:area'"` — przy braku ostrzeżeń wypisuje „brak ostrzeżeń dla obszaru”.
- [ ] **Step 5: Commit** — `feat: źródło imgw-meteo uruchamiane razem ze stacją`

---

### Task 8: Panel — strona `/sources` i zdrowie źródeł na pulpicie

**Files:**
- Create: `crates/aars-web/src/sources_page.rs`, `crates/aars-web/templates/sources.html`
- Modify: `crates/aars-web/src/lib.rs` (trasy, `DashboardPage`), `crates/aars-web/src/chrome.rs` (`source_label`, etykiety audytu), `crates/aars-web/templates/dashboard.html`, `crates/aars-web/templates/base.html` (link „Źródła” dla operatora)
- Modify: `crates/aars-web/src/role_matrix.rs`

**Interfaces:**
- Consumes: `SourceStateRepo::{statuses, config, active_items, set_enabled, save_config}`, `SourceStatus::is_stale`, `parse_area`, `area_text`, `INTERVAL_RANGE`, `AppState.source_refresh`, `both_times_now`, `priority_label`.
- Produces:
  - `chrome::source_label(id: &str) -> &str` (`"imgw-meteo"` → `"Ostrzeżenia meteorologiczne IMGW"`); `audit_action_label`: `source_enabled` → „źródło wł./wył.”, `source_refresh` → „odświeżenie źródła”, `source_settings` → „ustawienia źródła”.
  - `sources_page::health(status: &SourceStatus, cfg: &SourceConfig, now) -> (&'static str /*css*/, String /*tekst*/)`: `wyłączone` / `brak obszaru` / `działa` / `błąd (n prób)` / `przestarzałe` (przestarzałe wygrywa z „działa”).
  - `GET /sources` (operator): dla każdego źródła — nazwa, plakietka `health`, ostatnia próba i ostatni sukces (`both_times_now`, „nigdy”), `last_error`, tabela aktywnych ostrzeżeń (`summary`, priorytet PL, ważność od–do w obu zapisach); przyciski „Włącz/Wyłącz” i „Odśwież teraz”; dla admina formularz `area` (textarea, opis formatu z R8) i `interval_s`.
  - `POST /sources/{id}/enabled` (operator; pola `csrf`, `enabled`) → `303 /sources`, po włączeniu woła `source_refresh(id)`; `POST /sources/{id}/refresh` (operator) → `303`, brak `source_refresh` → `503` „odświeżanie niedostępne: źródła nie są uruchomione”; `POST /sources/{id}/settings` (admin; `area`, `interval_s`) → `303` albo `400` z treścią błędu `parse_area` / „interwał: 60–3600 s”, bez zapisu. Nieznane `id` → `404`. Każda zmiana → `audit` z id źródła w `detail`.
  - Pulpit: karta „Źródła danych” (każda rola): nazwa, plakietka `health`, liczba aktywnych ostrzeżeń; błąd odczytu → notka `Nie udało się odczytać stanu źródeł.` i dalej 200 (R17).

- [ ] **Step 1: Testy** (`sources_page.rs`, przez router)
  - `an_operator_sees_the_source_status_and_active_warnings` (wiersz `source_items` aktywny z `summary` „Burze, 1. stopień” → strona zawiera go i „Zwykły”).
  - `an_operator_enables_the_source_and_it_is_woken` (fałszywy `source_refresh` liczący wywołania → `enabled = 1` w bazie, licznik 1, audyt `source_enabled`).
  - `refresh_without_running_sources_is_503`.
  - `an_admin_saves_area_and_interval` (`area=1061 Łódź\n10`, `interval_s=600` → `config` w bazie ma 2 wpisy i 600).
  - `a_county_without_a_name_is_refused_and_nothing_changes` (`400`, w bazie dalej `''`); `an_interval_out_of_range_is_refused` (`30` → `400`).
  - `an_operator_cannot_change_the_area` (`403`); `a_viewer_cannot_open_sources` (`403`); `an_unknown_source_is_404`.
  - `the_dashboard_shows_source_health_to_a_viewer` (wyłączone źródło → „wyłączone”); `the_dashboard_survives_a_broken_source_table` (`DROP TABLE source_state` → `200`, notka, przycisk `/tx-enabled` dla operatora).
  - `role_matrix.rs`: dopisz GET `/sources` (OPERATOR); POST-y `/sources/imgw-meteo/enabled` (OPERATOR, z poprawnym CSRF `303`), `/sources/imgw-meteo/refresh` (OPERATOR, `503`), `/sources/imgw-meteo/settings` (ADMIN, `400`); do `POST_BODY` i `VALID_BODY` dopisz `&area=&interval_s=5`.
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars-web`)
- [ ] **Step 5: Commit** — `feat(web): strona źródeł danych i zdrowie źródeł na pulpicie`

---

### Task 9: Kryterium wyjścia — fiks IMGW do kolejki, dokumentacja

**Files:**
- Create: `crates/aars-sources/tests/imgw_meteo_to_queue.rs`
- Modify: `README.md` (sekcja „Źródła danych”: obszar, interwał, `aars source preview`), `docs/superpowers/plans/README.md` (wiersz 8: plik `2026-11-20-etap-08-imgw-meteo.md`, ✓), `aars.example.toml` (`[sources]` z komentarzem — jeśli nie zrobione w Task 7)

**Interfaces:**
- Consumes: `ImgwMeteo::new`, `SourceRunner::{new, cycle}`, `SourceStateRepo::{set_enabled, save_config}`, `QueueRepo::pending`, `aars_core::db::test_pool` (feature `testing`).

- [ ] **Step 1: Test** — wiremock serwuje `warningsmeteo-2026-07-23.json`; źródło włączone, obszar `1061 Łódź`; `now = 2026-07-23T06:00:00Z`:
```rust
let first = runner.cycle(now).await;
assert_eq!(first.announced, 1);
let pending = queue.pending(10).await.unwrap();
assert_eq!(pending.len(), 1);
assert_eq!(pending[0].priority, Priority::Normal);          // stopień 1
assert!(!pending[0].interrupting);
assert!(pending[0].rendered_text.starts_with(
    "Ostrzeżenie meteorologiczne pierwszego stopnia: Burze. Obszar: Łódź. Obowiązuje 23 lipca od 10:00 do 22:00."));
assert_eq!(pending[0].expires_at, Some(Utc.with_ymd_and_hms(2026, 7, 23, 20, 0, 0).unwrap()));

let second = runner.cycle(now + chrono::Duration::minutes(5)).await;
assert_eq!(second.announced + second.changed + second.cancelled, 0);
assert_eq!(queue.pending(10).await.unwrap().len(), 1, "powtórne pobranie bez zmian nic nie dokłada");
```
  Drugi test w tym pliku: `a_raised_level_is_announced_once_with_a_higher_priority` — trzecie odpytanie z fiksem, w którym `Sk20260722203344412` ma `stopien = "2"` → dokładnie jeden nowy element `High` zaczynający się od `Zmiana ostrzeżenia.`, a czwarte z tym samym → nic.
- [ ] **Step 2: FAIL / PASS** — powinien przejść od razu; czerwony wynik to błąd w Task 4–6, nie w teście.
- [ ] **Step 3: Pełna weryfikacja:** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`; ręcznie `cargo run -p aars -- run`, w `/sources` admin ustawia obszar `10`, operator włącza źródło i klika „Odśwież teraz” — status „działa”, ostatni sukces teraz; przy braku ostrzeżeń kolejka pusta, dziennik bez błędów.
- [ ] **Step 4: Commit** — `test(sources): ostrzeżenie IMGW z prawdziwego fiksu trafia do kolejki raz; dokumentacja etapu 8`

---

## Kryterium wyjścia z etapu

- Z fiksem prawdziwych danych IMGW (`warningsmeteo-2026-07-23.json`) ostrzeżenie dla skonfigurowanego obszaru (`1061 Łódź`) staje się **dokładnie jednym** polskim komunikatem w kolejce z właściwym priorytetem (stopień 1 → Zwykły; stopień 2 → Wysoki; stopień 3 → Alarmowy, przerywający) (Task 9, Task 5).
- Powtórne pobranie bez zmian nie wkłada do kolejki niczego (Task 9, Task 4).
- Macierz rola × endpoint przechodzi, łącznie z nowymi trasami `/sources` (Task 8).
- Pulpit pokazuje wyłącznik nadawania także wtedy, gdy odczyt kolejki zawiedzie (Task 1).
- `cargo test --workspace` zielone bez ruchu sieciowego, `clippy -D warnings` czyste.
