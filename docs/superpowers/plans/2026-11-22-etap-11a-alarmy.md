# Etap 11a: źródła alarmowe — RSO, NOAA SWPC, GIOŚ, EMSC

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cztery nowe moduły-źródła (`rso`, `swpc`, `gios-smog`, `emsc`) ogłaszają komunikaty ochrony ludności, pogodę kosmiczną, alerty smogowe i wstrząsy przez istniejący `SourceRunner`, włączane i ustawiane w panelu jak `imgw-meteo`.

**Architecture:** Każde źródło to `DataSource` w `crates/aars-sources/src/{id}/` (czysta funkcja odpowiedź → `SourceOutput` + cienka warstwa HTTP). Runner dostaje trzy uogólnienia: wymagania źródła (`Needs`), zakres R9 liczony przez źródło (`scope`) i brak komunikatu odwołania (pusty `cancel_text`). `aars-core` zyskuje `Needs` w `ModuleDescriptor` oraz rodzaje ustawień `Decimal` i `List`, żeby panel (który nie zna `aars-sources`) pokazywał poprawne zdrowie i formularze.

**Tech Stack:** jak w etapie 8: reqwest 0.13 (rustls), serde_json, chrono-tz 0.10, sha2 0.10 (dodać do `aars-sources`, ta sama wersja co w `aars-audio`), wiremock 0.6 (testy).

**Spec:** `docs/superpowers/specs/2026-11-22-etap-11a-alarmy-design.md` — **sekcja „Rozstrzygnięcia po przeglądzie” ma pierwszeństwo** przed resztą dokumentu. Tło: `docs/superpowers/specs/2026-10-13-aars-design.md` §7, plan etapu 8 (R1–R17 obowiązują dalej).

## Fiksy (prawdziwe odpowiedzi z 2026-11-22, niczego nie poprawiać)

Katalog `crates/aars-sources/tests/fixtures/`:

- `rso/wszystkie-ogolne-2026-11-22.json` — 42 pozycje, obiekt `{"pagination":{…},"newses":[…]}`. Pola pozycji: `id` (liczba), `title`, `shortcut`, `content` (tekst z `\r\n`), `rso_alarm` (`"0"`/`"1"`), `valid_from`, `valid_to`, `created_at`, `updated_at` (`"YYYY-MM-DD HH:MM:SS"`, czas polski), zbłąkane klucze `"0"`…`"3"`, `provinces` = **obiekt** kluczowany id: `{"1":{"id":"1","name":"dolnośląskie","city":"","slug_name":"dolnoslaskie"}}`. Zawiera `id 23521833`, `title "Alert RCB"`, `valid_to "2026-11-22 23:59:00"`, `updated_at "2026-11-19 17:07:27"`, treść 326 znaków. Kategorii w pozycji **nie ma**.
- `rso/wszystkie-informacje-drogowe-2026-11-22.json` (5 pozycji), `rso/wszystkie-atak-z-powietrza-2026-11-22-pusto.json` i `rso/lodzkie-ogolne-2026-11-22-pusto.json` (`"newses":[]`).
- `swpc/noaa-scales-2026-11-22.json` — klucze `"-1"`, `"0"`…`"3"`; `"0"`: `{"DateStamp":"2026-11-22","TimeStamp":"14:59:00","R":{"Scale":"0",…},"S":{"Scale":"0",…},"G":{"Scale":"0","Text":"none"}}`. Skale to **napisy** albo `null`.
- `gios/getWarnings-2026-11-22.json` — tablica 18 ostrzeżeń, wszystkie `PI`/`PM10`. Pola: `id`, `param.paramCode` (`"PM10"`), `param.paramName`, `standard.standardTypeCode` (`"PI"`/`"PA"`), `zone.zoneCode`, `zone.zoneName`, `sensor.stationId`, `recommendedPrecaution` (tekst z `\n- `), `active`. Strefa `PL1801` („miasto Rzeszów”) ma jedno ostrzeżenie (`id 32364`, stacja `17179`).
- `gios/findAll-size500-2026-11-22.json` — `{"Lista stacji pomiarowych":[…],"totalPages":1,…}`, 288 stacji; klucze polskie ze spacjami: `"Identyfikator stacji"`, `"Nazwa stacji"`, `"WGS84 φ N"`, `"WGS84 λ E"` (**napisy**, np. `"50.060381"`), `"Powiat"`, `"Województwo"`. Stacja 17179: Rzeszów, ul. Starzyńskiego, 50.060381 N, 21.980511 E.
- `emsc/pl-bbox-2026-11-22.json` — GeoJSON `FeatureCollection`, 20 zdarzeń; `properties`: `unid`, `time` (`"2026-11-22T12:26:38.654Z"`), `lat`, `lon`, `depth`, `mag` (liczba), `magtype`, `flynn_region` (`"POLAND"`, jedno `"HUNGARY"`), `auth`. Pierwsze: `20261122_0000158`, M2.1, 51.5552 N 16.1286 E.

Warianty (podniesiona skala, PA, inna magnituda, brak pola) testy robią w kodzie na `serde_json::Value`, jak w etapie 8.

## Decyzje etapu 11a

Decyzje etapu 8 (R1–R17) obowiązują dalej; nowe mają numery A.

- **A1** `Needs { Area, Position, Nothing }` w `aars_core::module`; pole `needs` w `ModuleDescriptor` (moduły nie-źródła: `Nothing`). Runner pomija cykl: `Area` + pusty obszar → `"brak obszaru"`; `Position` + brak pozycji → `"brak pozycji stacji"`. Panel (`sources_page::health`) robi to samo z deskryptora.
- **A2** Zakres R9: `DataSource::scope(&self, ctx: &SourceCtx) -> String`, domyślnie `ctx.area_text.clone()`. Runner porównuje i zapisuje **zakres** tam, gdzie dziś `cfg.area_text`. Dla `imgw-meteo` nic się nie zmienia.
- **A3** Pusty `cancel_text` = bez komunikatu odwołania (oba miejsca w runnerze; `Unresolved` → `cancel_pending = false`).
- **A4** `SourceCtx` zyskuje `area_text: String` i `tracked_keys: HashSet<String>` (klucze aktywnych pozycji tego źródła, wypełnia runner przed `fetch`).
- **A5** Interwał: domyślne i zakresy w deskryptorach mieszczą się w `INTERVAL_RANGE` 60..=3600; `ModuleRepo::register` już zapisuje domyślne ustawień (`INSERT OR IGNORE`), więc runner bierze np. 600 dla GIOŚ bez zmian w `source_state.rs`.
- **A6** Rodzaje ustawień: `SettingKind::Decimal { min: f64, max: f64 }` (kanonicznie jedna cyfra po kropce: `"3.0"`, przyjmuje przecinek) i `SettingKind::List(&'static [(&'static str, &'static str)])` (wejście: wartości po przecinku albo powtórzone pole; kanonicznie w kolejności z listy, rozdzielone `,`; pusta lista to błąd). `SettingKind` przestaje być `Eq` (f64) — zostaje `PartialEq`.
- **A7** Wspólne: `crate::http` (przeniesione `get_limited` + `client(timeout) -> Result<reqwest::Client, SourceError>` z User-Agent `aars/{wersja}`), `crate::speech` (`clean_text`, `truncate_sentence`, `km_phrase`, `warsaw_hhmm`, `warsaw_to_utc` przeniesione z `imgw_meteo::speech` z reeksportem tam).
- **A8** Adresy w `aars.toml` `[sources]`: `rso_url`, `swpc_url`, `gios_url`, `emsc_url`, każdy z `#[serde(default = …)]`, ta sama walidacja http(s) co `imgw_meteo_url`.
- **A9–A13** = rozstrzygnięcia 9–13 ze specu (SWPC epizody, RSO priorytety i „test”, słowa kluczowe, odcisk RSO, regiony EMSC).

## Global Constraints

- Kierunek zależności bez zmian: `aars-web` nie zależy od `aars-sources`.
- Źródło pisze cyfry, `HH:MM` i przecinek dziesiętny (`3,4`); słowa robi normalizacja w `aars-audio` (obsługuje „trzy przecinek cztery”).
- Żadne `unwrap`/`expect`/indeksowanie na danych z sieci; nieznany schemat = `SourceError::Parse`.
- Żaden test nie dotyka sieci zewnętrznej, karty dźwiękowej ani GPIO; HTTP tylko wiremock na 127.0.0.1.
- User-Agent `aars/{CARGO_PKG_VERSION}`, nic więcej.
- Każde źródło: timeout 20 s; limity odpowiedzi: RSO 2 MiB na żądanie, SWPC 64 KiB, GIOŚ 2 MiB, EMSC 1 MiB.
- Nowe moduły wyłączone domyślnie (`default_enabled: false`).
- Teksty komunikatów i panelu po polsku. Atrybucja w treści: „Dane: NOAA SWPC.”, „Źródło: GIOŚ.”, „Dane: EMSC.”, RSO: „Komunikat RSO”.
- Nie uruchamiać `cargo fmt` na całym repo (tylko na zmienionych plikach, jeśli w ogóle). Commity bez żadnej atrybucji AI (`Co-Authored-By`, `Claude-Session`, „Generated with”).
- Zakończenie każdego zadania: `cargo test --workspace` zielone, `cargo clippy --workspace --all-targets -- -D warnings` czyste.

## Review Focus

- **Włączenie RSO przy kilkudziesięciu długo ważnych komunikatach** — nadaje tylko te z `updated_at` w ostatnich `max_age_h` (plus Alert RCB/atak). Test w Task 7.
- **Zmiana filtra (kategorie, słowa, próg, promień)** nie nadaje fałszywych odwołań — pozycje znikają po cichu (zakres R9). Testy w Task 2 (runner) i w zadaniach źródeł (`scope` zawiera filtr).
- **SWPC oscyluje wokół progu** (3 → 2 → 3 w 30 min) — jedno ogłoszenie, bez „wygasła”. Test w Task 4.
- **Dwa różne wstrząsy o tej samej magnitudzie** — dwa komunikaty. Test w Task 2 (runner, odcisk z kluczem) i Task 5.
- **Pozycja stacji usunięta przy włączonym źródle pozycyjnym** — cykl pominięty z powodem „brak pozycji stacji”, panel pokazuje to samo, nic nie jest odwoływane. Test w Task 2 i Task 1.

---

### Task 1: `Needs`, `Decimal`, `List` w `aars-core` i panelu

**Files:**
- Modify: `crates/aars-core/src/module.rs` (typy, `validate`, `builtin()` + testy)
- Modify: `crates/aars-web/src/modules_page.rs:70-90` (formularz dla `Decimal` i `List`), szablon modułów w `crates/aars-web/templates/` (znajdź użycie `"choice"`)
- Modify: `crates/aars-web/src/sources_page.rs:24-40` (`health` bierze `Needs`), `crates/aars-web/src/modules_page.rs:121`
- Modify: `crates/aars-sources/src/imgw_meteo/mod.rs` (`needs: Needs::Area` w `DESCRIPTOR`)

**Interfaces:**
- Produces: `pub enum Needs { Area, Position, Nothing }` (Copy, Eq, Debug); `ModuleDescriptor.needs: Needs`; `SettingKind::Decimal { min: f64, max: f64 }`, `SettingKind::List(&'static [(&'static str, &'static str)])`; `pub fn health(status: &SourceStatus, cfg: &SourceConfig, needs: Needs, has_position: bool, now: DateTime<Utc>) -> (&'static str, String)`.

- [ ] **Step 1: Testy (w `module.rs` i `sources_page.rs`)**

```rust
#[test] fn decimal_settings_accept_a_comma_and_store_one_decimal() {
    // Decimal{min:1.0,max:9.0}: "3" -> "3.0", "4,5" -> "4.5", " 2.0 " -> "2.0";
    // "0.5", "9.5", "abc" -> Err zawierający etykietę i "1,0–9,0"
}
#[test] fn list_settings_store_known_slugs_in_declared_order() {
    // List(&[("ogolne","Ogólne"),("atak-z-powietrza","Atak z powietrza"),("informacje-drogowe","Drogowe")])
    // "informacje-drogowe, ogolne" -> "ogolne,informacje-drogowe"; "ogolne,ogolne" -> "ogolne";
    // "" -> Err("…: wybierz co najmniej jedną"); "stany-wod" -> Err("…: nieznana wartość")
}
#[test] fn builtin_modules_need_nothing() { assert!(builtin().iter().all(|m| m.needs == Needs::Nothing)); }
// sources_page:
#[test] fn a_position_source_without_a_position_reports_it() {
    // health(status ok, cfg enabled + pusty obszar, Needs::Position, false, now).1 == "brak pozycji stacji"
    // health(..., Needs::Position, true, now).1 == "działa"  (pusty obszar nie przeszkadza)
    // health(..., Needs::Nothing, false, now).1 == "działa"
    // Needs::Area + pusty obszar -> "brak obszaru" (jak dziś)
}
```

- [ ] **Step 2: `cargo test -p aars-core module` i `-p aars-web sources_page` — FAIL (brak typów).**
- [ ] **Step 3: Implementacja.** `Decimal`: parsowanie po zamianie `,` na `.`, `format!("{:.1}")`; komunikat błędu z przecinkami (`1,0–9,0`). `List`: wejście z formularza może przyjść jako jedno pole z przecinkami (pola wyboru w szablonie składa ukryte pole albo handler łączy powtórzone klucze — wybierz jedno, opisz w raporcie). Panel: `Decimal` jako `<input inputmode="decimal">`, `List` jako pola wyboru. `has_position` w `sources_page`/`modules_page` z `PositionRepo::load()` (błąd → `false`). Wiersz „Obszar” na `/sources` tylko dla `Needs::Area`.
- [ ] **Step 4: Testy zielone; clippy.**
- [ ] **Step 5: Commit** `feat: wymagania źródła i ustawienia dziesiętne oraz listy w module`

### Task 2: Runner — `Needs`, zakres, brak odwołania; wspólne `http` i `speech`

**Files:**
- Modify: `crates/aars-sources/src/source.rs`, `runner.rs`, `lib.rs`
- Create: `crates/aars-sources/src/http.rs` (przeniesione z `imgw_meteo/http.rs`, ten plik usuń, importy popraw), `crates/aars-sources/src/speech.rs`
- Modify: `crates/aars-sources/src/imgw_meteo/{mod.rs,speech.rs}` (użycie wspólnych)

**Interfaces:**
- Consumes: `Needs` z Task 1.
- Produces:
  - `trait DataSource { fn needs(&self) -> Needs { Needs::Area }  fn scope(&self, ctx: &SourceCtx) -> String { ctx.area_text.clone() } … }`
  - `SourceCtx { now, area, area_text: String, position, settings, tracked_keys: HashSet<String> }`
  - `http::client(timeout: Duration) -> Result<reqwest::Client, SourceError>`; `http::get_limited(client, url, max) -> Result<Vec<u8>, SourceError>` (url może mieć query)
  - `speech::clean_text(raw: &str) -> String` (usuwa tagi `<…>`, dekoduje `&amp; &lt; &gt; &quot; &#39; &nbsp;` i `&#NNN;`, zamienia `\r\n`/`\n`/wiele spacji na jedną spację, usuwa wiodące `- ` list → `; `), `speech::truncate_sentence(text: &str, max_chars: usize) -> (String, bool)` (tnie na ostatnim `. `/`! `/`? ` przed limitem, inaczej na ostatniej spacji; `true` gdy ucięto), `speech::km_phrase(km: u32) -> String` („1 kilometr”, „2 kilometry”, „5 kilometrów”, „22 kilometry”, „12 kilometrów”, „112 kilometrów”), `speech::warsaw_hhmm(t: DateTime<Utc>) -> String` (`"14:05"`), `speech::warsaw_to_utc(NaiveDateTime) -> Option<DateTime<Utc>>`, `speech::haversine_km(a: (f64,f64), b: (f64,f64)) -> f64`, `speech::compass8(from: (f64,f64), to: (f64,f64)) -> &'static str` („na północ”, „na północny wschód”, „na wschód”, …, „na północny zachód”).

- [ ] **Step 1: Testy runnera** (fałszywe źródło jak istniejące testy w `runner.rs`):
  - `a_position_source_without_a_position_is_skipped` — `needs()==Position`, brak pozycji → `report.skipped == Some("brak pozycji stacji")`, `fetched == false`.
  - `a_source_that_needs_nothing_runs_without_an_area` — `Needs::Nothing`, pusty obszar → `fetched == true`, ogłasza pozycję.
  - `an_item_without_cancel_text_disappears_silently` — pozycja nadana (`done`), potem dwa cykle bez niej → `cancelled == 1`, kolejka nie ma nowego elementu.
  - `an_unaired_item_without_cancel_text_does_not_leave_a_pending_cancellation` — oryginał `transmitting` → po odwołaniu `cancel_pending == false`, późniejsze `settle_pending_cancellations` nic nie wstawia.
  - `a_scope_change_drops_items_silently` — `scope()` zwraca `"x|m=3.0"`, potem `"x|m=4.0"` → pozycja `Dropped`, zero odwołań.
  - `two_keys_with_distinct_fingerprints_are_two_announcements` — dwie pozycje `a|3.0`, `b|3.0` w jednym cyklu → `announced == 2`.
  - `tracked_keys_reach_the_source` — po ogłoszeniu `k1` następny `fetch` widzi `ctx.tracked_keys == {"k1"}`.
- [ ] **Step 2: Testy `speech`** z wartościami z Interfaces (`km_phrase` dla 1, 2, 5, 12, 22, 112; `clean_text("<p>A&amp;B</p>\r\n- x\n- y")` == `"A&B; x; y"`; `truncate_sentence` na tekście 3 zdań; `compass8((51.77,19.46),(51.5552,16.1286)) == "na zachód"`; `haversine_km` Łódź–Warszawa (52.2297, 21.0122) w 118–122).
- [ ] **Step 3: FAIL.**
- [ ] **Step 4: Implementacja.** Sprawdzenie `Needs` obok obecnego sprawdzenia obszaru (`runner.rs:150`), przed `settle_pending_cancellations`. `Needs::Area` zostaje jedynym przypadkiem „brak obszaru”. Zakres: `let scope = self.source.scope(&ctx);` — wszędzie gdzie `cfg.area_text` (porównanie R9 i `track`). Kolejność: kontekst (z `tracked_keys` z `active_items`) liczony przed porównaniem R9. Pusty `cancel_text` w gałęzi `Fate::Aired` i w `settle_pending_cancellations`.
- [ ] **Step 5: Zielone (w tym wszystkie dotychczasowe testy `imgw_meteo` i runnera); clippy.**
- [ ] **Step 6: Commit** `feat: runner dla źródeł bez obszaru, zakres źródła i zdarzenia bez odwołania`

### Task 3: Adresy źródeł w `aars.toml`

**Files:** `crates/aars-core/src/config.rs`, `packaging/etc/aars.toml` (zakomentowane przykłady), `README.md` (sekcja konfiguracji, jeśli wymienia `imgw_meteo_url`).

**Interfaces:** Produces `SourcesConfig { imgw_meteo_url, rso_url, swpc_url, gios_url, emsc_url }` z domyślnymi:
`https://komunikaty.tvp.pl/komunikatyxml`, `https://services.swpc.noaa.gov/products/noaa-scales.json`, `https://api.gios.gov.pl/pjp-api/v1/rest`, `https://www.seismicportal.eu/fdsnws/event/1/query`.

- [ ] **Step 1: Testy:** `a_partial_sources_block_keeps_the_other_defaults` (`[sources]\nimgw_meteo_url = "http://127.0.0.1:9/x"` → `rso_url` domyślne); `every_source_url_is_validated` (dla każdego klucza `"ftp://x"` → błąd z nazwą klucza).
- [ ] **Step 2: FAIL. Step 3: implementacja** (pętla po parach (nazwa, wartość) w `validate`). **Step 4: zielone. Step 5: Commit** `feat: adresy nowych źródeł w aars.toml`.

### Task 4: `swpc` — pogoda kosmiczna

**Files:** Create `crates/aars-sources/src/swpc/{mod.rs,parse.rs,speech.rs}`.

**Interfaces:**
- Consumes: Task 2 (`http`, `speech`, `Needs`), Task 3 (`swpc_url`).
- Produces: `pub const DESCRIPTOR`, `pub struct Swpc`, `Swpc::new(url) / with_timeout(url, t)`, `parse::parse_scales(body: &[u8]) -> Result<Scales, SourceError>` z `Scales { stamp: DateTime<Utc>, r: u8, s: u8, g: u8 }` (`null` → 0), `pub fn to_output(scales: &Scales, ctx: &SourceCtx, episodes: &mut Episodes) -> SourceOutput`.

Deskryptor: id `swpc`, nazwa „Pogoda kosmiczna NOAA”, `needs: Nothing`, ustawienia `g_min` (Integer 1–5, `"3"`), `s_min` (1–5, `"2"`), `r_min` (1–5, `"3"`), `interval_s` (60–3600, `"300"`).

Epizody (A9): `Episodes` = mapa skala → `{ peak: u8, below_since: Option<DateTime<Utc>> }` w `Mutex` w `Swpc`. Poziom ≥ próg: epizod (nowy albo trwający), `peak = max`, `below_since = None`. Poziom < próg przy trwającym epizodzie: `below_since` ustawione raz; pozycja dalej w wyjściu aż `now − below_since ≥ 3 h`, wtedy epizod usunięty. Pozycja: `key`/`topic` = `"G"`/`"S"`/`"R"`, `fingerprint` = `"G{peak}"`, `priority: High`, `interrupting: false`, `valid_until: None`, `scope` = `"g={g_min}|s={s_min}|r={r_min}"`.

Teksty (`swpc/speech.rs`), wzór dla G: `"Uwaga, pogoda kosmiczna. {przymiotnik} burza geomagnetyczna, poziom G{n} w skali NOAA. {skutki} Dane: NOAA SWPC."` — przymiotniki 1–5: Słaba, Umiarkowana, Silna, Bardzo silna, Ekstremalna (te same dla S „burza radiacyjna” i R „zaniki łączności radiowej” — dla R: „{przymiotnik} zaniki łączności radiowej, poziom R{n}”). Skutki: G1–G2 „Możliwe pogorszenie propagacji na pasmach KF, zwłaszcza na trasach północnych.”; G3+ dodaje „Możliwa zorza widoczna w Polsce i łączności przez zorzę na 6 i 2 metrach.”; R: „Zaniki łączności na pasmach KF po dziennej stronie Ziemi.”; S: „Możliwe zaniki łączności KF w rejonach polarnych.”. Zmiana: `"Wzrost poziomu. "` + tekst. Odwołanie: G „Burza geomagnetyczna wygasła. Dane: NOAA SWPC.”, S „Burza radiacyjna wygasła. …”, R „Zaniki łączności radiowej ustały. …”.

- [ ] **Step 1: Testy** (`now` = 2026-11-22T15:00:00Z):
  - `the_quiet_fixture_has_no_items` (fiks → 0 pozycji, `complete`).
  - `a_g3_above_threshold_is_one_high_item` (Value: `"0".G.Scale = "3"` → 1 pozycja `key "G"`, `fingerprint "G3"`, tekst zawiera „Silna burza geomagnetyczna, poziom G3” i „zorza” i kończy się „Dane: NOAA SWPC.”).
  - `below_threshold_is_ignored` (G2 przy `g_min` 3 → 0).
  - `the_settings_change_the_threshold_and_the_scope` (`g_min=2` → G2 ogłoszone; `scope` == `"g=2|s=2|r=3"`).
  - `an_episode_keeps_its_peak_and_lingers_three_hours` — sekwencja na tych samych `Episodes`: 15:00 G3 → `G3`; 15:10 G2 → nadal `G3` (pozycja jest); 15:20 G4 → `G4`; 15:30 G2 → `G4`; 18:29 G2 → jest; 18:31 G2 → brak pozycji.
  - `a_missing_scale_is_a_parse_error` (usuń `"0".G`), `a_missing_current_key_is_a_parse_error`, `an_old_stamp_is_a_parse_error` (stamp 2026-11-22 11:00 przy now 15:00 → `Parse`; 12:30 → ok).
  - HTTP (wiremock): fiks pobrany i sparsowany; 503 → `Status(503)`; `needs() == Nothing`.
- [ ] **Step 2: FAIL. Step 3: implementacja. Step 4: zielone. Step 5: Commit** `feat: źródło swpc — burze geomagnetyczne, radiacyjne i zaniki KF`.

### Task 5: `emsc` — wstrząsy

**Files:** Create `crates/aars-sources/src/emsc/{mod.rs,parse.rs,speech.rs}`.

**Interfaces:** Produces `DESCRIPTOR`, `Emsc::new(url)/with_timeout`, `parse::parse_events(body) -> Result<Vec<Quake>, SourceError>` (`Quake { unid, time: DateTime<Utc>, lat, lon, mag: f64, region: String }`; zdarzenie bez wymaganego pola pominięte + `skipped += 1`), `to_output(events, skipped, ctx) -> SourceOutput`, `fn query_url(base: &str, ctx: &SourceCtx) -> Option<String>`.

Deskryptor: id `emsc`, nazwa „Wstrząsy sejsmiczne (EMSC)”, `needs: Position`, ustawienia `min_mag` (Decimal 1,0–9,0, `"3.0"`), `radius_km` (Integer 10–1000, `"150"`), `interval_s` (60–3600, `"120"`).

URL: `{base}?format=json&lat={lat:.3}&lon={lon:.3}&maxradius={radius_km/111.195:.3}&minmag={min_mag}&start={now−6h, %Y-%m-%dT%H:%M:%S}&orderby=time&limit=100`. Lokalnie też filtr `mag >= min_mag` i odległość ≤ `radius_km`.

Pozycja: `key = unid`, `topic = "emsc:{unid}"`, `fingerprint = "{unid}|{krok}"` gdzie krok = `(mag*2).floor()/2` z jedną cyfrą (`3.4` → `"3.0"`, `3.5` → `"3.5"`), `valid_from = time`, `valid_until = time + 30 min`, `cancel_text = ""`, priorytet `mag >= 4.5` → `High`, inaczej `Normal`, bez przerywania. `scope` = `"{lat:.2},{lon:.2}|r={radius_km}|m={min_mag}"`.

Tekst: `"Wstrząs sejsmiczny o magnitudzie {mag z przecinkiem, 1 miejsce}, {km_phrase} {compass8} od stacji{, region}, o godzinie {warsaw_hhmm}. Dane: EMSC."`; region (A13): `POLAND`→„Polska”, `CZECH REPUBLIC`/`CZECHIA`→„Czechy”, `SLOVAKIA`→„Słowacja”, `GERMANY`, `SOUTHERN GERMANY`, `NORTHERN GERMANY`→„Niemcy”, `UKRAINE`→„Ukraina”, `BELARUS`→„Białoruś”, `LITHUANIA`→„Litwa”, `KALININGRAD REGION, RUSSIA`→„obwód kaliningradzki”, `BALTIC SEA`→„Morze Bałtyckie”, `HUNGARY`→„Węgry”; inne — bez regionu. Zmiana: `"Poprawka danych wstrząsu. "` + tekst. Podsumowanie: `"M3,5, 120 km, Polska"`.

- [ ] **Step 1: Testy** (pozycja stacji 51.77 N 19.46 E):
  - `the_fixture_parses_twenty_events`.
  - `only_recent_strong_close_events_become_items` — now 2026-11-22T12:40:00Z, `min_mag 2.0`, `radius 300` → 1 pozycja `20261122_0000158`; tekst == `"Wstrząs sejsmiczny o magnitudzie 2,1, {km_phrase(..)} na zachód od stacji, Polska, o godzinie 13:26. Dane: EMSC."` (odległość policz w teście tą samą funkcją; asercja na resztę napisu); `valid_until` 12:56:38.654Z; now 13:00 → 0 pozycji (wygasło, runner nie śledzi).
  - `min_mag_and_radius_filter_locally` (`min_mag 3.0` → 0 dla tego zdarzenia; `radius 100` → 0).
  - `the_fingerprint_steps_by_half_a_magnitude` (3.3 i 3.4 → `"…|3.0"`; 3.5 → `"…|3.5"`).
  - `an_unknown_region_is_not_spoken` (`flynn_region "ROMANIA"` → tekst bez regionu, „od stacji, o godzinie”).
  - `the_query_url_carries_position_radius_and_threshold` (`maxradius=1.349` dla 150 km, `minmag=3.0`, `start=2026-11-22T06:40:00`).
  - `no_features_key_is_a_parse_error`; HTTP: zapytanie z parametrami trafia w wiremock (`query_param`), 503 → `Status`.
- [ ] **Step 2–5** jak wyżej. **Commit** `feat: źródło emsc — wstrząsy w promieniu stacji`.

### Task 6: `gios-smog` — alerty smogowe

**Files:** Create `crates/aars-sources/src/gios/{mod.rs,parse.rs,speech.rs}` (moduł `gios`, id `gios-smog`).

**Interfaces:** Produces `DESCRIPTOR`, `Gios::new(base)/with_timeout`, `parse::parse_stations(body) -> Result<(Vec<Station>, u32 /*totalPages*/), SourceError>` (`Station { id: i64, name: String, lat: f64, lon: f64 }`; współrzędne z napisów), `parse::parse_warnings(body) -> Result<(Vec<Warning>, usize /*skipped*/), SourceError>` (`Warning { zone_code, zone_name, level: String /*PI|PA|…*/, param_code, param_name, station_id: i64, precaution: String }`), `to_output(warnings, skipped, stations, ctx) -> SourceOutput`.

Deskryptor: id `gios-smog`, nazwa „Alerty smogowe GIOŚ”, `needs: Position`, `radius_km` (Integer 5–100, `"25"`), `interval_s` (300–3600, `"600"`).

Stacje: `{base}/station/findAll?size=500&page={n}` dla `n` w `0..totalPages` (sufit 10 stron), w pamięci z czasem pobrania; odświeżenie po 24 h; porażka odświeżenia przy starej kopii → stara kopia + `tracing::warn!`; porażka bez kopii → błąd cyklu. Ostrzeżenia: `{base}/levels/getWarnings`.

Pozycja na `(zone_code, level, param_code)` dla ostrzeżeń ze stacji w promieniu: `key = topic = "{zone_code}|{level}|{param_code}"`, `fingerprint` = to samo, `priority` PA → `High`, reszta `Normal`, bez przerywania, `valid_until: None`. `scope` = `"{lat:.2},{lon:.2}|r={radius_km}"`. Tekst: `"Alert smogowy GIOŚ, {zone_name}. Przekroczony poziom {informowania|alarmowy} dla {param_name}. {precaution oczyszczone, truncate 500}{ ' Pełne zalecenia na stronie GIOŚ.' gdy ucięte} Źródło: GIOŚ."` (`level` inny niż PI/PA → „przekroczony poziom {standardType z odpowiedzi}” — pole `standard.standardType`, dodaj do `Warning`). Odwołanie: `"Alert smogowy dla {zone_name} odwołany. Źródło: GIOŚ."`. Podsumowanie `"PM10, poziom informowania, miasto Rzeszów"`.

- [ ] **Step 1: Testy:**
  - `the_station_list_parses_with_string_coordinates` (288 stacji; 17179 → 50.060381, 21.980511).
  - `warnings_near_the_station_become_one_item_per_zone` — pozycja stacji = Rzeszów 50.04 N 22.00 E, promień 10 km → dokładnie 1 pozycja `key "PL1801|PI|PM10"`, tekst zaczyna się `"Alert smogowy GIOŚ, miasto Rzeszów. Przekroczony poziom informowania dla pył zawieszony PM10."` i kończy `"Źródło: GIOŚ."`, nie zawiera `"\n"`.
  - `a_wide_radius_merges_stations_of_one_zone` — Rzeszów, 100 km → pozycje unikalne po kluczu (strefa podkarpacka `PL1802` raz, choć 4 ostrzeżenia).
  - `pa_is_high_priority` (Value: `standardTypeCode = "PA"`).
  - `the_station_list_is_fetched_once_a_day` (wiremock `expect(1)` na findAll przy dwóch `fetch` w odstępie < 24 h; trzeci z `now + 25 h` → drugie pobranie).
  - `a_failed_refresh_keeps_the_old_station_list`; `a_failed_first_station_fetch_is_an_error`.
  - `a_warning_for_an_unknown_station_is_ignored` — stacja spoza listy to nie błąd danych (lista bywa niepełna): ostrzeżenie pominięte, `complete` bez zmian.
- [ ] **Step 2–5.** **Commit** `feat: źródło gios-smog — alerty smogowe w promieniu stacji`.

### Task 7: `rso` — Regionalny System Ostrzegania

**Files:** Create `crates/aars-sources/src/rso/{mod.rs,parse.rs,speech.rs}`.

**Interfaces:** Produces `DESCRIPTOR`, `Rso::new(base)/with_timeout`, `pub const VOIVODESHIPS: [(&str /*kod TERYT*/, &str /*slug*/, &str /*„województwo łódzkie”*/); 16]` (`02` dolnoslaskie, `04` kujawsko-pomorskie, `06` lubelskie, `08` lubuskie, `10` lodzkie, `12` malopolskie, `14` mazowieckie, `16` opolskie, `18` podkarpackie, `20` podlaskie, `22` pomorskie, `24` slaskie, `26` swietokrzyskie, `28` warminsko-mazurskie, `30` wielkopolskie, `32` zachodniopomorskie), `parse::parse_news(body) -> Result<(Vec<News>, usize), SourceError>` (`News { id: i64, title, shortcut, content, valid_until: Option<DateTime<Utc>>, updated_at: Option<DateTime<Utc>>, province_slugs: Vec<String> }`), `to_output(batches: Vec<(Category, Vec<News>)>, skipped, failed_requests: usize, ctx) -> SourceOutput`.

Deskryptor: id `rso`, nazwa „Regionalny System Ostrzegania (RSO)”, `needs: Area`, ustawienia: `area` (Area, `""`, pomoc: jak imgw, „województwo z dwóch pierwszych cyfr kodu”), `categories` (List: `ogolne` Ogólne (Alert RCB, sanepid, WCZK), `atak-z-powietrza` Atak z powietrza, `informacje-drogowe` Informacje drogowe, `meteorologiczne` Meteorologiczne (dubluje IMGW), `hydrologiczne` Hydrologiczne; domyślnie `"ogolne,atak-z-powietrza"`), `keywords` (Text max 300, `""`), `max_age_h` (Integer 1–720, `"24"`), `interval_s` (120–3600, `"180"`).

Pobieranie: dla każdego unikalnego województwa obszaru × każdej kategorii: `{base}/{slug}/{kategoria}/0?_format=json`. Błąd jednego żądania → reszta, `complete = false`; wszystkie zawiodły → pierwszy błąd jako `Err`. Duplikaty `id` między kategoriami — pierwsze wystąpienie (runner i tak deduplikuje po kluczu).

Filtr i priorytet:
- `alarm` = `title` zawiera „Alert RCB” albo kategoria `atak-z-powietrza`; `test` = tytuł lub treść zawiera „test” albo „ćwicz” (bez wielkości liter).
- `keywords` niepuste → nie-`alarm` musi zawierać któreś słowo (bez wielkości liter) w `title`/`shortcut`/`content`.
- Nieśledzone (`!ctx.tracked_keys.contains(key)`) i nie-`alarm` z `updated_at` starszym niż `max_age_h` → pominięte (brak `updated_at` → nie pomijać).
- Priorytet: `alarm && !test` → `Emergency` + `interrupting`; reszta `Normal`, bez przerywania.

Pozycja: `key = id.to_string()`, `topic = "rso:{id}"`, `fingerprint = "{title}|{sha256 hex 16 znaków z clean_text(content) pierwszych 2000 znaków}"`, `valid_until` z `valid_to` (Warszawa → UTC; brak → `None`), `scope = "{area_text}|cat={categories}|kw={keywords}"`.
Tekst: `"Komunikat RSO, {województwo łódzkie}. {title}. {clean_text(content) przez truncate_sentence 600}{ ' Pełna treść na stronie RSO.' gdy ucięte}"`; Alert RCB: zaczyna się `"Alert RCB, województwo dolnośląskie. "` zamiast `"Komunikat RSO, … Alert RCB."`. Zmiana: `"Zmiana komunikatu RSO. "` + tekst. Odwołanie: `"Komunikat RSO „{title}” nie obowiązuje."`. Podsumowanie: tytuł (do 80 znaków).

- [ ] **Step 1: Testy** (`now` = 2026-11-22T15:00:00Z, obszar `02`, kategorie domyślne, `ogolne` = fiks `wszystkie-ogolne`, `atak` = fiks pusty; w testach `to_output` podajesz paczki bezpośrednio):
  - `the_quirky_json_parses` (42 pozycje; `23521833` ma `province_slugs == ["dolnoslaskie"]`, `valid_until` 2026-11-22T22:59:00Z).
  - `an_alert_rcb_is_an_interrupting_emergency_even_when_old` — `updated_at` 19.11 (starsze niż 24 h) i tak ogłoszone; `priority Emergency`, `interrupting`; tekst zaczyna się `"Alert RCB, województwo dolnośląskie. Na wniosek wojewody dolnośląskiego"`.
  - `old_untracked_items_are_skipped_but_tracked_ones_stay` — dowolna nie-alarmowa pozycja dolnośląska z `updated_at` > 24 h: pominięta; ta sama z kluczem w `tracked_keys`: obecna.
  - `a_test_alert_does_not_interrupt` (Value: tytuł „Alert RCB – test” → `Normal`, bez przerywania).
  - `keywords_filter_everything_but_alarms` (`keywords "Pieńsk"` → Alert RCB zostaje; `keywords "xyz"` → nadal Alert RCB, żadna inna).
  - `only_items_of_the_area_voivodeships_are_kept` (fiks `wszystkie-ogolne` przy obszarze `10` → tylko pozycje z `lodzkie` w `province_slugs`; endpoint `wszystkie` zwraca cały kraj, więc filtr po `province_slugs` jest konieczny i tani).
  - `long_content_is_cut_at_a_sentence` (Value: treść 2000 znaków → tekst ≤ 700 znaków, kończy się „Pełna treść na stronie RSO.”).
  - `the_fingerprint_ignores_validity` (zmiana `valid_to` → ten sam odcisk; zmiana treści → inny).
  - `a_missing_valid_to_is_open_ended` (`valid_to: null` → `valid_until None`, `complete`).
  - `the_scope_carries_the_filters`.
  - HTTP: obszar `10\n1061 Łódź` → dokładnie jedno żądanie na kategorię (`/lodzkie/ogolne/0`, `/lodzkie/atak-z-powietrza/0`, `query_param("_format","json")`); jedna kategoria 503 → wynik z drugiej, `complete == false`; obie 503 → `Err(Status(503))`; pusty fiks → 0 pozycji, `complete`.
- [ ] **Step 2–5.** **Commit** `feat: źródło rso — Alert RCB, komunikaty wojewódzkie i sanepidu`.

### Task 8: Rejestracja, panel i kryterium wyjścia

**Files:** `crates/aars-sources/src/lib.rs` (`modules()` + test), `crates/aars-web/src/role_matrix.rs` (wiersze `/sources/{id}/enabled`, `/sources/{id}/refresh` OPERATOR, `/modules/{id}/enabled`, `/modules/{id}/settings` ADMIN dla czterech id — wzór `imgw-meteo` w liniach 62–75 i 319–337), `crates/aars/src/main.rs` (tylko jeśli `aars source preview` da się tanio uogólnić na `modules()`; inaczej bez zmian — zapisz decyzję w raporcie), `README.md` (lista źródeł i licencje/atrybucje), `docs/superpowers/specs/2026-10-13-aars-design.md` §13 pkt 11 (dopisek: podział na 11a/11b, link do projektu 11a).

**Interfaces:** Consumes wszystkie `DESCRIPTOR` i konstruktory z Task 4–7.

- [ ] **Step 1: Testy:**
  - `all_sources_are_registered_in_order` (`modules()` ids == `["imgw-meteo","rso","swpc","gios-smog","emsc"]`; każde `build(&Config::default())` daje źródło o tym samym id i `needs()` == `descriptor.needs`; wszystkie `default_enabled == false`).
  - `every_descriptor_interval_fits_the_runner_range` (każde `interval_s` Integer min ≥ 60, max ≤ 3600, default w zakresie).
  - Macierz ról: nowe wiersze przechodzą istniejący test macierzy.
  - Test integracyjny w `crates/aars/` (wzór testu wyjścia etapu 8 — znajdź `imgw` w `crates/aars/tests/` albo `station.rs` testach): stacja z wiremock dla `swpc` (G3) i `emsc` (świeże zdarzenie), oba włączone, pozycja ustawiona → po jednym cyklu w kolejce dwa elementy z oczekiwanymi priorytetami; nic nie kluczuje (fałszywe wyjście/PTT jak w istniejących testach).
- [ ] **Step 2–4.** Zielone całe `cargo test --workspace`, clippy czyste.
- [ ] **Step 5: Commit** `feat: rejestracja źródeł alarmowych, macierz ról i dokumentacja etapu 11a`.
