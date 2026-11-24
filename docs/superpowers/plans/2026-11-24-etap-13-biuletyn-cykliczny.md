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
2. Seria RCB (3× co minutę) przy wyłączonym nadawaniu nie zalega po odwołaniu alertu → reszta serii anulowana.
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

**Interfaces — Produces:** `speech::wind_direction(deg: f64) -> &'static str` (8 kierunków: „północny”, „północno-wschodni”, „wschodni”, „południowo-wschodni”, „południowy”, „południowo-zachodni”, „zachodni”, „północno-zachodni”; sektor 45° wyśrodkowany); `pogoda::speech::now_text(forecast, now) -> Option<String>` („Obecnie… Wilgotność… Ciśnienie… Wiatr… Widzialność…”), `forecast_text(forecast, now) -> Option<String>` („W najbliższych godzinach… Dziś/W nocy/Jutro…”), `sun_text(forecast, now) -> Option<String>` („Wschód słońca…, zachód…”); `bulletin(forecast, now)` = „Pogoda dla okolic stacji. ” + trzy powyższe + atrybucja (zostaje do czasu Task 6, który rozdziela części).

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

### Task 5: Pauzy w syntezie

**Files:** Modify `crates/aars-audio/src/cache.rs` (albo nowy `crates/aars-audio/src/pause.rs` + użycie w `AudioCache::render`).

**Interfaces — Produces:** znacznik pauzy w tekście kolejki: `[[pauza:MS]]` (MS całkowite 0–10000; większe obcinane do 10000, błędne — znacznik usuwany i ostrzeżenie w logu). `aars_audio::pause::split(text) -> Vec<Segment>`; `enum Segment { Text(String), Pause(Duration) }`. `AudioCache::render` dla tekstu ze znacznikami: syntezuje każdy odcinek tekstu osobno (każdy przez cache), wstawia ciszę `Pcm::silence(sample_rate, d)`, skleja i zapisuje całość w cache pod kluczem pełnego tekstu. Tekst bez znaczników — ścieżka bez zmian. Znacznik nigdy nie trafia do syntezatora ani do normalizera.
- [ ] Testy (FakeSynth): dwa odcinki + pauza 500 ms → długość = suma + 500 ms ±1 próbka; sama pauza; pauza na początku/końcu; `[[pauza:99999]]` → 10 s; `[[pauza:x]]` usunięty; brak znaczników → identyczny wynik jak dziś.
- [ ] Commit: `synteza: pauzy w tekście komunikatu ([[pauza:MS]])`.

### Task 6: Układ biuletynu z klocków i tryb cykliczny

**Files:** Create `crates/aars-core/src/bulletin_layout.rs`; Modify `aars-core/src/module.rs` (ustawienia biuletynu), `aars-sources/src/bulletin/{mod.rs,runner.rs}`, `aars-sources/src/pogoda/{mod.rs,speech.rs}` (podział na trzy części), `crates/aars-web/src/status.rs` (najbliższy biuletyn w trybie cyklicznym).

**Interfaces — Produces (aars-core, bo panel też tego używa):**
```rust
pub enum BlockKind { Wstep, Ostrzezenia, PogodaTeraz, Prognoza, Slonce, Wody, Propagacja, Tekst, Zakonczenie }
pub enum When { Zawsze, CoNCykli { n: u32 /*2..=24*/ }, WGodzinach { od: NaiveTime, do_: NaiveTime /*przez północ dozwolone*/ } }
pub struct Block { pub kind: BlockKind, pub enabled: bool, pub pause_before_ms: u32, pub pause_after_ms: u32 /*0..=10000*/, pub when: When, pub text: String /*Wstep/Zakonczenie: szablon; Tekst: treść; inne: puste*/ , pub say_when_empty: bool /*Ostrzezenia: „Brak obowiązujących ostrzeżeń…”*/ }
pub struct Layout { pub blocks: Vec<Block> /*1..=24*/ }
impl Layout { pub fn default_layout() -> Layout; pub fn parse(json: &str) -> Result<Layout, String>; pub fn to_json(&self) -> String; pub fn validate(&self) -> Result<(), String> }
```
JSON w `module:bulletin:layout` (serde, nazwy pól po polsku jak w enumach, snake_case). Brak ustawienia → `default_layout()`: Wstep („Tu automatyczna stacja informacyjna {znak}. {dzien}, {data}, godzina {godzina}.”, pauza po 700), Ostrzezenia (say_when_empty, pauza po 700), PogodaTeraz, Prognoza (pauza po 500), Slonce, Wody, Propagacja (CoNCykli 3), Zakonczenie („Tu {znak}. Koniec komunikatu.”). Zmienne szablonu: `{znak}`, `{dzien}` (dzień tygodnia), `{data}` („24 listopada”), `{godzina}` (`H:MM`); nieznana zmienna → błąd walidacji przy zapisie.
- Ustawienia biuletynu: `mode` (`Choice`: `cykl` „Co określony czas” / `pory` „O stałych porach”, domyślnie `cykl`), `interval_min` (`Integer 5..=60`, domyślnie 10), `times` (tryb `pory`), `layout` (JSON, edytowany edytorem z Task 9; walidacja `Layout::parse`). Stare `parts` i `intro` przenoszone do układu przy pierwszym odczycie (części nieobecne → bloki wyłączone; `intro` → tekst bloku Wstep), potem ignorowane.
- `pogoda` dzieli się na części `pogoda_teraz`, `prognoza`, `slonce` (funkcje z Task 3: `now_text`, `forecast_text`, `sun_text`; jeden odczyt prognozy na złożenie).
- Kompozytor: idzie po blokach układu; blok wyłączony albo `when` nie pasuje do numeru cyklu (`cycle_index = minuty od północy / interval`) lub godziny → pomijany; blok danych bez treści → pomijany; między blokami `[[pauza:MS]]` z `pause_before_ms`/`pause_after_ms` (sąsiednie pauzy sumują się, max 10 s). Limit długości: wypadają bloki od końca, z wyjątkiem Ostrzezenia, Wstep i Zakonczenie. `Composed.parts` raportuje każdy blok (polska nazwa, wynik).
- Runner: `cycle_fires(interval_min, now)` — chwile wyrównane do wielokrotności `interval_min` od północy czasu Warszawy, z ostatnich 24 h, minione, rosnąco; dalej ta sama ścieżka co `due_fires`. `EXPIRY` = `min(interval, 15 min)` w trybie cyklicznym.
- Pasek stanu/kafel „Najbliższy biuletyn” liczy najbliższą chwilę cyklu.
- [ ] Testy: domyślny układ, parse/validate (złe pauzy, puste bloki, zła zmienna, > 24 bloki), migracja `parts`/`intro`, `When` (co 3. cykl, godziny przez północ), pauzy w tekście, wypadanie przy limicie, `cycle_fires` (10, 15 min, DST 2026-10-25), restart w cyklu bez dubla, cisza nocna, kafel panelu.
- [ ] Commit: `biuletyn: układ z klocków, tryb cykliczny co 10 minut`.

### Task 7: Alert RCB trzy razy co minutę

**Files:** Modify `aars-sources/src/source.rs` (`SourceItem.repeat: Option<(u32, Duration)>`), `aars-sources/src/runner.rs` (przy pierwszym ogłoszeniu używa `repeat` jako `repeat_count`/`repeat_interval` kolejki; zmiana i odwołanie — bez powtórek), `aars-sources/src/rso/mod.rs` (ustawienia `rcb_repeats` Integer 1..=10 domyślnie 3, `rcb_interval_s` Integer 30..=600 domyślnie 60), wszystkie miejsca tworzące `SourceItem` (`repeat: None`).
- Alert RCB i atak z powietrza: `repeat = Some((rcb_repeats, rcb_interval_s))`. Potem pozycja jest żywym alertem, więc wraca w każdym cyklu biuletynu (blok Ostrzezenia).
- Odwołanie pozycji anuluje niewysłane powtórki serii (grupa kolejki — sprawdzić, czy istniejące anulowanie serii to robi; jeśli nie, dodać).
- [ ] Testy: RCB → 3 elementy co 60 s; ustawienia 5 × 120 s; zwykły komunikat RSO bez powtórek; odwołanie anuluje resztę serii.
- [ ] Commit: `alerty: Alert RCB trzy razy co minutę, potem w biuletynie`.

### Task 8: Panel bez przeładowań — mechanizm

**Files:** Modify `crates/aars-web/templates/base.html` (`hx-boost="true"` na `<body>` z `hx-target="#main"`/`hx-select`, kontener powiadomień `#toasts` z `aria-live`), `assets/app.css` (powiadomienia), nowy `assets/panel.js` (≤ 60 linii: obsługa `HX-Trigger` `toast` → powiadomienie, zachowanie fokusu), `crates/aars-web/src/render.rs` (helper `htmx::is_htmx(headers)`, `toast(response, kind, text)` ustawiający `HX-Trigger`), handlery POST: moduły (`enabled`, `settings`), źródła (`enabled`, `refresh`), ustawienia stacji/pozycji/sprzętu, komunikaty (`enabled`, `send`, `delete`), kolejka (`cancel`, `cancel_group`), użytkownicy.

**Interfaces — Produces:** przy żądaniu htmx (`HX-Request: true`) handler zwraca **HTML zaktualizowanej karty** (ten sam szablon co na stronie, wydzielony do `{% block card %}`/osobnego szablonu fragmentu) i nagłówek `HX-Trigger: {"toast":{"kind":"ok","text":"Zapisano"}}`; błąd walidacji → karta z błędem przy polu i wpisanymi wartościami, status 422 (htmx: `hx-swap` dla 422 przez `htmx.config.responseHandling`), toast „Popraw zaznaczone pola”. Bez `HX-Request` — dzisiejsze przekierowanie/strona. Formularze dostają `hx-post` + `hx-target="closest .card"` + `hx-swap="outerHTML"`.
- [ ] Testy (dla każdego przerobionego handlera): z `HX-Request` → 200 + fragment bez `<html>` + `HX-Trigger` z toastem; bez → jak dziś (istniejące testy zostają). Macierz ról zielona. `ui_checks`: brak skryptów inline, etykiety.
- [ ] Commit: `panel: zapis bez przeładowania strony, powiadomienia, nawigacja hx-boost`.

### Task 9: Strona Moduły od nowa i wizualny edytor biuletynu

**Files:** Modify `crates/aars-web/templates/modules.html`, `crates/aars-web/src/modules_page.rs`; Create `crates/aars-web/src/bulletin_editor.rs`, `crates/aars-web/templates/bulletin_editor.html`, `assets/bulletin-editor.js`; Modify `assets/app.css`, `lib.rs` (trasy), `role_matrix.rs`, menu (`base.html`: „Biuletyn” w grupie Nadawanie).

- Moduły: po lewej lista modułów (nazwa, plakietka stanu, przełącznik), po prawej szczegóły wybranego (`/modules?m=<id>`, htmx wymienia tylko panel szczegółów); na wąskim ekranie lista nad szczegółami. Szczegóły: opis, stan źródła, obszar automatyczny (Task 1), ustawienia z opisami.
- **Edytor biuletynu** `GET /bulletin` (operator ogląda i słucha, admin edytuje), `POST /bulletin/layout` (admin, CSRF, pole `layout` = JSON, walidacja `Layout::parse`):
  - tryb (cykl/pory), interwał, pory;
  - lista klocków jak karty: uchwyt do przeciągania (HTML5 drag and drop) **i** przyciski „W górę/W dół” (klawiatura, bez myszy), przełącznik włączony, nazwa i opis po polsku, „pauza przed/po” w sekundach (krok 0,1 s), „kiedy” (zawsze / co N-ty cykl / w godzinach od–do), pole tekstu dla Wstęp/Zakończenie/Tekst (z listą zmiennych), „Usuń” dla bloków Tekst, „Dodaj własny tekst”;
  - oś czasu: pasek pokazujący kolejność klocków i pauzy proporcjonalnie (szacowany czas: 10 znaków ≈ 1 s), suma vs limit nadawania;
  - „Podgląd” (składa tekst z bieżącego, niezapisanego układu przez `POST /bulletin/preview` z polem `layout`), „Odsłuchaj” (WAV z pauzami), „Zapisz”, „Przywróć domyślny”, „Nadaj teraz”.
  - JS tylko w `assets/bulletin-editor.js` (bez inline), stan w ukrytym polu `layout` (JSON); bez JS edytor pokazuje układ tylko do odczytu i formularz z polem JSON.
- [ ] Testy: role (podgląd — brak dostępu, operator — bez „Zapisz”, admin — pełny), zapis poprawnego i błędnego układu (błąd przy klocku), podgląd niezapisanego układu nie zmienia bazy, ui_checks.
- [ ] Zrzuty z `panel_demo` (1400 i 390 px) obejrzane przez kontrolera.
- [ ] Commit: `panel: strona Moduły i wizualny edytor biuletynu`.

### Task 10: Demo, poradnik, strona

- `panel_demo`: dane źródeł zgodne z prawdziwymi opisami (nie `sample_source` z „Obszarem” dla wszystkich), tryb cykliczny biuletynu.
- Poradnik sp7tq-web: rozdziały Biuletyn, Pozycja i obszar, Źródła (RCB co minutę), Panel; nowe zrzuty.
- [ ] Commit w obu repo (sp7tq-web z linijkami atrybucji tej strony).
