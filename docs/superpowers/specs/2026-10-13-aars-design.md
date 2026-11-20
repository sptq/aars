# AARS — Automatic Alert Radio Station

**Data:** 2026-10-13
**Status:** zatwierdzony design, gotowy do planu implementacji

## 1. Cel

Automatyczna stacja wczesnego ostrzegania nadająca komunikaty głosowe drogą
radiową. Urządzenie stoi przy radiu (typowo Raspberry Pi), samo składa
komunikaty z danych o zagrożeniach, nadaje je w zadanych porach lub
natychmiast po wystąpieniu zdarzenia, a operator może przez panel www wpisać
i nadać własny komunikat.

Inspiracja: [sr0wx](https://github.com/sq2ips/sr0wx) (Python, bank próbek
audio, moduły źródeł danych IMGW/PAA). AARS przejmuje ideę, ale nie kod:
całość w Rust, synteza przez lokalny TTS zamiast banku próbek, panel www
jako pierwszorzędny interfejs.

### Co AARS robi

- Nadaje komunikaty głosowe przez kartę dźwiękową do wejścia mikrofonowego radia.
- Steruje nadawaniem radia: GPIO, DTR/RTS po porcie szeregowym, albo VOX.
- Syntezuje dowolny tekst na głos lokalnie, bez internetu.
- Trzyma trwałą kolejkę komunikatów z priorytetami i przerywaniem.
- Nadaje komunikaty cyklicznie (cron), jednorazowo o zadanej godzinie,
  natychmiast, oraz z powtórzeniami.
- Pobiera dane o zagrożeniach z zewnętrznych źródeł i zamienia je w komunikaty.
- Udostępnia panel www: tworzenie komunikatów, podgląd audio, kolejka na żywo,
  konfiguracja, logi, użytkownicy z rolami.
- Wymusza interlocki bezpieczeństwa nadawania: twardy limit czasu TX,
  automatyczna identyfikacja stacji, globalna blokada nadawania.

### Czego AARS nie robi (świadome wykluczenia)

- Nie obsługuje wielu nadajników ani wielu stacji z jednego panelu.
- Nie ma serwera centralnego ani agentów — jedno urządzenie, jedna binarka.
- Nie wykrywa zajętości kanału (zostawione miejsce w konfiguracji, patrz §4.4).
- Nie steruje radiem po Hamlib/rigctld (możliwy późniejszy backend PTT).
- Nie przyjmuje CAP/RSS ani nie wystawia API dla zewnętrznych nadawców.
- Nie nadaje w trybach cyfrowych — tylko fonia.

## 2. Decyzje architektoniczne

Każda z tych decyzji została podjęta świadomie i ma uzasadnienie; zmiana
którejkolwiek unieważnia część projektu.

| # | Decyzja | Uzasadnienie |
|---|---------|--------------|
| D1 | Jedna binarka, jeden proces, SQLite na dysku | Zero deploymentu i zero sieci między komponentami. Stacja ostrzegawcza musi działać, gdy sieć padnie. |
| D2 | Wyłączny aktor-sekwencer posiada audio i PTT | „Dwa komunikaty naraz" staje się niemożliwe konstrukcyjnie, nie przez dyscyplinę wywołań. |
| D3 | SQLite jest źródłem prawdy dla kolejki i harmonogramu | RPi traci zasilanie dokładnie wtedy, gdy ostrzeżenia są potrzebne. Po restarcie kolejka i harmonogram muszą ocaleć, a dziennik nadań jest materiałem audytowym. |
| D4 | Lokalny TTS (Piper, ONNX) jako jedyny silnik syntezy | Jedna droga dla komunikatu automatycznego i ręcznego, więc jeden kod do utrzymania. Dowolny tekst z panelu jest natychmiast nadawalny. Działa offline. |
| D5 | SSR: Axum + Askama + HTMX, assety wkompilowane | Panel to formularze i tabele. SPA dołożyłaby drugi łańcuch narzędzi na RPi bez zysku. |
| D6 | `aars-core` nie wie o HTTP | Rozdzielenie na demon TX + panel (dwa procesy, IPC) zostaje możliwe jako dodanie warstwy, nie przepisanie. |
| D7 | Watchdog TX w osobnym wątku OS z monotonicznym zegarem | Musi opuścić PTT nawet gdy runtime tokio stanie. Wątek tokio nie daje tej gwarancji. |
| D8 | Źródła danych produkują tekst, nie audio | Synteza zostaje w jednym miejscu; źródło jest testowalne bez karty dźwiękowej. |

## 3. Struktura

Workspace Cargo, sześć crate'ów. Granice są takie, by każdy dał się
zrozumieć i przetestować bez pozostałych.

```
aars/
├── crates/
│   ├── aars-core/      # domena: model, kolejka, sekwencer, traity, konfiguracja
│   ├── aars-audio/     # synteza (Piper), normalizacja tekstu, wyjście audio, cache
│   ├── aars-ptt/       # backendy sterowania radiem: gpio, serial, vox
│   ├── aars-sources/   # trait DataSource + źródła: imgw-meteo, imgw-hydro, paa
│   ├── aars-web/       # Axum, Askama, HTMX, auth, SSE
│   └── aars/           # binarka: wiring, CLI, migracje, konfiguracja
├── assets/             # css, js (htmx), szablony — wkompilowane przez rust-embed
├── migrations/         # SQL, sqlx migrate
└── docs/
```

Kierunek zależności jest jednostronny: `aars` → {`aars-web`, `aars-sources`,
`aars-audio`, `aars-ptt`} → `aars-core`. `aars-core` nie zależy od niczego
poza bazą i typami domenowymi.

### 3.1 Traity graniczne

Trzy abstrakcje; każda ma implementację fake używaną w testach, więc cały
system jest testowalny bez karty dźwiękowej, GPIO i internetu.

```rust
// aars-core::synth
pub trait Synthesizer: Send + Sync {
    fn synthesize(&self, text: &str, voice: &VoiceParams) -> Result<Pcm, SynthError>;
}

// aars-core::ptt
pub trait PttBackend: Send + Sync {
    fn key(&self) -> Result<(), PttError>;        // podnieś PTT
    fn unkey(&self) -> Result<(), PttError>;      // opuść PTT
    fn force_unkey(&self);                        // sync, bez alokacji, dla watchdoga
    fn channel_busy(&self) -> Option<bool> { None } // miejsce na COS/RMS, §4.4
}

// aars-core::sink
pub trait AudioSink: Send + Sync {
    fn play(&self, pcm: &Pcm, cancel: &CancelToken) -> Result<PlayReport, SinkError>;
}
```

`force_unkey` jest celowo infallible i synchroniczne: watchdog nie może
czekać, logować ani alokować, a jego wywołanie musi być bezpieczne z obcego
wątku. Backendy trzymają uchwyt do urządzenia za `Arc` i sprowadzają
`force_unkey` do jednego zapisu do deskryptora.

## 4. Rdzeń nadawania

### 4.1 Pipeline komunikatu

```
definicja komunikatu
  → renderowanie szablonu (minijinja: dane źródła → tekst)
  → normalizacja tekstu (liczby, jednostki, skróty → słowa polskie)
  → synteza (Piper → PCM 22,05 kHz mono)
  → resampling do częstotliwości urządzenia + normalizacja głośności z limiterem
  → cache WAV (klucz: hash tekstu + parametrów głosu)
  → sekwencer: [ID] + cisza + treść + cisza + [ID]
  → PTT ↑ → tail delay → odtworzenie → tail delay → PTT ↓
```

Cache jest adresowany treścią, więc komunikat cykliczny syntezuje się raz, a
podgląd w panelu i nadanie na pasmo korzystają z tego samego pliku. Czyszczenie
po LRU przy przekroczeniu limitu rozmiaru z konfiguracji.

Normalizacja tekstu jest osobnym, czysto funkcyjnym modułem — żadnego I/O, więc
testowalnym tabelą przypadków. Odmiana liczebników po polsku, jednostki
(`12 m/s` → „dwanaście metrów na sekundę"), stopnie, kierunki wiatru, godziny,
poziomy ostrzeżeń. To miejsce, gdzie komunikat staje się zrozumiały przez QRM,
więc dostaje najgęstsze testy w projekcie.

### 4.2 Sekwencer

Jeden wątek, własna pętla, wyłączny właściciel `AudioSink` i `PttBackend`.
Komunikacja wyłącznie przez `mpsc`:

```rust
enum SeqCmd {
    Transmit(QueueItem),
    Abort { reason: AbortReason },
    SetInhibit(bool),
    Shutdown,
}
```

Przebieg nadania: pobranie interlocków → `ptt.key()` → odczekanie
`tx_tail_ms` (radio potrzebuje czasu na przełączenie) → odtworzenie PCM z
tokenem anulowania sprawdzanym per bufor → odczekanie `tx_tail_ms` →
`ptt.unkey()` → zapis do `tx_log`.

Każde wyjście z nadania — sukces, błąd, anulowanie, panika — przechodzi
przez jeden strażnik RAII, który opuszcza PTT. Nie ma ścieżki, w której
wyjątek zostawia nadajnik na antenie.

### 4.3 Watchdog twardego limitu

Osobny wątek OS, `Instant`, pętla z krótkim snem. Trzyma `Arc<dyn PttBackend>`
i `AtomicU64` ze znacznikiem podniesienia PTT ustawianym przez sekwencer.
Gdy `now - keyed_at > tx_max_duration` → `force_unkey()` + ustawienie flagi
`poisoned`, którą sekwencer sprawdza przed każdym kolejnym nadaniem i która
wymaga ręcznego zdjęcia w panelu. Limit domyślny 180 s, konfigurowalny.
Watchdog działa z limitem `tx_max_duration + tx_tail + WATCHDOG_GRACE`
(`WATCHDOG_GRACE` = 2 s), bo sekwencer sam ucina nadanie na limicie, a po
audio trzyma jeszcze PTT przez `tx_tail`; watchdog jest drugą linią obrony na
wypadek zawieszenia sekwencera. Bez `tx_tail` w limicie długi ogon radia
zatruwałby stację na zwykłym komunikacie o maksymalnej długości.

Flaga `poisoned` jest celowo lepka: jeśli watchdog musiał interweniować, coś
jest fundamentalnie zepsute i dalsze automatyczne nadawanie byłoby
nieodpowiedzialne.

### 4.4 Interlocki

Przed każdym nadaniem sekwencer przechodzi listę, w tej kolejności:

1. `poisoned` po interwencji watchdoga → odmowa, wymaga ręcznego reset.
2. Globalny kill switch (`settings.tx_enabled`) → odmowa.
3. Opcjonalne wejście GPIO blokujące (`inhibit_pin`) → odmowa.
4. Okna ciszy (`quiet_hours`) → odmowa, chyba że priorytet komunikatu jest
   co najmniej tak wysoki jak `quiet_min_priority` (czyli jego wartość
   liczbowa jest nie większa — patrz §5.1, gdzie `Emergency = 0`).
5. `channel_busy()` → jeśli backend zwraca `Some(true)`, odczekanie z losowym
   backoffem. W MVP każdy backend zwraca `None`, więc krok jest przelotowy;
   konfiguracja wystawia `channel_busy = "none"` i dołożenie COS/RMS nie
   wymaga ruszania sekwencera.

Odmowa nie gubi komunikatu: element wraca do kolejki z `not_before` i powodem
widocznym w panelu.

### 4.5 Automatyczna identyfikacja

Znak wywoławczy dokładany na początku i na końcu każdego nadania, a przy
nadaniu dłuższym niż `id_interval` — także w środku, w najbliższej przerwie
między segmentami. Dwa tryby: głos (ten sam TTS, cache'owany raz) albo CW
(generowany sinus kluczowany z tabeli Morse'a, konfigurowalne WPM i ton).
Konfiguracja: `callsign`, `id_mode`, `id_interval`.

## 5. Model danych

SQLite, `sqlx` z migracjami. Kluczowe tabele:

- **users** — `id, username, password_hash (Argon2id), role, disabled, created_at`
- **sessions** — `id, user_id, expires_at, created_at, user_agent`; sesje w
  bazie, nie tylko podpisane cookie, bo wylogowanie musi być natychmiastowe
- **messages** — definicja komunikatu: `title, kind, body_template, voice,
  priority, interrupting, repeat_count, repeat_interval_s, schedule_kind
  (immediate|at|cron), schedule_at, cron_expr, valid_from, valid_until,
  enabled, source_id, created_by`
- **queue_items** — konkretne zadanie nadania: `message_id?, rendered_text,
  audio_path?, priority, interrupting, state, not_before, repeat_group,
  repeat_index, attempt, created_at, started_at, finished_at, error`
- **schedule_fires** — `(message_id, fire_at)` UNIQUE; idempotencja
  harmonogramu po restarcie
- **tx_log** — `queue_item_id, started_at, ended_at, duration_ms,
  ptt_backend, result, note`; dziennik tego, co faktycznie poszło na pasmo
- **audio_cache** — `hash, path, bytes, created_at, last_used_at`
- **source_state** — `source_id, last_run_at, last_success_at,
  last_payload_hash, consecutive_failures, last_error`
- **settings** — klucz/wartość dla parametrów zmienianych z panelu
- **audit_log** — `at, user_id, action, detail`; kto kazał nadać i kiedy

Rozdział `messages` (co nadawać) od `queue_items` (jedno konkretne nadanie)
jest celowy: powtórzenia, harmonogram i historia operują na zadaniach, a
edycja definicji nie przepisuje przeszłości.

### 5.1 Priorytety i przerywanie

```rust
enum Priority { Emergency = 0, High = 1, Normal = 2, Low = 3 }
```

Kolejka jest sortowana po `(priority, not_before, created_at)`. Element z
`interrupting = true` i priorytetem ściśle wyższym (niższa liczba) niż
trwające nadanie powoduje `SeqCmd::Abort`: wyciszenie w ~150 ms, opuszczenie
PTT, oznaczenie przerwanego elementu jako `interrupted` i zwrot do kolejki,
jeśli jego definicja na to pozwala. Dopiero potem wchodzi element
przerywający — nie ma momentu, w którym oba grają.

### 5.2 Powtórzenia

Zlecenie „nadaj 3 razy co 10 minut" tworzy trzy `queue_items` z tym samym
`repeat_group` i narastającym `not_before`. Anulowanie grupy w panelu usuwa
pozostałe wystąpienia. Rozwinięcie z góry (zamiast przeliczania przy każdym
nadaniu) sprawia, że kolejka w panelu pokazuje prawdę o tym, co się stanie.

## 6. Harmonogram

Zadanie tokio, tik co sekundę. Dla każdego włączonego `messages` z
`schedule_kind in (at, cron)` wyznacza najbliższy czas odpalenia (`croner`),
i jeśli minął, wstawia wiersz do `schedule_fires`. Wstawienie jest
idempotentne po UNIQUE, więc restart nie dubluje nadań, a dwa tiki w tej
samej sekundzie też nie.

**Polityka spóźnień.** Po dłuższym postoju urządzenia nie nadajemy zaległej
lawiny: jeśli czas odpalenia minął dawniej niż `misfire_grace_s` (domyślnie
600 s), wystąpienie jest pomijane z adnotacją w logu — z wyjątkiem priorytetu
`Emergency`, który odpala i tak. Biuletyn pogodowy z zeszłej nocy nie ma
wartości; ostrzeżenie ma.

## 7. Źródła danych

```rust
#[async_trait]
pub trait DataSource: Send + Sync {
    fn id(&self) -> &'static str;
    fn poll_interval(&self) -> Duration;
    async fn fetch(&self, ctx: &SourceCtx) -> Result<SourceOutput, SourceError>;
}

pub struct SourceOutput {
    pub segments: Vec<Segment>,            // tekst, nie audio (D8)
    pub dedup_key: Option<String>,         // zmiana ⇒ nowe zdarzenie
    pub priority: Priority,
    pub valid_until: Option<DateTime<Utc>>,
}
```

Każde źródło żyje we własnym zadaniu tokio z własnym interwałem. Awaria
jednego nie dotyka pozostałych: błąd trafia do `source_state`, kolejne
próby idą z wykładniczym backoffem do sufitu, a panel pokazuje zdrowie
każdego źródła. Źródło niedostępne od dawna jest oznaczone jako przestarzałe
i jego dane wypadają z biuletynu, zamiast nadawać nieświeże liczby jako aktualne.

Dwa tryby użycia wyjścia źródła:

- **Biuletyn** — kompozytor składa segmenty z wielu źródeł w jeden komunikat
  nadawany o porach z harmonogramu.
- **Alert** — zmiana `dedup_key` wstawia komunikat do kolejki natychmiast, z
  priorytetem źródła. `dedup_key` zapobiega powtarzaniu tego samego ostrzeżenia
  przy każdym odpytaniu.

Źródła w MVP: `imgw-meteo` (ostrzeżenia meteorologiczne), `imgw-hydro` (stany
wód), `paa` (poziom promieniowania). `imgw-meteo` jest implementowane pierwsze
i weryfikuje interfejs realnym użyciem; pozostałe dwa dochodzą w fazie 2 planu
i są naturalnym miejscem cięcia zakresu, jeśli plan się przeciągnie — zostaje
wtedy działająca stacja z jednym źródłem.

## 8. Panel www

Axum + Askama (SSR) + HTMX dla żywych fragmentów + SSE (`/events`) dla statusu
TX, kolejki i zdrowia źródeł. Assety i szablony wkompilowane przez `rust-embed`,
więc wdrożenie to jeden plik.

| Ścieżka | Zawartość | Minimalna rola |
|---------|-----------|----------------|
| `/login` | logowanie | — |
| `/` | status TX na żywo, kolejka, ostatnie nadania, zdrowie źródeł, kill switch | viewer |
| `/compose` | szybki komunikat ad-hoc: tekst, podgłos audio, priorytet, „nadaj teraz" | operator |
| `/messages` | CRUD definicji komunikatów, harmonogram, powtórzenia, podgląd | operator |
| `/queue` | kolejka na żywo, anulowanie elementu i grupy | operator |
| `/sources` | status źródeł, wymuszenie odświeżenia, włączanie | operator |
| `/settings` | radio i PTT, urządzenie audio, znak i tryb ID, okna ciszy, limity | admin |
| `/users` | użytkownicy i role | admin |
| `/logs` | dziennik nadań i dziennik audytu | viewer |

Role: **viewer** czyta; **operator** nadaje, planuje i anuluje; **admin**
dodatkowo zmienia konfigurację sprzętu i użytkowników. Kill switch jest
dostępny operatorowi — zatrzymanie nadawania nie może wymagać admina.

Podgląd audio (`GET /api/messages/:id/preview.wav`) przechodzi tym samym
pipeline'em co nadanie, włącznie z normalizacją tekstu. Operator słyszy
dokładnie to, co pójdzie na pasmo.

Uwierzytelnianie: hasła Argon2id, sesja w cookie `HttpOnly` + `SameSite=Lax`,
`Secure` gdy skonfigurowany HTTPS. Ochrona CSRF tokenem per sesja na każdym
formularzu zmieniającym stan. Rate limiting na `/login`. Pierwszy start bez
użytkowników tworzy konto admina z hasłem wypisanym na konsolę i wymusza
zmianę przy pierwszym logowaniu.

## 9. Konfiguracja

Dwa poziomy, rozdzielone po tym, kto i kiedy je zmienia:

- **`aars.toml`** — rzeczy sprzętowe i rozruchowe: adres nasłuchu, ścieżka do
  bazy, urządzenie audio, backend PTT i jego parametry, ścieżka do modelu
  Piper, katalog cache. Zmiana wymaga restartu. Nadpisywalne zmiennymi
  `AARS_*` (figment).
- **`settings` w bazie** — rzeczy operacyjne: kill switch, okna ciszy, znak
  wywoławczy, tryb i interwał ID, limit czasu TX, `misfire_grace_s`, rozmiar
  cache. Zmieniane z panelu. Kill switch i okno ciszy działają od razu;
  *(etap 7: znak wywoławczy, tryb i interwał ID, limit czasu TX oraz
  `misfire_grace_s` są czytane przy starcie stacji, więc wchodzą w życie po
  restarcie — panel mówi to wprost przy formularzu ustawień).*

```toml
[server]
bind = "0.0.0.0:8080"

[audio]
device = "default"
sample_rate = 48000
gain_db = -3.0

[ptt]
backend = "gpio"        # gpio | serial | vox
channel_busy = "none"   # §4.4

[ptt.gpio]
chip = "/dev/gpiochip0"
line = 17
active_low = false
inhibit_line = 27       # opcjonalne wejście blokujące

[tts]
model = "/opt/aars/voices/pl_PL-darkman-medium.onnx"
```

## 10. Obsługa błędów

`thiserror` w bibliotekach, `anyhow` w binarce, `tracing` wszędzie.

- **Awaria syntezy** — element kolejki `failed` z komunikatem w panelu; nie
  blokuje pozostałych.
- **Awaria PTT lub audio** — natychmiastowe `unkey`, element `failed`, flaga
  degradacji widoczna na dashboardzie. Powtarzające się awarie PTT zatrzymują
  automatyczne nadawanie, bo sterowanie nadajnikiem przestało być pewne.
- **Awaria źródła** — izolowana, backoff, dane oznaczone jako przestarzałe.
- **Awaria panelu** — nie dotyka nadawania (D6): sekwencer i harmonogram nie
  zależą od warstwy HTTP.
- **Interwencja watchdoga** — `poisoned`, ręczny reset (§4.3).

## 11. Testowanie

TDD: test przed implementacją, dla każdej jednostki.

- **Normalizacja tekstu** — tabela przypadków: liczebniki i ich odmiana,
  jednostki, godziny, kierunki, poziomy ostrzeżeń, wejścia zdegenerowane.
- **Kolejka** — porządek po priorytecie i czasie, przerywanie, zwrot
  przerwanego elementu, anulowanie grupy powtórzeń.
- **Harmonogram** — wyliczanie cron, idempotencja po restarcie, polityka
  spóźnień wraz z wyjątkiem dla `Emergency`.
- **Sekwencer na fake'ach** — `FakePtt` i `FakeSink` zapisują zdarzenia ze
  znacznikami czasu; asercje na kolejność `key → tail → audio → tail → unkey`,
  na obecność ID, na każdy interlock i na to, że żadna ścieżka błędu nie
  zostawia PTT podniesionego.
- **Watchdog** — przeciągające się nadanie na `FakeSink` musi zakończyć się
  `force_unkey` w zadanym czasie i ustawieniem `poisoned`.
- **Źródła** — odpowiedzi HTTP z plików zapisanych na dysku (`wiremock`),
  włącznie z odpowiedziami zepsutymi i pustymi; zero ruchu sieciowego w testach.
- **Panel** — `oneshot` na routerze Axum z SQLite w pamięci: macierz rola ×
  endpoint, CSRF, cykl życia sesji.

Żaden test nie dotyka karty dźwiękowej, GPIO ani sieci.

## 12. Wdrożenie

Cross-compile na `aarch64-unknown-linux-gnu` (cargo-zigbuild), unit systemd z
`Restart=always`, użytkownik bez uprawnień z dostępem do grup `audio` i `gpio`.
Baza i cache w `/var/lib/aars`. `/healthz` dla nadzoru zewnętrznego.

## 13. Etapy

Plan implementacji w `docs/superpowers/plans/` dzieli pracę na etapy; każdy
kończy się czymś, co da się uruchomić i sprawdzić.

1. Szkielet workspace, konfiguracja, SQLite i migracje, logi, CLI
2. Synteza: normalizacja tekstu, Piper, cache, fake sink
3. PTT: backendy gpio/serial/vox, sekwencer, watchdog, auto-ID, interlocki
4. Kolejka trwała: priorytety, przerywanie, powtórzenia
5. Harmonogram: cron, czas jednorazowy, idempotencja, polityka spóźnień
6. Panel: auth, role, dashboard, compose, kolejka, SSE
7. Panel: CRUD komunikatów, ustawienia, użytkownicy, logi
8. `DataSource` + `imgw-meteo`
9. Panel admina: konfiguracja sprzętu i stacji z panelu (PTT: backend, linia GPIO,
   port szeregowy; audio: urządzenie, wzmocnienie; głos Piper; sieć/port panelu),
   test PTT i test audio bez nadawania na antenę, oraz moduły (wtyczki): lista
   funkcji stacji (źródła, harmonogram, auto-ID, biuletyn) z włącz/wyłącz
   i ustawieniami każdego modułu. Plik `aars.toml` zostaje tylko dla ścieżek
   i bootstrapu; ustawienia z panelu mają pierwszeństwo. Pozycja stacji
   (szerokość/długość albo lokator QTH) wybierana na mapie (Leaflet + podkład
   OpenStreetMap) — używają jej moduły pogodowe.
10. Obraz Raspberry Pi i autoaktualizacja: gotowy obraz `.img` (Raspberry Pi OS
    Lite 64-bit, Piper z polskim głosem, usługa systemd) w wydaniach GitHub;
    pierwsze uruchomienie bez SSH (Wi-Fi i hasło z Raspberry Pi Imager albo
    kreator pierwszego admina w panelu); autoaktualizacja z GitHub Releases
    na żądanie admina: suma kontrolna, podmiana binarki, restart, powrót do
    poprzedniej wersji, gdy nowa nie wstanie; nigdy w trakcie nadawania.
11. `imgw-hydro`, `paa`, kompozytor biuletynu, moduł pogody z otwartego API
    po współrzędnych stacji: Open-Meteo (bez klucza, CC BY 4.0 — „Źródło:
    Open-Meteo”), zapasowo MET Norway (`api.met.no`, wymaga User-Agent
    z kontaktem); prognoza o ustalonych godzinach i komunikat po przekroczeniu
    progów (wiatr, mróz, opady) ustawianych w panelu; mapa w panelu: powiaty
    pod ostrzeżeniem IMGW kolorem stopnia + pozycja stacji (granice powiatów
    z PRG GUGiK, uproszczone). Oba API sprawdzone zapytaniem 2026-11-20.

(2026-11-20, właściciel: „rób od razu obraz na raspberry pi… moduł autoupdate…
panel admina… zmienne… znak stacji… jakie funkcje uruchamiamy — system
pluginów”. Etapy 9-10 wstawione przed źródłami hydro/PAA, żeby obraz do testów
był wcześniej.)
