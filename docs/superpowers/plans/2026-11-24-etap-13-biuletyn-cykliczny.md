# Etap 13 — cykliczny komunikat, obszar z pozycji, panel bez przeładowań

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stacja nadaje co 10 minut pełny, bogaty komunikat (ostrzeżenia, pogoda z prognozą, wody, propagacja, słońce), RCB co minutę, obszar liczy się sam z pozycji, a panel nie przeładowuje stron.

**Architecture:** Rozszerzamy istniejące klocki: `BulletinRunner` dostaje tryb cykliczny, część `ostrzezenia` czyta pełną treść żywych alertów (nowa kolumna `tracked_items.text`), prognoza dostaje więcej pól i nowy tekst, runner źródeł powtarza alerty z polityką `repeat_every`, `aars-core::area` liczy powiat z wbudowanego GeoJSON. Panel: htmx (`hx-boost`, wymiana kart, powiadomienia przez `HX-Trigger`).

**Tech Stack:** Rust 2024, tokio, sqlx/SQLite, axum 0.8, askama, htmx (vendored), chrono-tz.

**Spec:** `docs/superpowers/specs/2026-11-24-etap-13-biuletyn-cykliczny-design.md`

## Global Constraints

- Commity bez jakiejkolwiek atrybucji AI (bez Co-Authored-By, Claude-Session, „Generated with”).
- Nie uruchamiać `cargo fmt` na całym repo; `rustfmt --edition 2024` tylko na nowych plikach.
- Nic nie otwiera portu szeregowego, karty dźwiękowej ani GPIO; nie uruchamiać binarki `aars` (wolno `panel_demo`).
- Nic nie może kluczyć nadajnika bez świadomej decyzji; przyciski z jawną wartością docelową, POST + CSRF.
- Teksty na antenę po polsku, bez angielskich słów i surowych kodów; jednostki jako tokeny normalizera (`°C`, `%`, `hPa`, `m/s`, `km/h`, `mm`).
- `aars-web` nie zależy od `aars-sources`.
- Każde zadanie: `cargo test --workspace` i `cargo clippy --workspace --all-targets --locked -- -D warnings` zielone, także `cargo +1.100.0 clippy … ` z `CARGO_TARGET_DIR=target/ci`.
- Bez JS panel działa jak dziś (formularze + przekierowania).

## Review Focus

1. Cykl co 10 min przy restarcie stacji i zmianie czasu (DST) nie nadaje podwójnie ani nie gubi cyklu → idempotencja przez `bulletin_fires`.
2. Powtórka RCB co 1 min przy niedziałającym nadawaniu (wyłączone/watchdog) nie zapycha kolejki → najwyżej jeden oczekujący element na pozycję.
3. Pozycja na granicy powiatów / poza Polską / brak pozycji → poprawny powiat albo czytelne „brak”.
4. Prognoza z brakującymi polami (MET Norway fallback, luki w godzinach) → zdania bez luk, nigdy „None”/„NaN”.
5. Panel: zapis z błędem walidacji przez htmx pokazuje błąd przy polu bez utraty wpisanych wartości.

---

### Task 1: Obszar z pozycji stacji

**Files:** Create `crates/aars-core/src/area_auto.rs`; Modify `crates/aars-core/src/lib.rs`, `crates/aars-sources/src/runner.rs` (kontekst obszaru), `crates/aars-core/src/source_state.rs` (jeśli `SourceConfig` składa obszar), panel źródeł/modułów (wyświetlenie).

**Interfaces — Produces:**
- `aars_core::area_auto::county_at(lat: f64, lon: f64) -> Option<County>`; `pub struct County { pub teryt: String /*4 cyfry*/, pub name: String /*„powiat opoczyński”*/ }`.
- `aars_core::area_auto::auto_area(position: Option<&StationPosition>, granularity: AreaGranularity) -> Option<AreaEntry>`; `pub enum AreaGranularity { County, Voivodeship }`. Etykieta: powiat → nazwa z GeoJSON („powiat opoczyński”), województwo → „województwo łódzkie”.
- `ModuleDescriptor` dostaje pole `auto_area: Option<AreaGranularity>`: IMGW meteo = County, IMGW hydro = Voivodeship, RSO = Voivodeship, pozostałe `None`.
- Runner: gdy ręczny obszar pusty i `auto_area` jest `Some`, obszar cyklu = `auto_area(position)`; `area_text` kanoniczny jak dla ręcznego (R9 bez zmian).

- [ ] Testy: Rzeszów (50.04, 21.99) → `1863` „powiat Rzeszów”/zgodnie z GeoJSON; Opoczno (51.38, 20.28) → `1007`; Warszawa centrum → `1465`; punkt na Bałtyku i Berlin → `None`; województwo dla Opoczna → `10` „województwo łódzkie”. GeoJSON wbudowany `include_bytes!("../../../assets/powiaty.geojson")`, parsowany raz (`OnceLock`), punkt-w-wielokącie z dziurami MultiPolygon (ray casting).
- [ ] Runner: źródło obszarowe z pustym ręcznym obszarem i pozycją → pobiera dla obszaru automatycznego (test z wiremock jak w `imgw_wodowskazy_to_queue.rs`); bez pozycji → `skipped = "brak pozycji stacji"`.
- [ ] Panel (Moduły i Źródła): przy polu obszaru napis „Automatycznie: powiat opoczyński (1007)” albo „Brak pozycji stacji — ustaw ją w Ustawieniach”; pole ręczne opisane „Nadpisz obszar (zostaw puste, żeby liczyć z pozycji)”. Dane do panelu przez `aars-core` (nie `aars-sources`).
- [ ] Commit: `obszar: powiat i województwo liczone z pozycji stacji`.

### Task 2: Prognoza — więcej danych

**Files:** Modify `crates/aars-sources/src/forecast/{mod.rs,open_meteo.rs,met_no.rs}`, fikstury testowe (nowe pliki obok obecnych).

**Interfaces — Produces:** `Now { temp_c, apparent_c: Option<f64>, humidity_pct: Option<f64>, pressure_hpa: Option<f64>, pressure_trend_hpa_3h: Option<f64>, wind_ms: Option<f64>, gust_ms: Option<f64>, wind_dir_deg: Option<f64>, cloud_pct: Option<f64>, visibility_m: Option<f64>, precip_mm: Option<f64>, code }`; `Day` + `sunrise: Option<NaiveDateTime>, sunset: Option<NaiveDateTime>, wind_dir_deg: Option<f64>`; `Hour` + `code: Option<Condition>, wind_ms: Option<f64>, pressure_hpa: Option<f64>`. Prędkości wiatru w modelu w **m/s** (Open-Meteo `&wind_speed_unit=ms`; MET Norway już m/s). Istniejące pola `*_kmh` usunąć i poprawić wszystkich użytkowników (alerty progowe pogody przeliczają progi km/h → m/s albo porównują w km/h przez `ms * 3.6` — zachować ich zachowanie, testy alertów zielone bez zmiany progów).
- [ ] Open-Meteo URL: `current=temperature_2m,apparent_temperature,relative_humidity_2m,pressure_msl,wind_speed_10m,wind_gusts_10m,wind_direction_10m,cloud_cover,visibility,weather_code,precipitation`, `hourly=temperature_2m,precipitation,wind_speed_10m,wind_gusts_10m,weather_code,pressure_msl`, `daily=…,sunrise,sunset,wind_direction_10m_dominant`, `past_hours=3` (tendencja ciśnienia z godzinowego `pressure_msl`: teraz minus 3 h temu).
- [ ] Nowa fikstura Open-Meteo z prawdziwej odpowiedzi (pobrać raz `curl` dla 51.38,20.28 i zapisać do `tests/fixtures/pogoda/`), test parsera na wszystkie pola; test luk (pola `null`).
- [ ] MET Norway: `air_pressure_at_sea_level`, `relative_humidity`, `wind_from_direction`, `cloud_area_fraction`; brak wschodu/zachodu → `None`.
- [ ] Commit: `prognoza: wilgotność, ciśnienie z tendencją, kierunek wiatru, zachmurzenie, słońce`.

### Task 3: Pogoda słowami

**Files:** Modify `crates/aars-sources/src/pogoda/speech.rs`, `crates/aars-sources/src/speech.rs` (wspólne: kierunek wiatru, opis zachmurzenia, tendencja), `pogoda/mod.rs` (`max_chars` → 900).

**Interfaces — Produces:** `speech::wind_direction(deg: f64) -> &'static str` (8 kierunków: „północny”, „północno-wschodni”, „wschodni”, „południowo-wschodni”, „południowy”, „południowo-zachodni”, „zachodni”, „północno-zachodni”; sektor 45° wyśrodkowany); `pogoda::speech::bulletin(forecast, now) -> Option<String>`.

Treść (przykład, każdy człon tylko gdy dane są):
„Pogoda dla okolic stacji. Obecnie pochmurno, temperatura 8 °C, odczuwalna 5 °C. Wilgotność 82 %. Ciśnienie 1020 hPa, rośnie. Wiatr południowo-zachodni 4 m/s, w porywach 9 m/s. Widzialność 3 km. W najbliższych godzinach przelotny deszcz, do 2 mm. Dziś do 10 °C. W nocy do 2 °C. Jutro małe zachmurzenie, od 3 do 11 °C, bez opadów, wiatr zachodni do 6 m/s. Wschód słońca 7:15, zachód 15:48. Dane: Open-Meteo.”
- Odczuwalna tylko przy różnicy ≥ 2 °C; widzialność tylko < 5 km („Widzialność 800 m” poniżej 1 km); tendencja: |Δ| < 1 hPa „bez zmian”, inaczej „rośnie”/„spada”; wiatr < 1 m/s → „Cisza” zamiast kierunku; „najbliższe godziny” = następne 6 h hourly (opis najdotkliwszy, suma opadu, max porywów jeśli > porywy teraz o 3 m/s).
- Przed 14:00 „Dziś… W nocy…”; od 14:00 „W nocy… Jutro…”; godziny słońca w czasie Warszawy `H:MM`.
- [ ] Testy na fiksturze (dokładny oczekiwany tekst), na lukach, `assert_speakable` (istniejący helper), test 8 kierunków i granic sektorów (22.4°, 22.5°, 337.5°, 359°).
- [ ] Commit: `pogoda: pełny komunikat — wilgotność, ciśnienie, wiatr z kierunkiem, prognoza, słońce`.

### Task 4: Pełna treść ostrzeżeń w komunikacie

**Files:** Create `migrations/0007_tracked_text.sql` (`ALTER TABLE tracked_items ADD COLUMN text TEXT NOT NULL DEFAULT ''`); Modify `aars-core/src/source_state.rs` (zapis/odczyt `text` z `SourceItem.text`, także przy zmianie), `aars-sources/src/runner.rs`, `aars-sources/src/bulletin/ostrzezenia.rs`.

**Interfaces — Produces:** `TrackedItem.text: String` (ostatnia pełna treść pozycji, ta sama, która poszła/pójdzie na antenę).
- Część `ostrzezenia` czyta pełne `text` aktywnych alertów (pusta → `summary`), sortuje: priorytet malejąco, potem `first_seen_at`; `max_chars` = 1200; gdy nie mieści się, pełne treści pierwszych N, potem „Oraz jeszcze K ostrzeżeń.” (licznik poprawnie odmieniony: 1 ostrzeżenie, 2–4 ostrzeżenia, 5+ ostrzeżeń).
- Brak ostrzeżeń: „Brak obowiązujących ostrzeżeń dla okolic stacji.”; niedostępne źródła jak dziś.
- [ ] Testy: migracja na istniejącej bazie (pozycje bez `text` → summary), pełna treść w biuletynie, kolejność, odmiana licznika, obcinanie.
- [ ] Commit: `biuletyn: obowiązujące ostrzeżenia pełną treścią`.

### Task 5: Komunikat cykliczny

**Files:** Modify `aars-core/src/module.rs` (ustawienia biuletynu), `aars-sources/src/bulletin/{runner.rs,mod.rs}`, `aars-core/src/bulletin.rs` jeśli potrzeba, `crates/aars-web/src/status.rs` (najbliższy biuletyn dla trybu cyklicznego).

**Interfaces — Produces:**
- Ustawienia `module:bulletin:*`: `mode` (`Choice`: `cykl` „Co określony czas” / `pory` „O stałych porach”, domyślnie `cykl`), `interval_min` (`Integer 5..=60`, domyślnie `10`), `times` bez zmian (używane w trybie `pory`).
- `runner::cycle_fires(interval_min: u32, now) -> Vec<DateTime<Utc>>`: chwile wyrównane do wielokrotności `interval_min` od północy czasu Warszawy (dla 10: :00, :10, …), z ostatnich 24 h, już minione, rosnąco — dalej ta sama ścieżka co `due_fires` (late/superseded/claim/quiet/compose/enqueue).
- `EXPIRY` elementu = `min(interval, 15 min)` w trybie cyklicznym.
- Kolejność części w `BULLETIN_PARTS`: `ostrzezenia`, `pogoda`, `wody`, `propagacja` (+ nowa `slonce` nie — słońce jest w pogodzie). Domyślne części: wszystkie. Wstęp domyślny: „Tu automatyczna stacja informacyjna {znak}. {dzień tygodnia} {data słownie}, godzina {H:MM}.” Zakończenie: „Tu {znak}. Koniec komunikatu.” (bez znaku: bez członu „Tu …”).
- Przy przekroczeniu długości wypadają części w kolejności: `propagacja`, `wody`, `pogoda`; `ostrzezenia` nie wypada nigdy (obcina się sama do `max_chars`).
- Pasek stanu/kafel „Najbliższy biuletyn” liczy najbliższą chwilę cyklu.
- [ ] Testy: `cycle_fires` (10 min, 15 min, DST 2026-10-25), restart w środku cyklu nie nadaje drugi raz, cisza nocna, długość, kolejność wypadania, kafel panelu.
- [ ] Commit: `biuletyn: tryb cykliczny co 10 minut, nowy wstęp i zakończenie`.

### Task 6: Powtórki alertów (RCB co minutę)

**Files:** Modify `aars-sources/src/source.rs` (`SourceItem.repeat_every: Option<Duration>`), `aars-sources/src/runner.rs`, `aars-sources/src/rso/mod.rs` (+ ustawienie `rcb_repeat_min` Integer 1..=10, domyślnie 1), wszystkie miejsca tworzące `SourceItem` (`repeat_every: None`).

**Interfaces — Produces:** runner w każdym cyklu (i niezależnie od interwału źródła — osobny tik co 15 s w `run`) dla aktywnych pozycji z `repeat_every`: jeśli ostatni element kolejki pozycji zakończył się (`done`, `expired`, `failed`, `cancelled`) i od jego końca minęło ≥ `repeat_every`, wstawia nowy element z tym samym `text`, priorytetem i `interrupting=false` (powtórka nie przerywa), `expires_at = now + repeat_every*2`; nigdy dwa oczekujące naraz. `repeat_every` nie jest zapisywane w bazie — przy restarcie odtwarza się z kolejnego `fetch`. RSO: Alert RCB i atak z powietrza → `Some(rcb_repeat_min)`; reszta `None`.
- [ ] Testy: powtórka po nadaniu, brak powtórki gdy poprzedni czeka (nadawanie wyłączone), koniec powtórek po odwołaniu/wygaśnięciu, tik 15 s nie odpytuje sieci.
- [ ] Commit: `alerty: Alert RCB i atak z powietrza powtarzane co minutę, dopóki obowiązują`.

### Task 7: Panel bez przeładowań — mechanizm

**Files:** Modify `crates/aars-web/templates/base.html` (`hx-boost="true"` na `<body>` z `hx-target="#main"`/`hx-select`, kontener powiadomień `#toasts` z `aria-live`), `assets/app.css` (powiadomienia), nowy `assets/panel.js` (≤ 60 linii: obsługa `HX-Trigger` `toast` → powiadomienie, zachowanie fokusu), `crates/aars-web/src/render.rs` (helper `htmx::is_htmx(headers)`, `toast(response, kind, text)` ustawiający `HX-Trigger`), handlery POST: moduły (`enabled`, `settings`), źródła (`enabled`, `refresh`), ustawienia stacji/pozycji/sprzętu, komunikaty (`enabled`, `send`, `delete`), kolejka (`cancel`, `cancel_group`), użytkownicy.

**Interfaces — Produces:** przy żądaniu htmx (`HX-Request: true`) handler zwraca **HTML zaktualizowanej karty** (ten sam szablon co na stronie, wydzielony do `{% block card %}`/osobnego szablonu fragmentu) i nagłówek `HX-Trigger: {"toast":{"kind":"ok","text":"Zapisano"}}`; błąd walidacji → karta z błędem przy polu i wpisanymi wartościami, status 422 (htmx: `hx-swap` dla 422 przez `htmx.config.responseHandling`), toast „Popraw zaznaczone pola”. Bez `HX-Request` — dzisiejsze przekierowanie/strona. Formularze dostają `hx-post` + `hx-target="closest .card"` + `hx-swap="outerHTML"`.
- [ ] Testy (dla każdego przerobionego handlera): z `HX-Request` → 200 + fragment bez `<html>` + `HX-Trigger` z toastem; bez → jak dziś (istniejące testy zostają). Macierz ról zielona. `ui_checks`: brak skryptów inline, etykiety.
- [ ] Commit: `panel: zapis bez przeładowania strony, powiadomienia, nawigacja hx-boost`.

### Task 8: Strona Moduły od nowa

**Files:** Modify `crates/aars-web/templates/modules.html`, `crates/aars-web/src/modules_page.rs`, `assets/app.css`.

- Układ: po lewej lista modułów (nazwa, plakietka stanu, przełącznik), po prawej szczegóły wybranego (`/modules?m=<id>`, htmx wymienia tylko panel szczegółów); na wąskim ekranie lista nad szczegółami.
- Szczegóły: opis, stan źródła (ostatnie pobranie, błąd), obszar automatyczny (Task 1), ustawienia z opisami, przy biuletynie: tryb, interwał, części (z opisem co mówi każda), przyciski „Podgląd biuletynu”, „Odsłuchaj”, „Nadaj biuletyn teraz”.
- [ ] Testy: lista i szczegóły dla trzech ról, `?m=` nieznany → pierwszy moduł, htmx zwraca sam panel szczegółów.
- [ ] Zrzuty z `panel_demo` (1400 i 390 px) obejrzane przez kontrolera.
- [ ] Commit: `panel: strona Moduły — lista i szczegóły, obszar automatyczny`.

### Task 9: Demo, poradnik, strona

- `panel_demo`: dane źródeł zgodne z prawdziwymi opisami (nie `sample_source` z „Obszarem” dla wszystkich), tryb cykliczny biuletynu.
- Poradnik sp7tq-web: rozdziały Biuletyn, Pozycja i obszar, Źródła (RCB co minutę), Panel; nowe zrzuty.
- [ ] Commit w obu repo (sp7tq-web z linijkami atrybucji tej strony).
