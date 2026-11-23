# Etap 11c — stany wód IMGW, mapa ostrzeżeń, poprawki z 11b

Data: 2026-11-22. Rozszerza spec główny (`2026-10-13-aars-design.md`, §7, §13 pkt 11)
i projekty 11a/11b. Właściciel (2026-11-22): „działaj dalej, potem się zrobi PR”.

## Cel i dlaczego

Domknąć pierwotny zakres etapu 11: ostrzeżenia hydrologiczne i stany
wodowskazów w okolicy stacji (wezbrania to w Polsce najczęstsze realne
zagrożenie), mapę w panelu pokazującą, które powiaty są pod ostrzeżeniem
meteo, i drobne poprawki odłożone w 11b.

## Cele

- Źródło `imgw-hydro`: ostrzeżenia hydrologiczne IMGW (`warningshydro`) dla
  obszaru stacji — ogłaszane jak `imgw-meteo`.
- Źródło `imgw-wodowskazy`: stany wodowskazów IMGW (`/api/data/hydro`) w
  promieniu od stacji — komunikat po przekroczeniu stanu ostrzegawczego
  i alarmowego.
- Część biuletynu `wody`: stany najbliższych wodowskazów i obowiązująca susza
  hydrologiczna dla obszaru.
- Strona panelu `/map` (viewer): powiaty kolorem najwyższego stopnia
  ostrzeżenia meteo IMGW, pozycja stacji, obszar stacji obrysowany.
- Poprawki z 11b (lista niżej).

## Poza zakresem (anty-cele)

- **PAA (promieniowanie)** — odłożone. Portal `monitoring.paa.gov.pl` nie ma
  udokumentowanego publicznego API (aplikacja Angular na wewnętrznym `_api`,
  bez licencji danych); badanie wewnętrznych ścieżek zatrzymała blokada
  uprawnień w tej sesji. Wraca po zgodzie/kontakcie z PAA (pytanie do
  właściciela).
- Mapa ostrzeżeń hydro (zlewnie bez geometrii w danych), mapa RSO/GIOŚ.
- Kafelki mapy z własnego serwera — jak na stronie ustawień: OpenStreetMap
  z przeglądarki operatora.
- Porażka: susza (stopień −1, ważna „do 9999 roku”) ogłaszana jako alert albo
  co cykl; nadanie stanu wody sprzed doby jako bieżącego; wodowskaz bez stanów
  ostrzegawczych traktowany jako „przekroczenie” (puste pola = 0); mapa
  mówiąca „brak ostrzeżeń”, gdy źródło jest wyłączone lub nieświeże.

## Ograniczenia

- Jak 11a/11b: testy bez sieci (fixtury z prawdziwych pobrań 2026-11-22 w
  `crates/aars-sources/tests/fixtures/imgw/`), wiremock, `crate::http`,
  User-Agent `aars/{wersja}`, nieznany schemat = `Parse`, nigdy zera.
- Atrybucja IMGW (regulamin IMGW-PIB): w treści komunikatów hydro
  „Źródło: IMGW-PIB.” (jak `imgw-meteo`); w README i w stopce mapy pełna
  formuła „Źródłem pochodzenia danych jest IMGW-PIB; dane zostały
  przetworzone”.
- Granice powiatów: PRG (GUGiK, WFS `A02_Granice_powiatow`, pobrane
  2026-11-22, 380 powiatów), uproszczone skryptem `tools/powiaty/` do
  `assets/powiaty.geojson` (~740 KB, ~190 KB gzip), wkompilowane jak reszta
  assetów. Atrybucja na mapie: „Granice: PRG, GUGiK”.
- `aars-web` nie zależy od `aars-sources` — dane mapy przez wywołanie zwrotne.

## Podejścia

1. **Wybrane: dwa źródła hydro + część biuletynu.** Ostrzeżenia (obszar,
   wojewódzkie) i wodowskazy (pozycja, lokalne) mają różne potrzeby (`Needs`)
   i różne interwały; runner obsługuje każde bez zmian.
2. Jedno źródło `imgw-hydro` robiące oba — wymagałoby i obszaru, i pozycji,
   a runner zna jedno `Needs`.
3. Tylko ostrzeżenia (bez wodowskazów) — tańsze, ale ostrzeżenia są
   wojewódzkie i spóźnione względem lokalnego wodowskazu.

## `imgw-hydro` — ostrzeżenia hydrologiczne

- URL `https://danepubliczne.imgw.pl/api/data/warningshydro`
  (`sources.imgw_hydro_url`). Needs `Area`. Interwał domyślny 600 s.
  Limit odpowiedzi 1 MiB.
- Odpowiedź: tablica; obiekt `{"message": …}` (jak w `warningsmeteo`) = brak
  ostrzeżeń. Pola: `numer`, `biuro`, `opublikowano`, `stopień` (napis),
  `data_od`, `data_do`, `prawdopodobienstwo`, `zdarzenie`, `przebieg`,
  `obszary[]` (`wojewodztwo`, `opis`, `kod_zlewni[]`).
- Dopasowanie: ostrzeżenie dotyczy stacji, gdy któreś `obszary[].wojewodztwo`
  (nazwa, małymi literami) jest województwem obszaru stacji (2 pierwsze cyfry
  TERYT → nazwa; tabela 16 województw jest już w `rso`).
- **Susza (`stopień` = `-1`) nie jest alertem** — źródło ją pomija; trafia
  tylko do części biuletynu `wody`.
- Stopień 1/2/3 → pozycja. Priorytet: 1 `Normal`, 2 `High`, 3 `Emergency`
  z przerwaniem (jak stopień 3 meteo). Klucz `{numer}|{biuro}`, temat ten sam,
  odcisk = stopień | `data_do` | skrót `przebieg`. `valid_until` = `data_do`
  (czas Warszawy).
- Tekst: „Ostrzeżenie hydrologiczne IMGW {stopień}. stopnia: {zdarzenie}.
  {opis obszarów dla województw stacji}. {przebieg} Ważne do {dzień miesiąca}
  godz. {HH:MM}. Źródło: IMGW-PIB.” — obcięte do 700 znaków na granicy zdania.
  Zmiana: „Zmiana ostrzeżenia hydrologicznego. …”. Odwołanie: „Ostrzeżenie
  hydrologiczne „{zdarzenie}” nie obowiązuje.”. Podsumowanie:
  „{zdarzenie}, {stopień}. stopień”.

## `imgw-wodowskazy` — stany wodowskazów

- URL `https://danepubliczne.imgw.pl/api/data/hydro` (`sources.imgw_hydro_stations_url`),
  ~550 KB, 913 stacji; limit 2 MiB. Needs `Position`. Interwał domyślny 900 s
  (min 600). Pamięć ostatniego udanego pobrania (jak `pogoda`) wspólna z częścią
  biuletynu `wody` — jedna instancja.
- Pola stacji (napisy albo `null`): `id_stacji`, `stacja`, `rzeka`,
  `wojewodztwo`, `lat`, `lon`, `stan_wody`, `stan_wody_data_pomiaru`
  (czas Warszawy), `stan_ostrzegawczy`, `stan_alarmowy`. Stacja bez
  współrzędnych albo bez `stan_wody` — pominięta. Brak tablicy = `Parse`.
- Ustawienia: `radius_km` Integer 5–100, domyślnie 25; `stations` Text
  (opcjonalna lista `id_stacji` po przecinku; niepusta = tylko te stacje,
  promień ignorowany). Zakres R9: `{lat:.2},{lon:.2}|r={}|s={}`.
- Świeżość pomiaru: ≤ 3 h od `stan_wody_data_pomiaru`; starszy — stacja
  pominięta (bez odwołania — runner potrzebuje 2 pełnych braków; pozycja
  pominięta dla starości zostaje śledzona, bo `complete = false`, gdy
  którakolwiek obserwowana stacja ma stary pomiar).
- Pozycja, gdy `stan_wody ≥ stan_alarmowy` (alarm) albo
  `≥ stan_ostrzegawczy` (ostrzegawczy); stan pusty/`null`/nie-liczba = brak
  progu (nigdy 0). Klucz `{id_stacji}`, temat ten sam, odcisk = poziom
  (`ostrzegawczy`/`alarmowy`) — zmiana poziomu to zmiana; wahania cm nie.
  Priorytet: ostrzegawczy `Normal`, alarmowy `High`, bez przerywania.
- Tekst: „Wodowskaz {stacja} na rzece {rzeka}: stan wody {cm} cm, powyżej
  stanu {ostrzegawczego|alarmowego} ({próg} cm). Pomiar z godz. {HH:MM}.
  Źródło: IMGW-PIB.” Odwołanie (spadek poniżej ostrzegawczego):
  „Wodowskaz {stacja}: stan wody spadł poniżej stanu ostrzegawczego.”
  Podsumowanie: „{rzeka} w {stacja}: stan {ostrzegawczy|alarmowy}”.
- `valid_until`: brak (stan trwa do spadku; runner odwołuje po 2 pełnych
  cyklach bez przekroczenia).

## Część biuletynu `wody`

- `BULLETIN_PARTS` += `("wody", "Stany wód w okolicy")`, kolejność:
  pogoda, wody, propagacja, ostrzezenia. Domyślne `parts` bez zmian (nowa
  część do zaznaczenia; nie zmieniamy biuletynu istniejących stacji).
- Treść: do 3 najbliższych wodowskazów (albo z listy `stations`) ze świeżym
  pomiarem: „Stany wód. {rzeka} w {stacja}: {cm} cm{, stan ostrzegawczy|,
  stan alarmowy}.” + susza: jeśli `warningshydro` ma obowiązującą suszę dla
  województwa stacji — „Obowiązuje ostrzeżenie o suszy hydrologicznej.”
  (pobranie przez `imgw-hydro`; błąd = zdanie wypada) + „Źródło: IMGW-PIB.”
- Bez pozycji / brak świeżych stacji i brak suszy → `Empty`. Limit 400 znaków.
- Biuletyn potrzebuje obszaru do suszy: `PartCtx` dostaje obszar stacji
  (ten sam co `source:imgw-meteo:area`? — nie: obszar jest per źródło.
  Bierzemy obszar `source:imgw-hydro:area`; pusty → zdanie o suszy wypada).

## Mapa `/map`

- Link w menu „Mapa” (viewer). Szablon z Leaflet (już wkompilowany),
  kafelki OSM jak w ustawieniach, `assets/powiaty.geojson`, nowy
  `assets/warnings-map.js`.
- `GET /map/data` (viewer, JSON): `{ "source": "imgw-meteo", "state":
  "ok"|"disabled"|"stale"|"never", "fetched_at": ISO|null, "warnings":
  [{"teryt": "1061", "level": 1|2|3, "event": "Burze", "until": ISO}],
  "station": {"lat","lon"}|null, "area": ["1061", …] }`.
  Dane ostrzeżeń z ostatniego udanego pobrania `imgw-meteo` (pamięć w źródle,
  dostęp przez wywołanie zwrotne `with_warnings_map`); ostrzeżenia po
  `until` odfiltrowane. Stan `disabled`/`stale`/`never` z `source_state`.
- Kolory: stopień 1 żółty, 2 pomarańczowy, 3 czerwony (najwyższy stopień
  w powiecie); obszar stacji obrysowany; legenda; czas pobrania.
  Stan inny niż `ok` → wyraźny baner „Źródło ostrzeżeń meteo wyłączone /
  nieaktualne — mapa nie pokazuje bieżących ostrzeżeń” i żadnego kolorowania.
- Kod TERYT powiatu: 4 cyfry (`JPT_KOD_JE` z PRG, `teryt[]` z IMGW). Miasta
  na prawach powiatu są osobnymi powiatami w obu zbiorach.
- Asset GeoJSON serwowany z `Cache-Control: max-age=86400`.

## Poprawki z 11b

1. Źródło włączone bez żadnego udanego pobrania liczy się w `ostrzezenia`
   jako niedostępne (nie „dostępne”) — koniec fałszywego „Brak obowiązujących
   ostrzeżeń” tuż po włączeniu.
2. „w godz. 7:00” bez drugiej godziny → „w godzinie siódmej” (liczba
   pojedyncza, miejscownik); zakres zostaje „w godzinach siódma – piętnasta”.
3. Komunikat 503 podglądu: „biuletyn nie wystartował — zobacz dziennik
   systemowy (journalctl -u aars)”.
4. Alert pogodowy dnia 0: odcisk liczony z wartości od teraz do końca dnia
   tylko rośnie — spadek kubełka w ciągu dnia nie jest zmianą (nie nadaje
   „Zmiana prognozy” w południe); wzrost jest. Implementacja: źródło porównuje
   z odciskiem śledzonej pozycji (`tracked_keys` nie wystarcza — potrzebny
   odcisk; `SourceCtx` dostaje `tracked: HashMap<String, String>` klucz →
   odcisk, zamiast lub obok `tracked_keys`).

## Rozstrzygnięcia (moje)

1. PAA odłożone (brak publicznego API, blokada uprawnień) — koszt: brak
   promieniowania w tym etapie.
2. Susza tylko w biuletynie — koszt: słuchacz bez biuletynu jej nie usłyszy.
3. Dopasowanie ostrzeżeń hydro po województwie, nie po zlewni — koszt:
   ogłasza też ostrzeżenia dla innych zlewni tego województwa.
4. Wodowskazy: próg z danych IMGW, świeżość 3 h, promień 25 km — koszt:
   wodowskaz nieco dalej nie jest śledzony.
5. Mapa tylko dla ostrzeżeń meteo, z pamięci źródła — koszt: po restarcie
   mapa pusta do pierwszego pobrania (stan „never”/„stale” z banerem).
6. Granice uproszczone (Douglas-Peucker 0,004°) — koszt: drobne szczeliny
   między powiatami przy dużym przybliżeniu.

## Rozstrzygnięcia po przeglądzie (mają pierwszeństwo przed tekstem wyżej)

1. **Wodowskazy a nieświeże pomiary.** `complete = false` tylko wtedy, gdy
   nieświeży (> 3 h) albo brakujący pomiar ma stacja, którą runner **już
   śledzi** (`tracked_keys`); nieświeże stacje bez śledzonej pozycji po
   prostu pomijamy. Każda pozycja wodowskazu ma `valid_until` = czas pomiaru
   + 24 h (odświeżany z każdym świeżym pomiarem) — martwy wodowskaz wygasa
   po cichu (runner: wygaśnięcie, bez „spadł poniżej”), nie wisi w
   `ostrzezenia` w nieskończoność. Odwołanie „spadł poniżej” tylko po
   świeżych pomiarach poniżej progu (2 pełne cykle).
2. **`imgw-hydro`: temat = klucz** celowo (IMGW wydaje równoległe ostrzeżenia
   o tym samym zdarzeniu dla jednego województwa — fixtura: 137 i 140).
   Odwołanie nazywa numer i województwa: „Ostrzeżenie hydrologiczne nr
   {numer} ({województwa}) „{zdarzenie}” nie obowiązuje.” Nowy numer przy
   aktualizacji = nowe ogłoszenie + późniejsze odwołanie starego (koszt
   przyjęty).
3. **Susza wypada z biuletynu w 11c.** Dopasowanie tylko po województwie
   dawałoby zdanie o suszy prawie zawsze (30 susz w fixturze). Wraca, gdy
   będzie mapowanie zlewni. Część `wody` = same wodowskazy; nie potrzebuje
   instancji `imgw-hydro`.
4. **Alert pogodowy dnia 0 (poprawka 4):** raz ogłoszona pozycja dnia 0 jest
   emitowana przez źródło do końca dnia z dotychczasowym odciskiem, gdy nowa
   wartość jest słabsza (niższy kubełek albo pod progiem); silniejsza
   (porywy/opady wyżej, mróz zimniej) = nowy odcisk = „Zmiana prognozy”.
   `SourceCtx` dostaje `tracked: HashMap<String, String>` (klucz → odcisk
   aktywnych pozycji, w zakresie, jak `tracked_keys`).
5. **Mapa po restarcie:** pusta pamięć źródła ⇒ stan `never` niezależnie od
   `source_state`. Źródło `imgw-meteo` zapamiętuje **wszystkie** ostrzeżenia
   z odpowiedzi (przed filtrem obszaru). Pusty obszar ⇒ runner nie pobiera ⇒
   mapa `never` z banerem „Ustaw obszar źródła ostrzeżeń meteo” (koszt
   przyjęty).
6. **Część `wody`:** do 3 najbliższych wodowskazów w `radius_km` (albo z
   listy `stations`), ustawienia czytane z `source:imgw-wodowskazy:*` (przez
   tę samą instancję co źródło, jak `pogoda`). Wodowskaz bez progów —
   podany sam stan, bez słowa o progu.
7. **Poprawka 1 tylko w `alert_source_health`** (`SourceStatus::is_stale` bez
   zmian).
8. (moje) Normalizer: „z godz.” → dopełniacz („z godziny dwudziestej
   dwadzieścia”). `powiaty.geojson` serwowany bez kompresji (743 KB),
   `Cache-Control: max-age=86400` tylko dla tego pliku.
