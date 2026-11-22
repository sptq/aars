# AARS: Automatic Alert Radio Station

Automatyczna stacja ostrzegawcza nadająca komunikaty głosowe przez radio.
Stoi przy radiotelefonie (typowo Raspberry Pi), syntezuje mowę lokalnie
(Piper, bez internetu), nadaje komunikaty o stałych porach, jednorazowo albo
natychmiast, a operator może wpisać i nadać własny komunikat z panelu www.

**Status: w trakcie prac** (etapy 1–10 z 11, patrz `docs/superpowers/plans/README.md`).

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
- źródła danych: ostrzeżenia IMGW dla wybranego obszaru oraz alarmy (komunikaty
  RSO, pogoda kosmiczna NOAA, smog GIOŚ, wstrząsy EMSC) trafiają do kolejki jako
  polski komunikat (sekcja „Źródła danych”),
- biuletyn o stałych porach lokalnych: pogoda dla pozycji stacji, propagacja
  KF i obowiązujące ostrzeżenia, z podglądem w panelu i w CLI (sekcja
  „Biuletyn (etap 11b)”),
- konfiguracja sprzętu, pozycji stacji i modułów z panelu admina, test PTT
  i test audio, restart stacji jednym przyciskiem (sekcja „Konfiguracja
  z panelu”),
- gotowy obraz Raspberry Pi, kreator pierwszego konta admina i aktualizacja
  z panelu z powrotem do poprzedniej wersji (sekcja „Raspberry Pi: obraz
  i aktualizacja”).

## Panel

Strony i minimalna rola (niższa rola dostaje 403, niezalogowany jest
kierowany na `/login`):

| Strona | Minimalna rola |
|---|---|
| `/`, `/logs`, `/account/password` | viewer |
| `/sources` (stan, włączanie, „Odśwież teraz”; obszar i interwał tylko do odczytu) | operator |
| `/modules` (stan modułów, „Podgląd biuletynu”) | operator do odczytu i podglądu, admin do zmian |
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
pulpicie. Podgląd bez zapisu i bez kolejki: `cargo run -p aars -- source preview [id]`
(domyślnie `imgw-meteo`) pobiera dane dla zapisanego obszaru albo pozycji
stacji i wypisuje tekst, priorytet oraz postać znormalizowaną do syntezy.

### Źródła alarmowe (etap 11a)

Wszystkie są po instalacji wyłączone; włącza się je tak samo jak `imgw-meteo`.
`swpc` nie potrzebuje niczego, `rso` (jak `imgw-meteo`) potrzebuje obszaru,
`gios-smog`, `emsc` i `pogoda` — pozycji stacji (ustawienia). Adresy w `[sources]` w `aars.toml`.

| Źródło | Dane | Licencja i atrybucja |
|---|---|---|
| `imgw-meteo` | IMGW-PIB, ostrzeżenia meteorologiczne | dane publiczne IMGW |
| `rso` | Regionalny System Ostrzegania (komunikaty.tvp.pl): Alert RCB, komunikaty wojewódzkie i sanepidu | brak jawnej licencji, dane publiczne administracji; w treści „Komunikat RSO” |
| `swpc` | NOAA SWPC, skale G/S/R pogody kosmicznej | domena publiczna; „Dane: NOAA SWPC” |
| `gios-smog` | GIOŚ, ostrzeżenia smogowe w promieniu stacji | CC BY 4.0; „Źródło: GIOŚ” |
| `emsc` | EMSC, wstrząsy sejsmiczne w promieniu stacji | CC BY 4.0; „Dane: EMSC” |
| `pogoda` (etap 11b) | Open-Meteo, prognoza dla pozycji stacji: komunikat po przekroczeniu progów (porywy od 75 km/h, mróz od −15 °C, dobowa suma opadów od 30 mm — progi w `/modules`) na dziś lub jutro | CC BY 4.0; „Dane: Open-Meteo.” |

Zmiana zakresu RSO w etapie 11b (zakres nie zależy już od pola „atak
z powietrza”, bo ta kategoria jest pobierana zawsze): przy pierwszym cyklu po
aktualizacji stacje, które miały to pole odznaczone, jednorazowo i po cichu
porzucają śledzone pozycje RSO (jak przy każdej zmianie obszaru) — bez
fałszywych odwołań na antenie. Następny cykl zaczyna śledzenie od nowa.

### Biuletyn (etap 11b)

Moduł `bulletin` składa w chwili nadania jeden komunikat z części i wstawia
go do kolejki (priorytet Zwykły, bez przerywania; nadaje dyspozytor jak
każdy inny element). Po instalacji jest wyłączony. Na `/modules`
administrator:

1. ustawia **pory nadania** — HH:MM w **czasie lokalnym** (Europe/Warsaw, nie
   UTC jak cron), po przecinku, najwyżej 8, domyślnie `07:00,19:00`;
2. wybiera **części** (kolejność w biuletynie jest stała, nie kolejność
   zaznaczenia) i opcjonalnie **wstęp** (pusty = „Biuletyn stacji {znak}.”);
3. włącza moduł.

Pora, która przypadnie w oknie ciszy nocnej (a priorytet Zwykły jest
poniżej progu ciszy), jest pominięta, nie przesunięta; okno kończące się
dokładnie o porze jej nie blokuje. Po przestoju stacja nie nadrabia: pora
spóźniona o więcej niż `misfire_grace_s` (od 60 s do 15 min) jest pomijana,
z kilku zaległych składana jest tylko najnowsza, a restart nie dubluje pory
już zrobionej. Gdy żadna część nie ma treści, nic nie idzie na antenę.

| Część | Treść | Dane |
|---|---|---|
| `pogoda` | teraz, noc, jutro (temperatura, opis, opady, porywy) dla pozycji stacji; bez pozycji część wypada | Open-Meteo (ta sama prognoza co źródło `pogoda`, świeża do 60 min), zapasowo MET Norway |
| `propagacja` | strumień słoneczny (SFI), indeks K, skale NOAA, najbliższa jonosonda i ocena pasm KF | NOAA SWPC, GIRO przez KC2G |
| `ostrzezenia` | obowiązujące ostrzeżenia ze źródeł włączonych i nieprzestarzałych, które poszły na antenę oraz czekające w kolejce (najwyżej 5, reszta — także bez opisu — jako „Pozostałych ostrzeżeń: n.”); brak = „Brak obowiązujących ostrzeżeń.”, a gdy część źródeł jest niedostępna — „Brak ostrzeżeń z dostępnych źródeł. Część źródeł ostrzeżeń jest niedostępna.”; bez żadnego włączonego źródła część wypada | `source_state` stacji |

Każda część ma 20 s na odpowiedź (części liczone równolegle); część, która
nie zdąży albo zawiedzie, wypada, reszta wychodzi. Progi oceny w części
`propagacja` są stałe:

- SFI: poniżej 90 niski, 90–149 umiarkowany, od 150 wysoki (dane do 36 h);
- Kp: poniżej 3 spokojne, 3–4,9 niespokojne, od 5 burza geomagnetyczna (dane do 6 h);
- jonosonda: najbliższa w promieniu 1200 km, pomiar nie starszy niż 60 min,
  skalowanie ręczne (`cs == -1`) albo pewność `cs` ≥ 70;
- łączności bliskie (NVIS, pasma 160/80/60/40/30 m): najwyższe pasmo o dolnej
  krawędzi poniżej foF2 × 1,0;
- łączności dalekie (DX, pasma 80…10 m): najwyższe pasmo o dolnej krawędzi
  poniżej MUF(3000) × 0,85.

Limit długości biuletynu to `min(1500, 10 × (limit nadawania w s − 10))`
znaków (10 s zostaje na identyfikację i ogony):
gdy złożony tekst się nie mieści, wypadają całe części od końca (wstęp
i „Koniec biuletynu.” zostają).

Podgląd — ta sama ścieżka co nadanie, bez zapisu i bez kolejki:
`aars bulletin preview` drukuje tekst (albo „nic do nadania: powód”) i stan
każdej części; w panelu przycisk „Podgląd biuletynu” w karcie biuletynu
(operator) pokazuje to samo z odsłuchem. Podgląd pyta źródła o dane, więc
naraz trwa najwyżej jeden.

Licencje danych biuletynu (stacja jest niekomercyjna): Open-Meteo CC BY 4.0
(„Dane: Open-Meteo.”), MET Norway CC BY 4.0 / NLOD („Dane: MET Norway.”),
NOAA SWPC domena publiczna, jonosondy GIRO przez KC2G CC BY-NC-SA („Dane:
NOAA, GIRO przez KC2G.”, KC2G tylko gdy jonosonda użyta). Adresy
(`open_meteo_url`, `met_no_url`, `swpc_f107_url`, `swpc_kp_url`, `kc2g_url`)
w `[sources]` w `aars.toml`.

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

## Raspberry Pi: obraz i aktualizacja

### Nagranie karty

Każde wydanie na GitHubie ma obraz `aars-{wersja}-rpi.img.xz`: Raspberry Pi
OS Lite 64-bit (Trixie) z AARS, Piperem 1.8.0 i polskim głosem
`pl_PL-darkman-medium`. Obsługiwane są Raspberry Pi 3, 4, 5 i Zero 2 W
(64-bit). Raspberry Pi OS Bookworm nie jest wspierany.

1. W Raspberry Pi Imager ustaw własne repozytorium systemów (ustawienia
   aplikacji, „Content Repository”, albo `rpi-imager --repo …`) na
   `https://github.com/sptq/aars/releases/latest/download/aars-os-list.json`.
   AARS pojawi się na liście systemów. Można też pobrać plik `.img.xz`
   z wydania i wybrać **Use custom**. Personalizacja Imagera przy „Use
   custom” nie jest jeszcze sprawdzona na sprzęcie.
2. W personalizacji Imagera ustaw Wi-Fi, nazwę hosta (domyślnie `aars`)
   i, jeśli chcesz, konto do SSH. Konto systemowe z Imagera **nie** jest
   kontem panelu: import hasła z Imagera nie jest jeszcze obsługiwany.
3. Bez Imagera: podłącz Pi kablem Ethernet albo wpisz sieć Wi-Fi do pliku
   `network-config` na partycji boot karty (cloud-init, format sieci v2).
4. Włóż kartę i włącz Pi. Pierwszy start trwa kilka minut (powiększenie
   partycji, cloud-init). Monitor i SSH nie są potrzebne.

### Pierwsze konto

Otwórz `http://aars.local:8080` (albo `http://{adres IP stacji}:8080`)
z komputera w tej samej sieci lokalnej.
Panel przekieruje na kreator `/setup`, w którym zakładasz konto
administratora (nazwa 3–32 znaki `a-z 0-9 . _ -`, hasło co najmniej
10 znaków). Kreator działa tylko:

- dopóki nie ma żadnego włączonego admina,
- z sieci lokalnej stacji: pętla zwrotna, RFC 1918, link-local, ULA
  `fc00::/7` oraz IPv6 z tym samym prefiksem `/64` co adres stacji,
- pod nazwą albo adresem stacji (`aars.local`, nazwa hosta, adres IP).

Uwaga: VPN z adresami ULA (np. Tailscale) też liczy się jako sieć lokalna.
Nie stawiaj przed panelem reverse proxy na samym Pi: wtedy każdy klient
przychodzi z pętli zwrotnej.

**Reset konta admina.** Gdy ktoś zajął stację pierwszy albo hasło zginęło,
wyłącz Pi, połóż na partycji boot karty pusty plik `aars-reset-admin`
i włącz Pi. Przechodzi też `aars-reset-admin.txt` (Notatnik w Windows
dopisuje `.txt`, a Eksplorator domyślnie je ukrywa), a wielkość liter nie ma
znaczenia. Przy starcie wszystkie konta admina zostają wyłączone, ich sesje
zakończone, plik usunięty, a w audycie pojawia się `admin_reset`. Kreator
`/setup` otwiera się znowu (ta sama nazwa konta dostaje nowe hasło). Konta
operatorów i obserwatorów zostają.

### Usługa

AARS działa jako usługa `aars.service` użytkownika `aars` (grupy `audio`,
`gpio`, `dialout`). Konfiguracja: `/etc/aars/aars.toml`, baza i głosy:
`/var/lib/aars`, wydania: `/opt/aars/releases/{wersja}` z dowiązaniami
`current` i `previous`. Na start PTT jest ustawione na VOX, więc nic nie
kluczuje radia, dopóki admin nie wybierze sterowania w panelu.

```sh
systemctl status aars        # active (running) i STATUS= z opisem stanu
journalctl -u aars -f
sudo systemctl stop aars     # przerywa nadanie, PTT opada od razu
```

Panel działa po HTTP, bez HTTPS: jest przeznaczony do sieci lokalnej.
Odpowiada tylko pod adresem IP, `localhost`, nazwą hosta stacji,
`{nazwa}.local`, `{nazwa}-{N}.local` (tak avahi nazywa drugą stację o tej
samej nazwie w sieci, np. `aars-2.local`), `{nazwa}.lan`, `{nazwa}.home`,
`{nazwa}.home.arpa`, `{nazwa}.localdomain` i nazwą z `server.bind`; obca
nazwa (np. domena przepięta przez DNS rebinding) dostaje `421`. Gdy nazwa
nie działa, otwórz panel pod adresem IP stacji (np. z listy urządzeń
routera). Inne nazwy, np. `aars.fritz.box`, dopisz do `server.allowed_hosts`
w `/etc/aars/aars.toml`.

Obraz słucha na `[::]:8080`: jedno gniazdo przyjmuje IPv6 i IPv4 (także
przy `net.ipv6.bindv6only = 1`), więc panel działa też z globalnego adresu
IPv6 w tej samej sieci `/64`. Gdy jądro nie ma IPv6, panel sam słucha na
`0.0.0.0:8080`.

Domyślnie cały panel (logowanie, `/setup`, `/healthz`) odpowiada tylko
klientom z sieci lokalnej stacji: pętla zwrotna, prywatne IPv4
(`10/8`, `172.16/12`, `192.168/16`), link-local, IPv6 ULA (`fc00::/7`) i ta
sama sieć `/64` co globalny adres IPv6 stacji. Każdy inny adres, np. z
internetu przez otwarty IPv6 routera, dostaje `403` ze stroną z
wyjaśnieniem. Zdalny dostęp włącza `allow_remote = true` w sekcji `[server]`
pliku `/etc/aars/aars.toml` (tylko w pliku), po czym
`sudo systemctl restart aars`.

> **Uwaga.** Panel działa po zwykłym HTTP: hasła i ciasteczko sesji idą
> jawnym tekstem, a logowanie nie ma limitu prób. Konta operatora i
> administratora kluczują nadajnik i zlecają aktualizację. Nie przekierowuj
> portu 8080 na routerze i nie otwieraj go dla IPv6 z internetu. Do pracy
> zdalnej użyj VPN do sieci domowej (np. WireGuard): klient VPN z adresem z
> sieci lokalnej przejdzie kontrolę bez `allow_remote`.

### Aktualizacja z panelu

**Ustawienia → Aktualizacja** (tylko admin): „Sprawdź” pobiera manifest
najnowszego wydania i pokazuje wersję i notatki. „Zainstaluj” po
potwierdzeniu:

1. pobiera i sprawdza wydanie: podpis minisign manifestu kluczem
   wkompilowanym w binarkę oraz sumę SHA-256 archiwum; nadawanie idzie w tym
   czasie normalnie,
2. odmawia, gdy w kolejce czeka komunikat `Emergency` albo `High` na
   najbliższą minutę,
3. czeka na koniec bieżącego nadania (nigdy nie przerywa nadania) i kończy
   proces,
4. strażnik startu (root, przed startem usługi) sprawdza podpis jeszcze raz,
   robi kopię bazy, przełącza `current` na nową wersję i otwiera próbę,
5. nowa wersja po minucie zdrowej pracy zapisuje potwierdzenie. Po trzech
   startach bez potwierdzenia albo gdy nowa wersja nie zbuduje stosu
   nadawczego, który stara budowała, strażnik wraca do poprzedniej wersji
   razem z kopią bazy.

**Przy powrocie przepadają dane zapisane przez nową wersję w czasie próby**
(najwyżej kilka minut: kolejka, dziennik nadań, zmiany ustawień). Wynik
instalacji i powrotu trafia do audytu.

Aktualizacja z panelu działa tylko w obrazie (usługa systemd na Linuksie
aarch64) i tylko w kompilacji z kluczem wydań. Pi bez zegara RTC po starcie
bez sieci może mieć złą datę; wtedy pobieranie kończy się błędem TLS
z podpowiedzią o NTP. Z konsoli:

```sh
aars --config /etc/aars/aars.toml update status     # wersje, próba, ostatni wynik
aars --config /etc/aars/aars.toml update check      # najnowsze wydanie
sudo systemctl stop aars
sudo aars --config /etc/aars/aars.toml update rollback   # ręczny powrót z kopią bazy
sudo systemctl start aars
```

`update rollback` odmawia, gdy usługa działa. Gdy nie da się tego ustalić
(brak pliku blokady `/var/lib/aars/update/run.lock`), odmawia także; po
sprawdzeniu `systemctl is-active aars` można wymusić powrót opcją `--force`.
Zlecona instalacja dzieje się tylko w tej chwili: gdy proces skończy się
inaczej niż aktualizacją (zatrzymanie, restart w trakcie pobierania, błąd),
żądanie jest wycofywane i trzeba je zlecić ponownie.

### Licencje

AARS: MIT. Piper (`piper-tts` 1.8.0, OHF-Voice/piper1-gpl) i wbudowany
espeak-ng: GPL-3.0; AARS wywołuje Pipera jako osobny proces. onnxruntime:
MIT. Głos `pl_PL-darkman-medium`: zbiór danych CC0. Pełna lista jest
w obrazie w `/usr/share/doc/aars/THIRD-PARTY.md`, a źródło Pipera
(`piper_tts-1.8.0.tar.gz`) jest dołączone do każdego wydania.

### Wydawanie (dla opiekuna)

1. Ustaw `version` w `crates/aars/Cargo.toml`, zatwierdź i wypchnij znacznik
   z opisem (notatki wydania): `git tag -a v0.10.0 -m "…" && git push origin v0.10.0`.
2. `.github/workflows/release.yml` buduje na `ubuntu-24.04-arm` binarkę,
   obraz (z przypiętymi sumami bazy Raspberry Pi OS, kół Pipera i głosu),
   `aars-os-list.json`, manifest i tworzy **szkic** wydania. CI niczego nie
   podpisuje. Zadania, które kompilują, mają tylko prawo odczytu i nie
   zapisują tokenu w `.git/config`; prawo zapisu ma wyłącznie ostatnie
   zadanie, które nic nie kompiluje, tylko zakłada szkic. Wydanie nie
   używa cache'u kompilacji.
3. Na swoim komputerze, z kluczem prywatnym poza GitHubem:
   `scripts/sign-release.sh v0.10.0 [--compare ścieżka/do/aars]`. Skrypt
   pobiera wszystkie pliki szkicu, liczy ich sumy i rozmiary i porównuje
   z manifestem (odmawia przy każdej różnicy, brakującym albo dodatkowym
   pliku), sprawdza manifest regułami stacji (wersja = znacznik, target,
   jedna binarka, limity), opcjonalnie porównuje binarkę z lokalną
   kompilacją, pokazuje podsumowanie z commitem znacznika i czeka na wpisanie
   wersji. Dopiero wtedy podpisuje z komentarzem zaufanym `aars 0.10.0`,
   sprawdza podpis kluczem `keys/aars-release.pub`, dołącza go i publikuje
   wydanie. Wymaga `gh`, `minisign` i `jq`. Binarkę buduje CI: bez
   `--compare` podpis potwierdza, że podpisujesz pliki, które widziałeś,
   a nie, że CI zbudowało je z tego źródła.

   Uwaga: budowa nie jest jeszcze powtarzalna. CI używa pływającego
   `toolchain: stable`, a ścieżki budowy (`/home/runner/…`, rejestr cargo)
   trafiają do binarki w lokalizacjach panik. `--compare` z innej maszyny
   prawie zawsze kończy się więc różnicą i odmową, nawet gdy CI zbudowało
   binarkę uczciwie. Zgodność wymaga tej samej wersji toolchaina i tych
   samych ścieżek budowy, dopóki `release.yml` nie dostanie
   `--remap-path-prefix` i przypiętego toolchaina. Dziś `--compare` to
   sprawdzenie dla opiekuna, który odtworzy środowisko CI, a nie zwykły
   krok wydania.

Dopóki `keys/aars-release.pub` zawiera `UNSET`, binarka nie ma klucza wydań,
aktualizacja z panelu jest wyłączona, a `release.yml` buduje tylko wydania
próbne (`v…-rc.N`). Wydania próbne nie są widoczne dla stacji jako nowsze.

Kompilacja na Linuksa arm64 z macOS:
`docker run --rm --platform linux/arm64 -v "$PWD:/src" -w /src rust:1.93-trixie cargo build -p aars --release`
(potrzebne `apt-get install libasound2-dev libudev-dev pkg-config`).

## W planach

Według spec §13:

- etap 11c: źródła IMGW hydro i PAA oraz mapa ostrzeżeń w panelu (11a,
  źródła alarmowe, i 11b, biuletyn z pogodą i propagacją, są gotowe).

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
