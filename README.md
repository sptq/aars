# AARS: Automatic Alert Radio Station

Automatyczna stacja ostrzegawcza nadająca komunikaty głosowe przez radio.
Stoi przy radiotelefonie (typowo Raspberry Pi), syntezuje mowę lokalnie
(Piper, bez internetu), nadaje komunikaty o stałych porach, jednorazowo albo
natychmiast, a operator może wpisać i nadać własny komunikat z panelu www.

**Status: w trakcie prac** (etapy 1–9 z 11, patrz `docs/superpowers/plans/README.md`).

## Co działa

- synteza mowy przez binarkę `piper` z normalizacją polskiego tekstu
  (liczby, jednostki, skróty) i cache audio,
- sterowanie nadawaniem: GPIO, DTR/RTS po porcie szeregowym, VOX,
- sekwencer z interlockami: twardy limit czasu TX (watchdog w osobnym wątku),
  automatyczna identyfikacja stacji telegrafią, globalna blokada nadawania,
- trwała kolejka w SQLite z priorytetami, przerywaniem i powtórzeniami,
- harmonogram: cron, czas jednorazowy, polityka spóźnień po restarcie,
- panel www (Axum + Askama + HTMX): logowanie, role, CSRF, nadawanie,
  kolejka na żywo, zarządzanie komunikatami i harmonogramem, ustawienia,
  użytkownicy, logi (szczegóły w sekcji „Panel”),
- źródło danych IMGW: ostrzeżenia meteorologiczne dla wybranego obszaru trafiają
  do kolejki jako polski komunikat (sekcja „Źródła danych”),
- konfiguracja sprzętu, pozycji stacji i modułów z panelu admina, test PTT
  i test audio, restart stacji jednym przyciskiem (sekcja „Konfiguracja
  z panelu”).

## Panel

Strony i minimalna rola (niższa rola dostaje 403, niezalogowany jest
kierowany na `/login`):

| Strona | Minimalna rola |
|---|---|
| `/`, `/logs`, `/account/password` | viewer |
| `/sources` (stan, włączanie, „Odśwież teraz”; obszar i interwał tylko do odczytu) | operator |
| `/modules` (stan modułów) | operator do odczytu, admin do zmian |
| `/compose`, `/messages` (lista, nowy, edycja, podgląd audio, nadaj teraz), `/queue` (w tym anulowanie grupy), `/preview` | operator |
| `/settings`, `/users`, czyszczenie blokady strażnika | admin |
| `/settings/hardware` (sprzęt), test PTT i test audio, `/station/restart`, `/settings/position` (pozycja stacji) | admin |
| włączanie modułów i ich ustawienia na `/modules` (w tym obszar i interwał źródeł, przeniesione z `/sources`) | admin |

Każdy formularz zmieniający stan wymaga tokenu CSRF. Wszystkie czasy w panelu
są pokazane podwójnie: w UTC i w czasie lokalnym urządzenia
(np. `18:00 UTC (19:00 lokalnie)`); wzorzec cron wpisuje się w UTC.

## Źródła danych

Źródło `imgw-meteo` odpytuje publiczne API ostrzeżeń meteorologicznych IMGW
(adres w `[sources]` w `aars.toml`) i dokłada do kolejki jeden komunikat na
ostrzeżenie: stopień 1 ma priorytet Zwykły, stopień 2 Wysoki, stopień 3
Alarmowy i przerywa nadawanie. Powtórne pobranie tej samej treści nic nie
dokłada; podniesiony stopień daje jeden komunikat „Zmiana ostrzeżenia”,
a ostrzeżenie, które zniknie z odpowiedzi dwa razy z rzędu, jest odwołane.

Po instalacji źródło jest wyłączone. W panelu, w tej kolejności:

1. administrator na stronie `/modules` ustawia **obszar** (jedna linia na wpis: dwucyfrowy kod
   województwa, np. `10`, albo czterocyfrowy kod powiatu TERYT z nazwą, którą
   stacja ma wypowiadać, np. `1061 Łódź`; najwyżej 40 wpisów) oraz
   **interwał** odpytywania (60–3600 s, domyślnie 300 s). Bez obszaru źródło
   nic nie pobiera i nic nie ogłasza,
2. operator (na `/sources`) albo administrator (na `/modules`) włącza źródło;
   operator może też kliknąć „Odśwież teraz” na `/sources`.

Stan źródła (działa, błąd, przestarzałe, wyłączone, brak obszaru) widać też na
pulpicie. Podgląd bez zapisu i bez kolejki: `cargo run -p aars -- source preview`
pobiera ostrzeżenia dla zapisanego obszaru i wypisuje tekst, priorytet oraz
postać znormalizowaną do syntezy.

## Konfiguracja z panelu

Na świeżej instalacji w `aars.toml` wystarczą ścieżki (`[storage]`,
`[tts] piper_bin` i `voices_dir`, `[sources]`). Resztę administrator ustawia
w panelu:

- **Ustawienia → Sprzęt** (`/settings/hardware`): sterowanie PTT (VOX, GPIO,
  port szeregowy), urządzenie audio, częstotliwość próbkowania, poziom
  wyjścia, opóźnienie PTT, głos (plik `*.onnx` z `*.onnx.json` obok,
  z katalogu `voices_dir`) i adres panelu. Listy urządzeń, portów i układów
  GPIO pochodzą z hosta, ale przy sprawdzaniu niczego się nie otwiera — port
  szeregowy otwarty na próbę mógłby podnieść DTR/RTS i kluczować radio.
- **Ustawienia → Położenie stacji** (`/settings/position`): szerokość
  i długość albo lokator QTH (4, 6 lub 8 znaków). Używają tego moduły
  pogodowe.
- **Moduły** (`/modules`): włączanie harmonogramu i źródeł oraz ich
  ustawienia. Automatycznej identyfikacji nie da się wyłączyć.

**Co działa od razu, a co po restarcie.** Od razu: wyłącznik nadawania, okno
ciszy (sprawdzane co 30 s), włączanie modułów (źródło od następnego cyklu,
harmonogram od następnego tiku), ustawienia modułów i pozycja stacji (od
następnego cyklu źródła). Po restarcie stacji: cały sprzęt oraz znak, tryb
i odstęp identyfikacji, limit czasu nadawania i tolerancja spóźnień. Strona
sprzętu pokazuje listę „czeka na restart”, a przycisk **Restart stacji**
uruchamia ją ponownie w tym samym procesie. Restart nigdy nie przerywa
nadania: stacja dokańcza bieżący komunikat, zwalnia PTT i kartę, a dopiero
potem wstaje z nowymi ustawieniami.

**Pierwszeństwo.** Wartości domyślne < `aars.toml` < zmienne `AARS_*` <
panel. Dopóki administrator niczego nie zapisze na stronie sprzętu, działa to,
co jest w pliku. Wartość z panelu, której nie da się odczytać, wraca do
wartości z pliku, a błąd widać na stronie sprzętu.

**Powrót do ostatniej działającej konfiguracji.** Przy każdym starcie stacja
próbuje po kolei: ustawień z panelu, ostatniej konfiguracji z panelu, która
działała, i samego pliku. Bierze pierwszą, z której wstaną PTT i wyjście
audio. Gdy ustawienia z panelu zawiodą (np. odłączony adapter USB), zapisane
wartości zostają, a błąd widać na pulpicie, na stronie sprzętu i w dzienniku
audytu (`sprzęt nie wstał`). Gdy nie wstanie nic, stacja działa **bez
nadawania**: panel, harmonogram i źródła pracują, kolejka czeka, a pulpit
pokazuje `Nadawanie niedostępne: …`.

**Ratunek bez panelu.** `aars hardware show` wypisuje konfigurację sprzętu
z pochodzeniem każdego klucza (panel albo plik) i stan ostatniego
zastosowania. `aars hardware reset` kasuje wszystkie ustawienia sprzętu
z panelu; przy następnym starcie obowiązuje `aars.toml`.

**Test PTT i test audio** (strona sprzętu, tylko admin, wpis w audycie):

- test PTT włącza nadajnik na 1–5 s (domyślnie 2) po zaznaczeniu
  potwierdzenia, bez identyfikacji. Przechodzi przez te same blokady co każde
  nadanie (wyłącznik, blokada strażnika, wejście blokujące, okno ciszy)
  i trafia do dziennika nadań,
- test audio gra ton 1 kHz i krótką syntezę na skonfigurowanym wyjściu przy
  **wyłączonym** PTT, najwyżej 15 s. Radio nie może być ustawione na VOX, bo
  wtedy dźwięk poszedłby na antenę,
- przy backendzie VOX oba testy odmawiają; właściwym testem jest wtedy krótki
  komunikat z `/compose`, który idzie z identyfikacją.

**Mapa.** Leaflet jest wkompilowany w binarkę, więc panel działa bez
internetu. Podkład mapy (OpenStreetMap) pobiera przeglądarka, nie stacja; bez
internetu mapa się nie wyświetli, ale formularz współrzędnych i lokatora
działa w pełni. Lokator wylicza stacja przy zapisie.

## W planach

Według spec §13:

- etap 10: gotowy obraz Raspberry Pi i autoaktualizacja z GitHub Releases,
- etap 11: źródła IMGW hydro i PAA z kompozytorem biuletynu, moduł pogody
  (Open-Meteo, zapasowo MET Norway) i mapa ostrzeżeń w panelu.

## Uruchomienie

```sh
cp aars.example.toml aars.toml   # ścieżki; sprzęt ustawisz w panelu
cargo run -p aars -- migrate
cargo run -p aars -- say "Wiatr 12 m/s"      # synteza do WAV
cargo run -p aars -- run                      # stacja + panel na :8080
```

Wymaga Rust 1.93+ i binarki [Piper](https://github.com/rhasspy/piper) z
polskim głosem (np. `pl_PL-darkman-medium`).

Licencja: MIT. Autor: Marcin SP7TQ.
