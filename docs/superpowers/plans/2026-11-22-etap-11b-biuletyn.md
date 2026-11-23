# Etap 11b: biuletyn — kompozytor, pogoda, propagacja

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Moduł `bulletin` nadaje o porach lokalnych jeden biuletyn złożony w chwili nadania z części `pogoda`, `propagacja`, `ostrzezenia`; nowe źródło `pogoda` ogłasza przekroczenia progów prognozy; podgląd biuletynu w CLI i w panelu; dwie poprawki odłożone z 11a.

**Architecture:** `aars-core` dostaje rodzaj ustawienia `TimeList`, przełączalny opis modułu `bulletin`, tabelę `bulletin_fires` (idempotencja), zapytanie o żywe ostrzeżenia i wspólną funkcję okna ciszy. `aars-sources` dostaje trait `BulletinPart`, kompozytor `Bulletin` (wspólna ścieżka nadania i podglądu), `BulletinRunner` (pory, spóźnienia, cisza, kolejka), wspólny model prognozy z parserami Open-Meteo i MET Norway, źródło+część `Pogoda` (jedna instancja) i część `Propagacja`. Panel nie zna `aars-sources`: podgląd idzie przez wywołanie zwrotne ustawiane w `station.rs`, jak „odśwież teraz”.

**Tech Stack:** jak w 11a: reqwest 0.13 (rustls), serde_json, chrono 0.4 + chrono-tz 0.10, sqlx 0.8 (dodać `sqlx.workspace = true` do `[dependencies]` `aars-sources` — dziś tylko w dev), tokio (`time::timeout`, `sync::Semaphore`, `task::JoinSet`, `join!`), wiremock 0.6 (testy).

**Spec:** `docs/superpowers/specs/2026-11-22-etap-11b-biuletyn-design.md` — **sekcja „Rozstrzygnięcia po przeglądzie” ma pierwszeństwo** przed resztą dokumentu. Tło: projekt i plan 11a (`docs/superpowers/plans/2026-11-22-etap-11a-alarmy.md`, decyzje A1–A13 obowiązują dalej), spec główny §7.

## Fiksy (prawdziwe odpowiedzi z 2026-11-22, niczego nie poprawiać)

Katalog `crates/aars-sources/tests/fixtures/`:

- `pogoda/open-meteo-rzeszow-2026-11-22.json` — `latitude 50.04659`, `longitude 21.985504`, `utc_offset_seconds 3600`, `timezone "Europe/Warsaw"`. `current`: `time "2026-11-22T19:30"` (czas lokalny, bez strefy), `temperature_2m 2.2`, `wind_speed_10m 9.7`, `wind_gusts_10m 17.6`, `weather_code 0`, `precipitation 0.0`. `daily` (**2 dni**, choć zapytanie prosi o 3): `time ["2026-11-22","2026-11-23"]`, `temperature_2m_max [6.2, 2.5]`, `temperature_2m_min [1.6, 0.9]`, `precipitation_sum [1.2, 2.4]`, `wind_gusts_10m_max [40.3, 45.0]`, `weather_code [53, 73]`. `hourly` 48 godzin.
- `pogoda/met-no-rzeszow-2026-11-22.json` — GeoJSON `Feature`, `geometry.coordinates [21.99, 50.04, 207]`, `properties.meta.updated_at "2026-11-22T17:17:09Z"`, `timeseries` 88 wpisów od `2026-11-22T18:00:00Z` (godzinowe do `2026-11-25T05:00Z`, potem co 6 h). Wpis: `data.instant.details.{air_temperature, wind_speed (m/s)}`, `next_1_hours`/`next_6_hours`/`next_12_hours` z `summary.symbol_code` i `details.precipitation_amount`. Wpis 19:00Z: −0,7 °C, `next_1_hours` `partlycloudy_night`. Dzień lokalny 2026-11-23: 24 wpisy, t −1,9…1,5, suma opadów 1 h = 4,6 mm, max `wind_speed` 7,1 m/s (25,6 km/h), symbole `lightsleet, lightsnow, snow, cloudy, partlycloudy_day/_night`. Porywów **brak**.
- `propagacja/f107-2026-11-22.json` — tablica 122 rekordów, **nieposortowana rosnąco** (pierwszy jest najnowszy, ale nie polegać): `{time_tag "2026-11-21T22:00:00", frequency 2800, flux 98.0, reporting_schedule "Afternoon", …}`; najnowszy `frequency == 2800` = 2026-11-21T22:00 UTC, 98.
- `propagacja/kp-2026-11-22.json` — tablica 62 obiektów `{time_tag, Kp (liczba), a_running, station_count}`; najnowszy `2026-11-22T15:00:00`, `Kp 0.33`.
- `propagacja/kc2g-stations-2026-11-22.json` — tablica 101 rekordów: `fof2`, `mufd` (liczby albo `null`), `cs` (liczba, `-1.0` = skalowanie ręczne), `time` (`"2026-11-22T18:15:00"`, UTC bez strefy), `source`, `station.{name, code, latitude, longitude}` — współrzędne to **napisy**, długość **0–360** (`"262.3"` = −97,7). Od Rzeszowa (50.04, 21.99): Warsaw, Poland (MZ152) 248 km, 18:15, `cs -1`, fof2 3.2, mufd 10.56; Sopron 469 km, `cs 60`; Kaliningrad 497 km, 2022 r.; Pruhonice, Czech Republic (PQ052) 528 km, 18:35, `cs 80`, fof2 3.4, mufd 11.814; Juliusruh 772 km, `cs 15`; Rome, Italy (RM041) 1172 km, `cs -1`; Rome, Italy RO041 `cs 70`; Dourbes 1238 km (poza 1200, stary).
- `swpc/noaa-scales-2026-11-22.json` (z 11a) — `"0"`: `DateStamp 2026-11-22`, `TimeStamp 14:59:00`, R/S/G = 0.

Warianty (inny `cs`, `null`, brak klucza) testy robią w kodzie na `serde_json::Value`.

## Decyzje etapu 11b

Decyzje A1–A13 z 11a obowiązują; nowe mają numery B.

- **B1** Jednostki w tekście jako tokeny normalizera: `°C`, `km/h`, `mm` (normalizer odmienia: „-3 °C” → „minus trzy stopnie Celsjusza”, „do 5 °C” → „do pięciu stopni Celsjusza”). Słowo „stopni” w treści się nie pojawia. Megaherce i metry pasm słownie („3,2 megaherca” — zawsze jedna cyfra po przecinku, żeby forma „megaherca” była poprawna; „do pasma 40 metrów”).
- **B2** `speech::decimal_comma(x: f64) -> String` — jedna cyfra po przecinku, `",0"` obcięte, `-0` → `"0"` (`1.2` → `"1,2"`, `2.0` → `"2"`, `-0.04` → `"0"`); `speech::one_decimal(x) -> String` bez obcinania (`4.0` → `"4,0"`, `10.56` → `"10,6"`); `speech::round_int(x) -> i64` (połówki od zera: `2.5` → 3, `-0.4` → 0).
- **B3** Spóźnienie biuletynu: pora jest do nadania, gdy `0 ≤ now − fire_at ≤ min(misfire_grace_s, 900 s)`. Element kolejki wygasa `fire_at + 15 min`, więc większa tolerancja dałaby element martwy od urodzenia. Gdy w jednym tiku do nadania jest kilka pór, składamy tylko najnowszą; starsze dostają `skipped_reason = "zastąpione nowszą porą"`.
- **B4** Cisza nocna: jedna funkcja w rdzeniu `aars_core::interlock::quiet_refuses(quiet_now: bool, quiet_min_priority: Priority, priority: Priority) -> bool` (używana przez `Interlocks::check`) i `StationSettings::quiet_now(&self, local: NaiveTime) -> bool` (używana przez `radio::apply_quiet_hours`). Biuletyn liczy `local = now.with_timezone(&chrono::Local).time()` — ten sam zegar co blokada.
- **B5** `Day.code: Option<Condition>` (nieznany kod → `None`), `Day.precip_mm: Option<f64>`, `Day.gust_max_kmh: Option<f64>`, dodatkowo `Day.wind_max_kmh: Option<f64>` (MET nie ma porywów — w zapasie „wiatr do …”). Opis dnia z MET = najcięższy `Condition` dnia (kolejność w B6), jak dzienny `weather_code` Open-Meteo.
- **B6** `Condition` (kolejność wariantów = rosnąca dotkliwość, `derive(PartialOrd, Ord)`): `Bezchmurnie` „bezchmurnie”, `MaleZachmurzenie` „małe zachmurzenie”, `Pochmurno` „pochmurno”, `Mgla` „mgła”, `Mzawka` „mżawka”, `PrzelotnyDeszcz` „przelotny deszcz”, `Deszcz` „deszcz”, `DeszczZeSniegiem` „deszcz ze śniegiem”, `Snieg` „śnieg”, `Burza` „burza”. WMO: 0 → Bezchmurnie; 1, 2 → MaleZachmurzenie; 3 → Pochmurno; 45, 48 → Mgla; 51–57 → Mzawka; 61–67 → Deszcz; 80–82 → PrzelotnyDeszcz; 71–77, 85, 86 → Snieg; 95–99 → Burza; inne → `None`. MET `symbol_code`: obciąć `_day`/`_night`/`_polartwilight`; zawiera `thunder` → Burza; obciąć przedrostek `light`/`heavy`; `clearsky` → Bezchmurnie, `fair`/`partlycloudy` → MaleZachmurzenie, `cloudy` → Pochmurno, `fog` → Mgla, `rain` → Deszcz, `rainshowers` → PrzelotnyDeszcz, `sleet`/`sleetshowers` → DeszczZeSniegiem, `snow`/`snowshowers` → Snieg; inne → `None`.
- **B7** Wiek skal NOAA w części `propagacja`: ≤ 6 h (spec nie podaje; jak Kp). Zdanie o skalach używa `swpc::speech::label`.
- **B8** `ostrzezenia`: powyżej 5 pozycji — „Pozostałych ostrzeżeń: {n}.” zamiast „i {n} innych” (normalizer dałby „dwa innych”).
- **B9** Podsumowanie (`summary`) alertu pogodowego bez „dziś/jutro” (zestarzałoby się po północy): „porywy wiatru do 85 km/h, 23 listopada” (miesiąc z `imgw_meteo::speech::MONTHS_GENITIVE`).
- **B10** Normalizer zyskuje skróty `("MUF", "M U F")`, `("KC2G", "K C dwa G")`; `assert_speakable` dopisuje je do `RAW`.
- **B11** Zmiana zakresu RSO (poprawka 2) jednorazowo porzuca po cichu (R9) pozycje stacji, które miały odznaczony „atak z powietrza” — bez fałszywych odwołań; zapisane w README.
- **B12** `compose` zwraca `Result<Composed, DbError>` (czyta bazę: ustawienia, pozycję, ostrzeżenia).

## Global Constraints

- Testy nigdy nie dotykają sieci: HTTP tylko wiremock na 127.0.0.1 i fiksy z `tests/fixtures`.
- Walidacja i testy nie otwierają portu szeregowego, karty dźwiękowej ani GPIO.
- Nic nie może niezamierzenie kluczować nadajnika: biuletyn i alerty tylko wstawiają do kolejki (`QueueRepo::enqueue`), nigdy nie wołają sekwencera; testy integracyjne bez prawdziwego PTT.
- Kierunek zależności: `aars-core` nie zależy od `aars-sources`; `aars-web` nie zależy od `aars-sources`.
- Żadne `unwrap`/`expect`/indeksowanie na danych z sieci; nieznany schemat = `SourceError::Parse`, nigdy zera.
- `crate::http` (limit rozmiaru, timeout 20 s). User-Agent `aars/{CARGO_PKG_VERSION}`; wyjątek MET Norway: `aars/{CARGO_PKG_VERSION} github.com/sptq/aars` (bez danych osobowych), współrzędne do MET i Open-Meteo zaokrąglone do 4 miejsc.
- Limity odpowiedzi: Open-Meteo 256 KiB, MET 1 MiB, f107 256 KiB, Kp 64 KiB, skale `swpc::MAX_BODY`, KC2G 1 MiB.
- Atrybucja w treści: „Dane: Open-Meteo.” / „Dane: MET Norway.”, „Dane: NOAA, GIRO przez KC2G.” (KC2G tylko gdy jonosonda użyta).
- Źródło pisze cyfry, `HH:MM`, przecinek dziesiętny i tokeny jednostek (B1); słowa robi normalizer w `aars-audio`.
- Nowe moduły wyłączone domyślnie (`pogoda`: `default_enabled: false`; `bulletin`: `module:bulletin:enabled = false`). Biuletyn nigdy nie jest `Emergency`.
- Teksty komunikatów i panelu po polsku.
- Nie uruchamiać `cargo fmt` na całym repo — tylko `rustfmt --edition 2024 <zmienione pliki>`.
- Commity bez żadnej atrybucji AI (bez `Co-Authored-By`, `Claude-Session`, „Generated with”). Praca na gałęzi `etap-11b-biuletyn`, bez nowych gałęzi.
- Koniec każdego zadania: `cargo test --workspace` zielone, `cargo clippy --workspace --all-targets -- -D warnings` czyste.

## Review Focus

- **Operator zmienia `times` tuż po porze** (o 07:03 dopisuje 07:00 przy tolerancji 600 s) — biuletyn wychodzi raz, kolejny tik i restart nic nie dublują. Test w Task 7 (`adding_a_time_just_after_it_fires_once`).
- **Pozycja stacji zmieniona, gdy w pamięci jest świeża prognoza** — część `pogoda` nie mówi o starym miejscu (klucz pamięci = pozycja do 0,01°). Test w Task 5 (`a_moved_station_does_not_reuse_the_cached_forecast`).
- **Luki w prognozie** (`null` w tablicach `daily`, `current` bez kodu pogody) — brak zdania albo opisu, nigdy „0 °C”/„bez opadów” zmyślone z braku. Test w Task 4 (`nulls_in_daily_arrays_are_gaps_not_zeros`).
- **Okno ciszy przez północ, kończące się dokładnie o porze biuletynu** (22:00-07:00, pora 07:00) — biuletyn wychodzi (koniec okna wyłącznie). Test w Task 7 (`a_quiet_window_ending_at_the_fire_time_does_not_block_it`).
- **Wolna część** (serwer wisi) — biuletyn składa się z pozostałych po limicie czasu części, części liczone równolegle (trzy wolne części nie dają 3 × limit). Test w Task 7 (`slow_parts_time_out_in_parallel`).

---

### Task 1: Poprawki z 11a — „godz.” i zakres RSO

**Files:**
- Modify: `crates/aars-audio/src/text/normalize.rs` (`HourCase`, `hour_case`, `spoken_time`, `ABBREVIATIONS`, testy)
- Modify: `crates/aars-sources/src/rso/mod.rs:113-128, 201-216` (zakres i pobieranie), test `the_scope_carries_the_filters` (`:822-838`)
- Modify: `crates/aars-sources/src/speech.rs` (`assert_speakable`: `RAW` += `"MUF"`, `"KC2G"`)

**Interfaces:**
- Produces: `HourCase::Bare` (po `godz` — rdzeń tokenu „godz.” po `split_punct`); `fn fetched_categories(ctx: &SourceCtx) -> Vec<Category>` w `rso/mod.rs` = `categories(ctx)` z dodanym `atak-z-powietrza`, w kolejności `CATEGORIES`; używane w `scope` i `fetch`.

- [ ] **Step 1: Testy**

```rust
// normalize.rs
#[test] fn godz_takes_a_bare_hour() {
    assert_eq!(n("godz. 09:22"), "godz. dziewiąta dwadzieścia dwa");
    assert_eq!(n("od godz. 18:00"), "od godz. osiemnasta");
    assert_eq!(n("18:00"), "godzina osiemnasta"); // bez zmian
}
#[test] fn muf_and_kc2g_are_spelled() {
    assert_eq!(n("MUF 10,6 megaherca."), "M U F dziesięć przecinek sześć megaherca.");
    assert_eq!(n("GIRO przez KC2G."), "GIRO przez K C dwa G.");
}
// rso/mod.rs — zastępuje końcówkę the_scope_carries_the_filters
#[test] fn the_attack_toggle_does_not_change_the_scope() {
    let src = Rso::new("http://127.0.0.1:1").unwrap();
    let with = ctx("02", &[("categories", "ogolne,atak-z-powietrza")]);
    let without = ctx("02", &[("categories", "ogolne")]);
    assert_eq!(src.scope(&with), src.scope(&without));
    assert_eq!(src.scope(&without), format!("{}|cat=ogolne,atak-z-powietrza|kw=", without.area_text));
    let c = ctx("02", &[("categories", "stany-wod,hydrologiczne")]);
    assert!(src.scope(&c).contains("|cat=atak-z-powietrza,hydrologiczne|"));
}
```

Istniejące testy HTTP RSO (po jednym żądaniu na kategorię, `atak` zawsze pobierany) zostają bez zmian.

- [ ] **Step 2: `cargo test -p aars-audio normalize` i `-p aars-sources rso` — FAIL.**
- [ ] **Step 3: Implementacja.** `hour_case`: `Some("godz") => HourCase::Bare`; `spoken_time` dla `Bare`: `hour_ordinal(hour)` bez przedrostka „godzina”, minuty jak dziś. Skróty dopisane na końcu `ABBREVIATIONS`. RSO: `fetch` iteruje po `fetched_categories(ctx)` (zastępuje dzisiejsze `push(Category(ATTACK))`), `scope` używa `categories_text(&fetched_categories(ctx))`.
- [ ] **Step 4: Zielone, w tym `assert_speakable` w testach `emsc` (nadal bez „godzinie godzina”); clippy.**
- [ ] **Step 5: Commit** `fix: normalizer — godzina po „godz.” bez słowa „godzina”; rso — zakres niezależny od pola ataku z powietrza`

### Task 2: Rdzeń — `TimeList`, moduł `bulletin`, `bulletin_fires`, żywe ostrzeżenia, cisza

**Files:**
- Modify: `crates/aars-core/src/module.rs` (`SettingKind::TimeList`, `validate`, `pub const BULLETIN`, `pub const BULLETIN_PARTS`, `builtin()`, testy — odwrócić `the_bulletin_cannot_be_enabled`, poprawić `builtin_modules_are_scheduler_auto_id_and_bulletin`)
- Create: `migrations/0006_bulletin.sql`
- Create: `crates/aars-core/src/bulletin.rs` (+ `pub mod bulletin;` w `lib.rs`)
- Modify: `crates/aars-core/src/source_state.rs` (`live_alerts`)
- Modify: `crates/aars-core/src/interlock.rs` (`quiet_refuses`), `crates/aars-core/src/settings.rs` (`StationSettings::quiet_now`), `crates/aars/src/radio.rs:194-206` (`apply_quiet_hours` przez `quiet_now`)
- Modify: `crates/aars-core/src/db.rs` (testy migracji: `bulletin_fires` w liście tabel; `migration_0006_applies_on_a_version_5_database`)
- Modify: `crates/aars-web/src/modules_page.rs` (`kind_name`/`field_view` dla `TimeList` → `"timelist"`; test `the_bulletin_cannot_be_enabled` → `the_bulletin_can_be_enabled`), `crates/aars-web/templates/modules.html` (gałąź `timelist` jako `<input type="text" inputmode="numeric" placeholder="07:00,19:00">`)

**Interfaces:**
- Produces:
  - `SettingKind::TimeList { max: usize }` — wejście `HH:MM` lub `H:MM` po przecinku (spacje dozwolone); kanonicznie `HH:MM` posortowane, bez duplikatów, złączone `,`. Błędy: puste → `"{label}: podaj co najmniej jedną porę"`; zła pora → `"{label}: pora „{x}” ma mieć postać HH:MM"`; więcej niż `max` po kanonizacji → `"{label}: najwyżej {max} pór"`.
  - `pub const BULLETIN_PARTS: &[(&str, &str)] = &[("pogoda", "Pogoda dla pozycji stacji"), ("propagacja", "Warunki propagacji KF"), ("ostrzezenia", "Obowiązujące ostrzeżenia")];`
  - `pub const BULLETIN: ModuleDescriptor` — id `bulletin`, nazwa „Biuletyn”, opis „Nadaje o stałych porach biuletyn: pogoda, propagacja, obowiązujące ostrzeżenia.”, `Toggle::Switchable`, `default_enabled: false`, `needs: Nothing`, `see_also: Some(("Pozycja, znak i okno ciszy", "/settings"))`, ustawienia:
    - `times` — „Pory nadania”, `TimeList { max: 8 }`, `"07:00,19:00"`, pomoc: „Czas lokalny, HH:MM po przecinku. Pora w oknie ciszy nocnej jest pomijana, gdy cisza zatrzymuje komunikaty zwykłe.”
    - `parts` — „Części”, `List(BULLETIN_PARTS)`, `"pogoda,propagacja,ostrzezenia"`, pomoc: „Kolejność w biuletynie jest stała.”
    - `intro` — „Wstęp”, `Text { max_len: 200 }`, `""`, pomoc: „Puste: „Biuletyn stacji {znak}.””
  - `pub struct BulletinFireRepo { pool }` w `aars_core::bulletin`: `new(pool: SqlitePool)`, `async fn claim(&self, fire_at: DateTime<Utc>, skipped_reason: Option<&str>) -> Result<bool, DbError>` (`INSERT OR IGNORE`; `true` = wstawione), `async fn set_queued(&self, fire_at, queue_item_id: i64) -> Result<(), DbError>`, `async fn set_skipped(&self, fire_at, reason: &str) -> Result<(), DbError>`, `async fn get(&self, fire_at) -> Result<Option<BulletinFire>, DbError>`; `pub struct BulletinFire { pub fire_at: DateTime<Utc>, pub skipped_reason: Option<String>, pub queue_item_id: Option<i64> }`. `fire_at` zapisywane jako `%Y-%m-%dT%H:%M:%SZ`.
  - `SourceStateRepo::live_alerts(&self, now: DateTime<Utc>) -> Result<Vec<TrackedItem>, DbError>`.
  - `pub fn quiet_refuses(quiet_now: bool, quiet_min_priority: Priority, priority: Priority) -> bool` (`quiet_now && quiet_min_priority.outranks(priority)`); `StationSettings::quiet_now(&self, local: NaiveTime) -> bool`.

Migracja `0006_bulletin.sql`:

```sql
-- Etap 11b: biuletyn staje się przełączalny (domyślnie wyłączony).
INSERT OR IGNORE INTO settings (key, value) VALUES ('module:bulletin:enabled', 'false');
-- Odpalenia biuletynu: wiersz przed składaniem, więc restart nie dubluje.
CREATE TABLE bulletin_fires (
    fire_at        TEXT PRIMARY KEY,
    skipped_reason TEXT,
    queue_item_id  INTEGER REFERENCES queue_items(id) ON DELETE SET NULL,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
```

`live_alerts`: SQL `source_items` ⋈ `source_state` (`state = 'active'`, `source_state.enabled = 1`, `valid_until IS NULL OR valid_until > now`, `aired = 1 OR queue_item_id IN (SELECT id FROM queue_items WHERE state = 'done')`), potem w Rust odrzucić źródła, dla których `SourceStatus::is_stale(config(id).interval_s, now)`; sortowanie `priority` rosnąco (Emergency pierwszy), potem `first_seen_at`.

- [ ] **Step 1: Testy**

```rust
// module.rs
#[test] fn time_lists_are_sorted_deduplicated_and_bounded() {
    // TimeList{max:3}: "19:00, 7:00,07:00" -> "07:00,19:00"; "" -> Err("…: podaj co najmniej jedną porę");
    // "25:00" i "7.00" -> Err zawierający "„25:00”"/"„7.00”" i "HH:MM";
    // "01:00,02:00,03:00,04:00" -> Err("…: najwyżej 3 pór")
}
#[test] fn the_bulletin_is_switchable_with_three_settings() {
    // BULLETIN.toggle == Switchable, !default_enabled; validate(defaults) == defaults;
    // parts "ostrzezenia,pogoda" -> "pogoda,ostrzezenia"
}
#[tokio::test] async fn the_bulletin_can_be_enabled() { /* set_enabled(true) -> is_enabled == true */ }
// bulletin.rs
#[tokio::test] async fn a_fire_is_claimed_once() {
    // claim(t, None) == true; claim(t, None) == false; set_queued(t, id) -> get(t).queue_item_id == Some(id)
}
// source_state.rs — po jednej pozycji na przypadek, now = 2026-11-22T19:00:00Z
#[tokio::test] async fn live_alerts_need_a_live_enabled_fresh_source_and_an_airing() {
    // aired=1, źródło włączone, last_success 18:55 -> jest;
    // źródło wyłączone -> brak; last_success 17:00 przy interval 300 (limit 30 min) -> brak;
    // aired=0, queue_item 'pending' -> brak; aired=0, queue_item 'done' -> jest;
    // valid_until 18:00 -> brak; kolejność: High przed Normal, przy równym priorytecie starszy first_seen_at
}
// interlock.rs / settings.rs
#[test] fn quiet_refuses_only_lower_priorities() {
    assert!(quiet_refuses(true, Priority::High, Priority::Normal));
    assert!(!quiet_refuses(true, Priority::Normal, Priority::Normal));
    assert!(!quiet_refuses(false, Priority::Emergency, Priority::Normal));
}
// db.rs: migration_0006_applies_on_a_version_5_database (wzór testu 0005): istniejące
// 'module:bulletin:enabled' = 'true' zostaje 'true'; bez wiersza -> 'false'; tabela bulletin_fires jest.
// modules_page.rs: the_bulletin_can_be_enabled — POST /modules/bulletin/enabled enabled=1 -> 303,
// audyt "bulletin: włączony"; GET /modules pokazuje pole „Pory nadania” z wartością 07:00,19:00.
```

- [ ] **Step 2: `cargo test -p aars-core` i `-p aars-web modules_page` — FAIL.**
- [ ] **Step 3: Implementacja** wg Interfaces. `builtin()` używa `BULLETIN`. `Interlocks::check` woła `quiet_refuses(state.quiet_now, state.quiet_min_priority, priority)`; `apply_quiet_hours` woła `settings.quiet_now(now)`.
- [ ] **Step 4: Zielone całe `cargo test --workspace` (testy `radio.rs` bez zmian); clippy.**
- [ ] **Step 5: Commit** `feat: moduł biuletynu przełączalny — pory, części, wstęp, tabela odpaleń i żywe ostrzeżenia`

### Task 3: Kompozytor — trait części, składanie, część `ostrzezenia`

**Files:**
- Create: `crates/aars-sources/src/bulletin/mod.rs` (trait, typy, `assemble`), `crates/aars-sources/src/bulletin/ostrzezenia.rs`
- Modify: `crates/aars-sources/src/lib.rs` (`pub mod bulletin;`), `crates/aars-sources/src/speech.rs` (B2), `crates/aars-sources/Cargo.toml` (`sqlx.workspace = true`)

**Interfaces:**
- Consumes: `BULLETIN_PARTS`, `TrackedItem` z Task 2.
- Produces (w `aars_sources::bulletin`):
  - `pub struct PartCtx { pub now: DateTime<Utc>, pub position: Option<StationPosition>, pub settings: BTreeMap<String, String> /* module:bulletin:* bez prefiksu */, pub active_alerts: Vec<TrackedItem> }`
  - `pub enum PartText { Text(String), Empty(&'static str) }` — `Empty(powód)`, np. „brak pozycji stacji”.
  - `pub type PartFuture<'a> = Pin<Box<dyn Future<Output = Result<PartText, SourceError>> + Send + 'a>>;`
  - `pub trait BulletinPart: Send + Sync { fn id(&self) -> &'static str; fn max_chars(&self) -> usize; fn compose<'a>(&'a self, ctx: &'a PartCtx) -> PartFuture<'a>; }`
  - `#[derive(Debug, Clone, PartialEq)] pub enum PartOutcome { Included(String), Empty(String), Failed(String), DroppedForLength }` z `pub fn describe(&self) -> String`: `Included(t)` → „w biuletynie ({n} znaków)”, `Empty(p)` → „pominięta — {p}”, `Failed(e)` → „błąd — {e}”, `DroppedForLength` → „pominięta — limit czasu nadawania”.
  - `#[derive(Debug, Clone, PartialEq)] pub struct Composed { pub text: Option<String>, pub parts: Vec<(&'static str, PartOutcome)>, pub skipped_reason: Option<&'static str> }`
  - `pub fn assemble(intro: &str, hhmm: &str, parts: Vec<(&'static str, PartOutcome)>, limit: usize) -> Composed` (czysta).
  - `pub fn intro_text(intro_setting: &str, callsign: &str) -> String` — niepusty wstęp (z kropką na końcu, gdy brak `.!?`); pusty → „Biuletyn stacji {znak}.”; bez znaku → „Biuletyn stacji.”
  - `pub fn length_limit(tx_max_duration_s: u64) -> usize` = `min(1500, tx_max_duration_s × 10)`.
  - `pub struct Ostrzezenia;` z `impl BulletinPart` (id `ostrzezenia`, `max_chars` 500).

`assemble`: części `Included` z tekstem zakończonym `.`/`!`/`?` (inaczej dopisz `.`); tekst = `"{intro} {hhmm}. {część} … Koniec biuletynu."` (spacje pojedyncze). Dopóki długość (znaki) > `limit` i jest jeszcze `Included` — ostatnią w kolejności zamień na `DroppedForLength`. Żadnej `Included` → `text: None`, `skipped_reason`: `"limit czasu nadawania"` gdy coś wypadło przez długość, inaczej `"brak danych"`.

Ostrzezenia: summaries z `active_alerts` (już posortowanych) bez końcowych `.;,` i spacji, puste pominięte; 0 → `Text("Brak obowiązujących ostrzeżeń.")`; 1–5 → `"Obowiązujące ostrzeżenia: {a}; {b}."`; > 5 → pięć pierwszych + `" Pozostałych ostrzeżeń: {n}."` (B8); całość przez `truncate_sentence(…, 500)`.

- [ ] **Step 1: Testy**

```rust
#[test] fn parts_join_in_order_between_intro_and_end() {
    let c = assemble("Biuletyn stacji SP9XYZ.", "07:00",
        vec![("pogoda", Included("A b".into())), ("propagacja", Empty("brak danych propagacji".into())),
             ("ostrzezenia", Included("C.".into()))], 1500);
    assert_eq!(c.text.as_deref(), Some("Biuletyn stacji SP9XYZ. 07:00. A b. C. Koniec biuletynu."));
}
#[test] fn parts_drop_from_the_end_to_fit_the_limit() {
    // trzy części po 200 znaków, limit length_limit(30) == 300 -> tylko pierwsza Included,
    // druga i trzecia DroppedForLength; describe() == "pominięta — limit czasu nadawania"
}
#[test] fn nothing_to_say_is_no_bulletin() {
    // wszystkie Empty/Failed -> text None, skipped_reason Some("brak danych");
    // limit 50 przy jednej części 200 znaków -> None, Some("limit czasu nadawania")
}
#[test] fn the_intro_defaults_to_the_callsign() {
    assert_eq!(intro_text("", "SP9XYZ"), "Biuletyn stacji SP9XYZ.");
    assert_eq!(intro_text("  ", ""), "Biuletyn stacji.");
    assert_eq!(intro_text("Tu stacja klubowa", "SP9XYZ"), "Tu stacja klubowa.");
}
#[test] fn length_limit_follows_the_tx_limit() { assert_eq!(length_limit(180), 1500); assert_eq!(length_limit(60), 600); }
#[tokio::test] async fn no_alerts_is_said_out_loud() { /* pusta lista -> Text("Brak obowiązujących ostrzeżeń.") */ }
#[tokio::test] async fn alerts_are_listed_up_to_five() {
    // summaries "G3 — silna burza geomagnetyczna", "PM10, poziom informowania, miasto Rzeszów", "" ->
    // "Obowiązujące ostrzeżenia: G3 — silna burza geomagnetyczna; PM10, poziom informowania, miasto Rzeszów."
    // 7 pozycji -> kończy się "Pozostałych ostrzeżeń: 2."; tekst ≤ 500 znaków
}
#[test] fn source_summaries_are_speakable() {
    // assert_speakable na „Obowiązujące ostrzeżenia: …” złożonym z podsumowań z testów źródeł 11a:
    // "Burze, 1. stopień", "Alert RCB", "G3 — silna burza geomagnetyczna",
    // "PM10, poziom informowania, miasto Rzeszów", "M2,1, 21 km, Polska".
    // Gdy któreś nie przejdzie — popraw `summary` w tym źródle (to wymóg specu), nie test.
}
#[test] fn decimals_are_spoken_with_a_comma() {
    assert_eq!(decimal_comma(1.2), "1,2"); assert_eq!(decimal_comma(2.0), "2"); assert_eq!(decimal_comma(-0.04), "0");
    assert_eq!(one_decimal(4.0), "4,0"); assert_eq!(one_decimal(10.56), "10,6");
    assert_eq!(round_int(2.5), 3); assert_eq!(round_int(-0.4), 0); assert_eq!(round_int(-1.9), -2);
}
```

- [ ] **Step 2: `cargo test -p aars-sources bulletin speech` — FAIL.**
- [ ] **Step 3: Implementacja** wg Interfaces.
- [ ] **Step 4: Zielone; clippy.**
- [ ] **Step 5: Commit** `feat: kompozytor biuletynu — części, limit długości, część ostrzeżeń`

### Task 4: Wspólny model prognozy, parsery Open-Meteo i MET Norway

**Files:**
- Create: `crates/aars-sources/src/forecast/mod.rs` (model, `Condition`, tabele B6), `crates/aars-sources/src/forecast/open_meteo.rs`, `crates/aars-sources/src/forecast/met_no.rs`
- Modify: `crates/aars-sources/src/lib.rs` (`pub mod forecast;`)

**Interfaces:**
- Produces:
  - `pub enum Provider { OpenMeteo, MetNorway }` z `attribution(self) -> &'static str` („Dane: Open-Meteo.” / „Dane: MET Norway.”).
  - `pub enum Condition { … }` (B6) z `pub fn words(self) -> &'static str`, `pub fn from_wmo(code: i64) -> Option<Condition>`, `pub fn from_met_symbol(symbol: &str) -> Option<Condition>`.
  - `pub struct Now { pub temp_c: f64, pub code: Option<Condition>, pub wind_kmh: Option<f64>, pub gust_kmh: Option<f64> }`
  - `pub struct Day { pub date: NaiveDate, pub t_min: f64, pub t_max: f64, pub precip_mm: Option<f64>, pub gust_max_kmh: Option<f64>, pub wind_max_kmh: Option<f64>, pub code: Option<Condition> }`
  - `pub struct Forecast { pub issued: DateTime<Utc>, pub provider: Provider, pub current: Option<Now>, pub days: Vec<Day> }` z `pub fn day(&self, date: NaiveDate) -> Option<&Day>`. (Wszystkie `#[derive(Debug, Clone, PartialEq)]`.)
  - `open_meteo::url(base: &str, lat: f64, lon: f64) -> String` — `{base}?latitude={lat:.4}&longitude={lon:.4}&current=temperature_2m,wind_speed_10m,wind_gusts_10m,weather_code,precipitation&hourly=temperature_2m,precipitation,wind_gusts_10m,weather_code&daily=temperature_2m_max,temperature_2m_min,precipitation_sum,wind_gusts_10m_max,weather_code&timezone=Europe%2FWarsaw&forecast_days=3`.
  - `open_meteo::parse(body: &[u8], fetched: DateTime<Utc>) -> Result<Forecast, SourceError>` — `daily.time` i pięć tablic wymagane, równe długości (inaczej `Parse`); dzień z `null` w `t_min`/`t_max` pominięty; `null` w opadach/porywach → `None`; `issued` = `current.time` minus `utc_offset_seconds` (brak `current` → `fetched`); `current` bez `temperature_2m` → `current: None`; `hourly` nieużywane (tylko pobierane).
  - `met_no::url(base: &str, lat: f64, lon: f64) -> String` — `{base}?lat={lat:.4}&lon={lon:.4}`.
  - `met_no::parse(body: &[u8], now: DateTime<Utc>) -> Result<Forecast, SourceError>` — `properties.timeseries` wymagane (inaczej `Parse`); `issued` = `meta.updated_at`; dni = daty lokalne (`Europe/Warsaw`) wpisów: `t_min/t_max` z `instant.air_temperature`, opady = suma `next_1_hours.precipitation_amount`, a dla wpisów bez `next_1_hours` — `next_6_hours`; `wind_max_kmh` = max `wind_speed` × 3,6; `gust_max_kmh: None`; `code` = najcięższy `Condition` z symboli `next_1_hours` (albo `next_6_hours`) dnia; `current` = ostatni wpis z `time ≤ now` (brak → pierwszy), `code` z jego `next_1_hours`.

- [ ] **Step 1: Testy**

```rust
#[test] fn the_open_meteo_fixture_parses() {
    let f = open_meteo::parse(OPEN_METEO, at("2026-11-22T18:50:00Z")).unwrap();
    assert_eq!(f.provider, Provider::OpenMeteo);
    assert_eq!(f.issued, at("2026-11-22T18:30:00Z"));
    let now = f.current.as_ref().unwrap();
    assert_eq!((now.temp_c, now.code, now.gust_kmh), (2.2, Some(Condition::Bezchmurnie), Some(17.6)));
    assert_eq!(f.days.len(), 2);
    let d1 = f.day(date(2026, 11, 23)).unwrap();
    assert_eq!((d1.t_min, d1.t_max, d1.precip_mm, d1.gust_max_kmh), (0.9, 2.5, Some(2.4), Some(45.0)));
    assert_eq!(d1.code, Some(Condition::Snieg));
    assert_eq!(f.day(date(2026, 11, 22)).unwrap().code, Some(Condition::Mzawka));
}
#[test] fn nulls_in_daily_arrays_are_gaps_not_zeros() {
    // Value: daily.precipitation_sum[1] = null, daily.weather_code[0] = 999, daily.temperature_2m_max[0] = null
    // -> dzień 22 pominięty; dzień 23 precip_mm None; current.weather_code = null -> current.code None
}
#[test] fn a_broken_open_meteo_schema_is_a_parse_error() {
    // brak "daily"; daily.time dłuższe niż temperature_2m_min; "daily": [] -> Err(Parse)
}
#[test] fn the_met_fixture_aggregates_local_days() {
    let f = met_no::parse(MET, at("2026-11-22T19:00:00Z")).unwrap();
    assert_eq!(f.issued, at("2026-11-22T17:17:09Z"));
    let now = f.current.as_ref().unwrap();
    assert_eq!((now.temp_c, now.code), (-0.7, Some(Condition::MaleZachmurzenie)));
    let d = f.day(date(2026, 11, 23)).unwrap();
    assert_eq!((d.t_min, d.t_max, d.gust_max_kmh), (-1.9, 1.5, None));
    assert!((d.precip_mm.unwrap() - 4.6).abs() < 1e-9);
    assert!((d.wind_max_kmh.unwrap() - 25.56).abs() < 1e-6);
    assert_eq!(d.code, Some(Condition::Snieg));
}
#[test] fn met_symbols_map_through_suffixes_and_intensity() {
    // "lightsleet" -> DeszczZeSniegiem; "heavyrainshowers_day" -> PrzelotnyDeszcz;
    // "lightssnowshowersandthunder_night" -> Burza; "partlycloudy_polartwilight" -> MaleZachmurzenie; "zzz" -> None
}
#[test] fn wmo_codes_map_to_polish_words() {
    // 0 bezchmurnie, 2 małe zachmurzenie, 3 pochmurno, 45 mgła, 53 mżawka, 63 deszcz, 81 przelotny deszcz,
    // 73 śnieg, 95 burza, 4 -> None
}
#[test] fn urls_round_coordinates_to_four_places() {
    assert!(met_no::url("http://x/compact", 50.040049, 21.99).ends_with("?lat=50.0400&lon=21.9900"));
    assert!(open_meteo::url("http://x/v1/forecast", 50.04, 21.99).contains("latitude=50.0400&longitude=21.9900"));
}
```

- [ ] **Step 2: `cargo test -p aars-sources forecast` — FAIL.**
- [ ] **Step 3: Implementacja** wg Interfaces.
- [ ] **Step 4: Zielone; clippy.**
- [ ] **Step 5: Commit** `feat: wspólny model prognozy — Open-Meteo, MET Norway i polskie opisy pogody`

### Task 5: `pogoda` — źródło alertów progowych i część biuletynu

**Files:**
- Create: `crates/aars-sources/src/pogoda/mod.rs` (`Pogoda`, deskryptor, pamięć, `DataSource`, `BulletinPart`), `crates/aars-sources/src/pogoda/alerts.rs` (`to_output`), `crates/aars-sources/src/pogoda/speech.rs` (tekst części, teksty alertów)
- Modify: `crates/aars-sources/src/lib.rs` (`modules()` + test kolejności), `crates/aars-sources/src/http.rs` (`client_with_agent`, `MET_USER_AGENT`)
- Modify: `crates/aars-core/src/config.rs` (`open_meteo_url`, `met_no_url` + walidacja + testy), `aars.example.toml`, `packaging/etc/aars.toml` (zakomentowane wiersze)

**Interfaces:**
- Consumes: Task 3 (`BulletinPart`, `PartCtx`, `PartText`, B2), Task 4 (`Forecast`, parsery).
- Produces:
  - `SourcesConfig.open_meteo_url` (domyślnie `https://api.open-meteo.com/v1/forecast`), `SourcesConfig.met_no_url` (`https://api.met.no/weatherapi/locationforecast/2.0/compact`).
  - `http::MET_USER_AGENT: &str = concat!("aars/", env!("CARGO_PKG_VERSION"), " github.com/sptq/aars")`; `http::client_with_agent(timeout: Duration, agent: &str) -> Result<reqwest::Client, SourceError>` (`client` woła ją z `aars/{wersja}`).
  - `pub const ID: &str = "pogoda"`, `pub const DESCRIPTOR: ModuleDescriptor` — nazwa „Pogoda (Open-Meteo)”, opis „Komunikat, gdy prognoza dla pozycji stacji przekroczy progi: porywy, mróz, opady. Biuletyn korzysta z tej samej prognozy.”, `needs: Position`, ustawienia: `gust_kmh` „Porywy wiatru od (km/h)” Integer 40–150 `"75"`; `frost_c` „Mróz od (°C)” Integer −40–0 `"-15"`; `precip_mm` „Suma opadów dobowych od (mm)” Integer 5–200 `"30"`; `interval_s` Integer 900–3600 `"1800"`.
  - `pub struct Pogoda` — `new(cfg: &SourcesConfig) -> Result<Self, SourceError>`, `with_urls(open_meteo: &str, met_no: &str, timeout: Duration) -> Result<Self, SourceError>`; pamięć `Mutex<Option<Cached>>` (`Cached { forecast: Forecast, fetched_at: DateTime<Utc>, pos: (i64, i64) /* round(lat·100), round(lon·100) */ }`, odzysk po zatruciu jak w `swpc`).
  - `impl DataSource for Pogoda` — id `pogoda`, `default_interval` 1800 s, `needs() == Position`, `scope` = `format!("{lat:.2},{lon:.2}|g={g}|f={f}|p={p}")`; `fetch`: tylko Open-Meteo, sukces zapisuje pamięć, `alerts::to_output`.
  - `impl BulletinPart for Pogoda` — id `pogoda`, `max_chars` 450; pozycja `None` → `Empty("brak pozycji stacji")`; pamięć świeża (≤ 60 min) i ta sama `pos` → z pamięci; inaczej Open-Meteo (sukces → pamięć), błąd → MET (sukces → pamięć); oba błędy → `Err` z błędem MET.
  - `alerts::to_output(forecast: &Forecast, ctx: &SourceCtx) -> SourceOutput` (czysta).
  - `speech::bulletin(forecast: &Forecast, now: DateTime<Utc>) -> Option<String>` (`None`, gdy żadne zdanie nie powstało).

Tekst części (`d0` = dzień daty lokalnej `now`, `d1` = następny; liczby `round_int`, opady `decimal_comma`; człony z brakującymi danymi wypadają):
- Zawsze: „Pogoda dla okolic stacji.”; gdy `current`: „Teraz {t} °C[, {opis}].”
- Przed 14:00 lokalnie: „Dziś[ {opis d0},] do {t_max d0} °C, {opady d0}[, {wiatr d0}].” i „W nocy do {t_min d1} °C.”
- Od 14:00: „W nocy do {t_min d1} °C.” i „Jutro[ {opis d1},] do {t_max d1} °C, {opady d1}[, {wiatr d1}].”
- {opady}: `precip_mm` < 0,1 → „bez opadów”; ≥ 0,1 → „opady do {mm} mm”; `None` → człon wypada. {wiatr}: porywy → „porywy wiatru do {g} km/h”; bez porywów, z `wind_max_kmh` → „wiatr do {w} km/h”; inaczej wypada.
- Koniec: `forecast.provider.attribution()`.

Alerty (`alerts.rs`), tylko dni o dacie lokalnej `now` („dziś”) i następnej („jutro”); progi z `ctx.settings` (błędna wartość → domyślna):

| rodzaj | warunek | odcisk | tekst (bez przedrostka) | podsumowanie |
|---|---|---|---|---|
| `gust` | `gust_max_kmh ≥ gust_kmh` | `gust:{floor(g/10)·10}` | „{dziś/jutro} porywy wiatru do {g} km/h.” | „porywy wiatru do {g} km/h, {D} {miesiąca}” |
| `frost` | `t_min ≤ frost_c` | `frost:-{floor(|t|/5)·5}` | „{dziś/jutro} spadek temperatury do {t} °C.” | „spadek temperatury do {t} °C, {D} {miesiąca}” |
| `precip` | `precip_mm ≥ precip_mm` | `precip:{floor(mm/10)·10}` | „{dziś/jutro} opady do {mm} mm.” | „opady do {mm} mm, {D} {miesiąca}” |

`key = topic = "{rodzaj}:{YYYY-MM-DD}"`, `text = "Prognoza pogody: " + tekst + " Dane: Open-Meteo."`, `change_text = "Zmiana prognozy: " + tekst + " Dane: Open-Meteo."`, `cancel_text = ""`, `Priority::Normal`, `interrupting: false`, `valid_from: None`, `valid_until = warsaw_to_utc(data+1 00:00)`. `complete: true`.

- [ ] **Step 1: Testy** (pozycja Rzeszów 50.04, 21.99; `OPEN_METEO`/`MET` = `include_bytes!` fiksów)

```rust
#[test] fn the_quiet_fixture_has_no_alerts_at_default_thresholds() { /* now 2026-11-22T18:50Z -> 0 pozycji, complete */ }
#[test] fn gusts_over_the_threshold_are_one_item_per_day() {
    // gust_kmh=40 -> 2 pozycje:
    // "gust:2026-11-22": text "Prognoza pogody: dziś porywy wiatru do 40 km/h. Dane: Open-Meteo.", fingerprint "gust:40",
    //   valid_until 2026-11-22T23:00:00Z, summary "porywy wiatru do 40 km/h, 22 listopada"
    // "gust:2026-11-23": "… jutro porywy wiatru do 45 km/h. …", fingerprint "gust:40", valid_until 2026-11-23T23:00:00Z
    // gust_kmh=41 -> tylko "gust:2026-11-23"; cancel_text pusty, Normal, bez przerywania
}
#[test] fn frost_and_rain_use_their_steps() {
    // Value: temperature_2m_min[1] = -17.3, frost_c=-15 -> "frost:2026-11-23", fingerprint "frost:-15",
    //   text zawiera "jutro spadek temperatury do -17 °C."; -21.0 -> "frost:-20"
    // precipitation_sum[0] = 35.2, precip_mm=30 -> "precip:2026-11-22", fingerprint "precip:30", text zawiera "dziś opady do 35 mm."
    // precip_text(7.26) == "7,3"; precip_text(35.2) == "35"; precip_text(2.0) == "2"
}
#[test] fn crossing_midnight_keeps_key_and_fingerprint() {
    // ten sam fiks, gust_kmh=41, now 2026-11-22T22:50Z ("jutro") i 2026-11-22T23:10Z (już 23-go, "dziś"):
    // klucz "gust:2026-11-23" i odcisk "gust:40" identyczne; tekst różni się tylko "jutro"/"dziś"
}
#[test] fn the_scope_carries_position_and_thresholds() { /* "50.04,21.99|g=75|f=-15|p=30" */ }
#[test] fn the_bulletin_text_after_two_pm() {
    let f = open_meteo::parse(OPEN_METEO, at("2026-11-22T18:50:00Z")).unwrap();
    assert_eq!(speech::bulletin(&f, at("2026-11-22T18:50:00Z")).unwrap(),
        "Pogoda dla okolic stacji. Teraz 2 °C, bezchmurnie. W nocy do 1 °C. Jutro śnieg, do 3 °C, opady do 2,4 mm, porywy wiatru do 45 km/h. Dane: Open-Meteo.");
}
#[test] fn the_bulletin_text_before_two_pm() {
    // now 2026-11-22T09:00:00Z (10:00 lokalnie), ten sam fiks:
    // "Pogoda dla okolic stacji. Teraz 2 °C, bezchmurnie. Dziś mżawka, do 6 °C, opady do 1,2 mm, porywy wiatru do 40 km/h. W nocy do 1 °C. Dane: Open-Meteo."
}
#[test] fn the_met_fallback_speaks_mean_wind() {
    // met_no::parse(MET, 19:00Z), bulletin(…, 19:00Z) ==
    // "Pogoda dla okolic stacji. Teraz -1 °C, małe zachmurzenie. W nocy do -2 °C. Jutro śnieg, do 2 °C, opady do 4,6 mm, wiatr do 26 km/h. Dane: MET Norway."
}
#[test] fn the_bulletin_texts_are_speakable() {
    // assert_speakable na trzech tekstach powyżej; normalize(tekst po 14:00) zawiera "Teraz dwa stopnie Celsjusza"
    // i "do czterdziestu pięciu kilometrów na godzinę"
}
// HTTP (wiremock, Pogoda::with_urls(server/v1/forecast, server/compact, 5 s)):
#[tokio::test] async fn the_source_asks_open_meteo_for_the_station_position() {
    // query_param latitude=50.0400, longitude=21.9900, timezone=Europe/Warsaw, forecast_days=3; fetch -> Ok
}
#[tokio::test] async fn the_part_falls_back_to_met_with_its_user_agent() {
    // Open-Meteo 503; MET z header("user-agent", MET_USER_AGENT), query lat=50.0400 lon=21.9900 -> tekst kończy się "Dane: MET Norway."
    // oba 503 -> Err(Status(503))
}
#[tokio::test] async fn the_part_reuses_a_fresh_forecast() {
    // fetch źródła (Open-Meteo .expect(1)), potem compose części 30 min później -> bez nowego żądania;
    // 61 min później -> drugie żądanie (Mock .expect(2) na drugim serwerze albo licznik)
}
#[tokio::test] async fn a_moved_station_does_not_reuse_the_cached_forecast() {
    // pamięć dla 50.04,21.99; część z pozycją 50.06,21.99 (inne 0,01°) -> nowe żądanie z latitude=50.0600
}
#[tokio::test] async fn without_a_position_the_part_is_empty() { /* Empty("brak pozycji stacji"), zero żądań */ }
// config.rs: rozszerz a_partial_sources_block_keeps_the_other_defaults i every_source_url_is_validated o oba klucze.
// lib.rs: all_sources_are_registered_in_order -> ["imgw-meteo","rso","swpc","gios-smog","emsc","pogoda"].
```

Uwaga do `frost_and_rain_use_their_steps`: opady w alercie i w części — `decimal_comma` dla < 10 mm, `round_int` dla ≥ 10 mm (jedna funkcja `speech::precip_text(mm) -> String`, używana w obu miejscach).

- [ ] **Step 2: `cargo test -p aars-sources pogoda` i `-p aars-core config` — FAIL.**
- [ ] **Step 3: Implementacja** wg Interfaces. Blokada `Mutex` tylko wokół odczytu/zapisu pamięci, nigdy przez `await`.
- [ ] **Step 4: Zielone; clippy.**
- [ ] **Step 5: Commit** `feat: źródło pogoda — alerty progowe z Open-Meteo i część biuletynu z zapasem MET Norway`

### Task 6: Część `propagacja`

**Files:**
- Create: `crates/aars-sources/src/propagacja/mod.rs` (`Propagacja`, `BulletinPart`), `crates/aars-sources/src/propagacja/parse.rs`, `crates/aars-sources/src/propagacja/speech.rs` (słowa, pasma, miasta)
- Modify: `crates/aars-sources/src/lib.rs` (`pub mod propagacja;`)
- Modify: `crates/aars-core/src/config.rs` (`swpc_f107_url`, `swpc_kp_url`, `kc2g_url` + walidacja + testy), `aars.example.toml`, `packaging/etc/aars.toml`

**Interfaces:**
- Consumes: Task 3, `swpc::parse::parse_scales`, `swpc::speech::label`, `speech::haversine_km`, `speech::one_decimal`.
- Produces:
  - `SourcesConfig.swpc_f107_url` (`https://services.swpc.noaa.gov/json/f107_cm_flux.json`), `swpc_kp_url` (`https://services.swpc.noaa.gov/products/noaa-planetary-k-index.json`), `kc2g_url` (`https://prop.kc2g.com/api/stations.json`).
  - `parse::latest_f107(body: &[u8]) -> Result<(DateTime<Utc>, f64), SourceError>` (max `time_tag` przy `frequency == 2800`; brak → `Parse`).
  - `parse::latest_kp(body: &[u8]) -> Result<(DateTime<Utc>, f64), SourceError>` (max `time_tag`).
  - `parse::ionosondes(body: &[u8]) -> Result<Vec<Ionosonde>, SourceError>` — `Ionosonde { name: String, lat: f64, lon: f64 /* −180..180 */, time: DateTime<Utc>, cs: f64, fof2: Option<f64>, mufd: Option<f64> }`; rekord bez czytelnych współrzędnych/czasu pominięty; korzeń nie-tablica → `Parse`.
  - `pub fn pick_ionosonde(all: &[Ionosonde], from: (f64, f64), now: DateTime<Utc>) -> Option<(&Ionosonde, f64 /*km*/)>` — filtr: `0 ≤ now − time ≤ 60 min`, `cs == -1.0 || cs >= 70.0`, `fof2` i `mufd` obecne; potem najbliższa w promieniu 1200 km.
  - `speech::sfi_word(sfi: f64) -> &'static str` („niski” < 90, „umiarkowany” 90–149, „wysoki” ≥ 150), `speech::nvis_band(fof2: f64) -> Option<u16>` (najwyższe z 160/80/60/40/30 o krawędzi 1,8/3,5/5,3/7,0/10,1 MHz < foF2), `speech::dx_band(mufd: f64) -> Option<u16>` (najwyższe z 80/40/30/20/17/15/12/10 o krawędzi 3,5/7,0/10,1/14,0/18,068/21,0/24,89/28,0 < MUF × 0,85), `speech::city(name: &str) -> String` (tabela po części przed przecinkiem: Warsaw→Warszawa, Pruhonice→Pruhonice, Juliusruh→Juliusruh, Sopron→Sopron, Chilton→Chilton, Dourbes→Dourbes, Rome→Rzym, Athens→Ateny, Moscow→Moskwa, Nicosia→Nikozja; nieznana → część przed przecinkiem).
  - `pub struct Propagacja` — `new(cfg: &SourcesConfig) -> Result<Self, SourceError>`, `with_urls(f107, kp, scales, kc2g: &str, timeout) -> Result<Self, SourceError>`; `impl BulletinPart` (id `propagacja`, `max_chars` 500). Cztery pobrania równolegle (`tokio::join!`); błąd albo za stare dane = zdanie wypada (`tracing::warn!`).

Tekst (zdania w tej kolejności, każde opcjonalne):
1. SFI (wiek ≤ 36 h): „Strumień słoneczny {round_int(sfi)}, {sfi_word}.”
2. Kp (wiek od początku okna ≤ 6 h; `r` = Kp do 1 miejsca): `r < 5` → „Indeks K {decimal_comma(r)}, warunki {spokojne (<3) | niespokojne}.”; `r ≥ 5` → „Indeks K {r}, burza geomagnetyczna.”
3. Skale (wiek ≤ 6 h, B7): wszystkie 0 → „Pogoda kosmiczna bez zaburzeń.”; inaczej „Pogoda kosmiczna: {label(G)}, {label(S)}, {label(R)}.” tylko dla poziomów > 0, kolejność G, S, R.
4. Jonosonda (pozycja stacji jest i `pick_ionosonde` coś dał): „Jonosonda {city}: częstotliwość krytyczna {one_decimal(fof2)} megaherca, MUF {one_decimal(mufd)} megaherca.” i — gdy któreś pasmo jest — „Łączności bliskie do pasma {N} metrów, dalekie do pasma {M} metrów.” (brak jednego → zdanie z tym drugim: „Łączności bliskie do pasma {N} metrów.” / „Łączności dalekie do pasma {M} metrów.”).
Przed zdaniami „Propagacja.”; na końcu atrybucja: NOAA użyte i jonosonda → „Dane: NOAA, GIRO przez KC2G.”; tylko NOAA → „Dane: NOAA.”; tylko jonosonda → „Dane: GIRO przez KC2G.”. Żadne zdanie → `Ok(PartText::Empty("brak danych propagacji"))`.

- [ ] **Step 1: Testy** (pozycja Rzeszów 50.04, 21.99; `now` = 2026-11-22T19:00:00Z, chyba że podano inaczej)

```rust
#[test] fn the_fixtures_give_the_latest_values() {
    assert_eq!(latest_f107(F107).unwrap(), (at("2026-11-21T22:00:00Z"), 98.0));
    assert_eq!(latest_kp(KP).unwrap(), (at("2026-11-22T15:00:00Z"), 0.33));
    // f107 w odwróconej kolejności -> ten sam wynik; [] -> Err(Parse)
}
#[test] fn longitudes_over_180_wrap_and_string_coordinates_parse() {
    // ionosondes(KC2G): "Austin, TX, USA" (longitude "262.3") -> lon ≈ -97.7; "Warsaw, Poland" -> (52.2, 21.1),
    //   time 2026-11-22T18:15:00Z, cs -1.0, fof2 Some(3.2), mufd Some(10.56); Kaliningrad mufd None
    // Value: jeden rekord z "latitude": "x" -> pominięty, liczba pozostałych == 100
}
#[test] fn warsaw_is_picked_for_rzeszow() {
    let (s, km) = pick_ionosonde(&ionosondes(KC2G).unwrap(), (50.04, 21.99), at("2026-11-22T19:00:00Z")).unwrap();
    assert_eq!(s.name, "Warsaw, Poland"); assert!((240.0..256.0).contains(&km));
}
#[test] fn the_filter_comes_before_the_distance() {
    // Warsaw cs=50 -> "Pruhonice, Czech Republic" (Sopron cs 60 odrzucony mimo bliższej odległości);
    // now 19:20 (Warszawa 65 min) -> Pruhonice;
    // now 19:20 i Pruhonice fof2 null -> "Rome, Italy RO041" (RM041 z 18:15 za stary, RO041 18:30 cs 70, 1172 km);
    // pozycja (0.0, 0.0) -> None (nic w 1200 km)
}
#[test] fn band_words_follow_the_fixed_thresholds() {
    assert_eq!(nvis_band(3.2), Some(160)); assert_eq!(nvis_band(7.5), Some(40)); assert_eq!(nvis_band(1.5), None);
    assert_eq!(dx_band(10.56), Some(40)); assert_eq!(dx_band(11.814), Some(40)); assert_eq!(dx_band(17.0), Some(20));
    assert_eq!(dx_band(4.0), None);
    assert_eq!(sfi_word(89.9), "niski"); assert_eq!(sfi_word(90.0), "umiarkowany"); assert_eq!(sfi_word(150.0), "wysoki");
    assert_eq!(city("Rome, Italy RO041"), "Rzym"); assert_eq!(city("Gibilmanna, Italy"), "Gibilmanna"); assert_eq!(city("IS141"), "IS141");
}
#[tokio::test] async fn the_full_propagation_text() {
    // wiremock: cztery ścieżki z fiksami -> Text ==
    // "Propagacja. Strumień słoneczny 98, umiarkowany. Indeks K 0,3, warunki spokojne. Pogoda kosmiczna bez zaburzeń. Jonosonda Warszawa: częstotliwość krytyczna 3,2 megaherca, MUF 10,6 megaherca. Łączności bliskie do pasma 160 metrów, dalekie do pasma 40 metrów. Dane: NOAA, GIRO przez KC2G."
    // assert_speakable na wyniku
}
#[tokio::test] async fn stale_or_failed_data_drops_only_its_sentence() {
    // now 2026-11-23T10:01Z: SFI (36 h 01 min) wypada, Kp i skale też (za stare), jonosonda też -> Empty("brak danych propagacji")
    // now 19:00, KC2G 503 -> tekst bez "Jonosonda", kończy się "Dane: NOAA."
    // now 19:00, bez pozycji stacji -> bez "Jonosonda"
    // f107, kp, scales 503 -> "Propagacja. Jonosonda Warszawa: … Dane: GIRO przez KC2G."
    // Kp 5.33 (Value) -> "Indeks K 5,3, burza geomagnetyczna."; skale G=3 -> "Pogoda kosmiczna: silna burza geomagnetyczna."
}
#[tokio::test] async fn unknown_schemas_never_become_zeros() {
    // KC2G zwraca {"a":1} -> zdanie o jonosondzie wypada (nie "0 megaherca"); f107 bez "flux" -> zdanie SFI wypada
}
// config.rs: trzy nowe klucze w testach domyślnych i walidacji.
```

- [ ] **Step 2: `cargo test -p aars-sources propagacja` — FAIL.**
- [ ] **Step 3: Implementacja** wg Interfaces.
- [ ] **Step 4: Zielone; clippy.**
- [ ] **Step 5: Commit** `feat: część propagacja — SFI, Kp, skale NOAA i jonosonda KC2G z oceną pasm`

### Task 7: `Bulletin` i `BulletinRunner` — pory, spóźnienia, cisza, idempotencja

**Files:**
- Create: `crates/aars-sources/src/bulletin/runner.rs` (`BulletinRunner`, `due_fires`)
- Modify: `crates/aars-sources/src/bulletin/mod.rs` (`Bulletin`, `compose`, `build`)
- Create: `crates/aars-sources/tests/bulletin.rs` (test integracyjny z fiksami)

**Interfaces:**
- Consumes: Task 2 (`BULLETIN`, `BulletinFireRepo`, `live_alerts`, `quiet_refuses`, `StationSettings::quiet_now`), Task 3 (`assemble`, `intro_text`, `length_limit`, `Ostrzezenia`), Task 5 (`Pogoda`), Task 6 (`Propagacja`).
- Produces:
  - `pub struct Bulletin` — `new(pool: SqlitePool, parts: Vec<Arc<dyn BulletinPart>>) -> Self` (kolejność wg `BULLETIN_PARTS`, nie wg wektora), `with_part_timeout(self, t: Duration) -> Self` (domyślnie 20 s), `async fn compose(&self, now: DateTime<Utc>) -> Result<Composed, DbError>` (B12): czyta `StationSettings` (znak, `tx_max_duration_s`), `module:bulletin:*`, pozycję, `live_alerts(now)` (tylko gdy `ostrzezenia` wybrane); wybrane części równolegle (`tokio::task::JoinSet` albo `join_all` na `tokio::time::timeout`), wynik → `PartOutcome` (timeout → `Failed("przekroczony czas {s} s")`, `Err(e)` → `Failed(e.to_string())`, przycięcie `truncate_sentence(text, max_chars)`); niewybrane części nie pojawiają się w `parts`; `hhmm = warsaw_hhmm(now)`; `assemble(…, length_limit(tx_max))`.
  - `pub fn build(cfg: &Config, pool: SqlitePool) -> Result<(Arc<Bulletin>, Arc<pogoda::Pogoda>), SourceError>` — jedna `Pogoda`, `Propagacja`, `Ostrzezenia`.
  - `pub fn due_fires(times: &[NaiveTime], now: DateTime<Utc>) -> Vec<DateTime<Utc>>` — pory dla wczoraj i dziś (data lokalna `now`), `warsaw_to_utc` (nieistniejąca → pominięta, podwójna → pierwsza), tylko `≤ now`, posortowane rosnąco.
  - `pub struct BulletinRunner` — `new(bulletin: Arc<Bulletin>, queue: Arc<QueueRepo>) -> Self`, `async fn tick(&self, now: DateTime<Utc>) -> Result<BulletinTick, DbError>`, `async fn run(self, shutdown: CancelToken)` (tik co 10 s, sen w kawałkach ≤ 1 s jak `SourceRunner::wait`; błąd tiku → `tracing::error!`, pętla trwa).
  - `#[derive(Debug, Default, PartialEq)] pub struct BulletinTick { pub disabled: bool, pub queued: Option<i64>, pub skipped: Vec<(DateTime<Utc>, String)> }`.

`tick` (wszystko czytane z bazy przy każdym wywołaniu, rozstrzygnięcie 6):
1. `ModuleRepo::is_enabled(&BULLETIN)` fałsz → `disabled: true`, nic nie zapisane.
2. `times` z `module:bulletin:times` (nieczytelne → domyślne); `grace = min(misfire_grace_s, 900)` (B3).
3. Dla każdego `fire_at` z `due_fires`: spóźnione ponad `grace` → `claim(fire_at, Some("spóźnione ponad misfire_grace_s"))`; spośród mieszczących się najnowsza jest kandydatem, starsze → `claim(…, Some("zastąpione nowszą porą"))`. `claim` zwraca `false` → już załatwione, bez wpisu w `skipped`.
4. Kandydat: `claim(fire_at, None)`; `false` → koniec. Cisza: `quiet_refuses(settings.quiet_now(now.with_timezone(&Local).time()), settings.quiet_min_priority, Priority::Normal)` → `set_skipped("cisza nocna")`. Inaczej `compose(now)` (`hhmm` z `fire_at`, nie z `now` — dodaj parametr wewnętrzny); `text: None` → `set_skipped(skipped_reason)`; tekst → `enqueue { message_id: None, priority: Normal, interrupting: false, not_before: Some(now), expires_at: Some(fire_at + 15 min), repeat_count: 1, repeat_interval: ZERO, created_by: None }`, `set_queued`.

- [ ] **Step 1: Testy runnera** (fałszywe części: `Fixed(&'static str id, PartText)`, `Failing`, `Slow(Duration)`; `test_pool`, znak `SP9XYZ`, moduł włączony, `times "07:00,19:00"`, `misfire_grace_s 600`; `t(local)` = Warszawa → UTC)

```rust
#[test] fn due_fires_cover_yesterday_and_dst() {
    // times [23:59], now 2026-11-24 00:00:30 lokalnie -> [2026-11-23T22:59:00Z]
    // times [02:30], now 2026-03-29 12:00 lokalnie -> tylko wczorajsze 2026-03-28T01:30:00Z (dzisiejsza nie istnieje)
    // times [02:30], now 2026-10-25 12:00 lokalnie -> zawiera 2026-10-25T00:30:00Z (pierwsza z podwójnych)
}
#[tokio::test] async fn a_due_time_fires_once_even_after_a_restart() {
    // tick(t("2026-11-23 07:00:05")) -> queued Some(id); element: tekst zaczyna się "Biuletyn stacji SP9XYZ. 07:00. ",
    //   kończy "Koniec biuletynu.", Normal, !interrupting, expires_at 2026-11-23T06:15:00Z
    // drugi tick 07:00:15 -> queued None; nowy BulletinRunner na tej samej bazie -> queued None; w kolejce 1 element
}
#[tokio::test] async fn a_disabled_bulletin_records_nothing() { /* bulletin_fires pusta */ }
#[tokio::test] async fn a_late_time_is_skipped_not_aired() {
    // tick 07:11 -> skipped [(07:00, "spóźnione ponad misfire_grace_s")], kolejka pusta
    // misfire_grace_s 3600, tick 07:16 -> też pominięte (B3: grace ≤ 900 s)
}
#[tokio::test] async fn no_avalanche_after_downtime() {
    // pierwszy tick 2026-11-23 20:30 -> 0 w kolejce; wczoraj 07:00, 19:00 i dziś 07:00, 19:00 — wszystkie "spóźnione…"
}
#[tokio::test] async fn adding_a_time_just_after_it_fires_once() {
    // times "19:00"; tick 07:02 nic; operator zapisuje "07:00,19:00"; tick 07:03 -> queued; tick 07:04 i restart -> nic
}
#[tokio::test] async fn only_the_newest_due_time_is_composed() {
    // times "07:00,07:05", tick 07:06 -> 1 element "… 07:05. …"; 07:00 ma skipped_reason "zastąpione nowszą porą"
}
#[tokio::test] async fn quiet_hours_skip_a_normal_bulletin() {
    // okno = [lokalny(Local) czas ticku − 1 h, + 1 h], quiet_min_priority High -> skipped "cisza nocna", kolejka pusta
    // quiet_min_priority Normal -> queued
}
#[tokio::test] async fn a_quiet_window_ending_at_the_fire_time_does_not_block_it() {
    // okno kończy się dokładnie o lokalnym (Local) czasie odpalenia, zaczyna 9 h wcześniej; tick 1 s po porze -> queued
}
#[tokio::test] async fn an_empty_bulletin_is_not_aired() {
    // wszystkie części Empty -> skipped "brak danych", kolejka pusta; Fixed("ostrzezenia", Text("Brak obowiązujących ostrzeżeń.")) -> queued
}
#[tokio::test] async fn a_failing_part_drops_out() { /* compose: Failing -> Failed(..), pozostałe Included */ }
#[tokio::test] async fn slow_parts_time_out_in_parallel() {
    // trzy Slow(5 s), with_part_timeout(200 ms), compose trwa < 1 s; wszystkie Failed("przekroczony czas …")
}
#[tokio::test] async fn settings_are_read_at_each_fire() {
    // zmiana callsign na SP9ABC między tikami 07:00 i 19:00 -> drugi tekst "Biuletyn stacji SP9ABC."
    // intro "Tu SP9XYZ" -> "Tu SP9XYZ. 19:00. …"; parts "ostrzezenia" -> w Composed.parts tylko ostrzezenia
}
#[tokio::test] async fn the_tx_limit_drops_parts_from_the_end() {
    // tx_max_duration_s 30, trzy części po 200 znaków -> tekst z pierwszą; parts[1..] DroppedForLength
}
```

- [ ] **Step 2: Test integracyjny `tests/bulletin.rs`** — wiremock: `/v1/forecast` (Open-Meteo), `/compact` (MET), `/f107`, `/kp`, `/scales`, `/stations` z fiksami; `Config` z tymi adresami; pozycja Rzeszów; znak `SP9XYZ`; `build(&cfg, pool)`; `compose(at("2026-11-22T19:00:00Z"))` → `text` ==

```text
Biuletyn stacji SP9XYZ. 20:00. Pogoda dla okolic stacji. Teraz 2 °C, bezchmurnie. W nocy do 1 °C. Jutro śnieg, do 3 °C, opady do 2,4 mm, porywy wiatru do 45 km/h. Dane: Open-Meteo. Propagacja. Strumień słoneczny 98, umiarkowany. Indeks K 0,3, warunki spokojne. Pogoda kosmiczna bez zaburzeń. Jonosonda Warszawa: częstotliwość krytyczna 3,2 megaherca, MUF 10,6 megaherca. Łączności bliskie do pasma 160 metrów, dalekie do pasma 40 metrów. Dane: NOAA, GIRO przez KC2G. Brak obowiązujących ostrzeżeń. Koniec biuletynu.
```

  oraz: włączone źródło `swpc` z nadaną pozycją `G3` (wiersz `source_items` jak w testach `source_state`) → zamiast „Brak…” jest „Obowiązujące ostrzeżenia: G3 — silna burza geomagnetyczna.”; `BulletinRunner::tick` o 19:00:05 lokalnie wstawia dokładnie jeden element; `normalize(text)` przechodzi `assert_speakable`-owe sprawdzenia (skopiuj warunki: brak „godzinie godzina”, brak surowych `NOAA`, `MUF`, `KC2G`).

- [ ] **Step 3: `cargo test -p aars-sources bulletin` i `--test bulletin` — FAIL.**
- [ ] **Step 4: Implementacja** wg Interfaces.
- [ ] **Step 5: Zielone; clippy.**
- [ ] **Step 6: Commit** `feat: biuletyn o porach lokalnych — bez dublowania po restarcie, bez lawiny po przestoju, z ciszą nocną`

### Task 8: Podłączenie — stacja, CLI, panel, dokumentacja

**Files:**
- Modify: `crates/aars/src/station.rs:518-665, 741-812` (budowa `Bulletin`, wspólna `Pogoda`, zadanie `BulletinRunner`, `with_bulletin_preview`)
- Modify: `crates/aars/src/cli.rs` (`Command::Bulletin { action: BulletinAction }`, `BulletinAction::Preview`), `crates/aars/src/main.rs` (`bulletin_preview`)
- Modify: `crates/aars-web/src/state.rs` (typy podglądu, pole, `with_bulletin_preview`, rejestr testowy += `"pogoda"`), `crates/aars-web/src/modules_page.rs` (handler `preview_bulletin`, `ModuleView.bulletin_preview`), `crates/aars-web/src/lib.rs` (trasa `POST /modules/bulletin/preview`), `crates/aars-web/templates/modules.html` (przycisk, wynik, odsłuch JS jak w `compose.html`), `crates/aars-web/src/role_matrix.rs`
- Modify: `README.md` (sekcja „Biuletyn (etap 11b)”, tabela źródeł += `pogoda`, „W planach” → 11c), `aars.example.toml`, `packaging/etc/aars.toml` (jeśli Task 5/6 czegoś nie dopisały), `docs/superpowers/specs/2026-10-13-aars-design.md` §13 pkt 11 (dopisek: 11b/11c, link do projektu 11b)

**Interfaces:**
- Consumes: Task 7 (`build`, `Bulletin::compose`, `BulletinRunner`, `PartOutcome::describe`), Task 5 (`pogoda::ID`).
- Produces (w `aars-web`, bez typów z `aars-sources`):
  - `pub struct BulletinPreviewView { pub text: Option<String>, pub parts: Vec<(String, String)> /* (id, describe) */, pub skipped_reason: Option<String> }`
  - `pub type BulletinPreviewFuture = Pin<Box<dyn Future<Output = Result<BulletinPreviewView, String>> + Send>>;`
  - `pub type BulletinPreview = Arc<dyn Fn() -> BulletinPreviewFuture + Send + Sync>;`
  - `AppState.bulletin_preview: Option<BulletinPreview>`, `AppState.bulletin_preview_gate: Arc<tokio::sync::Semaphore>` (1 zezwolenie), `with_bulletin_preview(self, p: BulletinPreview) -> Self`.
  - `POST /modules/bulletin/preview` (operator, CSRF) → strona modułów z wynikiem w karcie `bulletin`; brak wywołania zwrotnego → 503 z komunikatem „podgląd biuletynu niedostępny”; semafor zajęty (`try_acquire`) → 503 „podgląd biuletynu już trwa, spróbuj za chwilę”; `Err(e)` → 502 z „podgląd biuletynu zawiódł: {e}”. Wszystkie jako komunikat w karcie (`ModuleView.error`).
  - CLI: `aars bulletin preview` drukuje `tekst:` + tekst albo `nic do nadania: {powód}`, potem wiersz `{id}: {describe}` na część.

Stacja: przed pętlą źródeł `aars_sources::bulletin::build(&config, pool.clone())`; w pętli dla `id == pogoda::ID` źródłem jest `pogoda.clone() as Arc<dyn DataSource>` zamiast `(module.build)(&config)` (błąd `build` → `source_start_failed` dla `pogoda` i brak biuletynu, `tracing::error!`). `BulletinRunner::new(bulletin.clone(), queue.clone()).run(shutdown.clone())` w `tasks` (jak `scheduler.run`) — **startuje zawsze**, sam sprawdza włączenie co tik; bez stosu radiowego wstawia do kolejki jak harmonogram (nadaje dyspozytor, nie runner). Wywołanie zwrotne podglądu: `bulletin.compose(Utc::now())` → `BulletinPreviewView`.

- [ ] **Step 1: Testy**

```rust
// cli.rs
#[test] fn bulletin_preview_parses() {
    assert!(matches!(Cli::try_parse_from(["aars", "bulletin", "preview"]).unwrap().command,
        Command::Bulletin { action: BulletinAction::Preview }));
}
// modules_page.rs
#[tokio::test] async fn an_operator_previews_the_bulletin() {
    // with_bulletin_preview(|| Ok(view{text Some("Biuletyn stacji SP9XYZ. 07:00. Koniec biuletynu."),
    //   parts [("pogoda","pominięta — brak pozycji stacji")]})) ; POST jako operator -> 200,
    // treść zawiera tekst, "pogoda", "pominięta — brak pozycji stacji", przycisk "Odsłuchaj"
}
#[tokio::test] async fn the_bulletin_preview_without_a_callback_is_unavailable() { /* 503, "podgląd biuletynu niedostępny" */ }
#[tokio::test] async fn one_bulletin_preview_at_a_time() {
    // wywołanie zwrotne czeka na Notify; drugi POST w trakcie -> 503 "już trwa"; po zwolnieniu -> 200
}
#[tokio::test] async fn a_bulletin_preview_error_is_shown() { /* Err("x") -> 502, "podgląd biuletynu zawiódł: x" */ }
// role_matrix.rs: POSTS += ("/modules/bulletin/preview", OPERATOR), ("/modules/bulletin/enabled", ADMIN),
//   ("/modules/bulletin/settings", ADMIN), ("/sources/pogoda/enabled", OPERATOR), ("/sources/pogoda/refresh", OPERATOR),
//   ("/modules/pogoda/enabled", ADMIN), ("/modules/pogoda/settings", ADMIN);
//   oczekiwane statusy dopuszczonej roli: preview 503 (stan testowy bez wywołania), bulletin/enabled 303,
//   bulletin/settings 303 (ciało bez pól biuletynu = bez zmian), pogoda jak swpc (303/503, 303/400).
// station.rs (wzór istniejących testów startu ze station_config): stacja startuje z włączonym biuletynem i bez sieci
//   (adresy na 127.0.0.1:1) — start się udaje, w source_state jest wiersz "pogoda", nic nie trafia do kolejki
//   przed porą (pory ustaw na godzinę ≠ teraz ± 15 min), PTT nigdy nie kluczowany (fałszywy backend).
```

- [ ] **Step 2: `cargo test -p aars-web modules_page role_matrix` i `-p aars cli station` — FAIL.**
- [ ] **Step 3: Implementacja** wg Interfaces. Szablon: w karcie `bulletin` (gdy `id == "bulletin"`) formularz `POST /modules/bulletin/preview` z `csrf` i przyciskiem „Podgląd biuletynu”; wynik: `<p>` z tekstem, lista części; przycisk „Odsłuchaj” wysyła `text` + `csrf` na `/preview` (kopia skryptu z `compose.html`, tekst z ukrytego pola).
- [ ] **Step 4: Dokumentacja.** README: jak włączyć biuletyn i ustawić pory (czas lokalny, cisza nocna pomija porę), części i ich źródła, progi ocen pasm (SFI, Kp, NVIS foF2 × 1,0, DX MUF × 0,85, `cs == -1` albo ≥ 70, 1200 km, 60 min), limit długości `min(1500, 10 × limit nadawania)`, `aars bulletin preview`, licencje (Open-Meteo CC BY 4.0, MET Norway CC BY 4.0 / NLOD, NOAA domena publiczna, KC2G/GIRO CC BY-NC-SA — stacja niekomercyjna), uwaga B11. `aars.example.toml`/`packaging/etc/aars.toml`: zakomentowane `open_meteo_url`, `met_no_url`, `swpc_f107_url`, `swpc_kp_url`, `kc2g_url` z domyślnymi.
- [ ] **Step 5: Zielone całe `cargo test --workspace`; clippy czyste.**
- [ ] **Step 6: Commit** `feat: biuletyn w stacji, podgląd w panelu i w CLI, dokumentacja etapu 11b`
