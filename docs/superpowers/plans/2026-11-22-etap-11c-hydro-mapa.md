# Etap 11c: stany wód IMGW, mapa ostrzeżeń, poprawki z 11b

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Dwa nowe źródła IMGW (`imgw-hydro` — ostrzeżenia hydrologiczne dla województw obszaru, `imgw-wodowskazy` — przekroczenia stanów na wodowskazach w promieniu stacji), część biuletynu `wody`, strona panelu `/map` z powiatami pod ostrzeżeniem meteo i cztery poprawki odłożone z 11b.

**Architecture:** Źródła to kolejne `DataSource` w `aars-sources` (czyste `to_output` + cienki `fetch`), runner bez zmian poza przekazaniem odcisków śledzonych pozycji (`SourceCtx.tracked`) i odświeżaniem `valid_until` przy niezmienionym odcisku. `ImgwWodowskazy` jest jedną instancją dla źródła i części `wody` (jak `Pogoda`), `ImgwMeteo` jedną instancją dla runnera i mapy — pamięta wszystkie ostrzeżenia z ostatniego udanego pobrania. Panel nie zna `aars-sources`: mapa dostaje dane przez wywołanie zwrotne `WarningsMap` ustawiane w `station.rs`, jak podgląd biuletynu.

**Tech Stack:** jak w 11b: reqwest 0.13, serde_json, chrono 0.4 + chrono-tz 0.10, sha2 0.10 (już w `aars-sources`), sqlx 0.8, axum 0.8 (`axum::Json` z `#[derive(Serialize)]`), askama, rust-embed, Leaflet (zwendorowany), wiremock 0.6. `aars-web` dostaje `serde_json.workspace = true` tylko w `[dev-dependencies]` (testy czytają JSON).

**Spec:** `docs/superpowers/specs/2026-11-22-etap-11c-hydro-mapa-design.md` — **sekcja „Rozstrzygnięcia po przeglądzie” ma pierwszeństwo** przed resztą dokumentu (m.in. susza wypada z biuletynu, `valid_until` wodowskazu = pomiar + 24 h, odwołanie hydro z numerem). Tło: plany 11a/11b (decyzje A1–A13, B1–B12 obowiązują dalej), spec główny §7, §13.

## Fiksy (prawdziwe odpowiedzi z 2026-11-22, niczego nie poprawiać)

Katalog `crates/aars-sources/tests/fixtures/imgw/`:

- `hydro-2026-11-22.json` — tablica **913** obiektów; wszystkie pola to napisy albo `null`: `id_stacji`, `stacja`, `rzeka`, `wojewodztwo`, `lat`, `lon`, `stan_wody`, `stan_wody_data_pomiaru` (`"2026-11-22 20:20:00"`, czas Warszawy), `stan_ostrzegawczy`, `stan_alarmowy` (+ pola, których nie czytamy). 23 stacje mają `stan_wody: null`; współrzędne mają wszystkie. Pomiary: 665 × 20:xx, 188 × 19:xx, reszta starsza (najstarszy 2025-07-26). Od (50.04, 21.99): `150220010` Rzeszów/Wisłok 1,8 km, stan 72, ostrz. 300, alarm. 420; `149210130` Żarnowa/Wisłok 21,9 km, 164/400/490; `149210120` Godowa/Stobnica 24,8 km, 609/740/880; `149229999` Manasterz/Mleczka 27,8 km, stan 264, progi `null`, `wojewodztwo: null`; Wola raniżowska 28,3 km (19:50). Pomiar wszystkich czterech: `2026-11-22 20:20:00`. **17** stacji ≥ ostrzegawczy, z nich 2 ≥ alarmowy: `154180100` Władysławowo/Bałtyk 572/550/570 i `154180140` Gdańsk, rzeka `"-"`, 570/550/570. Przykład ostrzegawczego: `153180120` Hel/Bałtyk 566/550/570.
- `warningshydro-2026-11-22.json` — tablica 34 obiektów: `numer`, `biuro`, `opublikowano`, **`stopień`** (z „ś”, napis), `data_od`, `data_do`, `prawdopodobienstwo`, `zdarzenie`, `przebieg`, `komentarz`, `obszary[]` (`wojewodztwo` małymi literami, `opis`, `kod_zlewni[]`). 30 × susza (`stopień "-1"`, `data_do "9999-12-31 23:59:59"`). 4 × `stopień "2"`, `zdarzenie "Wezbranie z przekroczeniem stanów ostrzegawczych"`, wszystkie z `biuro "Biuro Prognoz Hydrologicznych w Gdyni, Wydział Prognoz i Opracowań Hydrologicznych w Gdyni"`: nr **137** pomorskie, `data_do "2026-11-24 12:00:00"`, opis „pomorskie, Wybrzeże Wschodnie i morskie wody wewnętrzne RP - Bałtyk Południowo-Wschodni, wezbranie z przekroczeniem stanów ostrzegawczych”; **138** zachodniopomorskie, do `2026-11-24 18:00:00`, opis z „Bałtyk Płd”; **139** zachodniopomorskie, do `2026-11-25 10:00:00`; **140** pomorskie + warmińsko-mazurskie, do `2026-11-24 15:00:00`.
- `warningsmeteo-2026-07-23.json` (z etapu 8) — 5 ostrzeżeń stopnia 1, m.in. `Sk20260722203349998` „Burze” 19 powiatów (`1801`, `1821`, `1806`, …), do `2026-07-23 22:00:00`; `warningsmeteo-2026-11-20-brak.json` — obiekt „Brak ostrzeżeń”.
- `assets/powiaty.geojson` — `FeatureCollection`, 380 `MultiPolygon`, `properties {teryt: "1863", nazwa: "powiat Rzeszów"}`.

Warianty (inny stan, `null`, brak klucza) testy robią w kodzie na `serde_json::Value`.

## Decyzje etapu 11c

Decyzje A1–A13 (11a) i B1–B12 (11b) obowiązują; nowe mają numery C.

- **C1** Atrybucja: nowe źródła piszą „Źródło: IMGW-PIB.”; `imgw-meteo` zostaje przy „Źródło: IMGW.” (spec mówi „jak `imgw-meteo`”, ale `imgw-meteo` dziś pisze „IMGW” — nie zmieniamy nadawanego tekstu istniejącego źródła). Normalizer dostaje skróty `("IMGW-PIB", "I M G W P I B")`, `("RP", "R P")`, `("Płd", "Południowy")`, `("nr", "numer")`; `assert_speakable` dopisuje `"PIB"`, `"RP"`, `"Płd"`, `"nr"` do `RAW`.
- **C2** Stopień w tekście hydro słownie, jak w meteo: „Ostrzeżenie hydrologiczne IMGW drugiego stopnia: …” (spec pisze „{stopień}. stopnia” — „2. stopnia” normalizer przeczytałby „dwa. stopnia”). `imgw_meteo::speech::{level_genitive, day_month}` stają się `pub(crate)`. Podsumowanie panelu zostaje cyframi: „{zdarzenie}, 2. stopień”.
- **C3** Opis obszarów hydro = `opis` pasujących obszarów (po `clean_text`), złączone „; ”, pierwsza litera wielka, z kropką. Województwa w odwołaniu = tylko pasujące do obszaru stacji, w kolejności z odpowiedzi, złączone „, ”.
- **C4** Limit 700 znaków hydro: skracany jest tylko `przebieg` (`truncate_sentence` do budżetu = 700 − długość reszty tekstu), początek, ważność i atrybucja zostają zawsze; budżet < 20 znaków → `przebieg` wypada. „Ważne do” z północą = „24:00” poprzedniego dnia (jak `imgw_meteo::speech::validity`).
- **C5** Odcisk hydro: `"{stopień}|{data_do %Y-%m-%d %H:%M}|{hex}"`, `hex` = pierwsze 8 bajtów SHA-256 z `clean_text(przebieg)` (jak RSO; `DefaultHasher` nie jest stabilny między wersjami Rusta, a odcisk leży w bazie). Klucz pola stopnia: `stopień`, zapasowo `stopien`. Stopień spoza {−1, 1, 2, 3}, brak `numer`/`zdarzenie`/dat → wpis pominięty, `complete = false`.
- **C6** Wodowskaz: liczby z napisów jako `f64` (skończone); próg ≤ 0, pusty, `null`, nie-liczba = brak progu. Alarm, gdy próg alarmowy jest i `stan ≥ alarm`; ostrzegawczy, gdy próg ostrzegawczy jest i `stan ≥ ostrz.` (sam próg alarmowy bez ostrzegawczego też działa). W tekście cm przez `round_int`. Świeży pomiar: `now − pomiar ≤ 3 h` (pomiar „z przyszłości” też świeży); czas nieistniejący w Warszawie = brak pomiaru.
- **C7** Wodowskaz — kompletność (rozstrzygnięcie 1): `complete = tracked_keys ⊆ świeże` (świeże = stacje z wybranych, które w tym cyklu mają świeży pomiar, niezależnie od progu). Stacja, która zniknęła z odpowiedzi albo straciła współrzędne, też nie jest „świeża”. Wpisy bez `id_stacji`/`stacja`/współrzędnych pomijane po cichu; niepusta tablica bez żadnej stacji → `Parse`.
- **C8** Tekst zmiany wodowskazu (spec nie podaje): „Zmiana stanu wody. {tekst}”. Rzeka pusta, `null` albo `"-"` → bez „ na rzece …” w tekście, podsumowanie „{stacja}: stan …”, w biuletynie „{stacja}: {cm} cm”.
- **C9** Część `wody` czyta ustawienia `source:imgw-wodowskazy:*` z nowego pola `PartCtx.sources` (wszystkie klucze `source:*` bez przedrostka `source:`); dane z pamięci instancji, gdy pobrane ≤ 20 min temu, inaczej pobiera sama i zapamiętuje. Zdanie: „Stany wód. {rzeka} w {stacja}: {cm} cm[, stan ostrzegawczy|, stan alarmowy]. … Źródło: IMGW-PIB.” (stacje kropką, do 3, rosnąco po odległości; lista `stations` też sortowana po odległości).
- **C10** Pogoda dzień 0 (rozstrzygnięcie 4): siła odcisku = |liczba| po dwukropku (`gust:80` → 80, `frost:-15` → 15, `precip:20` → 20). Dla pozycji „dziś” ze śledzonym odciskiem: nowa siła < śledzonej albo brak alertu → pozycja z dotychczasowym odciskiem; jej tekst przy braku alertu budowany z wartości kubełka (runner tylko ją dotyka — ten sam klucz i odcisk — więc nie idzie na antenę). „Jutro” bez zmian.
- **C11** Runner: `touch` przy niezmienionym odcisku zapisuje też `valid_until` z odpowiedzi (wymóg „odświeżany z każdym świeżym pomiarem”); dla `imgw-meteo` bez skutku (ważność jest w odcisku).
- **C12** Mapa — stan: konfiguracja `imgw-meteo` brak/wyłączona → `disabled` (przed wszystkim innym: pamięć wyłączonego źródła i tak nie jest pokazywana); pamięć pusta → `never`; `now − fetched_at > max(3 · interval_s, 1800 s)` → `stale`; inaczej `ok`. Poza `ok` serwer wysyła `warnings: []`. Ostrzeżenie meteo bez stopnia → `level 1` (lepiej pokazać niż ukryć); `valid_until` nieistniejące w Warszawie → pominięte. Jeden wpis na parę (ostrzeżenie, powiat), posortowane po `teryt`, potem stopień malejąco. Czasy w JSON: RFC 3339 UTC z `Z`, sekundy.
- **C13** Mapa odświeża `/map/data` co 300 s; GeoJSON pobierany raz. Kolory: 1 `#ffd400`, 2 `#ff8c00`, 3 `#d7191c`, `fillOpacity 0.55`; powiat bez ostrzeżenia bez wypełnienia, obrys `#888` 0,5 px; obszar stacji obrys `#1f4fd1` 2,5 px (kod 2-cyfrowy = wszystkie powiaty z tym przedrostkiem).
- **C14** Kształt składania: `bulletin::build` zwraca `BuiltBulletin { bulletin, pogoda, wodowskazy }`; `station.rs` zbiera instancje współdzielone w `SharedSources { pogoda, wodowskazy, meteo }` i `source_for(module, config, &SharedSources)`.

## Global Constraints

- Testy nigdy nie dotykają sieci: HTTP tylko wiremock na 127.0.0.1 i fiksy z `tests/fixtures`.
- Walidacja i testy nie otwierają portu szeregowego, karty dźwiękowej ani GPIO.
- Nic nie może kluczować nadajnika: źródła i biuletyn tylko wstawiają do kolejki (`QueueRepo::enqueue`), nigdy nie wołają sekwencera.
- Kierunek zależności: `aars-core` nie zależy od `aars-sources`; `aars-web` nie zależy od `aars-sources` (dane mapy przez wywołanie zwrotne `WarningsMap`, jak `BulletinPreview`).
- Żadne `unwrap`/`expect`/indeksowanie na danych z sieci; nieznany schemat = `SourceError::Parse`, nigdy zera.
- `crate::http` (limit rozmiaru, timeout 20 s), User-Agent `aars/{CARGO_PKG_VERSION}`. Limity: `warningshydro` 1 MiB, `hydro` 2 MiB.
- Atrybucja: w treści komunikatów hydro „Źródło: IMGW-PIB.”; w README i w stopce mapy „Źródłem pochodzenia danych jest IMGW-PIB; dane zostały przetworzone”; na mapie „Granice: PRG, GUGiK”.
- Źródło pisze cyfry, `HH:MM`, przecinek dziesiętny i tokeny jednostek (`cm`); słowa robi normalizer w `aars-audio`.
- Nowe źródła `default_enabled: false`; domyślne `parts` biuletynu bez zmian.
- Teksty komunikatów i panelu po polsku.
- Nie uruchamiać `cargo fmt` na repo — tylko `rustfmt --edition 2024` na nowych plikach; w zmienionych plikach formatować ręcznie tylko dotknięte linie.
- Commity bez żadnej atrybucji AI: bez `Co-Authored-By`, `Claude-Session`, „Generated with” i bez wzmianki o AI w treści. Praca na gałęzi `etap-11c-hydro-paa`, bez nowych gałęzi.
- Koniec każdego zadania: `cargo test --workspace` zielone, `cargo clippy --workspace --all-targets -- -D warnings` czyste.

## Review Focus

- **Śledzony wodowskaz chwilowo bez pomiaru** (IMGW zwraca `stan_wody: null` albo pomija stację na dwa cykle) — żadnego „spadł poniżej”; pozycja wisi do `valid_until` i wygasa po cichu. Test w Task 3 (`a_tracked_gauge_without_a_fresh_reading_makes_the_fetch_incomplete`).
- **Wodowskaz bez rzeki** (`rzeka "-"`, Gdańsk w fiksie) — tekst bez „na rzece -”. Test w Task 3 (`a_gauge_without_a_river_is_named_alone`).
- **Odpowiedź `hydro` to niepusta tablica bez czytelnej stacji** (zmiana schematu) — `Parse`, nie „wszystko poniżej progu” i nie seria odwołań. Test w Task 3 (`an_array_without_any_gauge_is_a_parse_error`).
- **Ostrzeżenie hydro z nieznanym stopniem albo złą datą** — nie jest ogłaszane i nie odwołuje innych (odpowiedź niekompletna). Test w Task 2 (`an_unknown_level_or_bad_date_is_skipped_and_incomplete`).
- **Mapa po restarcie przy źródle „zdrowym” w bazie** (ostatni sukces sprzed restartu świeży) — `never` i baner, nie „brak ostrzeżeń”. Test w Task 5 (`an_empty_memory_is_never_even_if_the_database_says_fresh`).

---

### Task 1: Poprawki z 11b — zdrowie źródeł, normalizer, 503, odcisk pogody dnia 0

**Files:**
- Modify: `crates/aars-core/src/source_state.rs` (`alert_source_health`, `touch`, test `alert_source_health_counts_enabled_and_stale_sources`)
- Modify: `crates/aars-audio/src/text/normalize.rs` (`godz_expansion`, wywołanie w `normalize`, `ABBREVIATIONS`, testy w `:520-531`)
- Modify: `crates/aars-sources/src/speech.rs` (`assert_speakable`: `RAW` += `"PIB"`, `"RP"`, `"Płd"`, `"nr"`)
- Modify: `crates/aars-web/src/modules_page.rs:429` i test `:1018`
- Modify: `crates/aars-sources/src/source.rs` (`SourceCtx.tracked`), `crates/aars-sources/src/runner.rs` (`run_cycle`, `context`, `touch`, `FakeSource`), wszystkie literały `SourceCtx { … }` (testy `imgw_meteo`, `rso`, `emsc`, `swpc`, `gios`, `pogoda`, `runner.rs:1155`, `crates/aars/src/main.rs:606`) — `tracked: Default::default()`
- Modify: `crates/aars-sources/src/pogoda/alerts.rs` (`to_output`)

**Interfaces:**
- Produces:
  - `SourceCtx.tracked: HashMap<String, String>` — klucz → odcisk aktywnych pozycji źródła przy bieżącym zakresie (te same pozycje co `tracked_keys`, które zostają).
  - `SourceStateRepo::touch(&self, row: i64, now: DateTime<Utc>, valid_until: Option<DateTime<Utc>>) -> Result<(), DbError>` (C11).
  - `AlertSourceHealth`: włączone źródło z `last_success_at = None` liczy się jako niedostępne (także bez żadnej próby). `SourceStatus::is_stale` bez zmian.

- [ ] **Step 1: Testy**

```rust
// source_state.rs — w alert_source_health_counts_enabled_and_stale_sources
// (fresh, old, never włączone; never bez żadnej próby):
assert_eq!(health, AlertSourceHealth { enabled: 3, unavailable: 2 }); // przed record_failure("never")
// po record_failure("never") — nadal 2
#[tokio::test] async fn touch_refreshes_valid_until() {
    // upsert_active(tracked("a") z valid_until 20:00Z), touch(row, now, Some(at("2026-07-24T08:00:00Z")))
    assert_eq!(repo.active_items("imgw-meteo").await.unwrap()[0].valid_until, Some(at("2026-07-24T08:00:00Z")));
}

// normalize.rs
#[test] fn godz_after_w_is_singular_unless_a_range_follows() {
    assert_eq!(n("w godz. 7:00"), "w godzinie siódmej");
    assert_eq!(n("w godz. 7:00."), "w godzinie siódmej.");
    assert_eq!(n("w godz. 7:00 – 15:00"), "w godzinach siódma – piętnasta");
}
#[test] fn godz_after_z_is_genitive() {
    assert_eq!(n("Pomiar z godz. 20:20."), "Pomiar z godziny dwudziestej dwadzieścia.");
}
#[test] fn imgw_pib_rp_and_nr_are_spelled() {
    assert_eq!(n("Źródło: IMGW-PIB."), "Źródło: I M G W P I B.");
    assert_eq!(n("wody wewnętrzne RP - Bałtyk Płd,"), "wody wewnętrzne R P - Bałtyk Południowy,");
    assert_eq!(n("nr 140"), "numer sto czterdzieści");
}
// Istniejące `assert_eq!(n("w godz. 7:00"), "w godzinach siódma")` usunąć (zastąpione).

// modules_page.rs:1018
assert!(html.contains("biuletyn nie wystartował — zobacz dziennik systemowy (journalctl -u aars)"), "{html}");

// runner.rs — FakeSource zapisuje też `last_tracked: Mutex<Option<HashMap<String, String>>>`
#[tokio::test] async fn tracked_fingerprints_reach_the_source() {
    // cykl 1: warning("k1","f1"); cykl 2: to samo
    assert_eq!(last_tracked, HashMap::from([("k1".to_owned(), "f1".to_owned())]));
}
#[tokio::test] async fn an_unchanged_item_gets_its_new_valid_until() {
    // cykl 1: k1/f1 valid_until now+1h; cykl 2: k1/f1 valid_until now+3h → announced 0, changed 0
    assert_eq!(r.state.active_items("imgw-meteo").await.unwrap()[0].valid_until, Some(now() + TimeDelta::hours(3)));
}

// pogoda/mod.rs (tests) — ctx_at(now, settings) + pole tracked; prognoza przez edited(...)
#[test] fn a_weaker_value_today_keeps_the_announced_fingerprint() {
    // now = 2026-11-22T07:00:00Z, porywy godzinowe dziś max 85 → bez śledzenia "gust:80"
    // ze śledzonym {"gust:2026-11-22": "gust:90"} → item("gust:2026-11-22").fingerprint == "gust:90"
    // porywy dziś max 60 (pod progiem 75), śledzone "gust:80" → pozycja jest, fingerprint "gust:80"
}
#[test] fn a_stronger_value_today_is_a_new_fingerprint() {
    // śledzone "gust:80", dziś max 95 → "gust:90"; śledzone "frost:-15", dziś min -21 → "frost:-20"
}
#[test] fn tomorrow_is_not_held() {
    // śledzone {"gust:2026-11-23": "gust:90"}, jutro porywy 45 → brak pozycji "gust:2026-11-23"
}
```

- [ ] **Step 2: `cargo test -p aars-core source_state`, `-p aars-audio normalize`, `-p aars-sources runner pogoda`, `-p aars-web modules_page` — FAIL (kompilacja: brak `tracked`, `touch` z 3 argumentami).**
- [ ] **Step 3: Implementacja.**
  - `alert_source_health`: niedostępne, gdy `status.last_success_at.is_none() || status.is_stale(..)`.
  - `touch`: `UPDATE source_items SET last_seen_at = ?, misses = 0, valid_until = ? WHERE id = ?` (format jak w `upsert_active`); runner przekazuje `item.valid_until`.
  - Runner: `ctx.tracked` z tych samych `active` (filtr `a.area == scope`) co `tracked_keys`.
  - Normalizer: `fn godz_expansion(previous_core: Option<&str>, range_follows: bool) -> (&'static str, bool, bool)` — `"w"` z zakresem → `("godzinach", false, true)`, bez → `("godzinie", true, false)`; `"z"` → `("godziny", true, false)`. `range_follows` liczy wywołujący przed rozwinięciem (ten sam warunek co dziś po `idx += 2`). Skróty z C1 dopisane na końcu `ABBREVIATIONS`.
  - 503: `"biuletyn nie wystartował — zobacz dziennik systemowy (journalctl -u aars)"`.
  - `pogoda::alerts::to_output`: reguła C10 tylko dla okresu „dziś”; `fn strength(fingerprint: &str) -> Option<i64>`; brak alertu przy śledzonej pozycji dnia 0 → `SourceItem` z kluczem/tematem `{kind}:{data}`, odciskiem śledzonym, `valid_until = end_of_day(date)`, tekstem i podsumowaniem z wartości kubełka (dla mrozu ze znakiem minus).
- [ ] **Step 4: Zielone, w tym `assert_speakable` w testach `emsc`, `rso`, `pogoda`; clippy.**
- [ ] **Step 5: Commit** `fix: poprawki z 11b — źródło bez sukcesu niedostępne, „w/z godz.”, 503 podglądu, odcisk pogody dnia 0 tylko rośnie`

### Task 2: Źródło `imgw-hydro`

**Files:**
- Create: `crates/aars-sources/src/imgw_hydro/{mod.rs,parse.rs,speech.rs}`
- Modify: `crates/aars-sources/src/lib.rs` (`pub mod imgw_hydro;`, `build_imgw_hydro`, `modules()`, test kolejności)
- Modify: `crates/aars-sources/src/imgw_meteo/speech.rs` (`level_genitive`, `day_month` → `pub(crate)`)
- Modify: `crates/aars-core/src/config.rs` (`SourcesConfig.imgw_hydro_url`, domyślna, `validate`, testy `a_partial_sources_block_keeps_the_other_defaults`, `every_source_url_is_validated`)
- Modify: `crates/aars-web/src/chrome.rs` (`source_label`: `"imgw-hydro" => "Ostrzeżenia hydrologiczne IMGW"`)

**Interfaces:**
- Consumes: `aars_core::area::voivodeship_name(code) -> Option<&'static str>`; `crate::speech::{warsaw_to_utc, clean_text, truncate_sentence}`; `imgw_meteo::speech::{level_priority, level_genitive, day_month}`.
- Produces:
  - `pub const ID: &str = "imgw-hydro"`, `pub const DEFAULT_URL = "https://danepubliczne.imgw.pl/api/data/warningshydro"`, `pub const MAX_BODY: usize = 1024 * 1024`.
  - `parse::HydroWarning { number: String, office: String, level: i8, valid_from: NaiveDateTime, valid_until: NaiveDateTime, event: String, course: String, areas: Vec<HydroArea> }`, `parse::HydroArea { voivodeship: String, description: String }`, `parse::ParsedHydro { warnings: Vec<HydroWarning>, skipped: usize }`, `parse::parse_warnings(body: &[u8]) -> Result<ParsedHydro, SourceError>` (obiekt „Brak ostrzeżeń…” = pusto; inny obiekt / nie-tablica = `Parse`).
  - `pub fn to_output(parsed: &ParsedHydro, ctx: &SourceCtx) -> SourceOutput`.
  - `pub struct ImgwHydro` z `new(url) / with_timeout(url, Duration)`; `DataSource`: `needs() = Area`, `default_interval() = 600 s`, zakres domyślny (`area_text`).
  - `SourcesConfig.imgw_hydro_url: String` (serde default = `DEFAULT_URL` jak wyżej).
  - `DESCRIPTOR`: id `imgw-hydro`, nazwa „Ostrzeżenia hydrologiczne IMGW”, opis „Ostrzeżenia hydrologiczne IMGW-PIB dla województw obszaru stacji, ogłaszane po zmianie. Susza hydrologiczna nie jest ogłaszana.”, `Switchable`, `default_enabled: false`, `Needs::Area`, ustawienia: `area` (`Area`, `""`, pomoc: „Kody jak w ostrzeżeniach meteo (np. 18 albo 1863 Rzeszów). Ostrzeżenia hydrologiczne IMGW dotyczą województw, więc powiat oznacza całe swoje województwo. Puste pole — źródło nic nie ogłasza.”), `interval_s` (`Integer { min: 300, max: 3600 }`, `"600"`, „Jak często odpytywać źródło.”).

Pozycja (spec + rozstrzygnięcie 2 + C2–C5): pomijane `level == -1`; dopasowanie, gdy któreś `areas[].voivodeship` ∈ nazwy województw z `ctx.area` (2 pierwsze cyfry kodu). `key = topic = "{numer}|{biuro}"`; priorytet `level_priority(Some(level))`; `valid_from/until` z `warsaw_to_utc` (brak → pominięte, `complete = false`).

Teksty (copy ustalone):
- ogłoszenie: `"Ostrzeżenie hydrologiczne IMGW {level_genitive} stopnia: {zdarzenie}. {Opis obszarów}. {przebieg} Ważne do {day_month} godz. {HH:MM}. Źródło: IMGW-PIB."`
- zmiana: `"Zmiana ostrzeżenia hydrologicznego. {ogłoszenie}"`
- odwołanie: `"Ostrzeżenie hydrologiczne nr {numer} ({województwa}) „{zdarzenie}” nie obowiązuje."`
- podsumowanie: `"{zdarzenie}, {level}. stopień"`

- [ ] **Step 1: Testy (`imgw_hydro/mod.rs`, `parse.rs`)**

```rust
const FIX: &[u8] = include_bytes!("../../tests/fixtures/imgw/warningshydro-2026-11-22.json");
const GDYNIA: &str = "Biuro Prognoz Hydrologicznych w Gdyni, Wydział Prognoz i Opracowań Hydrologicznych w Gdyni";

#[test] fn the_fixture_has_34_warnings_and_30_droughts() {
    let p = parse::parse_warnings(FIX).unwrap();
    assert_eq!((p.warnings.len(), p.skipped), (34, 0));
    assert_eq!(p.warnings.iter().filter(|w| w.level == -1).count(), 30);
    let w140 = p.warnings.iter().find(|w| w.number == "140").unwrap();
    assert_eq!(w140.areas.iter().map(|a| a.voivodeship.as_str()).collect::<Vec<_>>(), ["pomorskie", "warmińsko-mazurskie"]);
}
#[test] fn the_no_warnings_object_is_empty_and_another_object_is_a_parse_error() { /* {"message":"Brak ostrzeżeń"} → 0; {"error":1} → Parse; "<html>" → Parse */ }
#[test] fn droughts_are_never_items() {
    // obszar z każdym województwem, które ma suszę w fiksie (np. "30\n24\n12") → żadna pozycja z level -1
    assert!(run(FIX, "30").items.iter().all(|i| !i.summary.contains("Susza")));
}
#[test] fn warnings_match_by_voivodeship() {
    let keys = |area| run(FIX, area).items.iter().map(|i| i.key.split('|').next().unwrap().to_owned()).collect::<Vec<_>>();
    assert_eq!(keys("22"), ["137", "140"]);   // kolejność jak w odpowiedzi; dopasować do fiksu
    assert_eq!(keys("3262 Szczecin"), ["138", "139"]);
    assert_eq!(keys("28"), ["140"]);
    let none = run(FIX, "18");
    assert!(none.items.is_empty() && none.complete);
}
#[test] fn the_item_for_137() {
    let out = run(FIX, "22");
    let i = out.items.iter().find(|i| i.key == format!("137|{GDYNIA}")).unwrap();
    assert_eq!(i.topic, i.key);
    assert_eq!((i.priority, i.interrupting), (Priority::High, false));
    assert_eq!(i.summary, "Wezbranie z przekroczeniem stanów ostrzegawczych, 2. stopień");
    assert_eq!(i.valid_until.unwrap().to_rfc3339(), "2026-11-24T11:00:00+00:00");
    assert!(i.text.starts_with("Ostrzeżenie hydrologiczne IMGW drugiego stopnia: Wezbranie z przekroczeniem stanów ostrzegawczych. Pomorskie, Wybrzeże Wschodnie"));
    assert!(i.text.ends_with("Ważne do 24 listopada godz. 12:00. Źródło: IMGW-PIB."));
    assert!(i.change_text.starts_with("Zmiana ostrzeżenia hydrologicznego. Ostrzeżenie hydrologiczne IMGW"));
    assert!(i.fingerprint.starts_with("2|2026-11-24 12:00|"));
}
#[test] fn the_cancellation_names_number_and_station_voivodeships() {
    let c = |area| run(FIX, area).items.into_iter().find(|i| i.key.starts_with("140|")).unwrap().cancel_text;
    assert_eq!(c("22"), "Ostrzeżenie hydrologiczne nr 140 (pomorskie) „Wezbranie z przekroczeniem stanów ostrzegawczych” nie obowiązuje.");
    assert_eq!(c("22\n28"), "Ostrzeżenie hydrologiczne nr 140 (pomorskie, warmińsko-mazurskie) „Wezbranie z przekroczeniem stanów ostrzegawczych” nie obowiązuje.");
}
#[test] fn level_three_interrupts() { /* stopień "3" → (Emergency, true) */ }
#[test] fn the_fingerprint_follows_level_end_and_course_only() {
    // zmiana komentarz/opublikowano/prawdopodobienstwo → ten sam; stopień "3", data_do, przebieg → inny
}
#[test] fn a_long_course_is_cut_but_validity_and_source_stay() {
    // przebieg = "Zdanie numer jeden. " × 100 → text ≤ 700 znaków, ends_with("Źródło: IMGW-PIB."), zawiera "Ważne do"
}
#[test] fn an_unknown_level_or_bad_date_is_skipped_and_incomplete() {
    // 137: stopień "4" → brak 137, complete false; osobno data_do "2026-03-29 02:30:00" → brak 137, complete false
}
#[test] fn texts_are_speakable() { /* assert_speakable dla text/change/cancel wszystkich pozycji z "22\n32\n28" */ }
// http_tests: fiks przez wiremock (path /warningshydro) → 2 pozycje dla "22"; 503 → Status; HTML → Parse;
// > MAX_BODY → TooLarge(MAX_BODY); id/needs/default_interval.
```

`lib.rs`: `all_sources_are_registered_in_order` → `[…, "pogoda", "imgw-hydro"]`. `config.rs`: domyślny adres w `a_partial_sources_block_keeps_the_other_defaults`, klucz `imgw_hydro_url` w `every_source_url_is_validated`.

- [ ] **Step 2: `cargo test -p aars-sources imgw_hydro` — FAIL.**
- [ ] **Step 3: Implementacja** według Interfaces i C2–C5; `fetch` = `get_limited` + `parse_warnings` + `to_output`.
- [ ] **Step 4: `cargo test --workspace`, clippy — zielone.**
- [ ] **Step 5: Commit** `feat: źródło imgw-hydro — ostrzeżenia hydrologiczne IMGW dla województw obszaru`

### Task 3: Źródło `imgw-wodowskazy` i część biuletynu `wody`

**Files:**
- Create: `crates/aars-sources/src/imgw_wodowskazy/{mod.rs,parse.rs,speech.rs}`
- Modify: `crates/aars-sources/src/lib.rs` (moduł, `build_imgw_wodowskazy`, `modules()`, test kolejności → `[…, "imgw-hydro", "imgw-wodowskazy"]`)
- Modify: `crates/aars-core/src/module.rs` (`BULLETIN_PARTS`, test kanonizacji)
- Modify: `crates/aars-sources/src/bulletin/mod.rs` (`PartCtx.sources`, `compose_at`, `BuiltBulletin`, `build`); literały `PartCtx` w `pogoda`, `propagacja`, `bulletin/ostrzezenia.rs` → `sources: BTreeMap::new()`
- Modify: `crates/aars-core/src/config.rs` (`imgw_hydro_stations_url`, jak w Task 2)
- Modify: `crates/aars/src/station.rs` (`SharedSources`, `source_for`, test `the_weather_source_is_the_bulletin_part_instance`), `crates/aars/src/main.rs:524` (`build(...)?.bulletin`)
- Modify: `crates/aars-web/src/chrome.rs` (`"imgw-wodowskazy" => "Stany wodowskazów IMGW"`)

**Interfaces:**
- Consumes: `crate::speech::{haversine_km, warsaw_to_utc, warsaw_hhmm, round_int}`; `BulletinPart`, `PartCtx`, `PartText`.
- Produces:
  - `pub const ID: &str = "imgw-wodowskazy"`, `DEFAULT_URL = "https://danepubliczne.imgw.pl/api/data/hydro"`, `MAX_BODY: usize = 2 * 1024 * 1024`.
  - `parse::Gauge { id: String, name: String, river: Option<String>, lat: f64, lon: f64, level_cm: Option<f64>, measured_at: Option<NaiveDateTime>, warning_cm: Option<f64>, alarm_cm: Option<f64> }`; `Gauge::exceedance(&self) -> Option<Exceedance>`; `pub enum Exceedance { Warning, Alarm }` z `fn adjective_genitive(self) -> &'static str` („ostrzegawczego”/„alarmowego”) i `fn noun(self) -> &'static str` („ostrzegawczy”/„alarmowy”, to też odcisk); `parse::parse_gauges(body: &[u8]) -> Result<Vec<Gauge>, SourceError>` (C6, C7; `river` `None` dla pustej, `null`, `"-"`).
  - `pub(crate) fn radius_km(settings: &BTreeMap<String, String>) -> u32` (5–100, domyślnie 25), `pub(crate) fn station_ids(settings) -> Vec<String>` (po przecinku, przycięte, tylko cyfry, bez duplikatów, posortowane), `pub(crate) fn selected<'a>(gauges: &'a [Gauge], pos: (f64, f64), radius_km: u32, ids: &[String]) -> Vec<(&'a Gauge, f64)>` (niepuste `ids` = tylko te, promień ignorowany; rosnąco po km, remis po `id`).
  - `pub fn to_output(gauges: &[Gauge], ctx: &SourceCtx) -> SourceOutput`.
  - `pub struct ImgwWodowskazy` z `new(url) / with_timeout(url, Duration)` i pamięcią `Mutex<Option<(DateTime<Utc>, Arc<Vec<Gauge>>)>>`; `DataSource` (`needs() = Position`, `default_interval() = 900 s`, `scope` = `"{lat:.2},{lon:.2}|r={r}|s={ids,}"`) i `BulletinPart` (`id() = "wody"`, `max_chars() = 400`).
  - `PartCtx.sources: BTreeMap<String, String>` (`"imgw-wodowskazy:radius_km"` → `"25"`), `PartCtx::source_settings(&self, source_id: &str) -> BTreeMap<String, String>` (klucze bez `"{id}:"`).
  - `pub struct BuiltBulletin { pub bulletin: Arc<Bulletin>, pub pogoda: Arc<pogoda::Pogoda>, pub wodowskazy: Arc<imgw_wodowskazy::ImgwWodowskazy> }`; `pub fn build(cfg: &Config, pool: SqlitePool) -> Result<BuiltBulletin, SourceError>` (części: pogoda, wodowskazy, propagacja, ostrzeżenia).
  - `BULLETIN_PARTS = [("pogoda", …), ("wody", "Stany wód w okolicy"), ("propagacja", …), ("ostrzezenia", …)]`; domyślne `parts` bez zmian.
  - `station.rs`: `struct SharedSources { pogoda: Option<Arc<Pogoda>>, wodowskazy: Option<Arc<ImgwWodowskazy>> }` (Task 6 dopisze `meteo`), `fn source_for(module, config, shared: &SharedSources) -> Option<Result<Arc<dyn DataSource>, SourceError>>` — `pogoda`/`imgw-wodowskazy` z `shared` (brak → `None`), reszta przez `module.build`.
  - `DESCRIPTOR`: id `imgw-wodowskazy`, nazwa „Stany wodowskazów IMGW”, opis „Komunikat, gdy wodowskaz w pobliżu stacji przekroczy stan ostrzegawczy lub alarmowy według IMGW-PIB. Część biuletynu „Stany wód w okolicy” korzysta z tych samych danych.”, `Switchable`, `default_enabled: false`, `Needs::Position`, ustawienia: `radius_km` („Promień od stacji (km)”, `Integer { min: 5, max: 100 }`, `"25"`, „Wodowskazy bliżej niż tyle kilometrów od stacji są śledzone.”), `stations` („Wodowskazy (id IMGW)”, `Text { max_len: 200 }`, `""`, „Opcjonalnie identyfikatory stacji IMGW (id_stacji) po przecinku, np. 150220010. Niepuste pole zastępuje promień.”), `interval_s` (`Integer { min: 600, max: 3600 }`, `"900"`).

Pozycja: `key = topic = id_stacji`, `fingerprint = exceedance.noun()`, priorytet ostrzegawczy `Normal` / alarmowy `High`, `interrupting: false`, `valid_from = pomiar`, `valid_until = pomiar + 24 h`. Teksty (copy ustalone, C8):
- ogłoszenie: `"Wodowskaz {stacja} na rzece {rzeka}: stan wody {cm} cm, powyżej stanu {ostrzegawczego|alarmowego} ({próg} cm). Pomiar z godz. {HH:MM}. Źródło: IMGW-PIB."`
- zmiana: `"Zmiana stanu wody. {ogłoszenie}"`
- odwołanie: `"Wodowskaz {stacja}: stan wody spadł poniżej stanu ostrzegawczego."`
- podsumowanie: `"{rzeka} w {stacja}: stan {ostrzegawczy|alarmowy}"`

- [ ] **Step 1: Testy**

```rust
const FIX: &[u8] = include_bytes!("../../tests/fixtures/imgw/hydro-2026-11-22.json");
// ctx: now 2026-11-22T20:00:00Z, pozycja (50.04, 21.99), ustawienia jak podane
#[test] fn the_fixture_parses_913_gauges_and_17_exceed() {
    let g = parse::parse_gauges(FIX).unwrap();
    assert_eq!(g.len(), 913);
    assert_eq!(g.iter().filter(|g| g.exceedance().is_some()).count(), 17);
    assert_eq!(g.iter().filter(|g| g.exceedance() == Some(Exceedance::Alarm)).count(), 2);
    let m = g.iter().find(|g| g.id == "149229999").unwrap();
    assert_eq!((m.warning_cm, m.alarm_cm, m.exceedance()), (None, None, None));
}
#[test] fn empty_zero_or_junk_thresholds_are_no_threshold() {
    // Rzeszów stan_wody "500" + stan_ostrzegawczy "" / "0" / "abc" / null, stan_alarmowy null → exceedance None
}
#[test] fn nearby_gauges_by_radius_or_list() {
    let ids = |s: &[(&str, &str)]| selected(&g, (50.04, 21.99), radius_km(&map(s)), &station_ids(&map(s))).iter().map(|(g, _)| g.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&[]), ["150220010", "149210130", "149210120"]);
    assert_eq!(ids(&[("radius_km", "30")]).len(), 5);
    assert_eq!(ids(&[("stations", " 149210120, 150220010,x")]), ["150220010", "149210120"]);
}
#[test] fn quiet_rzeszow_has_no_items_and_is_complete() { let o = to_output(&g, &ctx(&[])); assert!(o.items.is_empty() && o.complete); }
#[test] fn hel_and_wladyslawowo_exceed() {
    let o = to_output(&g, &ctx(&[("stations", "153180120,154180100")]));
    let hel = item(&o, "153180120");
    assert_eq!((hel.fingerprint.as_str(), hel.priority), ("ostrzegawczy", Priority::Normal));
    assert_eq!(hel.text, "Wodowskaz Hel na rzece Bałtyk: stan wody 566 cm, powyżej stanu ostrzegawczego (550 cm). Pomiar z godz. 20:20. Źródło: IMGW-PIB.");
    assert_eq!(hel.summary, "Bałtyk w Hel: stan ostrzegawczy");
    assert_eq!(hel.cancel_text, "Wodowskaz Hel: stan wody spadł poniżej stanu ostrzegawczego.");
    assert_eq!(hel.valid_until.unwrap().to_rfc3339(), "2026-11-23T19:20:00+00:00");
    let wl = item(&o, "154180100");
    assert_eq!((wl.fingerprint.as_str(), wl.priority, wl.interrupting), ("alarmowy", Priority::High, false));
    assert!(wl.text.contains("powyżej stanu alarmowego (570 cm)"));
    assert!(wl.change_text.starts_with("Zmiana stanu wody. Wodowskaz Władysławowo"));
}
#[test] fn a_gauge_without_a_river_is_named_alone() {
    let o = to_output(&g, &ctx(&[("stations", "154180140")]));
    assert!(item(&o, "154180140").text.starts_with("Wodowskaz Gdańsk: stan wody 570 cm, powyżej stanu alarmowego"));
    assert_eq!(item(&o, "154180140").summary, "Gdańsk: stan alarmowy");
}
#[test] fn a_reading_older_than_three_hours_is_skipped() {
    // now = 2026-11-22T22:30:00Z (pomiar 19:20Z, 3 h 10 min) → Hel pominięty, complete true (nieśledzony)
}
#[test] fn a_tracked_gauge_without_a_fresh_reading_makes_the_fetch_incomplete() {
    // tracked_keys {"153180120"}: (a) pomiar sprzed 4 h, (b) stan_wody null, (c) stacji brak w odpowiedzi → complete false
    // tracked {"153180120"} i świeży pomiar poniżej progu (stan "500") → brak pozycji, complete true
}
#[test] fn an_array_without_any_gauge_is_a_parse_error() {
    // [{"x":1}] → Parse; {"message":"…"} → Parse; [] → Ok(vec![])
}
#[test] fn the_scope_names_position_radius_and_list() {
    assert_eq!(src.scope(&ctx(&[("stations", "150220010, 149210120")])), "50.04,21.99|r=25|s=149210120,150220010");
}
#[test] fn texts_are_speakable() { /* assert_speakable dla wszystkich 17 pozycji przy stations = ich id */ }

// część `wody` (wiremock, pamięć instancji):
#[tokio::test] async fn the_part_names_the_three_nearest_gauges() {
    // PartCtx { now 20:00Z, pozycja Rzeszów, sources: {} } →
    assert_eq!(text, "Stany wód. Wisłok w Rzeszów: 72 cm. Wisłok w Żarnowa: 164 cm. Stobnica w Godowa: 609 cm. Źródło: IMGW-PIB.");
}
#[tokio::test] async fn the_part_marks_exceedances_and_reads_source_settings() {
    // sources {"imgw-wodowskazy:stations": "153180120,154180100"} → zawiera "Bałtyk w Hel: 566 cm, stan ostrzegawczy." i "Bałtyk w Władysławowo: 572 cm, stan alarmowy."
}
#[tokio::test] async fn the_part_reuses_a_recent_fetch_of_the_source() {
    // fetch źródła (expect(1) na mocku), potem compose części 10 min później → bez drugiego żądania
}
#[tokio::test] async fn without_position_or_fresh_gauges_the_part_is_empty() {
    // pozycja None → Empty("brak pozycji stacji"); now 2026-11-23T12:00Z → Empty("brak świeżych pomiarów wodowskazów")
}
// module.rs: input "ostrzezenia,wody,pogoda" → "pogoda,wody,ostrzezenia"; BULLETIN_PARTS[1] == ("wody", "Stany wód w okolicy")
// station.rs: the_weather_source_is_the_bulletin_part_instance rozszerzony — wodowskazy z source_for to Arc::as_ptr(&built.wodowskazy); bez SharedSources → None
// bulletin.rs (integracyjny) bez zmian w asercjach; `build` zwraca BuiltBulletin
```

- [ ] **Step 2: `cargo test -p aars-sources imgw_wodowskazy bulletin`, `-p aars-core module` — FAIL.**
- [ ] **Step 3: Implementacja.** `to_output` według C6–C8; `fetch` zapamiętuje listę po udanym parsowaniu (Mutex nigdy przez `await`, zatruty odzyskany jak w `pogoda`). `compose_at` wypełnia `PartCtx.sources` z `SettingsRepo::kv_prefix("source:")`. W `station.rs` i `main.rs` tylko dostosowanie do `BuiltBulletin`/`SharedSources` (biuletyn startuje jak dziś; błąd budowy wyłącza biuletyn, `pogoda` i `imgw-wodowskazy` — `source_start_failed` dla obu id).
- [ ] **Step 4: `cargo test --workspace`, clippy — zielone.**
- [ ] **Step 5: Commit** `feat: źródło imgw-wodowskazy i część biuletynu „Stany wód w okolicy”`

### Task 4: Pamięć ostrzeżeń `imgw-meteo` i typ danych mapy w panelu

**Files:**
- Modify: `crates/aars-sources/src/imgw_meteo/mod.rs` (pole pamięci, `fetch`, `snapshot`, testy)
- Modify: `crates/aars-web/src/state.rs` (typy, pole `warnings_map`, `with_warnings_map`, oba konstruktory)

**Interfaces:**
- Produces:
  - `pub struct MeteoSnapshot { pub fetched_at: DateTime<Utc>, pub warnings: Vec<parse::MeteoWarning> }` (`Clone`); `ImgwMeteo::snapshot(&self) -> Option<MeteoSnapshot>`. `fetch` po udanym `parse_warnings` zapisuje **wszystkie** ostrzeżenia (przed filtrem obszaru) z `fetched_at = ctx.now`; błąd sieci/parsowania pamięci nie zmienia.
  - `aars_web::state::MapWarning { pub teryt: String, pub level: u8, pub event: String, pub until: DateTime<Utc> }`, `aars_web::state::WarningsSnapshot { pub fetched_at: DateTime<Utc>, pub warnings: Vec<MapWarning> }` (oba `Debug, Clone, PartialEq`), `pub type WarningsMap = Arc<dyn Fn() -> Option<WarningsSnapshot> + Send + Sync>;`, `AppState.warnings_map: Option<WarningsMap>`, `AppState::with_warnings_map(self, map: WarningsMap) -> Self`.

- [ ] **Step 1: Testy (`imgw_meteo::tests::http_tests`)**

```rust
#[tokio::test] async fn a_fetch_remembers_every_warning_not_only_the_area() {
    // JULY, ctx("1061 Łódź") → out.items.len() == 1, ale snapshot().unwrap().warnings.len() == 5, fetched_at == ctx.now
}
#[tokio::test] async fn the_no_warnings_object_is_remembered_as_empty() { /* BRAK → Some(snapshot) z pustą listą */ }
#[tokio::test] async fn a_failed_fetch_keeps_the_previous_memory() {
    // najpierw JULY (mock up_to_n_times(1)), potem 503 → snapshot nadal 5 ostrzeżeń i stare fetched_at
}
#[test] fn a_new_source_remembers_nothing() { assert!(ImgwMeteo::new("http://127.0.0.1:1/x").unwrap().snapshot().is_none()); }
```

- [ ] **Step 2: `cargo test -p aars-sources imgw_meteo` — FAIL.**
- [ ] **Step 3: Implementacja** (`Mutex<Option<MeteoSnapshot>>`, blokada tylko wokół zapisu/odczytu); w `state.rs` same typy i pole (`None` w `new` i `for_tests`).
- [ ] **Step 4: `cargo test --workspace`, clippy — zielone.**
- [ ] **Step 5: Commit** `feat: imgw-meteo pamięta ostatnie ostrzeżenia; typ danych mapy w panelu`

### Task 5: Strona `/map`, `/map/data`, `warnings-map.js`

**Files:**
- Create: `crates/aars-web/src/map_page.rs` (+ `mod map_page;` w `lib.rs`), `crates/aars-web/templates/map.html`, `assets/warnings-map.js`
- Modify: `crates/aars-web/src/lib.rs` (trasy `/map`, `/map/data`)
- Modify: `crates/aars-web/templates/base.html` (link „Mapa” po „Pulpit”, każda rola)
- Modify: `crates/aars-web/src/assets.rs` (`Cache-Control`, testy)
- Modify: `crates/aars-web/src/role_matrix.rs` (`get("/map", VIEWER)`, `get("/map/data", VIEWER)`)
- Modify: `crates/aars-web/Cargo.toml` (`[dev-dependencies] serde_json.workspace = true`)

**Interfaces:**
- Consumes: `AppState.warnings_map` (Task 4), `SourceStateRepo::config("imgw-meteo") -> Option<SourceConfig>`, `PositionRepo::load`.
- Produces:
  - `#[derive(Serialize)] pub(crate) struct MapData { source: &'static str /* "imgw-meteo" */, state: &'static str, fetched_at: Option<String>, warnings: Vec<MapWarningView>, station: Option<LatLon>, area: Vec<String> }`, `MapWarningView { teryt, level: u8, event, until: String }`, `LatLon { lat: f64, lon: f64 }`.
  - `pub(crate) fn map_data(cfg: Option<&SourceConfig>, snapshot: Option<WarningsSnapshot>, station: Option<&StationPosition>, now: DateTime<Utc>) -> MapData` (C12; `area` = kody z `cfg.area`, także przy `disabled`).
  - `GET /map` (viewer) → `map.html`; `GET /map/data` (viewer) → `axum::Json(MapData)`, błąd bazy → `db_error`.
  - `assets::serve`: `powiaty.geojson` → `public, max-age=86400`, reszta bez zmian (`public, max-age=3600`).

`map.html` (rozszerza `base.html`, blok `head`: `leaflet.css`, `leaflet.js` i `warnings-map.js` z `defer`): `<div id="warnings-map" data-tiles="https://tile.openstreetmap.org/{z}/{x}/{y}.png" data-geojson="/assets/powiaty.geojson" data-src="/map/data" style="height:560px">`, `<p id="map-banner" class="error" hidden>`, `<div id="map-legend">` (trzy kolory z podpisami „1. stopień”, „2. stopień”, „3. stopień” i „obszar stacji”), `<p id="map-fetched">`, stopka „Źródłem pochodzenia danych jest IMGW-PIB; dane zostały przetworzone. Granice: PRG, GUGiK.”

`warnings-map.js` (wzór: `station-map.js` — IIFE, `'use strict'`, bez Leafleta nic nie robi, `tileerror` → notka): pobiera GeoJSON raz, `/map/data` przy starcie i co 300 s; koloruje tylko przy `state === "ok"` (najwyższy `level` na `teryt`, kolory C13); obszar: powiat, którego `teryt` równa się kodowi 4-cyfrowemu albo zaczyna się od 2-cyfrowego; znacznik stacji przy `station`; dymek: `nazwa` + „{event} ({level}. stopień, do {DD.MM HH:MM})” w czasie lokalnym przeglądarki; `#map-fetched` = „Pobrano: {DD.MM HH:MM}” albo „—”. Baner (dokładne teksty):
- `disabled`: „Źródło ostrzeżeń meteo wyłączone — mapa nie pokazuje bieżących ostrzeżeń.”
- `stale`: „Źródło ostrzeżeń meteo nieaktualne — mapa nie pokazuje bieżących ostrzeżeń.”
- `never` i pusty `area`: „Ustaw obszar źródła ostrzeżeń meteo — bez niego źródło nic nie pobiera, a mapa nie pokazuje ostrzeżeń.”
- `never` z obszarem: „Źródło ostrzeżeń meteo jeszcze nic nie pobrało od startu stacji — mapa nie pokazuje bieżących ostrzeżeń.”
- błąd pobrania `/map/data`: „Nie udało się pobrać danych mapy.” (i zdjęcie kolorowania)

- [ ] **Step 1: Testy (`map_page.rs`, `assets.rs`)**

```rust
fn cfg(enabled: bool, area: &str, interval_s: u64) -> SourceConfig { /* parse_area + area_text */ }
fn snap(fetched: DateTime<Utc>, w: Vec<MapWarning>) -> WarningsSnapshot { … }
const NOW: &str = "2026-07-23T12:00:00Z";
#[test] fn a_fresh_snapshot_is_ok_with_live_warnings_only() {
    let d = map_data(Some(&cfg(true, "1863 Rzeszów", 300)), Some(snap(now - 2min, vec![
        mw("1863", 2, "Silny wiatr", now + 1h), mw("1806", 1, "Burze", now - 1min)])), None, now);
    assert_eq!(d.state, "ok");
    assert_eq!(d.warnings.len(), 1);
    assert_eq!((d.warnings[0].teryt.as_str(), d.warnings[0].level), ("1863", 2));
    assert_eq!(d.warnings[0].until, "2026-07-23T13:00:00Z");
    assert_eq!(d.fetched_at.as_deref(), Some("2026-07-23T11:58:00Z"));
    assert_eq!(d.area, ["1863"]);
}
#[test] fn an_empty_memory_is_never_even_if_the_database_says_fresh() {
    let d = map_data(Some(&cfg(true, "18", 300)), None, None, now);
    assert_eq!((d.state, d.fetched_at, d.warnings.len()), ("never", None, 0));
    assert_eq!(d.area, ["18"]);
}
#[test] fn an_old_snapshot_is_stale_and_uncoloured() {
    // fetched now − 31 min, interval 300 → "stale", warnings []; fetched now − 29 min → "ok"
    // interval 900, fetched now − 44 min → "ok"; now − 46 min → "stale"
}
#[test] fn a_disabled_or_unconfigured_source_is_disabled() {
    // cfg(false, …) ze świeżym snapshotem → "disabled", warnings []; cfg None → "disabled"
}
#[tokio::test] async fn map_data_is_json_for_a_viewer_with_the_station() {
    // AppState::for_tests + with_warnings_map(Arc::new(|| Some(snap(...)))), save_config("imgw-meteo","1863 Rzeszów",300), set_enabled, PositionRepo::save(Some(50.04, 21.99))
    // GET /map/data jako viewer → 200, application/json; v["station"]["lat"] == 50.04, v["state"] == "ok", v["source"] == "imgw-meteo"
}
#[tokio::test] async fn the_map_page_carries_the_map_and_the_attribution() {
    // GET /map jako viewer → 200; html zawiera "id=\"warnings-map\"", "/assets/warnings-map.js", "/assets/powiaty.geojson",
    // "Źródłem pochodzenia danych jest IMGW-PIB; dane zostały przetworzone", "Granice: PRG, GUGiK", "href=\"/map\""
}
// assets.rs
#[tokio::test] async fn the_county_borders_are_cached_for_a_day() {
    // GET /assets/powiaty.geojson → 200, Content-Type zawiera "json", Cache-Control "public, max-age=86400"
    // GET /assets/station-map.js → Cache-Control "public, max-age=3600"
}
// leaflet_and_the_map_script_are_embedded: lista += "/assets/warnings-map.js"
// role_matrix: get("/map", VIEWER), get("/map/data", VIEWER)
```

- [ ] **Step 2: `cargo test -p aars-web map_page assets role_matrix` — FAIL.**
- [ ] **Step 3: Implementacja** według Interfaces, C12, C13; `no_template_loads_scripts_or_styles_from_another_origin` musi dalej przechodzić (kafelki to obrazki z `data-tiles`).
- [ ] **Step 4: `cargo test --workspace`, clippy — zielone. Ręcznie: `node --check assets/warnings-map.js` (jeśli `node` jest), inaczej przegląd składni.**
- [ ] **Step 5: Commit** `feat: mapa ostrzeżeń meteo w panelu (/map) z granicami powiatów PRG`

### Task 6: Składanie w stacji, konfiguracja przykładowa, dokumentacja

**Files:**
- Modify: `crates/aars/src/station.rs` (`SharedSources.meteo`, budowa `ImgwMeteo`, `source_for`, `warnings_snapshot`, `with_warnings_map`, testy)
- Modify: `aars.example.toml` (zakomentowane `imgw_hydro_url`, `imgw_hydro_stations_url` z domyślnymi adresami)
- Modify: `README.md` (§ „Co działa”, tabela źródeł, tabela części biuletynu, nowa § „Mapa ostrzeżeń”, „W planach”)
- Modify: `docs/superpowers/specs/2026-10-13-aars-design.md` (§13 pkt 11 — dopisek po 11c)

**Interfaces:**
- Consumes: `ImgwMeteo::snapshot` (Task 4), `aars_web::state::{WarningsSnapshot, MapWarning, WarningsMap}`, `SharedSources` (Task 3).
- Produces: `pub fn warnings_snapshot(s: aars_sources::imgw_meteo::MeteoSnapshot) -> aars_web::state::WarningsSnapshot` (C12: para ostrzeżenie × `teryt`, `level.unwrap_or(1)`, `until = warsaw_to_utc(valid_until)`, brak → pominięte). `SharedSources.meteo: Option<Arc<ImgwMeteo>>`; `source_for` dla `imgw-meteo` zwraca `shared.meteo` (brak → `None`, porażka już zapisana przez `source_start_failed`).

- [ ] **Step 1: Testy (`station.rs`)**

```rust
#[test] fn a_meteo_snapshot_becomes_map_warnings() {
    // MeteoSnapshot z JULY (parse_warnings) → warnings.len() == suma teryt wszystkich 5 ostrzeżeń;
    // wpis ("1806", level 1, "Burze", until 2026-07-23T20:00:00Z) obecny; level None → 1; obowiazuje_do "2026-03-29 02:30:00" → pominięte
}
#[tokio::test] async fn the_meteo_source_is_the_map_instance() {
    // source_for(imgw-meteo, &config, &shared) to ten sam Arc co shared.meteo (std::ptr::addr_eq); shared.meteo None → None
}
```

- [ ] **Step 2: `cargo test -p aars station` — FAIL.**
- [ ] **Step 3: Implementacja.** W `run`: `ImgwMeteo::new(&config.sources.imgw_meteo_url)` przed pętlą źródeł (błąd → `source_start_failed(.., "imgw-meteo", ..)`); przy `Some` — `state.with_warnings_map(Arc::new(move || meteo.snapshot().map(warnings_snapshot)))`.
- [ ] **Step 4: Dokumentacja.**
  - README, tabela źródeł: `imgw-hydro` (etap 11c) — „IMGW-PIB, ostrzeżenia hydrologiczne dla województw obszaru (stopnie 1–3; susza nie jest ogłaszana); obszar jak w `imgw-meteo`, powiat oznacza całe województwo” | „Źródło: IMGW-PIB.”; `imgw-wodowskazy` (etap 11c) — „IMGW-PIB, stany wodowskazów w promieniu stacji (domyślnie 25 km) albo z listy `id_stacji`: komunikat po przekroczeniu stanu ostrzegawczego i alarmowego z danych IMGW; pomiar starszy niż 3 h jest pomijany, stan bez nowego pomiaru wygasa po dobie” | „Źródło: IMGW-PIB.”. Pod tabelą akapit atrybucji: „Źródłem pochodzenia danych jest IMGW-PIB; dane zostały przetworzone” (regulamin IMGW-PIB, dotyczy też `imgw-meteo` i mapy).
  - README, części biuletynu: wiersz `wody` — „do 3 najbliższych wodowskazów ze świeżym pomiarem (ustawienia źródła `imgw-wodowskazy`), z dopiskiem o stanie ostrzegawczym/alarmowym; susza hydrologiczna w 11c nie jest podawana (dopasowanie po województwie dawałoby ją prawie zawsze)” | „IMGW-PIB (ta sama instancja co źródło, dane świeże do 20 min)”.
  - README, § „Mapa ostrzeżeń (etap 11c)”: `/map` dla każdej roli, powiaty kolorem najwyższego stopnia ostrzeżenia meteo z ostatniego pobrania `imgw-meteo`, baner, gdy źródło wyłączone/nieaktualne/po restarcie bez pobrania albo bez obszaru; podkład OSM z przeglądarki; granice PRG (GUGiK), skrypt `tools/powiaty/`.
  - README, „W planach”: PAA odłożone — portal `monitoring.paa.gov.pl` nie ma udokumentowanego publicznego API ani licencji danych; wraca po kontakcie z PAA. Mapa hydro i RSO/GIOŚ — poza zakresem.
  - Spec główny §13 pkt 11: dopisek „Dopisek po 11c: **11c** zrealizowano jako `imgw-hydro`, `imgw-wodowskazy`, część `wody` i mapę ostrzeżeń meteo w panelu ([projekt](2026-11-22-etap-11c-hydro-mapa-design.md)); **PAA odłożone** (brak publicznego API).”
- [ ] **Step 5: `cargo test --workspace`, clippy — zielone; `cargo run -p aars -- --help` (bez sieci) nie panikuje.**
- [ ] **Step 6: Commit** `feat: stacja łączy imgw-meteo z mapą; README i spec po etapie 11c`
