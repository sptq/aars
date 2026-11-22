# Etap 11a — źródła alarmowe: RSO, NOAA SWPC, GIOŚ, EMSC

Data: 2026-11-22. Rozszerza spec główny (`2026-10-13-aars-design.md`, §7 i §13 pkt 11).
Etap 11 podzielono na 11a (alarmy, ten dokument) i 11b (kompozytor biuletynu,
pogoda, `imgw-hydro`, `paa`, biuletyn propagacyjny, mapa).

## Cel i dlaczego

Stacja ma ogłaszać więcej niż ostrzeżenia meteo IMGW: komunikaty ochrony
ludności i wydarzenia lokalne (RSO: Alert RCB, WCZK, sanepid, testy syren,
atak z powietrza, utrudnienia), pogodę kosmiczną ważną dla krótkofalowców
(burze geomagnetyczne, burze radiacyjne, zaniki KF), alerty smogowe dla
okolicy stacji i odczuwalne wstrząsy. Właściciel (2026-11-22): „wydarzenia
lokalne — widzę, że to już jest” (pokrywa je RSO), „działaj autonomicznie”.

## Cele

- Cztery nowe moduły-źródła w `aars-sources`, każde jak `imgw-meteo`:
  wyłączone domyślnie, włączane i ustawiane w panelu (`source:{id}:*`),
  działające przez istniejący `SourceRunner` (nowe, zmiana, odwołanie,
  dedup, backoff, przestarzałość).
- Źródło, które nie potrzebuje obszaru, nie jest przez runner pomijane.
- Źródło może nie mieć komunikatu odwołania (zdarzenie jednorazowe).

## Poza zakresem (anty-cele)

- Kompozytor biuletynu, propagacja w biuletynie, pogoda, hydro, PAA, mapa — 11b.
- RSO CAP z tokenem, websocket EMSC, `alerts.json` SWPC jako treść — później.
- Porażka: nadawanie nieaktualnych albo zmyślonych danych (zera przy zmianie
  formatu), zalanie anteny komunikatami (każde odpytanie = nowe ogłoszenie),
  nadanie surowego HTML-a lub tekstu na kilka minut.

## Ograniczenia

- Walidacja i testy nie otwierają sieci na zewnątrz: odpowiedzi z prawdziwych
  pobrań zapisane jako fixtury (`crates/aars-sources/tests/fixtures/{id}/`),
  HTTP przez `wiremock`. Pobranie fixtur jest jednorazowe, ręczne.
- User-Agent `aars/{wersja}` bez danych osobowych (jak `imgw-meteo`).
- Każde źródło ma limit rozmiaru odpowiedzi i timeout (`http::get_limited`
  przeniesione do wspólnego modułu `crate::http`).
- Nieznany schemat odpowiedzi to `SourceError::Parse`, nigdy pusty wynik.
- Atrybucja w treści komunikatu, gdzie licencja jej wymaga (CC BY 4.0):
  „Źródło: GIOŚ”, „Dane: EMSC”. RSO i SWPC: nazwa nadawcy w treści.

## Podejścia

1. **Wybrane: każde źródło osobnym `DataSource` na istniejącym runnerze.**
   Runner jest przetestowany na dedupie i odwołaniach; nowe źródło to czysta
   funkcja odpowiedź → pozycje plus fixtury. Wymaga dwóch małych zmian
   w runnerze (wymagania źródła, brak odwołania).
2. Wspólny „moduł alarmów” z wtyczkami parserów — więcej abstrakcji bez
   zysku, przepisuje działający mechanizm.
3. Agregacja po stronie zewnętrznej (np. tylko RSO, bo przekazuje część
   IMGW) — RSO nie ma pogody kosmicznej, wstrząsów ani alertów GIOŚ dla
   stacji pomiarowej.

## Zmiany wspólne

### Wymagania źródła

`DataSource` dostaje `fn needs(&self) -> Needs` z domyślnym `Needs::Area`.
Warianty: `Area` (dzisiejsze zachowanie, pusty obszar = „brak obszaru”),
`Position` (brak pozycji stacji = pominięcie z powodem „brak pozycji stacji”),
`Nothing`. Panel pokazuje powód pominięcia tak jak dziś.

R9 (zmiana obszaru zrzuca śledzone pozycje po cichu) dla źródeł `Position`
porównuje zamiast tekstu obszaru tekst zakresu: pozycja zaokrąglona do
0,01° i ustawienia wpływające na zakres (promień), np. `51.77,19.46|r=150`.
Funkcja `fn scope(&self, ctx) -> String` w traicie, domyślnie tekst obszaru.
Dla `Nothing` zakres to pusty tekst.

### Brak odwołania

`SourceItem.cancel_text` pusty oznacza „bez komunikatu odwołania”: runner
oznacza pozycję jako odwołaną i wycofuje czekający element kolejki, ale nic
nie wstawia. Test w runnerze.

## Źródła

### `rso` — Regionalny System Ostrzegania

- URL: `https://komunikaty.tvp.pl/komunikatyxml/{slug}/wszystkie/0?_format=json`
  dla każdego województwa z obszaru (kod TERYT 2-cyfrowy → slug: `10` →
  `lodzkie`; mapa 16 kodów w kodzie). Obszar z samych powiatów bierze
  województwo z pierwszych dwóch cyfr. Bazowy URL w `aars.toml`
  (`sources.rso_url`), jak `imgw_meteo_url`.
- Needs: `Area`. Interwał domyślny 180 s (min 120 — `max-age=120`).
- Parser: tolerancyjny na znane dziwactwa (`provinces` jako obiekt
  kluczowany id, zbłąkane klucze `"0"`, `"1"`), ale brak listy komunikatów
  albo pól `id`/`title`/`valid_to` = błąd parsowania pozycji (pozycja
  pominięta, `complete = false`), nieczytelny cały dokument = `Parse`.
- Kategorie: ustawienie `categories` (Choice wielokrotny nie istnieje —
  tekst z listą slugów oddzielonych przecinkami, walidowany przy zapisie);
  domyślnie `ogolne,atak-z-powietrza`. `meteorologiczne`/`hydrologiczne`
  dublują IMGW i domyślnie są wyłączone; `informacje-drogowe` opcjonalnie.
  Pozycja RSO nie niesie kategorii (sprawdzone 2026-11-22), więc źródło
  pobiera jeden URL na parę (województwo, kategoria):
  `/komunikatyxml/{slug}/{kategoria}/0?_format=json`. Alert RCB przychodzi
  w `ogolne` (sprawdzone), dlatego `ogolne` jest w domyślnych. Pusta
  kategoria to `{"pagination":{…},"newses":[]}` — poprawny pusty wynik.
  `stany-wod` (300+ pozycji bez tytułu) nie jest dozwolona w tym źródle.
- Pierwsze zobaczenie: ustawienie `max_age_h` (domyślnie 24, 1–720).
  Źródło pomija pozycję, której runner jeszcze nie śledzi, gdy jej
  `updated_at` jest starsze niż `max_age_h` — chyba że to Alert RCB albo
  atak z powietrza. Pozycji już śledzonych limit nie dotyczy (inaczej
  zestarzałaby się i runner by ją odwołał). Do tego `SourceCtx` dostaje
  pole `tracked_keys: HashSet<String>` (klucze aktywnych pozycji tego
  źródła), wypełniane przez runner. Bez tego limitu włączenie
  modułu nadałoby kilkadziesiąt długich komunikatów (susze ważne do końca
  roku) naraz. EMSC używa tego samego pola zamiast reguły „30 min”.
- Filtr lokalny: ustawienie `keywords` (tekst, słowa/rdzenie po przecinku,
  np. `zgiersk, Łódź, Ozorków`). Puste = całe województwo. Niepuste =
  pozycja musi zawierać którekolwiek słowo (bez rozróżniania wielkości liter)
  w tytule, skrócie albo treści. **Wyjątek: Alert RCB i atak z powietrza
  przechodzą zawsze dla całego województwa** — lepiej ogłosić za dużo niż
  przegapić.
- Priorytet: tytuł „Alert RCB” albo kategoria `atak-z-powietrza` →
  `Emergency`, przerywa. Reszta `Normal`, bez przerywania.
- Tekst: tytuł + treść z usuniętym HTML-em i encjami, białe znaki
  zwinięte, obcięta do 600 znaków na granicy zdania z dopiskiem „Pełna treść
  na stronie RSO.”; początek „Komunikat RSO, województwo łódzkie.”.
  Zmiana: „Zmiana komunikatu RSO. …”. Odwołanie: krótkie „Komunikat RSO
  „{tytuł}” nie obowiązuje.”, tylko gdy oryginał poszedł na antenę (runner).
- Klucz `id`; temat `rso:{id}` (RSO nie wiąże starych i nowych id);
  odcisk: tytuł | `valid_to` | skrót treści (sha256 pierwszych 2000 znaków
  po normalizacji) — edycja treści to zmiana. `valid_until` z `valid_to`
  (czas lokalny Warszawy → UTC jak w `imgw_meteo::speech::warsaw_to_utc`).

### `swpc` — pogoda kosmiczna NOAA

- URL: `https://services.swpc.noaa.gov/products/noaa-scales.json`
  (`sources.swpc_url`). Needs: `Nothing`. Interwał domyślny 300 s.
- Bierzemy tylko klucz `"0"` (stan bieżący): `R.Scale`, `S.Scale`, `G.Scale`
  (liczby 0–5 jako tekst albo null). Brak klucza `"0"` albo którejś skali =
  `Parse`. Prognozy `"1"`–`"3"` w 11a pomijamy.
- Ustawienia progów: `g_min` (domyślnie 3), `s_min` (2), `r_min` (3),
  Integer 1–5. Każda skala ≥ progu to pozycja: klucz `G`/`S`/`R`, temat ten
  sam, odcisk `G3` itd. Wzrost/spadek poziomu nad progiem = zmiana; spadek
  pod próg = odwołanie („Burza geomagnetyczna wygasła.”).
- Priorytet `High`, bez przerywania. `valid_until` = brak (stan trwa do
  zniknięcia).
- Tekst po polsku z opisem skutków dla krótkofalowca, np. G3: „Uwaga,
  pogoda kosmiczna. Silna burza geomagnetyczna, poziom G3 w skali NOAA.
  Możliwe pogorszenie propagacji na pasmach KF i zorza widoczna w Polsce;
  możliwe łączności przez zorzę na 6 i 2 metrach. Dane: NOAA SWPC.”
  R3+: zaniki KF po dziennej stronie Ziemi. S2+: zaniki w rejonach
  polarnych. Tabela tekstów dla poziomów 1–5 każdej skali w `speech.rs`.

### `gios-smog` — alerty smogowe GIOŚ

- URL bazowy: `https://api.gios.gov.pl/pjp-api/v1/rest`
  (`sources.gios_url`); `levels/getWarnings` co cykl, `station/findAll`
  co najwyżej raz na dobę (pamięć w źródle; błąd findAll przy braku pamięci
  = błąd cyklu). Limit 2 zapytania/min → interwał domyślny 600 s, min 300.
- Needs: `Position`. Ustawienie `radius_km` (domyślnie 25, 5–100): alerty
  ze stacji pomiarowych w tym promieniu od pozycji stacji (haversine).
- Pozycja na ostrzeżenie (klucz: `param|standardTypeCode|zoneCode`, kilka
  stacji z tej samej strefy to jeden komunikat), odcisk: kod poziomu
  (PI/PA) + parametr, temat: parametr. Priorytet: PA (alarmowy) `High`,
  PI (informowania) `Normal`; bez przerywania.
- Tekst: „Alert smogowy GIOŚ, {strefa}. Przekroczony poziom
  {informowania|alarmowy} dla {PM10|PM2,5|…}.” + `recommendedPrecaution`
  oczyszczone i obcięte jak w RSO + „Źródło: GIOŚ.”. Odwołanie: „Alert
  smogowy dla {strefa} odwołany.”.
- `findAll` może być stronicowane (`page`, `size`) — pobrać wszystkie strony
  z limitem rozmiaru; fixtury z prawdziwego pobrania.

### `emsc` — wstrząsy

- URL: `https://www.seismicportal.eu/fdsnws/event/1/query`
  (`sources.emsc_url`) z `format=json&lat&lon&maxradius={stopnie}&minmag&start`
  (start = teraz − 6 h). Needs: `Position`. Interwał domyślny 120 s.
- Ustawienia: `min_mag` (tekst liczba dziesiętna, domyślnie `3.0`, zakres
  1,0–9,0; walidacja przy zapisie), `radius_km` (150, 10–1000).
- Pozycja na zdarzenie: klucz `unid`, temat `emsc:{unid}`, odcisk:
  magnituda w krokach 0,5 (`3.0`, `3.5`, …). Drobne poprawki magnitudy
  (3,3 → 3,4) nie dają komunikatu zmiany; przeskok kroku daje. Prosto i bez
  zalewu poprawkami.
- `valid_until` = czas zdarzenia + 6 h (zdarzenie wypada z okna i z
  odpowiedzi; runner wygasza po cichu). `cancel_text` pusty — wstrząsów się
  nie odwołuje. Zdarzenie starsze niż 30 min w chwili pierwszego zobaczenia
  i nieśledzone (`tracked_keys`) nie jest ogłaszane — po włączeniu modułu
  nie czytamy historii; źródło je po prostu pomija.
- Priorytet: M ≥ 4,5 `High`, reszta `Normal`; bez przerywania.
- Tekst: „Wstrząs sejsmiczny o magnitudzie 3,4, {odległość} kilometrów na
  {kierunek} od stacji, {region po polsku jeśli POLAND → „Polska”}, o
  godzinie {HH:MM czasu polskiego}. Dane: EMSC.” Odległość i kierunek
  (8 kierunków) liczone lokalnie.

## Panel i konfiguracja

- Nowe klucze `sources.*_url` w `aars.toml` z walidacją http/https jak
  `imgw_meteo_url`; domyślne prawdziwe adresy.
- Nowe moduły w `aars_sources::modules()`; macierz ról (`role_matrix.rs`)
  dostaje wiersze `/modules/{id}/enabled` dla każdego.
- Typ ustawienia „liczba dziesiętna” — jeśli potrzebny, nowy
  `SettingKind::Decimal { min, max }`; walidacja listy slugów RSO jako
  `SettingKind::Text` z walidatorem po stronie modułu albo nowy
  `SettingKind::List(&[...])` — decyzja implementera, z testem walidacji.

## Zostawione budowniczemu

- Dokładne nazwy pól JSON z prawdziwych fixtur (RSO, GIOŚ findAll mają
  polskie klucze ze spacjami i znakami diakrytycznymi).
- Czy RSO zwraca kategorię w pozycji (patrz wyżej).
- Polskie teksty komunikatów poza wzorami powyżej.

## Rozstrzygnięcia po przeglądzie budowniczego (2026-11-22)

Mają pierwszeństwo przed tekstem wyżej, gdy się z nim nie zgadzają.

Zgodność z runnerem:

1. **Odcisk zawiera klucz** tam, gdzie różne zdarzenia mogą mieć ten sam
   opis (reguła R4 runnera łączy nowe klucze po odcisku): EMSC
   `{unid}|{krok magnitudy}`, GIOŚ `{strefa}|{PI/PA}|{parametr}`. Temat GIOŚ
   = klucz. RSO, SWPC — temat = klucz już teraz. Test runnera „dwa wstrząsy
   o tej samej magnitudzie dają dwa komunikaty”.
2. **EMSC `valid_until` = czas zdarzenia + 30 min** (zawsze). Runner nie
   śledzi starszych zdarzeń, a element kolejki wygasa razem z nimi — raport
   o wstrząsie nie pójdzie godziny później. Okno zapytania `start` = 6 h
   zostaje. `tracked_keys` dla EMSC niepotrzebne.
3. **Pusty `cancel_text`** obsłużony w obu miejscach runnera (odwołanie
   w cyklu i `settle_pending_cancellations`); w przypadku `Unresolved`
   `cancel_pending` = false. Testy obu ścieżek.
4. **Interwał domyślny i zakres z deskryptora**: `SourceStateRepo::config`
   (albo jego wywołujący) dostaje domyślny interwał i zakres modułu zamiast
   stałych 300 i 60..=3600.
5. **`needs` w `ModuleDescriptor`** (`aars-core`), nie tylko w traicie:
   runner i panel źródeł (`sources_page.rs`, dziś sam liczy „brak obszaru”)
   czytają to samo. Wiersz obszaru ukryty dla źródeł bez `Needs::Area`.
6. **Zakres R9 obejmuje wszystkie ustawienia filtrujące**: RSO
   `obszar|cat=…|kw=…`, SWPC `g=…|s=…|r=…`, GIOŚ `poz|r=…`, EMSC
   `poz|r=…|m=…`. Zmiana filtra zrzuca pozycje po cichu zamiast nadawać
   fałszywe „nie obowiązuje”.
7. **Nowe rodzaje ustawień**: `SettingKind::Decimal { min, max }` (postać
   kanoniczna `3.0`) i `SettingKind::List(&[(slug, etykieta)])` (pola
   wyboru, kanonicznie po przecinku w stałej kolejności), z walidacją
   w `module.rs` i wyświetlaniem w `modules_page.rs`.
8. **RSO bez `valid_to`** to `valid_until = None`, nie błąd pozycji.
   Kilka województw: błąd jednego → reszta z `complete = false`; `Err` tylko,
   gdy wszystkie zawiodły.

Decyzje właściciela, które podjąłem sam (właściciel: „działaj autonomicznie”;
w ledgerze jako `Ruling:`):

9. **SWPC bez trzepotania**: źródło trzyma w pamięci epizod każdej skali.
   Epizod zaczyna się po przekroczeniu progu, a odcisk to szczytowy poziom
   epizodu. Dlatego wzrost daje zmianę, a spadek przy poziomie nadal ponad
   progiem nie daje. Pozycja zostaje w wyjściu jeszcze 3 h po spadku pod
   próg i dopiero potem znika, a wtedy runner nadaje „wygasła”. Ponowny
   wzrost w tych 3 h nie jest nowym ogłoszeniem. Po restarcie pamięć znika,
   co jest dopuszczalne: najgorszy przypadek to wcześniejsze „wygasła”.
10. **RSO: priorytet alarmowy z przerywaniem** dostają tylko Alert RCB
    i atak z powietrza. Gdy tytuł albo treść zawiera „test” lub „ćwicz”
    (bez rozróżniania wielkości liter), komunikat idzie jako `Normal`
    bez przerywania. Testy syren mają zwykły priorytet.
11. **RSO: słowa kluczowe** filtrują wszystko poza Alertem RCB i atakiem
    z powietrza. Komunikaty bez nazwy miejsca odpadają przy niepustym filtrze;
    domyślnie filtr jest pusty, czyli obejmuje całe województwo.
12. **Odcisk RSO = tytuł + skrót treści**, bez `valid_to`: przedłużenie
    ważności nie powoduje ponownego czytania komunikatu.
13. **Regiony EMSC po polsku**: POLAND → Polska, a do tego mała tabela
    sąsiadów (Czechy, Słowacja, Niemcy, Ukraina, Białoruś, Litwa, Rosja
    – obwód kaliningradzki, Morze Bałtyckie). Nieznany region nie jest
    wypowiadany; zostają odległość i kierunek.

Drobne:

- **GIOŚ:** stacje pomiarowe trzymane w `Mutex` w źródle i odświeżane raz
  na dobę. Gdy odświeżenie się nie uda, źródło używa starej kopii i loguje
  ostrzeżenie.
- **SWPC:** dane starsze niż 3 h (`DateStamp`/`TimeStamp` klucza `"0"`)
  to błąd `Parse`.
- **Wspólny moduł `crate::http`:** `get_limited` z parametrami zapytania
  i wspólny klient z User-Agent. Wspólne `crate::speech` zawiera liczebniki
  („22 kilometry”, „25 kilometrów”), przecinek dziesiętny i czas warszawski.
- **Konfiguracja:** test, że częściowy blok `[sources]` w `aars.toml` nadal
  się ładuje.
- **`aars source preview`:** obsługuje dowolne źródło z `modules()`,
  jeśli to tanie; w przeciwnym razie zostaje bez zmian.
