# Etap 9: panel admina — sprzęt, pozycja stacji, moduły

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Administrator na świeżej instalacji (w `aars.toml` tylko ścieżki) ustawia z panelu backend PTT, urządzenie audio, głos, adres panelu, pozycję stacji na mapie i włączone moduły, sprawdza PTT i audio testem, który nie nadaje dłużej niż 5 s (test audio nie nadaje wcale), i restartuje stację jednym przyciskiem, nigdy w trakcie nadania; każda zmiana trafia do dziennika audytu.

**Architecture:** Ustawienia sprzętu żyją w tabeli `settings` pod kluczami `hw:*` i nakładają się na `aars.toml` (`aars_core::hardware`). Binarka przy każdym (re)starcie składa kandydatów konfiguracji (panel → ostatnia działająca → plik) i bierze pierwszego, z którego da się zbudować stos nadawczy. Gdy żaden się nie zbuduje, stacja działa bez nadawania, ale z panelem. Restart jest w procesie: dyspozytor dokańcza bieżące nadanie, zamyka sekwencer (join wątku), dopiero potem wszystko startuje od nowa. Testy PTT i audio idą kanałem sterującym dyspozytora do sekwencera, więc sekwencer dalej jest jedynym właścicielem PTT i audio (D2), a interlocki i watchdog obejmują test. Moduły to opisy (`ModuleDescriptor`: id, rodzaj, przełącznik, schemat ustawień) w rejestrze; źródło danych z etapu 11 dokłada opis i `DataSource`, a panel generuje formularz ze schematu. `aars-web` dalej zależy tylko od `aars-core` (spec §3, D6): host odpytuje binarka przez trait `HardwareProbe`, a stacją steruje przez trait `StationControl`.

**Tech Stack:** axum 0.8 / askama 0.16 / HTMX (panel), sqlx/SQLite, cpal 0.18 (lista wyjść), serialport 4.10 (`available_ports`), gpiocdev 0.8 (tylko Linux, odczyt `chip.info()`), Leaflet 1.9.4 zwendorowany w `assets/vendor/leaflet/` (BSD-2-Clause), podkład `tile.openstreetmap.org`.

**Spec:** `docs/superpowers/specs/2026-10-13-aars-design.md` (§2 D2/D6, §3, §4.3–4.5, §5 `settings`/`audit_log`/`tx_log`, §8 role, §9 konfiguracja, §10, §11, §13 pkt 9)

## Słowa właściciela (2026-11-20)

„Panel admina tak, aby móc ustawiać zmienne środowiskowe jak sposób wywołania itp. Znak stacji itp. Dodatkowo jakie funkcje uruchamiamy — coś jak system pluginów”; „API pogodowe… koordynaty w panelu. Dodatkowo mapka”.

## Decyzje etapu 9

- **R1 Pierwszeństwo:** wartości domyślne < `aars.toml` < `AARS_*` < panel (`settings`, klucze `hw:*`). Brak klucza `hw:x` znaczy „bez nadpisania”. Klucz z wartością, której nie da się odczytać, wraca do wartości z pliku, a opis błędu trafia do stanu zastosowania (R6).
- **R2 Co ustawia panel** (`HW_KEYS`, 13 kluczy): `server.bind`, `audio.device`, `audio.sample_rate`, `audio.gain_db`, `ptt.backend`, `ptt.tx_tail_ms`, `ptt.gpio.chip`, `ptt.gpio.line`, `ptt.gpio.active_low`, `ptt.gpio.inhibit_line`, `ptt.serial.port`, `ptt.serial.line`, `tts.model`. **Tylko w pliku (rozruch):** `storage.*`, `tts.piper_bin`, nowe `tts.voices_dir` (domyślnie `/var/lib/aars/voices`), `sources.imgw_meteo_url`. Ścieżek wykonywalnych i katalogów nie zmienia się z panelu, bo to droga do uruchomienia dowolnego programu.
- **R3 Co działa od razu, a co po restarcie.** Od razu: wyłącznik nadawania, okno ciszy (≤ 30 s), włączanie modułów (źródło od następnego cyklu, harmonogram od następnego tiku), ustawienia modułów (następny cykl, zapis budzi źródło), pozycja stacji (następny cykl modułu). Po restarcie: cały sprzęt z R2 oraz to, co już w etapie 7 było po restarcie (znak, tryb i odstęp ID, limit TX, `misfire_grace_s`). Panel pokazuje listę „czeka na restart” jako różnicę między tym, co zapisane, a tym, co działa.
- **R4 Restart w procesie, nigdy w trakcie nadania.** `POST /station/restart` (admin) ustawia żądanie. Dyspozytor przestaje brać zadania, dokańcza bieżące nadanie (najwyżej limit TX, a strażnik działa dalej), zamyka sekwencer z joinem wątku i dopiero wtedy reszta zadań się zatrzymuje, a `run_once` zwraca `Exit::Restart`. Dzięki joinowi stare PTT i karta audio są zwolnione, zanim nowe zostaną otwarte. Ctrl-C dalej przerywa bieżące nadanie (PTT w dół w ~150 ms) i też czeka na join.
- **R5 Kandydaci konfiguracji:** panel → ostatnia działająca (`hw_good:*`, zapisywana po udanym zbudowaniu stosu z ustawień panelu) → sam plik. Kandydaci identyczni są scalani. Pierwszy, z którego zbudują się PTT i wyjście audio, wygrywa.
- **R6 Nieudane zastosowanie:** zapisane w panelu wartości zostają, żeby admin widział, co wpisał. Stan zastosowania trafia do kluczy `hw_state:origin` / `hw_state:error`, panel pokazuje go na stronie sprzętu i na pulpicie, a do audytu idzie wpis `hardware_apply_failed` (bez użytkownika), z błędem każdego kandydata. Gdy nie zbuduje się żaden kandydat, stacja działa **bez nadawania**: panel, harmonogram i źródła pracują, kolejka czeka, dyspozytora nie ma, a pulpit każdej roli pokazuje `Nadawanie niedostępne: {błąd}`. Gdy `bind` z panelu się nie uda, panel wraca na `bind` z pliku. Gdy i ten zawiedzie, start kończy się błędem.
- **R7 Ratunek bez panelu:** `aars hardware show` wypisuje konfigurację efektywną z pochodzeniem każdego klucza, a `aars hardware reset` kasuje `hw:*`, `hw_good:*` i `hw_state:*`. Zmienne `AARS_*` nie przebijają panelu, bo stacja ma jedno źródło prawdy.
- **R8 Sprawdzanie na hoście bez dotykania sprzętu.** Wolno tylko wylistować: wyjścia cpal (nazwy jak w `CpalSink::open`), `serialport::available_ports()` i `gpiocdev` `chip.info()` (nazwa, etykieta, liczba linii; bez żądania linii). **Portu szeregowego nie otwieramy przy walidacji**, bo otwarcie tty na Linuksie podnosi DTR/RTS, czyli kluczuje radio. Reguły: `default` jest zawsze poprawnym urządzeniem audio. Gdy listę udało się pobrać, wartość spoza niej to błąd z listą dostępnych. Gdy listy nie udało się pobrać, wartość jest przyjmowana z ostrzeżeniem `nie sprawdzono: {powód}`. Pola GPIO są sprawdzane tylko przy backendzie GPIO, a pola portu szeregowego tylko przy backendzie szeregowym. Model głosu musi być na liście `list_voices(voices_dir)` (plik `*.onnx` z `*.onnx.json` obok) albo być istniejącym plikiem z `.onnx.json`.
- **R9 Zakresy:** `bind` to `SocketAddr` z portem ≥ 1024 (usługa działa bez roota). `sample_rate` ∈ {8000, 16000, 22050, 32000, 44100, 48000}. `gain_db` (poziom szczytowy, dBFS) w zakresie −30…0, przecinek dziesiętny dozwolony. `tx_tail_ms` 0–2000. Linia GPIO 0–511 i mniejsza niż liczba linii układu. Linia blokująca różna od linii PTT. Port szeregowy zaczyna się od `/dev/`. Model kończy się na `.onnx`. Urządzenie audio ma 1–128 znaków, bez znaków sterujących.
- **R10 Test PTT:** tylko admin, wymaga pola `confirm=tak` (checkbox „Rozumiem, że radio wejdzie na nadawanie”) i czasu 1–5 s (domyślnie 2). Idzie przez dyspozytor do sekwencera jako `TransmitRequest { id: TEST_TX_ID (-1), priority: Low, body: cisza, id_audio: None }`, więc sprawdzane są wszystkie interlocki w kolejności z §4.4: blokada strażnika, wyłącznik, wejście blokujące i okno ciszy (`Low` w ciszy dostaje odmowę). Watchdog jest oznaczany jak przy każdym nadaniu. Wpis w `tx_log` ma `queue_item_id = NULL` i notkę `test PTT: {użytkownik}`. Bez identyfikacji, bo test trwa ≤ 5 s, mniej niż najmniejszy limit TX (10 s). Gdy stacja nadaje albo trwa inny test, odpowiedź `Busy` przychodzi od razu.
- **R11 Test audio:** gra ton 1 kHz przez 0,5 s na −6 dBFS, potem 0,3 s ciszy, potem syntezę `Test audio stacji.` (z wzmocnieniem i bieżącym głosem) na **skonfigurowanym** urządzeniu, przez sekwencer (`SeqCmd::PlayLocal`), który woła `unkey()` i nigdy `key()`. Odtwarzanie jest ucięte po 15 s. Gdy synteza zawiedzie, gra sam ton, a wynik to mówi. Panel ostrzega: „Upewnij się, że radio nie jest ustawione na VOX”.
- **R12 VOX:** przy backendzie `vox` oba testy odmawiają (`Unsupported`). Test PTT nie ma czego sprawdzić, a test audio nadałby na antenę. Treść odmowy wskazuje `/compose` jako właściwy test, bo tam nadanie idzie z identyfikacją.
- **R13 Moduły:** rejestr opisów `ModuleDescriptor` w `aars-core` (czyste dane). Opis źródła żyje w crate źródeł (`aars_sources::modules()`), a binarka składa rejestr z `module::builtin()` i źródeł. Przełącznik ma trzy postacie: `Switchable`, `AlwaysOn(powód)` albo `Planned(etap)`. Wbudowane: `scheduler` (przełączany, domyślnie włączony, klucz `module:scheduler:enabled`), `auto-id` (zawsze włączony: „identyfikacja stacji jest obowiązkowa”, to niezmiennik z etapu 3), `bulletin` (`Planned("etap 11")`). Źródło `imgw-meteo` jest przełączane flagą `source_state.enabled`, a jego ustawienia mają dotychczasowe klucze `source:{id}:area` i `source:{id}:interval_s`. Moduły inne niż źródła używają `module:{id}:{klucz}`. Przy starcie `ModuleRepo::register` wstawia brakujące wiersze i domyślne ustawienia (`INSERT OR IGNORE`), więc moduł z etapu 11 nie potrzebuje własnej migracji.
- **R14 Wyłączenie modułu nie kasuje kolejki:** elementy już wstawione przez źródło czy harmonogram dalej czekają, a operator może je anulować w `/queue`. Wyłączony harmonogram nie zapisuje `schedule_fires`. Po ponownym włączeniu zaległości ocenia polityka spóźnień (§6), więc nie ma lawiny.
- **R15 Role:** `/settings`, `/settings/hardware`, wszystkie POST-y sprzętu, testów, pozycji, restartu i modułów: tylko **admin**. `GET /modules`: **operator**, przy czym operator widzi tylko stan, bez formularzy. Wyjątek z etapu 8 R14 zostaje: włączanie i odświeżanie źródła na `/sources` jest dostępne operatorowi (zatrzymanie źródła nie może wymagać admina, jak wyłącznik). Ustawienia źródła (obszar, interwał) przenoszą się z `/sources` do `/modules`: trasa `POST /sources/{id}/settings` znika, a `/sources` pokazuje obszar tylko do odczytu i, adminowi, link.
- **R16 Audyt:** `hardware` (szczegół: `klucz: stara → nowa` dla każdej zmiany; bez zmian nie ma wpisu), `test_ptt` (`2 s: {wynik}`), `test_audio` (`{wynik}`), `station_restart`, `position` (`51.77083, 19.45833 (JO91RS)` albo `usunięto`), `module_enabled` (`scheduler: wyłączony`), `module_settings` (`imgw-meteo: area, interval_s`) oraz systemowe `hardware_apply_failed`. Każdy wpis ma polską etykietę w `audit_action_label`.
- **R17 Pozycja:** kanonicznie `station:lat` i `station:lon` (stopnie dziesiętne, 5 miejsc), do tego `station:locator`. Formularz ma szerokość, długość i lokator. Gdy podane są obie współrzędne, wygrywają one, a lokator to wpisany lokator, jeśli zawiera ten punkt, w przeciwnym razie lokator 6-znakowy wyliczony z punktu. Gdy podany jest sam lokator (4, 6 albo 8 znaków, wielkość liter dowolna), pozycja to środek jego kwadratu. Wszystkie pola puste kasują pozycję. Lokator zapisujemy wielkimi literami.
- **R18 Mapa:** Leaflet 1.9.4 jest **zwendorowany** (`assets/vendor/leaflet/`, rust-embed), jak HTMX: panel ładuje się bez internetu, bez CDN w czasie działania. Podkład OSM z `https://tile.openstreetmap.org/{z}/{x}/{y}.png` z atrybucją „© OpenStreetMap” wymaga internetu przeglądarki, nie stacji. Bez podkładu formularz współrzędnych i lokatora działa w pełni, a skrypt pokazuje `Mapa niedostępna (brak internetu?) — wpisz współrzędne albo lokator.` Kliknięcie albo przeciągnięcie znacznika wpisuje szerokość i długość (5 miejsc) i czyści lokator, który serwer wylicza (R17). Lokatora nie przeliczamy w JS.
- **R19 Wzmocnienie i głos naprawdę działają:** dziś `audio.gain_db` nie jest nigdzie stosowane, a klucz cache nie zawiera modelu ani częstotliwości. Po zmianie głosu cache oddałby stary głos, a po zmianie `sample_rate` sekwencer dostałby `SampleRateMismatch`. Klucz cache dostaje `Synthesizer::identity()` (Piper: `piper|{model}|{rate}`), a wzmocnienie jest nakładane po cache w rendererze, w identyfikacji i w podglądzie.
- **R20 `SourceCtx` jako interfejs wtyczki:** dochodzą `position: Option<StationPosition>` i `settings: BTreeMap<String, String>` (ustawienia modułu bez prefiksu). Wypełnia je runner przy każdym cyklu, a `imgw-meteo` ich nie używa. Moduł z etapu 11 to: `DataSource` + `ModuleDescriptor` + wpis w `aars_sources::modules()`.

## Global Constraints

- Kierunek zależności (spec §3): `aars` → {`aars-web`, `aars-sources`, `aars-audio`, `aars-ptt`} → `aars-core`. `aars-web` nie zależy od `aars-sources`, `aars-audio` ani `aars-ptt`.
- Sekwencer jest jedynym właścicielem PTT i audio (D2). Żaden kod panelu ani binarki nie otwiera `CpalSink` ani backendu PTT poza `build_stack`.
- Niezmienniki TX z etapów 3/7/8 zostają: wyłącznik nadawania dostępny operatorowi i zawsze widoczny na pulpicie, limit TX w sekwencerze, watchdog w osobnym wątku z limitem `tx_max + tx_tail + 2 s`, lepka flaga `poisoned`, kolejność interlocków z §4.4, strażnik RAII opuszczający PTT. Test PTT jest zwykłym nadaniem dla wszystkich tych mechanizmów.
- Role (spec §8): viewer czyta, operator nadaje, planuje i anuluje, admin konfiguruje sprzęt, moduły i użytkowników.
- CSRF na każdym formularzu zmieniającym stan. Pola formularzy jako `String` z `#[serde(default)]`, żeby brak pola nie dawał 422 przed sprawdzeniem roli (macierz z etapu 7).
- Czasy w panelu w obu zapisach przez `chrome::both_times_now`. Wszystkie teksty panelu po polsku.
- Żaden test nie dotyka karty dźwiękowej, GPIO, portu szeregowego ani sieci (spec §11). Hosta odpytujemy tylko przez `FakeProbe`, sprzęt podmieniamy przez `FakeHardware`, a funkcji listujących host testy nie wywołują.
- Bez nowych zależności poza `tempfile` (dev, listowanie głosów) w `aars-core` i `tower` (dev) w `aars`. Leaflet to pliki, nie crate.
- TDD, commit po każdym zielonym cyklu. Zakończenie: `cargo test --workspace` zielone, `cargo clippy --workspace --all-targets -- -D warnings` czyste.

## Review Focus

- **Zmiana głosu albo częstotliwości przy pełnym cache:** po restarcie słychać nowy głos, a nadanie nie kończy się `SampleRateMismatch`. Testy w Task 5 (`changing_the_model_does_not_reuse_cached_audio`, `cached_audio_follows_the_sample_rate`).
- **Restart zlecony w trakcie długiego nadania:** nadanie kończy się `ok`, PTT opada raz, nowy stos nie powstaje przed joinem starego wątku sekwencera, drugie zadanie z kolejki nie zostaje wzięte przez stary dyspozytor. Testy w Task 6 (`drain_finishes_the_current_transmission_and_takes_nothing_new`) i Task 7 (`a_restart_requested_mid_transmission_waits_for_the_unkey`).
- **Admin zapisuje port USB, który zniknął (albo urządzenie audio odłączone przy restarcie):** stacja wstaje na ostatniej działającej konfiguracji, błąd jest widoczny na pulpicie i w audycie, a zapisane wartości zostają. Testy w Task 7 (`a_failing_panel_config_falls_back_to_the_last_good_one_and_records_the_error`) i Task 8 (`the_page_shows_the_apply_error_and_the_origin`).
- **Test PTT przy wyłączonym nadawaniu, po interwencji strażnika albo w oknie ciszy:** odmowa z polskim powodem, PTT nie drgnie, brak wpisu `ok` w `tx_log`. Testy w Task 6 (`a_test_ptt_is_refused_by_the_kill_switch`, `a_test_ptt_is_refused_after_a_watchdog_intervention`) i Task 9 (`a_refusal_is_shown_with_its_reason`).
- **Lokator na granicach i dane z przecinkiem:** `51,759` jest przyjmowane, a `90`/`180` dają `RR99XX`, nie panikę ani znak spoza alfabetu. Testy w Task 3 (`edges_of_the_world_stay_inside_the_alphabet`, `a_decimal_comma_is_accepted`).

---

### Task 1: Nadpisania sprzętu z panelu (`aars_core::hardware`)

**Files:**
- Create: `crates/aars-core/src/hardware.rs`
- Modify: `crates/aars-core/src/lib.rs`, `crates/aars-core/src/config.rs` (`TtsConfig.voices_dir`), `crates/aars-core/src/settings.rs` (pomocnicze `kv_*`)

**Interfaces:**
- Produces (`settings.rs`, metody `SettingsRepo`): `kv_get(&self, key: &str) -> Result<Option<String>, DbError>`; `kv_put(&self, key: &str, value: &str) -> Result<(), DbError>` (upsert z `updated_at`); `kv_prefix(&self, prefix: &str) -> Result<BTreeMap<String, String>, DbError>` (klucze **bez** prefiksu); `kv_delete_prefix(&self, prefix: &str) -> Result<u64, DbError>`.
- Produces (`config.rs`): `TtsConfig.voices_dir: PathBuf`, domyślnie `/var/lib/aars/voices`; stary `aars.toml` bez tego pola dalej się wczytuje.
- Produces (`hardware.rs`):
```rust
pub const HW_PREFIX: &str = "hw:";
pub const HW_GOOD_PREFIX: &str = "hw_good:";
pub const HW_KEYS: [&str; 13] = ["server.bind", "audio.device", "audio.sample_rate", "audio.gain_db",
    "ptt.backend", "ptt.tx_tail_ms", "ptt.gpio.chip", "ptt.gpio.line", "ptt.gpio.active_low",
    "ptt.gpio.inhibit_line", "ptt.serial.port", "ptt.serial.line", "tts.model"];
pub const SAMPLE_RATES: [u32; 6] = [8000, 16000, 22050, 32000, 44100, 48000];

#[derive(Debug, Clone, PartialEq)]
pub struct HardwareSettings {
    pub bind: String, pub audio_device: String, pub sample_rate: u32, pub gain_db: f32,
    pub ptt_backend: PttBackendKind, pub tx_tail_ms: u64,
    pub gpio_chip: String, pub gpio_line: u32, pub gpio_active_low: bool, pub gpio_inhibit_line: Option<u32>,
    pub serial_port: String, pub serial_line: SerialPttLine, pub voice_model: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError { pub field: &'static str, pub message: String }

impl HardwareSettings {
    pub fn from_config(c: &Config) -> Self;
    pub fn apply_to(&self, base: &Config) -> Config;
    pub fn to_pairs(&self) -> BTreeMap<String, String>;          // klucze z HW_KEYS, bool "true"/"false", brak linii blokującej ""
    pub fn from_pairs(base: &Config, pairs: &BTreeMap<String, String>) -> (Self, Vec<String>);
    pub fn validate(&self) -> Result<(), Vec<FieldError>>;      // R9, bez hosta
    pub fn diff(&self, other: &Self) -> Vec<String>;            // ["ptt.backend: vox → gpio", …] w kolejności HW_KEYS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOrigin { Panel, LastGood, File, None }
impl ConfigOrigin { pub fn as_str(self) -> &'static str; pub fn parse(s: &str) -> Option<Self>; pub fn label(self) -> &'static str; }
// label: "ustawienia z panelu" | "ostatnia działająca konfiguracja" | "plik aars.toml" | "brak — nadawanie niedostępne"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyStatus { pub origin: ConfigOrigin, pub error: Option<String> }

pub struct HardwareRepo { /* pool */ }
impl HardwareRepo {
    pub fn new(pool: SqlitePool) -> Self;
    pub async fn overrides(&self) -> Result<BTreeMap<String, String>, DbError>;   // hw:*
    pub async fn last_good(&self) -> Result<BTreeMap<String, String>, DbError>;   // hw_good:*
    pub async fn save(&self, s: &HardwareSettings) -> Result<(), DbError>;        // wszystkie 13 kluczy, jedna transakcja
    pub async fn save_last_good(&self, pairs: &BTreeMap<String, String>) -> Result<(), DbError>;
    pub async fn clear(&self) -> Result<u64, DbError>;                              // hw:*, hw_good:*, hw_state:*
    pub async fn set_apply_status(&self, s: &ApplyStatus) -> Result<(), DbError>;  // hw_state:origin, hw_state:error ("" = brak)
    pub async fn apply_status(&self) -> Result<Option<ApplyStatus>, DbError>;
}
```
- Komunikaty `FieldError` (dokładnie): `adres panelu: podaj adres:port, np. 0.0.0.0:8080`; `port panelu poniżej 1024 wymaga uprawnień roota`; `częstotliwość próbkowania: 8000, 16000, 22050, 32000, 44100 albo 48000 Hz`; `poziom wyjścia: od −30 do 0 dBFS`; `opóźnienie PTT: 0–2000 ms`; `linia GPIO: 0–511`; `linia blokująca musi być inna niż linia PTT`; `port szeregowy: podaj ścieżkę /dev/…`; `model głosu: plik .onnx`; `urządzenie audio: 1–128 znaków bez znaków sterujących`.

- [ ] **Step 1: Testy** (`hardware.rs`, `settings.rs`, `config.rs`)
  - `pairs_round_trip_through_from_pairs` (`from_config(&Config::default())` → `to_pairs` → `from_pairs` daje to samo i pustą listę ostrzeżeń).
  - `panel_values_override_the_file` (pary `ptt.backend=gpio`, `ptt.gpio.line=22`, `audio.device=hw:1,0` → `apply_to(base)` ma `PttBackendKind::Gpio`, `22`, `hw:1,0`; `storage.db_path` i `tts.piper_bin` bez zmian).
  - `a_broken_stored_value_falls_back_to_the_file_and_is_reported` (`audio.sample_rate=abc` → `48000` i ostrzeżenie zawierające `audio.sample_rate`).
  - `validation_rejects_out_of_range_values` (każdy przypadek z R9 osobno, asercja na `field` i dokładny `message`; `bind=0.0.0.0:80` → port; `gain_db=0.5`; `inhibit = line`).
  - `diff_names_each_changed_key` (`vox → gpio` i `default → hw:1,0`, kolejność `HW_KEYS`).
  - `repo_saves_all_keys_and_clear_removes_them` (po `save` `overrides().len() == 13`; `clear` usuwa także `hw_good:*` i `hw_state:*`; `tx_enabled` nietknięte).
  - `apply_status_round_trips` (`LastGood` + błąd; potem `Panel` + `None`).
  - `config.rs`: `a_config_without_voices_dir_uses_the_default`.
- [ ] **Step 2: FAIL** (`cargo test -p aars-core hardware`)
- [ ] **Step 3: Implementacja** — `from_pairs` parsuje każdy klucz osobno: wartość z pary albo z `base` plus ostrzeżenie `"{klucz}: nieczytelna wartość „{v}”, używam {z pliku}"`.
- [ ] **Step 4: PASS** (`cargo test -p aars-core`)
- [ ] **Step 5: Commit**: `feat(core): ustawienia sprzętu z panelu nakładane na aars.toml`

---

### Task 2: Co jest na hoście (`HardwareProbe`, lista głosów)

**Files:**
- Create: `crates/aars-core/src/hardware/probe.rs` (zamień `hardware.rs` na `hardware/mod.rs`), `crates/aars-ptt/src/probe.rs`, `crates/aars/src/probe.rs`
- Modify: `crates/aars-audio/src/output.rs` (`output_device_names`), `crates/aars-ptt/src/lib.rs`, `crates/aars-core/Cargo.toml` (dev: `tempfile`)

**Interfaces:**
- Consumes: `HardwareSettings`, `FieldError` (Task 1).
- Produces (`aars_core::hardware::probe`):
```rust
#[derive(Debug, Clone, PartialEq, Eq)] pub struct GpioChipInfo { pub path: String, pub label: String, pub lines: u32 }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct VoiceFile { pub path: String, pub name: String } // name = nazwa pliku bez .onnx
pub trait HardwareProbe: Send + Sync {
    fn audio_outputs(&self) -> Result<Vec<String>, String>;
    fn serial_ports(&self) -> Result<Vec<String>, String>;
    fn gpio_chips(&self) -> Result<Vec<GpioChipInfo>, String>;
}
pub struct HostLists { pub audio: Result<Vec<String>, String>, pub serial: Result<Vec<String>, String>,
                       pub gpio: Result<Vec<GpioChipInfo>, String>, pub voices: Vec<VoiceFile> }
pub fn list_voices(dir: &Path) -> Vec<VoiceFile>;               // *.onnx z *.onnx.json obok, sort po name; brak katalogu → []
pub fn host_lists(probe: &dyn HardwareProbe, voices_dir: &Path) -> HostLists;
#[derive(Debug, Default, PartialEq, Eq)] pub struct HostCheck { pub errors: Vec<FieldError>, pub warnings: Vec<String> }
pub fn check_against_host(s: &HardwareSettings, lists: &HostLists) -> HostCheck;   // R8
#[cfg(any(test, feature = "testing"))]
pub struct FakeProbe { pub audio: Result<Vec<String>, String>, pub serial: Result<Vec<String>, String>, pub gpio: Result<Vec<GpioChipInfo>, String> }
```
- Komunikaty: `urządzenie audio „{x}” nie istnieje na tym komputerze (dostępne: {lista})`; `port {x} nie istnieje (dostępne: {lista albo „brak”})`; `układ GPIO {x} nie istnieje (dostępne: {lista})`; `linia {n} poza zakresem {chip} (0–{lines-1})`; `model głosu {x}: brak pliku albo brak {x}.json obok`; ostrzeżenie `{co}: nie sprawdzono — {powód}`.
- Produces (host, **bez testów**, spec §11): `aars_audio::output::output_device_names() -> Result<Vec<String>, SinkError>` (te same nazwy, które porównuje `CpalSink::open`); `aars_ptt::probe::serial_ports() -> Result<Vec<String>, String>` (`serialport::available_ports`, tylko `port_name`; **nie otwiera** portu); `aars_ptt::probe::gpio_chips() -> Result<Vec<GpioChipInfo>, String>` (Linux: `gpiocdev::chip::chips()` + `Chip::from_path(..).info()`; poza Linuksem `Err("GPIO wymaga Linuksa z /dev/gpiochip*")`); `aars::probe::HostProbe` implementujący trait przez te trzy funkcje.

- [ ] **Step 1: Testy** (`probe.rs`, z `FakeProbe` i `tempfile::tempdir`)
  - `voices_need_their_json_sidecar` (`a.onnx`+`a.onnx.json`, `b.onnx` bez json, `c.txt` → tylko `a`).
  - `a_missing_voices_dir_is_an_empty_list`.
  - `default_audio_is_always_accepted` (lista `Ok(vec![])`).
  - `an_unknown_audio_device_is_an_error_listing_the_available_ones`.
  - `an_unreadable_list_is_a_warning_not_an_error` (`serial: Err("brak uprawnień")`, backend serial → `errors` puste, `warnings` z `nie sprawdzono — brak uprawnień`).
  - `gpio_fields_are_checked_only_for_the_gpio_backend` (backend vox, `gpio_chip` nieistniejący → bez błędów).
  - `a_gpio_line_beyond_the_chip_is_refused` (`gpiochip0`, 28 linii, linia 28 → `linia 28 poza zakresem gpiochip0 (0–27)`; linia blokująca 30 → drugi błąd).
  - `the_voice_must_be_a_listed_model_or_an_existing_one_with_json`.
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja** (core, potem funkcje hosta i `HostProbe`; `cargo build --workspace`)
- [ ] **Step 4: PASS** (`cargo test -p aars-core -p aars-ptt -p aars-audio`)
- [ ] **Step 5: Commit**: `feat: lista urządzeń, portów, układów GPIO i głosów na hoście, bez dotykania sprzętu`

---

### Task 3: Pozycja stacji i lokator QTH (`aars_core::geo`)

**Files:**
- Create: `crates/aars-core/src/geo.rs`
- Modify: `crates/aars-core/src/lib.rs`

**Interfaces:**
- Produces:
```rust
#[derive(Debug, Clone, PartialEq)] pub struct StationPosition { pub lat: f64, pub lon: f64, pub locator: String }
pub fn locator_from(lat: f64, lon: f64, len: usize) -> Result<String, String>;  // len ∈ {4, 6, 8}; lat 90 i lon 180 przycinane do wnętrza
pub fn locator_center(locator: &str) -> Result<(f64, f64), String>;             // 4/6/8 znaków; środek kwadratu, zaokrąglony do 5 miejsc
pub fn parse_position(lat: &str, lon: &str, locator: &str) -> Result<Option<StationPosition>, String>; // R17
pub struct PositionRepo { /* pool */ }
impl PositionRepo { pub fn new(pool: SqlitePool) -> Self;
    pub async fn load(&self) -> Result<Option<StationPosition>, DbError>;        // brak albo zepsute klucze → None + tracing::error!
    pub async fn save(&self, p: Option<&StationPosition>) -> Result<(), DbError>; } // station:lat/lon/locator; None kasuje
```
- Komunikaty: `szerokość: liczba od −90 do 90`; `długość: liczba od −180 do 180`; `podaj obie współrzędne albo lokator`; `lokator: 4, 6 albo 8 znaków, np. JO91 albo JO91RS`.

- [ ] **Step 1: Testy** (wartości policzone niezależnie, 2026-11-20)
```rust
#[test]
fn known_points_give_known_locators() {
    assert_eq!(locator_from(51.759, 19.456, 4).unwrap(), "JO91");
    assert_eq!(locator_from(51.759, 19.456, 6).unwrap(), "JO91RS");
    assert_eq!(locator_from(51.759, 19.456, 8).unwrap(), "JO91RS42");
    assert_eq!(locator_from(48.14666, 11.60833, 6).unwrap(), "JN58TD");
    assert_eq!(locator_from(52.22977, 21.01178, 6).unwrap(), "KO02MF");
    assert_eq!(locator_from(-34.91, -56.21, 6).unwrap(), "GF15VC");
    assert!(locator_from(51.0, 19.0, 5).is_err());
}
#[test]
fn a_locator_means_the_centre_of_its_square() {
    assert_eq!(locator_center("JO91").unwrap(), (51.5, 19.0));
    assert_eq!(locator_center("jo91rs").unwrap(), (51.77083, 19.45833));
    assert_eq!(locator_center("JO91RS42").unwrap(), (51.76042, 19.45417));
    for bad in ["JO9", "JO91R", "ZZ91", "JO91ZZ", "JOAB", ""] { assert!(locator_center(bad).is_err(), "{bad}"); }
}
#[test]
fn edges_of_the_world_stay_inside_the_alphabet() {
    assert_eq!(locator_from(90.0, 180.0, 6).unwrap(), "RR99XX");
    assert_eq!(locator_from(-90.0, -180.0, 6).unwrap(), "AA00AA");
    assert_eq!(locator_from(0.0, 0.0, 6).unwrap(), "JJ00AA");
}
```
  Plus: `a_decimal_comma_is_accepted` (`parse_position("51,759", "19,456", "")` → `JO91RS`); `coordinates_win_and_keep_a_matching_locator` (`"51.759","19.456","JO91"` → locator `JO91`; `"51.759","19.456","KO02"` → `JO91RS`); `a_locator_alone_gives_its_centre`; `half_a_position_is_refused` (tylko szerokość → `podaj obie współrzędne albo lokator`); `out_of_range_is_refused`; `all_empty_clears` (`Ok(None)`); `position_round_trips_and_none_clears` (repo).
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars-core geo`)
- [ ] **Step 5: Commit**: `feat(core): pozycja stacji i lokator QTH`

---

### Task 4: Rejestr modułów (`aars_core::module`)

**Files:**
- Create: `crates/aars-core/src/module.rs`, `migrations/0005_modules.sql`
- Modify: `crates/aars-core/src/lib.rs`, `crates/aars-core/src/db.rs` (test listy tabel i kluczy, jeśli sprawdza)

**Interfaces:**
- Consumes: `parse_area`, `area_text` (etap 8), `SettingsRepo::kv_*` (Task 1), `SourceStateRepo::set_enabled`.
- Migracja: `INSERT INTO settings (key, value) VALUES ('module:scheduler:enabled', 'true');`
- Produces:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum ModuleKind { Source, Scheduler, Ident, Bulletin }
// label: "źródło danych" | "harmonogram" | "identyfikacja stacji" | "biuletyn"
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Toggle { Switchable, AlwaysOn(&'static str), Planned(&'static str) }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum SettingKind {
    Text { max_len: usize }, Integer { min: i64, max: i64 }, Area, Choice(&'static [(&'static str, &'static str)]) }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct SettingSpec {
    pub key: &'static str, pub label: &'static str, pub help: &'static str, pub kind: SettingKind, pub default: &'static str }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct ModuleDescriptor {
    pub id: &'static str, pub name: &'static str, pub description: &'static str, pub kind: ModuleKind,
    pub toggle: Toggle, pub default_enabled: bool, pub settings: &'static [SettingSpec],
    pub see_also: Option<(&'static str, &'static str)> }   // (tekst linku, ścieżka)
impl ModuleDescriptor {
    pub fn setting_key(&self, key: &str) -> String;        // Source: "source:{id}:{key}", inne: "module:{id}:{key}"
    pub fn validate(&self, input: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>, String>;
    // brak klucza → błąd „{label}: brak wartości”; Integer poza zakresem → „{label}: {min}–{max}”;
    // Area → parse_area, wynik w postaci area_text; Choice spoza listy → „{label}: nieznana wartość”; Text > max_len → „{label}: najwyżej {n} znaków”
}
pub fn builtin() -> Vec<ModuleDescriptor>;  // scheduler, auto-id, bulletin (R13); see_also scheduler → ("Tolerancja spóźnień", "/settings"), auto-id → ("Znak i tryb identyfikacji", "/settings")
pub struct ModuleRegistry { /* Vec<ModuleDescriptor> */ }
impl ModuleRegistry { pub fn new(modules: Vec<ModuleDescriptor>) -> Self;   // panic przy duplikacie id (błąd programisty, test)
    pub fn get(&self, id: &str) -> Option<&ModuleDescriptor>; pub fn iter(&self) -> impl Iterator<Item = &ModuleDescriptor>; }
#[derive(Debug, thiserror::Error)] pub enum ModuleError {
    #[error("tego modułu nie da się wyłączyć: {0}")] AlwaysOn(&'static str),
    #[error("moduł jeszcze niedostępny ({0})")] Planned(&'static str),
    #[error(transparent)] Db(#[from] DbError) }
pub struct ModuleRepo { /* pool */ }
impl ModuleRepo { pub fn new(pool: SqlitePool) -> Self;
    pub async fn register(&self, registry: &ModuleRegistry) -> Result<(), DbError>; // INSERT OR IGNORE: source_state (enabled = default_enabled) dla źródeł, module:{id}:enabled dla Switchable innych, domyślne ustawienia
    pub async fn is_enabled(&self, m: &ModuleDescriptor) -> Result<bool, DbError>;   // AlwaysOn → true, Planned → false
    pub async fn set_enabled(&self, m: &ModuleDescriptor, enabled: bool) -> Result<(), ModuleError>;
    pub async fn settings(&self, m: &ModuleDescriptor) -> Result<BTreeMap<String, String>, DbError>; // brak → default
    pub async fn save_settings(&self, m: &ModuleDescriptor, values: &BTreeMap<String, String>) -> Result<(), DbError>; }
#[cfg(any(test, feature = "testing"))]
pub fn sample_source(id: &'static str) -> ModuleDescriptor;   // ten sam schemat co imgw-meteo (Task 5a), do testów panelu:
// area: label „Obszar”, Area, default ""; interval_s: label „Interwał odpytywania (s)”, Integer { min: 60, max: 3600 }, default "300"
```

- [ ] **Step 1: Testy**
  - `builtin_modules_are_scheduler_auto_id_and_bulletin` (id, `Toggle`: `Switchable` / `AlwaysOn(_)` / `Planned("etap 11")`).
  - `source_settings_keep_their_stage_8_keys` (`sample_source("imgw-meteo").setting_key("area") == "source:imgw-meteo:area"`; `builtin` scheduler → `module:scheduler:x`).
  - `validation_canonicalises_the_area_and_checks_ranges` (`area="1061 Łódź; 10"` → dwie linie; `interval_s="30"` → `Interwał odpytywania (s): 60–3600`; `1061` bez nazwy → błąd z `parse_area`).
  - `register_inserts_missing_rows_without_touching_existing_ones` (świeża baza: `imgw-meteo` wyłączone z migracji 0003 zostaje wyłączone; nowy `sample_source("test-src")` z `default_enabled: false` dostaje wiersz `source_state` i `source:test-src:interval_s = 300`; drugi `register` nic nie zmienia).
  - `the_scheduler_toggle_round_trips` i `auto_id_cannot_be_disabled` (`ModuleError::AlwaysOn`, `is_enabled` dalej `true`); `the_bulletin_cannot_be_enabled` (`Planned`).
  - `a_source_toggle_writes_source_state` (`set_enabled(imgw, true)` → `SourceStateRepo::config("imgw-meteo").enabled`).
  - `duplicate_ids_are_a_programming_error` (`#[should_panic]`).
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars-core`)
- [ ] **Step 5: Commit**: `feat(core): rejestr modułów stacji z przełącznikiem i schematem ustawień`

---

### Task 5: Moduły w harmonogramie i źródłach; wzmocnienie i głos w syntezie

Dwie niezależne części w jednym zadaniu, bo obie tylko wpinają istniejące typy w istniejące pętle. Recenzent może odrzucić każdą osobno: commit 5a i commit 5b.

**Files:**
- 5a: Modify `crates/aars-core/src/scheduler.rs`, `crates/aars-sources/src/source.rs`, `crates/aars-sources/src/runner.rs`, `crates/aars-sources/src/imgw_meteo/mod.rs`, `crates/aars-sources/src/lib.rs`
- 5b: Modify `crates/aars-core/src/synth.rs`, `crates/aars-audio/src/piper.rs`, `crates/aars-audio/src/cache.rs`, `crates/aars/src/render.rs`, `crates/aars/src/main.rs` (przekazanie `gain_db`)

**Interfaces:**
- Consumes: `ModuleRepo`, `ModuleDescriptor`, `builtin` (Task 4); `PositionRepo`, `StationPosition` (Task 3); `apply_gain_db` (istniejące).
- Produces (5a):
  - `Scheduler::tick` najpierw czyta `module:scheduler:enabled` (`kv_get`; brak albo nie-`false` = włączony); wyłączony zwraca `TickReport { disabled: true, ..Default::default() }` bez dotykania `schedule_fires`. Nowe pole `TickReport.disabled: bool`.
  - `SourceCtx { now, area, position: Option<StationPosition>, settings: BTreeMap<String, String> }` (R20); runner wypełnia je w każdym cyklu (`PositionRepo::load` — błąd → `None` + log, nie przerywa cyklu; `kv_prefix("source:{id}:")`).
  - `aars_sources::imgw_meteo::DESCRIPTOR: ModuleDescriptor` (id `imgw-meteo`, nazwa `Ostrzeżenia meteorologiczne IMGW`, `Source`, `Switchable`, `default_enabled: false`, ustawienia `area` i `interval_s` z etykietami, rodzajami i domyślnymi wartościami jak `sample_source` z Task 4, pomoc obszaru z R8 etapu 8).
  - `pub struct SourceModule { pub descriptor: ModuleDescriptor, pub build: fn(&Config) -> Result<Arc<dyn DataSource>, SourceError> }`; `pub fn modules() -> Vec<SourceModule>` (dziś jeden: `imgw-meteo` z `ImgwMeteo::new(&config.sources.imgw_meteo_url)`).
- Produces (5b):
  - `Synthesizer::identity(&self) -> String` z domyślną implementacją `String::new()`; `PiperSynth` → `format!("piper|{model}|{target_sample_rate}")`; `FakeSynth::with_identity(self, id: &str) -> Self`.
  - `AudioCache::cache_key` haszuje także `synth.identity()` (sygnatura `render` bez zmian, klucz liczony w środku).
  - `PiperRenderer::new(cache, synth, callsign, id_mode, sample_rate, gain_db: f32)` i `PiperPreview::new(cache, synth, gain_db: f32)`: `apply_gain_db` na treści, identyfikacji (głos i CW) i podglądzie, **po** cache (w cache leży PCM bez wzmocnienia).

- [ ] **Step 1: Testy (5a)**
  - `scheduler.rs`: `a_disabled_scheduler_fires_nothing_and_records_nothing` (cron co minutę, `module:scheduler:enabled=false`, dwa tiki → `pending` puste, `schedule_fires` puste, `report.disabled`); `re_enabling_does_not_replay_missed_fires` (wyłączony przez 20 min symulowanego czasu, `misfire_grace` 300 s → po włączeniu jeden tik nie wstawia zaległości starszych niż 300 s).
  - `runner.rs`: `a_source_disabled_between_cycles_does_not_fetch_again` (licznik `FakeSource`: cykl → 1; `set_enabled(false)`; cykl → dalej 1, `skipped == Some("wyłączone")`); `the_context_carries_position_and_module_settings` (`FakeSource` zapisuje ostatni `SourceCtx`; po `PositionRepo::save(JO91RS)` → `ctx.position.unwrap().locator == "JO91RS"`, `ctx.settings["interval_s"] == "300"`).
  - `imgw_meteo/mod.rs`: `the_descriptor_matches_the_stage_8_keys_and_ranges`.
- [ ] **Step 2: FAIL** (`cargo test -p aars-core scheduler -p aars-sources`)
- [ ] **Step 3: Implementacja (5a)**
- [ ] **Step 4: PASS**, **Commit**: `feat: wyłączany harmonogram, pozycja i ustawienia modułu w kontekście źródła`
- [ ] **Step 5: Testy (5b)**
  - `cache.rs`: `changing_the_model_does_not_reuse_cached_audio` (ten sam tekst, `FakeSynth::with_identity("a")` potem `"b"` → dwie syntezy); `cached_audio_follows_the_sample_rate` (`FakeSynth::new(22050)` i `new(48000)` z tożsamościami różniącymi się częstotliwością → drugi `render` zwraca `sample_rate == 48000`).
  - `render.rs`: `the_output_level_follows_gain_db` (szczyt treści po `render` przy `gain_db = -6.0` w granicach ±1 % od `32767 · 10^(−6/20)`; przy `-3.0` wyższy); `the_cw_ident_follows_gain_db_too`.
- [ ] **Step 6: FAIL → implementacja → PASS** (`cargo test -p aars-audio -p aars`)
- [ ] **Step 7: Commit**: `fix(audio): wzmocnienie z konfiguracji i klucz cache zależny od głosu i częstotliwości`

---

### Task 6: Test PTT, test audio i zatrzymanie bez przerywania nadania (rdzeń)

**Files:**
- Modify: `crates/aars-core/src/txlog.rs`, `crates/aars-core/src/synth.rs` (`tone`), `crates/aars-core/src/sequencer.rs`, `crates/aars-core/src/dispatcher.rs`
- Modify: `crates/aars-web/src/logs.rs`, `crates/aars-web/templates/logs.html` (wiersz bez zadania), `crates/aars/src/main.rs` (nowa sygnatura `run`)

**Interfaces:**
- Produces:
  - `TxLogEntry.queue_item_id: Option<i64>`, `TxLogRow.queue_item_id: Option<i64>`. `/logs` dla `None` pokazuje notkę (`test PTT: admin`) zamiast treści.
  - `synth::tone(sample_rate: u32, hz: f32, duration: Duration, dbfs: f32) -> Pcm`.
  - Sekwencer: `SeqCmd::PlayLocal { pcm: Pcm, reply: tokio::sync::oneshot::Sender<Result<Duration, String>> }`; `SequencerHandle::play_local(&self, pcm: Pcm) -> Result<oneshot::Receiver<Result<Duration, String>>, String>` (R11: `ptt.unkey()`; błąd → `Err` bez grania; odtwarzanie z `CancelToken::expire_at(now + 15 s)`; nigdy `key()`, nigdy `mark_keyed`); `SequencerHandle::abort_current(&self)` (anuluje token trwającego nadania).
  - Dyspozytor:
```rust
pub const TEST_TX_ID: i64 = -1;
pub const TEST_PTT_RANGE: RangeInclusive<u64> = 1..=5;   // s
#[derive(Debug, Clone, PartialEq, Eq)] pub enum StationTest { Ptt { seconds: u64, by: String }, Audio { by: String } }
#[derive(Debug, Clone, PartialEq)] pub enum TestOutcome {
    Done { played: Duration, note: Option<String> }, Refused(RefusalReason), Busy, Unsupported(&'static str), Failed(String) }
#[derive(Clone)] pub struct DispatcherControl { /* mpsc::Sender<(StationTest, oneshot::Sender<TestOutcome>)> (pojemność 1), busy: Arc<AtomicBool> */ }
impl DispatcherControl { pub async fn run_test(&self, test: StationTest, timeout: Duration) -> TestOutcome; } // busy albo pełny kanał → Busy od razu; timeout → Failed("brak odpowiedzi stacji")
impl Dispatcher {
    pub fn control(&mut self) -> DispatcherControl;
    pub async fn run(self, shutdown: CancelToken, drain: CancelToken);   // ZMIANA sygnatury
}
```
  - Semantyka `run`: przed każdym `claim_next` obsługuje jeden test z kanału (`try_recv`). `busy` jest ustawione na czas `handle()` i testu. `drain` → nie bierze nic nowego, kończy bieżące nadanie i wraca. `shutdown` → `abort_current()`, czeka na `Finished` najwyżej 2 s i wraca. **Oba wyjścia** wołają `self.sequencer.shutdown()` (join wątku) przed powrotem.
  - Test PTT (R10): `seconds` przycięte do `TEST_PTT_RANGE`. Gdy `ptt_backend == "vox"`, odpowiedź `Unsupported("VOX nie steruje nadajnikiem — test PTT nie ma czego sprawdzić; nadaj krótki komunikat z /compose")`. Wynik idzie do `tx_log` (`queue_item_id: None`, `note: "test PTT: {by}"`, `result` według `tx_result`). `Refused` nie trafia do `tx_log`.
  - Test audio (R11): przy VOX `Unsupported("przy VOX dźwięk na wyjściu nadaje na antenę — test audio niedostępny")`. PCM = `tone(rate, 1000.0, 500 ms, -6.0)` + cisza 300 ms + `renderer.render("Test audio stacji.")`. Błąd syntezy daje sam ton i `note: Some("synteza nie powiodła się: …")`.

- [ ] **Step 1: Testy** (`dispatcher.rs` i `sequencer.rs`, `FakePtt` + `FakeSink` w czasie rzeczywistym, krótkie czasy)
  - `a_test_ptt_keys_for_the_requested_time_and_logs_without_a_queue_item` (1 s: zdarzenia `FakePtt` = `[Key, Unkey]`; `played` 0,9–1,3 s; `tx_log` ma 1 wiersz z `queue_item_id IS NULL`, `result = 'ok'`, `note = 'test PTT: admin'`; kolejka nietknięta).
  - `a_test_ptt_is_refused_by_the_kill_switch` (`set_tx_enabled(false)` → `Refused(TxDisabled)`, w `FakePtt` brak `Key`, `tx_log` pusty).
  - `a_test_ptt_is_refused_after_a_watchdog_intervention` (zatruty strażnik → `Refused(Poisoned)`).
  - `a_test_ptt_in_quiet_hours_is_refused` (`set_quiet(true, Emergency)` → `Refused(QuietHours)`).
  - `the_test_duration_is_clamped_to_five_seconds` (`seconds: 60` → `played ≤ 5,2 s`; test trwa ~5 s, bez `#[ignore]`).
  - `test_audio_never_keys_the_ptt` (zdarzenia `FakePtt` bez `Key`; `FakeSink::played_samples() > 0`; `Done` z `played ≥ 0,8 s`).
  - `test_audio_with_a_failing_synth_plays_the_tone_and_says_so` (renderer testowy, którego `render` zawsze zwraca `Err` → `Done`, `played ≈ 0,8 s`, `note` zawiera `synteza nie powiodła się`).
  - `vox_refuses_both_tests` (`ptt_backend: "vox"` → `Unsupported` dla obu, w `FakePtt` zero zdarzeń).
  - `a_test_while_transmitting_is_busy` (zadanie 2 s w trakcie → `run_test` zwraca `Busy` w < 100 ms).
  - `drain_finishes_the_current_transmission_and_takes_nothing_new` (dwa zadania, pierwsze 1,5 s; `drain.cancel()` po 300 ms → pierwsze `done`, drugie `pending`, `run` wraca ≤ 2,5 s, ostatnie zdarzenie `FakePtt` to `Unkey`).
  - `shutdown_aborts_the_current_transmission_quickly` (zadanie 5 s; `shutdown.cancel()` po 300 ms → `run` wraca ≤ 1 s, PTT opuszczone, zadanie wraca do kolejki jako przerwane).
  - `sequencer.rs`: `play_local_never_keys_and_is_bounded` (PCM 20 s → wynik ≤ 15,2 s, brak `Key` i `mark_keyed`).
  - `txlog.rs`: `an_entry_without_a_queue_item_is_listed_with_its_note`.
- [ ] **Step 2: FAIL** (`cargo test -p aars-core dispatcher sequencer txlog`)
- [ ] **Step 3: Implementacja**: przy okazji zaktualizuj wywołanie w `main.rs` (`dispatcher.run(shutdown.clone(), CancelToken::new())`) i istniejące testy dyspozytora.
- [ ] **Step 4: PASS** (`cargo test --workspace`)
- [ ] **Step 5: Commit**: `feat(core): test PTT i test audio przez sekwencer, zatrzymanie dyspozytora bez przerywania nadania`

---

### Task 7: Stacja w binarce: kandydaci konfiguracji, praca bez nadawania, restart w procesie

**Files:**
- Create: `crates/aars/src/station.rs`
- Modify: `crates/aars/src/main.rs` (`Run`, `Tx`, `Info` → przez `station`), `crates/aars/src/radio.rs` (`build_stack` przez fabrykę), `crates/aars/src/cli.rs` (`hardware show|reset`), `crates/aars/Cargo.toml` (dev: `tower`, `aars-web` z `testing`)
- Modify: `crates/aars-web/src/state.rs` (trait `StationControl` — tylko definicja, użycie w Task 8)

**Interfaces:**
- Consumes: `HardwareRepo`, `HardwareSettings`, `ApplyStatus`, `ConfigOrigin` (Task 1); `HostProbe` (Task 2); `ModuleRegistry`, `ModuleRepo::register`, `builtin` (Task 4); `aars_sources::modules()` (Task 5); `Dispatcher::{control, run}`, `DispatcherControl`, `StationTest`, `TestOutcome` (Task 6).
- Produces (`aars_web::state`):
```rust
pub type TestFuture<'a> = Pin<Box<dyn Future<Output = TestOutcome> + Send + 'a>>;
pub trait StationControl: Send + Sync {
    fn apply_status(&self) -> ApplyStatus;
    fn running_hardware(&self) -> Option<HardwareSettings>;  // None = bez nadawania
    fn running_station(&self) -> StationSettings;             // wczytane przy starcie (lista „czeka na restart”)
    fn request_restart(&self);
    fn run_test(&self, test: StationTest) -> TestFuture<'_>;  // bez stosu → TestOutcome::Unsupported("stacja działa bez sprzętu nadawczego — popraw ustawienia sprzętu")
}
```
- Produces (`aars::station`):
```rust
pub trait HardwareFactory: Send + Sync {
    fn ptt(&self, cfg: &PttConfig) -> Result<Arc<dyn PttBackend>, String>;
    fn sink(&self, cfg: &AudioConfig) -> Result<Arc<dyn AudioSink>, String>;
}
pub struct RealHardware;                                   // aars_ptt::build + CpalSink::open
pub struct Candidate { pub origin: ConfigOrigin, pub config: Config, pub pairs: BTreeMap<String, String> }
pub fn candidates(base: &Config, panel: &BTreeMap<String, String>, last_good: &BTreeMap<String, String>) -> (Vec<Candidate>, Vec<String> /* ostrzeżenia from_pairs */);
pub async fn build_first(cands: Vec<Candidate>, factory: &dyn HardwareFactory, pool: &SqlitePool)
    -> (Option<(RadioStack, Candidate)>, Vec<(ConfigOrigin, String)>);
// zapisuje ApplyStatus; sukces kandydata Panel → save_last_good(pairs); porażka kandydata Panel → audit hardware_apply_failed (user NULL)
pub enum Exit { Restart, Stop }
pub async fn run_once(base: &Config, pool: &SqlitePool, factory: Arc<dyn HardwareFactory>,
                      probe: Arc<dyn HardwareProbe>, stop: CancelToken) -> anyhow::Result<Exit>;
pub struct StationHandle { /* Option<DispatcherControl>, restart: CancelToken, status, hardware, station, tx_tail */ }  // impl StationControl
```
  - `radio::build_stack(config: &Config, pool: &SqlitePool, factory: &dyn HardwareFactory) -> Result<RadioStack>`. PTT i sink z fabryki, reszta bez zmian. Gdy sink zawiedzie po udanym PTT, PTT jest zwalniane przed kolejnym kandydatem.
  - `run_once`: `ModuleRepo::register` → `candidates` → `build_first` → (stos albo praca bez nadawania, R6) → `SourceRunner` dla każdego `SourceModule` (porażka `build` → `source_start_failed`, jak w etapie 8) → `AppState` z `with_station`, `with_probe`, `with_modules`, `with_base_config` (Task 8) → nasłuch na `bind` z kandydata, a przy błędzie na `bind` z pliku, z dopisaniem błędu do `ApplyStatus` → czekanie na `stop` albo `restart`. Restart: `drain.cancel()`, `await` zadania dyspozytora, potem `shutdown.cancel()` dla reszty, `await` wszystkich zadań, `watchdog.stop()` → `Exit::Restart`. Stop: `shutdown.cancel()`, to samo → `Exit::Stop`.
  - `main`: jeden `CancelToken stop` od `ctrl_c`, potem `loop { match run_once(..).await? { Exit::Restart => continue, Exit::Stop => break } }`. `aars tx` i `aars info` używają konfiguracji pierwszego kandydata (panel, jeśli jest), żeby CLI nadawał tym samym sprzętem co stacja.
  - CLI: `aars hardware show` wypisuje dla każdego z `HW_KEYS` linię `klucz = wartość  (panel|plik)` i `ApplyStatus`; `aars hardware reset` → `HardwareRepo::clear`, wypisuje `usunięto {n} ustawień sprzętu z panelu; przy następnym starcie obowiązuje aars.toml`.
  - Fałszywki testowe w `station.rs::tests`: `FakeHardware { fail_devices: Vec<String>, seen: Mutex<Vec<(PttBackendKind, String)>>, ptt: Arc<FakePtt> }` z `seen(&self) -> Vec<(PttBackendKind, String)>` (wpis przy każdej próbie `sink`; sink zawodzi dla urządzenia z listy, inaczej `FakeSink`; PTT to wspólny `FakePtt`).

- [ ] **Step 1: Testy** (`station.rs`)
  - `the_file_config_is_used_when_the_panel_has_nothing` (origin `File`, brak wpisu audytu).
  - `panel_settings_win_over_the_file` (`hw:ptt.backend=gpio`, `hw:audio.device=hw:1,0` → `seen == [(Gpio, "hw:1,0")]`, origin `Panel`, `hw_good:audio.device = hw:1,0`).
  - `a_failing_panel_config_falls_back_to_the_last_good_one_and_records_the_error` (panel `zly`, last good `hw:0,0` → origin `LastGood`, `hw_state:error` zawiera `zly`, jeden wpis `hardware_apply_failed` z `user_id IS NULL`; `hw:audio.device` dalej `zly`).
  - `without_any_working_config_the_station_runs_without_radio` (wszystkie zawodzą → `None`, origin `None`, błąd wymienia każdego kandydata).
  - `identical_candidates_are_tried_once` (panel = plik → jedna próba w `seen`).
  - `a_restart_requested_mid_transmission_waits_for_the_unkey` (`run_once` z `FakeHardware`, `bind` `127.0.0.1:0` przez `hw:server.bind` wpisane bezpośrednio do bazy (walidacja panelu tu nie działa), zadanie ~1,5 s w kolejce; gdy `FakePtt::is_keyed()`, `request_restart()` → `run_once` zwraca `Exit::Restart` dopiero po `Unkey`, `tx_log` ma `ok` dla zadania, `FakePtt` ma dokładnie jedną parę `Key`/`Unkey`).
  - `stop_ends_the_loop` (`stop.cancel()` → `Exit::Stop` ≤ 2 s).
  - `crates/aars/tests/cli.rs`: `hardware_reset_clears_panel_overrides` (wpis `hw:audio.device` w bazie testowej → `aars hardware reset` → brak kluczy `hw:`, wyjście zawiera `aars.toml`).
- [ ] **Step 2: FAIL** (`cargo test -p aars station`)
- [ ] **Step 3: Implementacja** (przeniesienie ciała `Command::Run` z `main.rs` do `run_once`; `with_hardware` z etapu 7 znika w Task 8)
- [ ] **Step 4: PASS** (`cargo test --workspace`)
- [ ] **Step 5: Commit**: `feat: konfiguracja sprzętu z panelu z powrotem do ostatniej działającej i restart stacji w procesie`

---

### Task 8: Panel — strona „Sprzęt”, zapis i restart

**Files:**
- Create: `crates/aars-web/src/hardware_page.rs`, `crates/aars-web/templates/hardware.html`, `crates/aars-web/templates/restart_card.html`
- Modify: `crates/aars-web/src/state.rs` (pola i buildery, `FakeStation` pod `testing`), `crates/aars-web/src/lib.rs` (trasy, banner na pulpicie), `crates/aars-web/src/settings_page.rs` i `templates/settings.html` (karta „Sprzęt (tylko do odczytu)” → link + `restart_card`), `crates/aars-web/src/chrome.rs` (etykiety audytu), `crates/aars-web/templates/base.html` (link „Sprzęt” dla admina), `crates/aars-web/templates/dashboard.html`

**Interfaces:**
- Consumes: `StationControl` (Task 7); `HardwareRepo`, `HardwareSettings`, `FieldError`, `ApplyStatus` (Task 1); `HardwareProbe`, `host_lists`, `check_against_host`, `FakeProbe` (Task 2); `ModuleRegistry` (Task 4).
- Produces (`AppState`): pola `station: Option<Arc<dyn StationControl>>`, `probe: Option<Arc<dyn HardwareProbe>>`, `modules: Arc<ModuleRegistry>`, `base_config: Arc<Config>`; buildery `with_station`, `with_probe`, `with_modules`, `with_base_config`. **Usunięte:** `hardware` i `with_hardware`. `for_tests`: brak stacji i sondy, rejestr `builtin()` + `sample_source("imgw-meteo")`, `Config::default()`. `state::testing::FakeStation { pub status: ApplyStatus, pub hardware: Option<HardwareSettings>, pub station: StationSettings, pub restarts: AtomicUsize, pub outcome: Mutex<TestOutcome>, pub tests: Mutex<Vec<StationTest>> }`.
- Produces (`hardware_page`):
  - `pub fn pending_restart(state: &AppState, stored_hw: &HardwareSettings, stored_station: &StationSettings) -> Vec<String>`: pusta bez stacji; inaczej `diff` sprzętu (gdy stos nie działa, każda zapisana różnica od pliku) plus zmienione klucze stacji (`callsign`, `id_mode`, `id_interval_s`, `tx_max_duration_s`, `misfire_grace_s`).
  - `GET /settings/hardware` (admin) pokazuje cztery karty. **Stan:** `origin.label()`, błąd zastosowania, lista „czeka na restart”, a przy braku stacji notka `Panel działa bez stacji (tryb testowy) — restart i testy niedostępne`. **Formularz:** listy rozwijane z `host_lists`; gdy listy brak albo zawiodła, pole tekstowe z notką powodu. Pola `bind`, `audio_device`, `sample_rate`, `gain_db`, `ptt_backend` (radio: VOX / GPIO / port szeregowy), `tx_tail_ms`, `gpio_chip`, `gpio_line`, `gpio_active_low` (checkbox `1`), `gpio_inhibit_line` (puste = brak), `serial_port`, `serial_line`, `voice_model`. Przy każdym polu wartość z `aars.toml` jako podpowiedź `w pliku: …`. **Testy** (Task 9). **Restart** (`restart_card.html`).
  - `POST /settings/hardware` (admin, CSRF): pola `String` z `#[serde(default)]`. `parse_form` daje `Result<HardwareSettings, Vec<FieldError>>`, potem `validate`, potem `check_against_host` (gdy `probe` jest `None`, tylko `validate`). Przy błędzie odpowiedź `400` ze stroną, na której zostają wpisane wartości i widać błędy przy polach. Przy sukcesie `HardwareRepo::save`, audyt `hardware` z `diff(stored, new).join("; ")` (bez różnic nie ma wpisu) i `303` na `/settings/hardware?saved=1`. Strona po zapisie mówi: `Zapisano. Zmiany wejdą w życie po restarcie stacji.` oraz pokazuje ostrzeżenia `HostCheck`.
  - `POST /station/restart` (admin, CSRF): bez stacji `503` `stacja nie działa w tym procesie`. Inaczej `request_restart()`, audyt `station_restart`, `303` na `/settings/hardware?restart=1`, a strona mówi: `Restart zlecony. Stacja dokończy bieżące nadanie i uruchomi się ponownie — odśwież stronę za kilka sekund.` Przy zmienionym `bind` dochodzi: `Panel będzie pod adresem {bind}.`
  - Pulpit (każda rola): gdy `apply_status().origin == None`, banner `Nadawanie niedostępne: {błąd}`, a adminowi dodatkowo link `Ustawienia sprzętu`. Gdy `LastGood`, admin widzi notkę `Działa ostatnia działająca konfiguracja — ustawienia z panelu nie dały się zastosować: {błąd}`. Wyłącznik nadawania widać zawsze (etap 8 R17).
  - `audit_action_label`: `hardware` → „zmiana sprzętu”, `hardware_apply_failed` → „sprzęt nie wstał”, `station_restart` → „restart stacji”, `test_ptt` → „test PTT”, `test_audio` → „test audio”, `position` → „pozycja stacji”, `module_enabled` → „moduł wł./wył.”, `module_settings` → „ustawienia modułu”.

- [ ] **Step 1: Testy** (`hardware_page.rs`, przez router, `FakeProbe`, `FakeStation`, `tempdir` z `pl_PL-test.onnx` i `.onnx.json`)
  - `the_page_lists_host_devices_and_voices` (zawiera `hw:1,0`, `/dev/ttyUSB0`, `gpiochip0`, `pl_PL-test`).
  - `an_admin_saves_a_gpio_setup_and_it_is_stored_with_an_audit_entry` (`303`; `hw:ptt.backend = gpio`, `hw:ptt.gpio.line = 17`; audyt `hardware` zawiera `ptt.backend: vox → gpio`).
  - `saving_unchanged_values_writes_no_audit_entry`.
  - `an_unknown_audio_device_is_refused_and_nothing_is_stored` (`hw:9,0` → `400`, strona zawiera `hw:9,0` i `nie istnieje`, `overrides()` puste).
  - `a_gpio_line_outside_the_chip_is_refused` (28 linii, linia 40 → `400`, `poza zakresem`).
  - `when_the_host_cannot_list_serial_ports_the_port_is_accepted_with_a_warning` (`serial: Err("brak uprawnień")` → zapis, strona `nie sprawdzono — brak uprawnień`).
  - `a_low_port_is_refused` (`0.0.0.0:80` → `400`, `wymaga uprawnień roota`).
  - `a_decimal_comma_in_gain_is_accepted` (`-4,5` → `hw:audio.gain_db = -4.5`).
  - `the_page_shows_the_apply_error_and_the_origin` (`FakeStation` z `LastGood` i błędem `nie znaleziono urządzenia hw:9,0` → oba teksty na stronie).
  - `pending_restart_lists_changed_keys` (działa `vox`, zapisane `gpio` → `ptt.backend: vox → gpio`; zmieniony znak → `callsign`).
  - `restart_is_requested_once_and_audited` (`restarts == 1`, audyt `station_restart`); `restart_without_a_station_is_503`.
  - `an_operator_cannot_open_hardware` (`GET` i `POST` → `403`, nic nie zapisane).
  - `the_dashboard_shows_that_transmitting_is_unavailable` (origin `None` → banner dla viewera, przycisk `/tx-enabled` dla operatora).
  - `settings_page.rs`: zamień `the_page_lists_the_hardware_from_the_config_file` na `the_settings_page_links_to_hardware_and_shows_pending_restart`.
- [ ] **Step 2: FAIL** (`cargo test -p aars-web hardware`)
- [ ] **Step 3: Implementacja**: także `main.rs`/`station.rs` (`with_hardware` → nowe buildery).
- [ ] **Step 4: PASS** (`cargo test --workspace`)
- [ ] **Step 5: Commit**: `feat(web): strona sprzętu — PTT, audio, głos, adres panelu, restart stacji`

---

### Task 9: Panel — test PTT i test audio

**Files:**
- Modify: `crates/aars-web/src/hardware_page.rs`, `crates/aars-web/templates/hardware.html`, `crates/aars-web/src/lib.rs` (trasy)

**Interfaces:**
- Consumes: `StationControl::run_test`, `StationTest`, `TestOutcome`, `TEST_PTT_RANGE`, `RefusalReason` (Task 6, 7); `FakeStation` (Task 8).
- Produces:
  - `pub fn outcome_text(test: &StationTest, outcome: &TestOutcome, device: &str) -> (&'static str /*css*/, String)`. Dokładne teksty:
    - PTT `Done`: `Test PTT: nadajnik włączony na {s} s i wyłączony. Wpis w dzienniku nadań.` (`s` z jedną cyfrą po przecinku, przecinek dziesiętny).
    - Audio `Done`: `Test audio: odtworzono {s} s na urządzeniu {device} przy wyłączonym PTT.` + ` {note}`, gdy jest notka.
    - `Refused(r)`: `Odmowa: {powód}`. Powody: `TxDisabled` → „nadawanie wyłączone (wyłącznik)”, `Poisoned` → „blokada watchdoga — zdejmij ją na pulpicie”, `Inhibited` → „blokada sprzętowa”, `QuietHours` → „okno ciszy”, `ChannelBusy` → „kanał zajęty”.
    - `Busy`: `Stacja właśnie nadaje albo trwa inny test — spróbuj za chwilę.`; `Unsupported(m)`: `m`; `Failed(e)`: `Test nie powiódł się: {e}`.
  - `POST /settings/hardware/test-ptt` (admin, CSRF; pola `confirm`, `seconds`): bez `confirm == "tak"` → `400` `Zaznacz potwierdzenie — test PTT włącza nadajnik.`; `seconds` spoza `1–5` → `400` `Czas testu: 1–5 s`; bez stacji → `503`. Inaczej `run_test(StationTest::Ptt { seconds, by: username })`, potem strona sprzętu `200` z wynikiem i audyt `test_ptt` (`{seconds} s: {tekst wyniku}`).
  - `POST /settings/hardware/test-audio` (admin, CSRF): bez stacji → `503`. Inaczej `run_test(Audio)`, `200` z wynikiem i audyt `test_audio`.
  - Formularz testu PTT na stronie: checkbox z etykietą `Rozumiem, że radio wejdzie na nadawanie (bez identyfikacji, najwyżej 5 s).`, pole liczby 1–5 (domyślnie 2). Przy teście audio uwaga `Upewnij się, że radio nie jest ustawione na VOX — wtedy dźwięk poszedłby na antenę.` Gdy działa VOX, oba formularze są wyszarzone i opisane tekstem z R12.

- [ ] **Step 1: Testy** (przez router z `FakeStation`)
  - `test_ptt_needs_the_confirmation` (`400`, `tests` w `FakeStation` puste).
  - `test_ptt_duration_is_bounded` (`0` i `6` → `400`).
  - `a_confirmed_test_ptt_reaches_the_station_and_is_audited` (`tests == [Ptt { seconds: 2, by: "u" }]`, strona zawiera `nadajnik włączony na 2,0 s`, audyt `test_ptt`).
  - `a_refusal_is_shown_with_its_reason` (`outcome = Refused(TxDisabled)` → `Odmowa: nadawanie wyłączone (wyłącznik)`).
  - `test_audio_says_the_ptt_stayed_off` (`przy wyłączonym PTT`).
  - `tests_without_a_station_are_503`; `an_operator_cannot_run_tests` (`403`, `tests` puste).
  - `outcome_text` — tabela wszystkich wariantów (test jednostkowy).
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars-web`)
- [ ] **Step 5: Commit**: `feat(web): test PTT z potwierdzeniem i test audio przy wyłączonym PTT`

---

### Task 10: Panel — pozycja stacji z mapą

**Files:**
- Create: `assets/vendor/leaflet/leaflet.js`, `leaflet.css`, `images/marker-icon.png`, `images/marker-icon-2x.png`, `images/marker-shadow.png`, `LICENSE`, `VERSION`; `assets/station-map.js`
- Modify: `crates/aars-web/src/settings_page.rs`, `crates/aars-web/templates/settings.html`, `crates/aars-web/templates/base.html` (`{% block head %}{% endblock %}` w `<head>`), `crates/aars-web/src/lib.rs` (trasa)

**Interfaces:**
- Consumes: `parse_position`, `PositionRepo`, `StationPosition` (Task 3).
- Leaflet: pliki z paczki npm `https://registry.npmjs.org/leaflet/-/leaflet-1.9.4.tgz` (`package/dist/…`), pobrane raz przy implementacji. `VERSION` zawiera `leaflet 1.9.4` i `sha256` tarballa, `LICENSE` to plik z paczki (BSD-2-Clause). Niczego nie minifikować ani nie zmieniać.
- `station-map.js` (nasz, bez zależności poza `L`): szuka `#station-map`; gdy nie ma `window.L`, wychodzi bez błędu. Środek mapy to `data-lat`/`data-lon`, a bez pozycji `52.0, 19.4` przy zoomie 6 (z pozycją zoom 12). Warstwa `L.tileLayer(dataset.tiles, { maxZoom: 19, attribution: '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a>' })`. Kliknięcie albo `dragend` znacznika wpisuje `#lat`/`#lon` (`toFixed(5)`) i czyści `#locator`. Pierwsze `tileerror` pokazuje `#map-note` z tekstem z R18.
- Produces:
  - Karta `Położenie stacji` (`id="polozenie"`) na `/settings`. Pola `lat`, `lon`, `locator` mają podpowiedź `Szerokość i długość w stopniach (np. 51.75900) albo lokator QTH (4, 6 lub 8 znaków, np. JO91RS). Używają jej moduły pogodowe.` Dalej `<div id="station-map" data-lat data-lon data-tiles="https://tile.openstreetmap.org/{z}/{x}/{y}.png">`, `<p id="map-note" hidden>`, `<noscript>Mapa wymaga JavaScriptu — formularz działa bez niej.</noscript>`. Gdy pozycja jest zapisana, karta pokazuje `51.77083, 19.45833 · JO91RS`. Skrypty i style Leafleta są ładowane tylko na tej stronie, w bloku `head`.
  - `POST /settings/position` (admin, CSRF; `lat`, `lon`, `locator` z `#[serde(default)]`): `parse_position`. Błąd daje `400` ze stroną ustawień z błędem w karcie. Sukces to `PositionRepo::save`, audyt `position` (`{lat:.5}, {lon:.5} ({locator})` albo `usunięto`) i `303` na `/settings#polozenie`.

- [ ] **Step 1: Testy** (`settings_page.rs` i `assets.rs`)
  - `an_admin_sets_the_position_from_coordinates` (`lat=51.759&lon=19.456` → `PositionRepo::load()` = `JO91RS`, audyt `position`).
  - `an_admin_sets_the_position_from_a_locator_alone` (`locator=jo91rs` → `lat == 51.77083`).
  - `half_a_position_is_refused_and_nothing_changes` (`400`, `podaj obie współrzędne albo lokator`).
  - `clearing_all_fields_removes_the_position` (audyt `usunięto`).
  - `an_operator_cannot_set_the_position` (`403`).
  - `the_settings_page_has_the_map_and_the_fallback_form` (zawiera `id="station-map"`, `name="locator"`, `/assets/vendor/leaflet/leaflet.js`, `noscript`).
  - `assets.rs`: `leaflet_and_the_map_script_are_embedded` (`GET /assets/vendor/leaflet/leaflet.js` i `/assets/station-map.js` → `200`, `content-type` z `javascript`; `/assets/vendor/leaflet/images/marker-icon.png` → `image/png`).
  - `no_template_loads_scripts_or_styles_from_another_origin` (żaden plik w `templates/` nie zawiera `<script src="http` ani `<link rel="stylesheet" href="http`; spec D5: assety wkompilowane).
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja**: Leaflet z tarballa, skrypt, karta, trasa.
- [ ] **Step 4: PASS** (`cargo test -p aars-web`); ręcznie: `cargo run -p aars -- run`, `/settings`, kliknięcie w mapę wypełnia pola, zapis pokazuje lokator; po wyłączeniu sieci przeglądarki formularz z lokatorem działa i pojawia się notka.
- [ ] **Step 5: Commit**: `feat(web): pozycja stacji ze współrzędnych albo lokatora, mapa Leaflet zwendorowana`

---

### Task 11: Panel — strona „Moduły”

**Files:**
- Create: `crates/aars-web/src/modules_page.rs`, `crates/aars-web/templates/modules.html`
- Modify: `crates/aars-web/src/lib.rs` (trasy; usunięcie `/sources/{id}/settings`), `crates/aars-web/src/sources_page.rs` i `templates/sources.html` (formularz obszaru → odczyt + link; testy etapu 8 dotyczące zapisu obszaru przenoszą się tutaj), `crates/aars-web/templates/base.html` (link „Moduły” dla operatora), `crates/aars/src/station.rs` (rejestr do `AppState`)

**Interfaces:**
- Consumes: `ModuleRegistry`, `ModuleRepo`, `ModuleError`, `ModuleDescriptor::validate`, `SettingKind`, `Toggle` (Task 4); `sources_page::health`, `SourceStateRepo` (etap 8); `AppState.source_refresh`.
- Produces:
  - `GET /modules` (operator): karta każdego modułu z rejestru, w kolejności rejestru. Zawiera nazwę, `kind.label()`, opis i plakietkę stanu: `włączony` / `wyłączony` / `zawsze włączony — {powód}` / `planowany — {etap}`. Źródła mają też plakietkę `health` z etapu 8 i link `/sources`, a moduły z `see_also` link. **Tylko admin** widzi przycisk Włącz/Wyłącz (gdy `Switchable`) i formularz ustawień generowany ze schematu: `Text` → `input maxlength`, `Integer` → `input type=number min max`, `Area` → `textarea` z pomocą, `Choice` → `select`. Karta `auto-id` pokazuje bieżący znak albo `Uwaga: bez znaku wywoławczego stacja nadaje bez identyfikacji.`
  - `POST /modules/{id}/enabled` (admin, CSRF, `enabled`). Nieznany moduł → `404`. `ModuleError::AlwaysOn` albo `Planned` → `400` z tekstem błędu. Sukces → `303 /modules` i audyt `module_enabled` (`{id}: włączony|wyłączony`). Po włączeniu źródła `source_refresh(id)`.
  - `POST /modules/{id}/settings` (admin, CSRF; formularz jako `HashMap<String, String>`, brane są tylko klucze ze schematu, pole `csrf` sprawdzane osobno). Brak pola ze schematu oznacza obecną wartość. Błąd `validate` → `400` z komunikatem i wpisanymi wartościami. Sukces → `save_settings`, audyt `module_settings` (`{id}: {zmienione klucze}`), dla źródła `source_refresh(id)`, `303 /modules#{id}`. Moduł bez ustawień → `404`.
  - `/sources`: obszar i interwał tylko do odczytu, adminowi link `Zmień w Moduły` (`/modules#imgw-meteo`). Trasa `POST /sources/{id}/settings` i `sources_page::save_settings` znikają.

- [ ] **Step 1: Testy** (`modules_page.rs`)
  - `an_operator_sees_modules_without_forms` (`200`, zawiera `Harmonogram`, `Ostrzeżenia meteorologiczne IMGW`, `planowany — etap 11`; brak `action="/modules/`).
  - `an_admin_disables_the_scheduler_and_it_is_audited` (`module:scheduler:enabled = false`, audyt `scheduler: wyłączony`).
  - `auto_id_cannot_be_disabled` (`400`, `obowiązkowa`); `the_bulletin_cannot_be_enabled` (`400`, `etap 11`).
  - `enabling_a_source_module_writes_source_state_and_wakes_it` (licznik fałszywego `source_refresh` = 1).
  - `an_admin_saves_imgw_area_through_the_module_form` (`area=1061 Łódź\n10`, `interval_s=600` → `SourceStateRepo::config` ma 2 wpisy i 600; audyt `module_settings`).
  - `a_county_without_a_name_is_refused_and_nothing_changes` i `an_interval_out_of_range_is_refused` (przeniesione z etapu 8, nowa ścieżka, `400`, w bazie bez zmian).
  - `an_operator_cannot_change_module_settings` (`403`); `an_unknown_module_is_404`.
  - `sources_page.rs`: `the_sources_page_shows_the_area_read_only_with_a_link_for_the_admin`; usuń testy zapisu obszaru (przeniesione).
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars-web`)
- [ ] **Step 5: Commit**: `feat(web): moduły stacji — stan, włączanie i ustawienia ze schematu`

---

### Task 12: Macierz ról, kryterium wyjścia, dokumentacja

**Files:**
- Modify: `crates/aars-web/src/role_matrix.rs`
- Modify: `crates/aars/src/station.rs` (test kryterium wyjścia)
- Modify: `README.md`, `aars.example.toml`, `docs/superpowers/plans/README.md` (wiersz 9 → ✓)

**Interfaces:**
- Consumes: wszystko powyżej.
- Macierz:
  - GET: `get("/settings/hardware", ADMIN)`, `get("/modules", OPERATOR)`.
  - `POSTS` i wiersze z poprawnym CSRF: `/settings/hardware` (ADMIN, `400`: `VALID_BODY` nie ma pól sprzętu), `/settings/hardware/test-ptt` (ADMIN, `400`: brak `confirm`), `/settings/hardware/test-audio` (ADMIN, `503`), `/settings/position` (ADMIN, `303`: puste pola kasują pozycję), `/station/restart` (ADMIN, `503`), `/modules/scheduler/enabled` (ADMIN, `303`), `/modules/imgw-meteo/settings` (ADMIN, `400`: `interval_s=5`). Usuń wiersze `/sources/imgw-meteo/settings`.
- Test kryterium wyjścia `a_fresh_install_is_configured_entirely_from_the_panel` (`station.rs::tests`). Konfiguracja bazowa to `Config::default()` ze ścieżkami z `tempdir` (`storage.*`, `tts.voices_dir` z plikiem `pl_PL-test.onnx` + `.onnx.json`). `AppState` ma `FakeProbe` (audio `["default", "hw:1,0"]`, GPIO `gpiochip0` z 28 liniami), `FakeStation` i rejestr `builtin()` + `aars_sources::modules()`. Admin loguje się przez router i wysyła `POST /settings/hardware` (gpio, `gpiochip0`, linia 17, `hw:1,0`, 48000, `-3`, głos z `tempdir`, `0.0.0.0:8080`), `POST /settings/position` (`locator=JO91RS`), `POST /modules/imgw-meteo/settings` (`area=10`, `interval_s=300`), `POST /modules/imgw-meteo/enabled` (`1`) i `POST /modules/scheduler/enabled` (`0`). Asercje:
```rust
let (cands, _) = candidates(&base, &HardwareRepo::new(pool.clone()).overrides().await?, &BTreeMap::new());
let (built, errors) = build_first(cands, &hardware, &pool).await;
let (_, used) = built.expect("stos z ustawień panelu");
assert!(errors.is_empty());
assert_eq!(used.origin, ConfigOrigin::Panel);
assert_eq!(hardware.seen(), vec![(PttBackendKind::Gpio, "hw:1,0".to_owned())]);
assert_eq!(used.config.ptt.gpio.line, 17);
assert!(used.config.tts.model.ends_with("pl_PL-test.onnx"));
assert_eq!(PositionRepo::new(pool.clone()).load().await?.unwrap().locator, "JO91RS");
assert!(SourceStateRepo::new(pool.clone()).config("imgw-meteo").await?.unwrap().enabled);
assert!(scheduler.tick(Utc::now()).await?.disabled);
for action in ["hardware", "position", "module_settings", "module_enabled"] { assert!(audit_count(&pool, action).await >= 1, "{action}"); }
```
- Dokumentacja:
  - README: tabela panelu (`/settings/hardware`, testy, `/station/restart`, `/settings/position` — admin; `/modules` — operator do odczytu, admin do zmian; obszar źródła przeniesiony do `/modules`). Nowa sekcja „Konfiguracja z panelu”: co działa od razu, a co po restarcie (R3), pierwszeństwo (R1), powrót do ostatniej działającej konfiguracji (R5, R6), ratunek `aars hardware reset` (R7), testy PTT i audio z ograniczeniami (R10–R12), mapa i internet (R18). Status: etapy 1–9 z 11.
  - `aars.example.toml`: aktywne zostają tylko `[storage]`, `[tts] piper_bin` i `voices_dir`, `[sources]`. Sekcje `[server]`, `[audio]`, `[ptt]` i `tts.model` stają się komentarzem z notą `Ustawiane w panelu (Ustawienia → Sprzęt). Wartości tutaj są tylko startowe; panel ma pierwszeństwo.`
  - `docs/superpowers/plans/README.md`: wiersz 9 `9 ✓`.

- [ ] **Step 1: Testy** (macierz + kryterium wyjścia)
- [ ] **Step 2: Uruchom**: powinny przejść od razu. Czerwony wynik to błąd w Task 1–11, nie w teście.
- [ ] **Step 3: Pełna weryfikacja:** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`. Ręcznie: świeża baza i `aars.toml` z samymi ścieżkami, `cargo run -p aars -- run`. Admin wybiera VOX i `default`, zapisuje, restartuje; pulpit działa, test PTT odmawia z tekstem R12. Potem urządzenie, którego nie ma (pole tekstowe przy padniętej liście) i restart → stacja wstaje na ostatniej działającej konfiguracji, a błąd widać na pulpicie. Do tego pozycja z mapy, wyłączenie harmonogramu i `/logs` z wpisami audytu.
- [ ] **Step 4: Commit**: `test: kryterium wyjścia etapu 9 i macierz ról z nowymi trasami; dokumentacja`

---

## Kryterium wyjścia z etapu

- Administrator na świeżej instalacji (w `aars.toml` tylko ścieżki) ustawia **wyłącznie z panelu** backend PTT, urządzenie audio, głos, pozycję stacji i moduły, a stacja buduje się z tych ustawień (Task 12, Task 7).
- Każda zmiana z panelu jest w dzienniku audytu: sprzęt, testy, restart, pozycja, moduły, a nieudane zastosowanie trafia tam jako wpis systemowy (Task 8–11, Task 7).
- Wyłączenie modułu zatrzymuje źródło najpóźniej w następnym cyklu, a wyłączony harmonogram nic nie odpala (Task 5, Task 11).
- Test PTT trwa najwyżej 5 s, przechodzi przez interlocki i watchdog i trafia do dziennika nadań. Test audio nigdy nie podnosi PTT. Przy VOX oba odmawiają (Task 6, Task 9).
- Restart nigdy nie przerywa nadania. Nieudana konfiguracja sprzętu nie zostawia stacji bez panelu: działa ostatnia działająca konfiguracja albo tryb bez nadawania z widocznym błędem (Task 6, Task 7).
- Macierz rola × endpoint przechodzi ze wszystkimi nowymi trasami (Task 12).
- `cargo test --workspace` zielone, `cargo clippy --workspace --all-targets -- -D warnings` czyste. Żaden test nie dotyka karty dźwiękowej, GPIO, portu szeregowego ani sieci.

Uwaga przy wdrożeniu: migracja 0005 tylko dodaje wiersz `module:scheduler:enabled`, więc bazy z etapu 8 przechodzą bez zmian. Po aktualizacji `aars.toml` ze sprzętem dalej działa (R1): panel niczego nie nadpisuje, dopóki admin niczego nie zapisze.
