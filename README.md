# AARS: Automatic Alert Radio Station

Automatyczna stacja ostrzegawcza nadająca komunikaty głosowe przez radio.
Stoi przy radiotelefonie (typowo Raspberry Pi), syntezuje mowę lokalnie
(Piper, bez internetu), nadaje komunikaty o stałych porach, jednorazowo albo
natychmiast, a operator może wpisać i nadać własny komunikat z panelu www.

**Status: w trakcie prac** (etapy 1–6 z 10, patrz `docs/superpowers/plans/README.md`).

## Co działa

- synteza mowy przez binarkę `piper` z normalizacją polskiego tekstu
  (liczby, jednostki, skróty) i cache audio,
- sterowanie nadawaniem: GPIO, DTR/RTS po porcie szeregowym, VOX,
- sekwencer z interlockami: twardy limit czasu TX (watchdog w osobnym wątku),
  automatyczna identyfikacja stacji telegrafią, globalna blokada nadawania,
- trwała kolejka w SQLite z priorytetami, przerywaniem i powtórzeniami,
- harmonogram: cron, czas jednorazowy, polityka spóźnień po restarcie,
- panel www (Axum + Askama + HTMX): logowanie, role, CSRF, nadawanie,
  kolejka na żywo.

## W planach

Reszta panelu (komunikaty, ustawienia, użytkownicy, logi), źródła danych
IMGW meteo / hydro i PAA z kompozytorem biuletynu, paczka na Raspberry Pi
z systemd.

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
