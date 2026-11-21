# AARS — plan implementacji

**Spec:** `docs/superpowers/specs/2026-10-13-aars-design.md`

Praca podzielona na 11 etapów (spec §13). Każdy etap kończy się czymś, co da się
uruchomić i sprawdzić, i ma własny plik planu z zadaniami w cyklu TDD.

| Etap | Plik | Cel | Kryterium wyjścia |
|------|------|-----|-------------------|
| 1 | `2026-10-13-etap-01-szkielet.md` | Workspace, konfiguracja, SQLite + migracje, logi, CLI | `aars migrate` tworzy schemat, `aars info` wypisuje konfigurację; testy konfiguracji i migracji przechodzą |
| 2 | `2026-10-13-etap-02-synteza.md` | Normalizacja tekstu PL, trait `Synthesizer`, backend Piper, cache audio | `aars say "wiatr 12 m/s"` zapisuje WAV; tabela przypadków normalizacji przechodzi |
| 3 | `2026-10-13-etap-03-nadawanie.md` | Backendy PTT, sekwencer, watchdog, auto-ID, interlocki | `aars tx "test"` nadaje przez wybrany backend; testy na fake'ach dowodzą, że żadna ścieżka nie zostawia PTT podniesionego |
| 4 | `2026-10-13-etap-04-kolejka.md` | Trwała kolejka: priorytety, przerywanie, powtórzenia | Komunikat `Emergency` przerywa trwające nadanie; kolejka ocala restart |
| 5 | `2026-10-13-etap-05-harmonogram.md` | Cron, czas jednorazowy, idempotencja, polityka spóźnień | Komunikat cykliczny odpala raz na okres także po restarcie; zaległości starsze niż `misfire_grace_s` są pomijane poza `Emergency` |
| 6 | `2026-10-13-etap-06-panel-rdzen.md` | Auth, role, dashboard, compose, kolejka, SSE | Logowanie działa, operator nadaje komunikat z panelu, status TX aktualizuje się na żywo |
| 7 ✓ | `2026-11-20-etap-07-panel-reszta.md` | CRUD komunikatów, ustawienia, użytkownicy, logi | Harmonogram konfigurowalny z panelu; macierz rola × endpoint przechodzi |
| 8 ✓ | `2026-11-20-etap-08-imgw-meteo.md` | `trait DataSource` + `imgw-meteo` | Ostrzeżenie meteo pojawia się w kolejce bez powtarzania przy każdym odpytaniu |
| 9 ✓ | `2026-11-20-etap-09-panel-admin.md` | Panel admina: konfiguracja sprzętu i stacji (PTT, audio, głos, sieć, pozycja stacji na mapie), test PTT i audio bez anteny, moduły włącz/wyłącz z ustawieniami | Ustawienia z panelu mają pierwszeństwo przed `aars.toml` i działają po restarcie; świeża instalacja z samymi ścieżkami w `aars.toml` konfiguruje się w całości z panelu, każda zmiana w audycie; test audio nie nadaje na antenę, test PTT kluczuje najwyżej 5 s po jawnym potwierdzeniu (spec §13 pkt 9: „gdzie to możliwe”); restart nigdy w trakcie nadania |
| 10 ✓ | `2026-11-21-etap-10-obraz-rpi.md` | Obraz Raspberry Pi (`.img`, Piper z polskim głosem, systemd), kreator pierwszego admina i autoaktualizacja z GitHub Releases (podpis minisign, strażnik startu, powrót z kopią bazy) | Świeży obraz startuje bez SSH; aktualizacja sprawdza sumę, podmienia binarkę i wraca do poprzedniej wersji, gdy nowa nie wstanie, nigdy w trakcie nadawania |
| 11 | (plan powstanie przed etapem) | `imgw-hydro`, `paa`, kompozytor biuletynu, pogoda Open-Meteo (zapasowo MET Norway), mapa ostrzeżeń w panelu | Biuletyn składa segmenty ze źródeł; źródło przestarzałe wypada z biuletynu; prognoza i komunikat po przekroczeniu progów |

## Kolejność i cięcie zakresu

Etapy 1–7 to działająca stacja sterowana ręcznie z panelu — to jest MVP i
nie podlega cięciu. Etap 8 weryfikuje interfejs źródeł realnym użyciem.
Etapy 9–10 (konfiguracja z panelu, obraz Raspberry Pi) idą przed kolejnymi
źródłami, żeby obraz do testów był wcześniej (spec §13, zmiana z 2026-11-20).
Etap 11 jest naturalnym miejscem cięcia, jeśli praca się przeciąga: bez niego
zostaje stacja z jednym źródłem danych, w pełni użyteczna.

## Zasada pisania planów

Plany etapów 1–3 są rozpisane z góry. Plany etapów 4–11 powstają
bezpośrednio przed wykonaniem danego etapu, na podstawie kodu, który już
istnieje — plan napisany teraz dla etapu 9 opisywałby API, których jeszcze
nie ma, i byłby fikcją do wyrzucenia. Kryteria wyjścia z tabeli powyżej są
wiążące i nie zmieniają się (poza zmianą podziału etapów 9–11 w spec §13).

## Odstępstwa od speca

- **D4 (synteza).** Spec mówi „Piper (ONNX)". Crate `piper-rs` 0.2.0 nie
  zbudował dokumentacji na docs.rs i ciągnie natywne `espeak-rs` + `ort`,
  więc nie jest podstawą, na której można oprzeć MVP. Domyślnym backendem
  traitu `Synthesizer` jest wołanie binarki `piper` jako podprocesu
  (udokumentowana zależność systemowa). Native `piper-rs` zostaje za feature
  flagiem jako późniejszy spike. Trait się nie zmienia, więc podmiana nie
  dotyka wywołujących.
- **Platforma rozwoju.** Praca idzie na macOS arm64, cel to Linux aarch64.
  Backend PTT GPIO i urządzenia ALSA nie istnieją na macOS, więc backendy
  zależne od Linuksa są za `#[cfg(target_os = "linux")]` i feature flagami.
  Testy chodzą na fake'ach na obu platformach.
- **§12 Wdrożenie: budowa binarki (etap 10, R1).** Spec przewiduje
  cross-compile przez cargo-zigbuild. `cpal` potrzebuje `libasound` dla
  aarch64, a `libsqlite3-sys` kompilatora C, więc zamiast sysrootu binarka
  powstaje natywnie na runnerze GitHub `ubuntu-24.04-arm` (glibc 2.39). Obraz
  bazuje na Raspberry Pi OS Lite 64-bit Trixie (glibc 2.41); Bookworm (glibc
  2.36) nie jest wspierany. Gdyby darmowe runnery arm64 zniknęły, wraca
  cargo-zigbuild z sysrootem Debiana (jedno zadanie CI).
- **Etap 10, R13 (zadanie 10) przycięte.** Import hasła ustawionego
  w Raspberry Pi Imager odpadł na razie: pierwsze konto zakłada kreator
  `/setup`. Zostaje reset kont admina plikiem `aars-reset-admin` z partycji
  boot, ale konta wyłącza usługa `aars` przy starcie, a nie root w osobnej
  usłudze (`aars-reset-admin.service` tylko przekazuje prośbę). Przechodzi
  też `aars-reset-admin.txt` (Notatnik w Windows dopisuje rozszerzenie).
- **Etap 10, R15: `TimeoutStopSec=660` zamiast 30 s.** SIGTERM przerywa
  nadanie przez sekwencer, więc zwykłe zatrzymanie trwa sekundy. Limit
  obejmuje najgorszy ograniczony przypadek: najdłuższe nadanie do strażnika
  TX (600 s + 2 s opóźnienia PTT + 2 s zapasu) i 30 s drenażu. SIGKILL
  wcześniej zostawiłby linię PTT jądru.
- **Etap 10, R15: `StateDirectoryMode=0750`** (plus `d /var/lib/aars 0750
  aars aars -` w tmpfiles), żeby baza z hashami haseł nie była czytelna dla
  innych kont na Pi.
- **Etap 10, R14: obraz słucha na `[::]:8080`, a nie na `0.0.0.0:8080`.**
  Jedno gniazdo dla IPv6 i IPv4 (Linux, `bindv6only = 0`), więc kreator
  działa też z globalnego IPv6 w sieci `/64` stacji. Bez IPv6 w jądrze panel
  sam przechodzi na `0.0.0.0` tego samego portu.
