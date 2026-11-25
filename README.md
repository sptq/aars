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
- biuletyn informacyjny co 10 minut (albo o stałych porach) sklejany
  z klocków: wstęp ze znakiem i godziną, obowiązujące ostrzeżenia, pogoda
  bieżąca, prognoza, wschód/zachód słońca, jakość powietrza i pyłki
  (Open-Meteo, CAMS), stany wód z wodowskazów IMGW, propagacja KF, zakończenie; z pauzami i podglądem w panelu i w CLI (sekcja
  „Biuletyn (etap 11b, 13)”),
- źródła hydrologiczne IMGW (ostrzeżenia, przekroczenia stanów na
  wodowskazach) i mapa ostrzeżeń meteo w panelu (sekcje „Źródła danych”
  i „Mapa ostrzeżeń (etap 11c)”),
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
pulpicie. **Dziennik źródła** (karta na `/sources`, wszystkie źródła razem na
`/logs`) mówi, dlaczego źródło nie działało: błąd sieci po polsku z nazwą serwera
(np. „serwer nie odpowiedział w wyznaczonym czasie”, z podpowiedzią przy DNS
i certyfikacie), kod HTTP, nieczytelna odpowiedź albo pominięcie z podpowiedzią
(„brak pozycji stacji — ustaw położenie stacji w Ustawieniach”), oraz kiedy
wróciło. Powtarzający się błąd to jeden wpis z licznikiem; zostaje 50 ostatnich
wpisów na źródło. Podgląd bez zapisu i bez kolejki: `cargo run -p aars -- source preview [id]`
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
| `imgw-hydro` (etap 11c) | IMGW-PIB, ostrzeżenia hydrologiczne dla województw obszaru (stopnie 1–3; susza nie jest ogłaszana); obszar jak w `imgw-meteo`, powiat oznacza całe województwo | „Źródło: IMGW-PIB.” |
| `imgw-wodowskazy` (etap 11c) | IMGW-PIB, stany wodowskazów w promieniu stacji (domyślnie 25 km) albo z listy `id_stacji`: komunikat po przekroczeniu stanu ostrzegawczego i alarmowego z danych IMGW; pomiar starszy niż 3 h jest pomijany, stan bez nowego pomiaru wygasa po dobie | „Źródło: IMGW-PIB.” |
| `powietrze` | Open-Meteo Air Quality (model CAMS), europejski indeks jakości powietrza, PM2,5, PM10 i pyłki dla pozycji stacji: samo nic nie ogłasza, dostarcza dane klockowi biuletynu „Jakość powietrza” (włączone odświeża je w tle co `interval_s`; biuletyn pobiera je też sam); ustawienie „Pyłki” (domyślnie tak) | CC BY 4.0; dane: Open-Meteo, CAMS — atrybucja tylko w panelu i tutaj, nie na antenie |
| `pogoda` (etap 11b) | Open-Meteo, prognoza dla pozycji stacji: komunikat po przekroczeniu progów (porywy od 75 km/h, mróz od −15 °C, dobowa suma opadów od 30 mm — progi w `/modules`; na antenie porywy w m/s, jak w biuletynie) na dziś (z godzin od bieżącej do końca doby) lub jutro | CC BY 4.0; „Dane: Open-Meteo.” |

Źródłem pochodzenia danych jest IMGW-PIB; dane zostały przetworzone
(regulamin IMGW-PIB; dotyczy też `imgw-meteo` i mapy ostrzeżeń).

Zmiana zakresu RSO w etapie 11b (zakres nie zależy już od pola „atak
z powietrza”, bo ta kategoria jest pobierana zawsze): przy pierwszym cyklu po
aktualizacji stacje, które miały to pole odznaczone, jednorazowo i po cichu
porzucają śledzone pozycje RSO (jak przy każdej zmianie obszaru) — bez
fałszywych odwołań na antenie. Następny cykl zaczyna śledzenie od nowa.

### Biuletyn (etap 11b, 13)

Moduł `bulletin` składa w chwili nadania jeden komunikat z klocków układu
i wstawia go do kolejki (priorytet Zwykły, bez przerywania; nadaje
dyspozytor jak każdy inny element). Po instalacji jest wyłączony. Na
`/modules` administrator:

1. wybiera **kiedy nadawać**: „co określony czas” (domyślnie, co
   `interval_min` = 10 minut, 5–60; chwile wyrównane do wielokrotności
   interwału od północy czasu polskiego: …:00, :10, :20) albo „o stałych
   porach” — HH:MM w **czasie lokalnym** (Europe/Warsaw, nie UTC jak cron),
   po przecinku, najwyżej 8, domyślnie `07:00,19:00`;
2. opcjonalnie zmienia **układ** w menu **Biuletyn** (`/bulletin`): wizualny
   edytor klocków z osią czasu, podglądem i odsłuchem (pod spodem ten sam
   JSON, pusty = domyślny; bez JavaScriptu edytuje się go wprost);
3. włącza moduł.

**Po aktualizacji do etapu 13:** tryb „co określony czas” jest domyślny także
dla stacji, które miały biuletyn o stałych porach — stacja bez zapisanego
trybu nadaje od teraz co 10 minut, a przy starcie ostrzega o tym w logu
(„biuletyn działa teraz w trybie cyklicznym…”). Żeby wrócić do stałych pór,
w menu **Biuletyn** wybierz „o stałych porach” (pory zostały zachowane)
i zapisz.

Domyślny układ: wstęp („Tu automatyczna stacja informacyjna {znak}.
{dzien}, {data}, godzina {godzina}.”, pauza 0,7 s), obowiązujące ostrzeżenia
(także „brak ostrzeżeń”, pauza 0,7 s), pogoda bieżąca, prognoza (pauza
0,5 s), wschód i zachód słońca, jakość powietrza, stany wód, propagacja
(co 3. cykl),
zakończenie („Tu {znak}. Koniec komunikatu.”). Każdy klocek ma przełącznik,
pauzę przed i po (0–10 s; sąsiednie się sumują, najwyżej 10 s), warunek
(`zawsze`, `co_n_cykli` 2–24 — numer cyklu to minuty od północy / interwał,
w trybie pór zawsze pasuje — albo `w_godzinach` od–do, także przez
północ); wstęp, zakończenie i własne teksty (`tekst`, wiele razy) są
szablonami ze zmiennymi `{znak}`, `{dzien}` („wtorek”), `{data}`
(„24 listopada”), `{godzina}` („7:05”) — nieznana zmienna jest błędem
zapisu. Bez znaku stacji zdania z `{znak}` wypadają w całości. Ostrzeżenia
bez „mów, gdy brak” znikają, gdy nic nie obowiązuje. Format:

```json
{"bloki": [
  {"rodzaj": "wstep", "tekst": "Tu {znak}. Godzina {godzina}.", "pauza_po_ms": 700},
  {"rodzaj": "ostrzezenia", "mow_gdy_brak": true},
  {"rodzaj": "propagacja", "kiedy": {"typ": "co_n_cykli", "n": 3}},
  {"rodzaj": "tekst", "tekst": "Dobranoc.", "kiedy": {"typ": "w_godzinach", "od": "22:00", "do": "06:00"}},
  {"rodzaj": "zakonczenie", "tekst": "Tu {znak}. Koniec komunikatu.", "wlaczony": false}
]}
```

Rodzaje: `wstep`, `ostrzezenia`, `pogoda_teraz`, `prognoza`, `slonce`,
`powietrze`, `wody`, `propagacja`, `tekst`, `zakonczenie`; pola pominięte mają wartości
domyślne (`wlaczony` tak, pauzy 0, `kiedy` zawsze). Stare ustawienia
`parts` i `intro` (do etapu 12) są przy pierwszym odczycie przenoszone do
układu i usuwane: lista części niezmieniona (dawny domyślny wybór) daje
pełny nowy układ, lista zmieniona przez operatora — klocki niewybranych
części wyłączone; wstęp → tekst wstępu z godziną. Układ równy domyślnemu
zapisuje się jako pusty, więc ulepszenia domyślnego dochodzą do stacji.
Zapisany własny układ nie dostaje nowych klocków sam (np. `powietrze`
dodaje się w edytorze); przy przenoszeniu starej, zmienionej listy części
`powietrze` jest wyłączone.

W trybie cyklicznym element kolejki wygasa po długości cyklu (najwyżej
15 min), więc biuletyn nie zalega do następnego; wpisy odpaleń starsze niż
7 dni są usuwane.

Pora (chwila cyklu), która przypadnie w oknie ciszy nocnej (a priorytet Zwykły jest
poniżej progu ciszy), jest pominięta, nie przesunięta; okno kończące się
dokładnie o porze jej nie blokuje. Po przestoju stacja nie nadrabia: pora
spóźniona o więcej niż `misfire_grace_s` (od 60 s do 15 min) jest pomijana,
z kilku zaległych składana jest tylko najnowsza, a restart nie dubluje pory
już zrobionej. Gdy żadna część nie ma treści, nic nie idzie na antenę.

| Część | Treść | Dane |
|---|---|---|
| `pogoda_teraz`, `prognoza`, `slonce` | warunki bieżące; najbliższe godziny, noc, dziś/jutro; wschód i zachód słońca — dla pozycji stacji; pierwsza z nich w układzie zaczyna się od „Pogoda dla okolic stacji według danych Open-Meteo.”; bez pozycji wypadają | Open-Meteo (ta sama prognoza co źródło `pogoda`, świeża do 60 min, jedno pobranie na biuletyn), zapasowo MET Norway |
| `propagacja` | strumień słoneczny (SFI), indeks K, skale NOAA, najbliższa jonosonda i ocena pasm KF | NOAA SWPC, GIRO przez KC2G |
| `ostrzezenia` | obowiązujące ostrzeżenia ze źródeł włączonych i nieprzestarzałych, które poszły na antenę oraz czekające w kolejce (najwyżej 5, reszta — także bez opisu — jako „Pozostałych ostrzeżeń: n.”); brak = „Brak obowiązujących ostrzeżeń.”, a gdy część źródeł jest niedostępna — „Brak ostrzeżeń z dostępnych źródeł. Część źródeł ostrzeżeń jest niedostępna.”; bez żadnego włączonego źródła część wypada | `source_state` stacji |
| `powietrze` | „Jakość powietrza {ocena}.” według europejskiego indeksu (EEA 2024: do 20 bardzo dobra, do 40 dobra, do 60 umiarkowana, do 80 dostateczna, do 100 zła, wyżej bardzo zła); stężenie pyłu (PM2,5 albo PM10, który gorszy) słownie w mikrogramach na metr sześcienny już od oceny „umiarkowana”, i tylko gdy sam pył jest co najmniej umiarkowany; najgorsza ocena z najbliższych 24 h, gdy gorsza niż teraz („Jutro rano dostateczna, pył zawieszony PM2,5 do 68 mikrogramów na metr sześcienny.”); pyłki olchy, brzozy, traw, bylicy, ambrozji i oliwki z najbliższych 12 h od stężenia średniego („Wysokie stężenie pyłków brzozy.”; progi w ziarnach/m³: olcha 30/100, brzoza i oliwka 20/75, trawy 20/50, bylica i ambrozja 10/30); bez pozycji albo bez bieżącej wartości wypada | Open-Meteo Air Quality, model CAMS (ta sama instancja co źródło `powietrze`, dane świeże do 60 min) |
| `wody` (etap 11c) | do 3 najbliższych wodowskazów ze świeżym pomiarem (ustawienia źródła `imgw-wodowskazy`), z dopiskiem o stanie ostrzegawczym/alarmowym; susza hydrologiczna w 11c nie jest podawana (dopasowanie po województwie dawałoby ją prawie zawsze) | IMGW-PIB (ta sama instancja co źródło, dane świeże do 20 min) |

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

Limit długości biuletynu to `min(1500, (limit nadawania w s − 10) × 13,5 ×
tempo mowy ÷ 1,5)` znaków (10 s zostaje na identyfikację i ogony; 1,5 to
wydłużenie tekstu przez normalizator — liczby i jednostki czytane słownie;
przy 180 s i tempie 87% to 1331 znaków). Sekunda pauzy kosztuje tyle znaków,
ile w niej da się powiedzieć; tak samo liczy oś czasu w edytorze. Gdy złożony tekst się nie mieści,
wypadają całe klocki: najpierw propagacja, potem słońce, jakość powietrza,
stany wód, a dalej pozostałe od końca układu — nigdy wstęp, ostrzeżenia
i zakończenie (zapowiedź pogody z atrybucją przechodzi wtedy na następny
klocek pogodowy).
Ostrzeżenia dostają tyle miejsca, ile zostaje po wstępie i zakończeniu
(najwyżej 1200 znaków), i skracają się całymi zdaniami („Oraz jeszcze…”),
a gdy nie mieści się żadne zdanie — „Obowiązujące ostrzeżenia: N.”.
Niezależnie od tego dyspozytor nie podnosi PTT dla żadnego komunikatu, którego
nagranie (z identyfikacją i ogonem) jest dłuższe niż limit nadawania: element
kolejki kończy się błędem „komunikat dłuższy niż limit nadawania” zamiast
ucięcia na antenie. Wyjątek: komunikat Alarmowy (Alert RCB, atak z powietrza)
idzie mimo to — lepiej jego początek uciętego twardym limitem niż nic.

Podgląd — ta sama ścieżka co nadanie, bez zapisu i bez kolejki:
`aars bulletin preview` drukuje tekst (albo „nic do nadania: powód”) i stan
każdej części; w panelu przycisk „Podgląd biuletynu” w karcie biuletynu
(operator) pokazuje to samo z odsłuchem. Podgląd pyta źródła o dane, więc
naraz trwa najwyżej jeden.

Licencje danych biuletynu (stacja jest niekomercyjna): Open-Meteo CC BY 4.0
(„…według danych Open-Meteo.”), MET Norway CC BY 4.0 / NLOD („…według
danych MET Norway.”),
NOAA SWPC domena publiczna, jonosondy GIRO przez KC2G CC BY-NC-SA („Dane:
NOAA, GIRO przez KC2G.”, KC2G tylko gdy jonosonda użyta), jakość powietrza
Open-Meteo Air Quality CC BY 4.0, dane: Open-Meteo, CAMS (Copernicus
Atmosphere Monitoring Service; atrybucja w panelu i tutaj, na antenie
klocek jej nie podaje). Adresy (`open_meteo_url`, `met_no_url`,
`open_meteo_aq_url`, `swpc_f107_url`, `swpc_kp_url`, `kc2g_url`)
w `[sources]` w `aars.toml`.

### Mapa ostrzeżeń (etap 11c)

Strona `/map` w panelu (dla każdej roli) koloruje powiaty stopniem
najwyższego ostrzeżenia meteo z ostatniego pobrania źródła `imgw-meteo` i
zaznacza obszar stacji. Gdy źródło jest wyłączone, dane są nieaktualne, po
restarcie nie było jeszcze pobrania albo stacja nie ma obszaru, mapa pokazuje
baner zamiast „brak ostrzeżeń”. Podkład OSM ładuje przeglądarka; granice
powiatów pochodzą z PRG (GUGiK) i są przygotowane skryptem `tools/powiaty/`.

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

Aktualizacja zmienia program, nie głosy: katalog głosów (`/var/lib/aars/voices`)
zostaje bez zmian, więc stacja mówi dotychczasowym głosem. Nowe głosy (np.
Gosia, MC Speech) przychodzą z nowym obrazem karty.

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

- PAA odłożone: portal `monitoring.paa.gov.pl` nie ma udokumentowanego
  publicznego API ani licencji danych; wraca po kontakcie z PAA. Mapa hydro
  i RSO/GIOŚ na mapie — poza zakresem (11a, 11b i 11c są gotowe).

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
