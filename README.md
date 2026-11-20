# AARS: Automatic Alert Radio Station

Automatyczna stacja ostrzegawcza nadająca komunikaty głosowe przez radio.
Stoi przy radiotelefonie (typowo Raspberry Pi), syntezuje mowę lokalnie
(Piper, bez internetu), nadaje komunikaty o stałych porach, jednorazowo albo
natychmiast, a operator może wpisać i nadać własny komunikat z panelu www.

**Status: w trakcie prac** (etapy 1–8 z 11, patrz `docs/superpowers/plans/README.md`).

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
  do kolejki jako polski komunikat (sekcja „Źródła danych”).

## Panel

Strony i minimalna rola (niższa rola dostaje 403, niezalogowany jest
kierowany na `/login`):

| Strona | Minimalna rola |
|---|---|
| `/`, `/logs`, `/account/password` | viewer |
| `/sources` (stan, włączanie, „Odśwież teraz”) | operator |
| `/compose`, `/messages` (lista, nowy, edycja, podgląd audio, nadaj teraz), `/queue` (w tym anulowanie grupy), `/preview` | operator |
| `/settings`, `/users`, czyszczenie blokady strażnika, obszar i interwał źródeł | admin |

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

Po instalacji źródło jest wyłączone. W panelu, na stronie `/sources`, w tej
kolejności:

1. administrator ustawia **obszar** (jedna linia na wpis: dwucyfrowy kod
   województwa, np. `10`, albo czterocyfrowy kod powiatu TERYT z nazwą, którą
   stacja ma wypowiadać, np. `1061 Łódź`; najwyżej 40 wpisów) oraz
   **interwał** odpytywania (60–3600 s, domyślnie 300 s). Bez obszaru źródło
   nic nie pobiera i nic nie ogłasza,
2. operator włącza źródło i klika „Odśwież teraz”.

Stan źródła (działa, błąd, przestarzałe, wyłączone, brak obszaru) widać też na
pulpicie. Podgląd bez zapisu i bez kolejki: `cargo run -p aars -- source preview`
pobiera ostrzeżenia dla zapisanego obszaru i wypisuje tekst, priorytet oraz
postać znormalizowaną do syntezy.

## W planach

Według spec §13:

- etap 9: konfiguracja sprzętu i stacji z panelu admina (PTT, audio, głos,
  sieć, pozycja stacji na mapie) oraz moduły włączane i ustawiane z panelu,
- etap 10: gotowy obraz Raspberry Pi i autoaktualizacja z GitHub Releases,
- etap 11: źródła IMGW hydro i PAA z kompozytorem biuletynu, moduł pogody
  (Open-Meteo, zapasowo MET Norway) i mapa ostrzeżeń w panelu.

## Uruchomienie

```sh
cp aars.example.toml aars.toml   # dostosuj audio, PTT, ścieżkę do modelu Piper
cargo run -p aars -- migrate
cargo run -p aars -- say "Wiatr 12 m/s"      # synteza do WAV
cargo run -p aars -- run                      # stacja + panel na :8080
```

Wymaga Rust 1.93+ i binarki [Piper](https://github.com/rhasspy/piper) z
polskim głosem (np. `pl_PL-darkman-medium`).

Licencja: MIT. Autor: Marcin SP7TQ.
