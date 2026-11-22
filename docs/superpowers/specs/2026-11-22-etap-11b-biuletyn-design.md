# Etap 11b — biuletyn: kompozytor, pogoda, propagacja

Data: 2026-11-22. Rozszerza spec główny (`2026-10-13-aars-design.md`, §7 i §13 pkt 11)
i projekt 11a (`2026-11-22-etap-11a-alarmy-design.md`). Etap 11b z §13 podzielono:
**11b** (ten dokument): kompozytor biuletynu, moduł pogody, biuletyn propagacyjny,
dwie odłożone poprawki z 11a. **11c**: `imgw-hydro`, `paa`, mapa w panelu.

## Cel i dlaczego

Właściciel (2026-11-22): „dodatkowo inne powiadomienia. Jak propagacja…”,
„działaj autonomicznie”. Stacja ma nadawać o stałych porach (np. 7:00 i 19:00)
jeden biuletyn złożony w chwili nadania z kilku części: pogoda dla pozycji
stacji, warunki propagacji KF, przegląd obowiązujących ostrzeżeń. Do tego
komunikat od razu, gdy prognoza przekroczy progi (porywy, mróz, opady).

## Cele

- Moduł `bulletin` (dziś zaślepka `Planned("etap 11")`) staje się przełączalny,
  domyślnie wyłączony, z ustawieniami: pory nadania, wybór części, wstęp.
- Trzy części biuletynu: `pogoda`, `propagacja`, `ostrzezenia`.
- Nowe źródło `pogoda` (Open-Meteo, zapasowo MET Norway) z alertami progowymi
  na istniejącym `SourceRunner`.
- Podgląd biuletynu: CLI `aars bulletin preview` (tekst) i przycisk w panelu
  (tekst + odsłuch istniejącym podglądem audio).
- Poprawki odłożone w 11a: „godz. 09:22” i zakres RSO zależny od pola ataku.

## Poza zakresem (anty-cele)

- Wstawki/zmienne w zwykłych komunikatach (`{pogoda}` w treści) — później;
  biuletyn ma własny moduł.
- hamqsl (N0NBH): „All Rights Reserved”, brak licencji na XML — nie używamy.
  Oceny pasm liczymy sami z liczb (poniżej), jawnie i stałymi progami.
- PSK Reporter, OVATION, GFZ Hp30 — później.
- Porażka: nadanie nieświeżych liczb jako aktualnych; biuletyn z samym wstępem
  (wszystkie części puste); biuletyn nadany dwa razy o tej samej porze po
  restarcie; lawina zaległych biuletynów po przestoju; zmyślona ocena pasm
  przy braku danych z jonosondy.

## Ograniczenia

- Jak 11a: testy bez sieci (fixtury z prawdziwych pobrań 2026-11-22 w
  `crates/aars-sources/tests/fixtures/{pogoda,propagacja}/`, plus istniejące
  `swpc/noaa-scales-2026-11-22.json`), HTTP przez `wiremock`, `crate::http`
  (limit rozmiaru, timeout), User-Agent `aars/{wersja}`; MET Norway wymaga
  identyfikacji z kontaktem: `aars/{wersja} github.com/sptq/aars` (bez danych
  osobowych), współrzędne zaokrąglone do 4 miejsc (wymóg MET).
- Nieznany schemat = `SourceError::Parse`, nigdy zera.
- `aars-core` nie zależy od `aars-sources`; `aars-web` nie zależy od
  `aars-sources` — dostęp z panelu przez wywołanie zwrotne jak „odśwież teraz”.
- Atrybucja w treści: „Dane: Open-Meteo.” / „Dane: MET Norway.”,
  „Dane: NOAA, GIRO przez KC2G.” (KC2G/GIRO CC BY-NC-SA — stacja
  amatorska jest niekomercyjna; ocena słowna to produkt pochodny).

## Podejścia

1. **Wybrane: osobny moduł biuletynu z własnym zadaniem w `aars-sources`.**
   `BulletinRunner` zna części (traity w `aars-sources`), sam pilnuje pór
   (czas lokalny Warszawy), składa tekst i wstawia jeden element do kolejki.
   Operator ustawia tylko pory i części. Pasuje do istniejącej zaślepki
   modułu `bulletin`.
2. Zmienne w treści komunikatów harmonogramu (`{pogoda}`) rozwijane przez
   `Scheduler` przez wstrzyknięty resolver. Elastyczne, ale cron w UTC,
   podgląd musiałby rozwijać zmienne, a pusta część zostawia dziurę w zdaniu.
   Odłożone.
3. Części jako pozycje źródeł w runnerze (stały klucz, zmiana = nadanie) —
   nadawałoby przy każdej zmianie liczby, nie o porach.

## Kompozytor

### Ustawienia modułu `bulletin` (klucze `module:bulletin:*`)

| klucz | rodzaj | domyślnie | znaczenie |
|---|---|---|---|
| `enabled` | przełącznik | `false` | jak `scheduler` |
| `times` | nowy `SettingKind::TimeList { max: 8 }` | `07:00,19:00` | pory HH:MM czasu lokalnego (Europe/Warsaw), po przecinku, posortowane i bez duplikatów po kanonizacji; pusty = błąd walidacji |
| `parts` | `List` | `pogoda,propagacja,ostrzezenia` | kolejność stała (jak w tabeli części), nie kolejność zaznaczenia |
| `intro` | `Text { max_len: 200 }` | pusty | pusty → „Biuletyn stacji {znak}.” (znak z `StationSettings.callsign`; bez znaku → „Biuletyn stacji.”) |

Toggle zmienia się z `Planned` na `Switchable`; migracja dodaje
`module:bulletin:enabled = false` (`INSERT OR IGNORE`). Testy, które dziś
twierdzą, że biuletynu nie da się włączyć, odwracamy.

### Pory i odpalanie

- `BulletinRunner::run(shutdown)` tyka co 10 s. Dla bieżącej daty lokalnej
  i każdej pory wyznacza chwilę UTC (`speech::warsaw_to_utc`; pora
  nieistniejąca przy zmianie czasu → pominięta, podwójna → pierwsza).
- Pora jest „do zrobienia”, gdy minęła, a spóźnienie ≤ `misfire_grace_s`
  ze `StationSettings` (jak harmonogram). Starsza — pomijana, z wpisem
  `skipped` (bez lawiny po przestoju; biuletyn nigdy nie jest `Emergency`).
- Idempotencja: nowa tabela `bulletin_fires(fire_at TEXT PRIMARY KEY,
  skipped_reason TEXT, queue_item_id INTEGER, created_at)` — `INSERT OR IGNORE`
  przed składaniem; brak wstawienia = już zrobione (restart nie dubluje).
  Moduł wyłączony: nic nie zapisuje.
- Także poprzedni dzień: pora 23:59 sprawdzana tuż po północy (liczymy pory
  dla dziś i wczoraj).

### Składanie

- Każda wybrana część dostaje `PartCtx { now, position, settings części,
  active_alerts }` i limit czasu 20 s; części liczone równolegle.
- Wynik części: `Ok(Some(tekst))`, `Ok(None)` (nic do powiedzenia, np. brak
  ostrzeżeń — ale patrz niżej), `Err` (log, część wypada).
- Tekst = wstęp + „Godzina {HH:MM}.” + części w stałej kolejności, każda
  zakończona kropką, + „Koniec biuletynu.”. Gdy żadna część nie dała tekstu:
  nic nie nadajemy, `skipped_reason = "brak danych"`.
- Limit długości 1500 znaków po złożeniu (część przekraczająca swój limit
  ucinana `truncate_sentence` w środku części — patrz limity części).
- Element kolejki: `message_id: None`, priorytet `Normal`, bez przerywania,
  `not_before = now`, `expires_at = fire_at + 15 min`, 1 powtórzenie.

### Podgląd

- `aars-sources` udostępnia `async fn compose(&self, now) -> Composed {
  text: Option<String>, parts: Vec<(id, PartOutcome)> }` — ta sama ścieżka co
  nadanie, bez zapisu do `bulletin_fires` i kolejki.
- CLI: `aars bulletin preview` drukuje tekst i stan każdej części.
- Panel (strona modułów, karta `bulletin`, rola operator): przycisk
  „Podgląd biuletynu” → POST z CSRF → wywołanie zwrotne `with_bulletin_preview`
  ustawiane w `station.rs` → tekst + stany części, odsłuch przez istniejący
  podgląd audio z tekstu (limit 2000 znaków już tam jest). Jeden podgląd naraz
  (semafor), błąd = komunikat na stronie.

## Część `pogoda` i źródło `pogoda`

### Dane

- Open-Meteo: `https://api.open-meteo.com/v1/forecast?latitude=..&longitude=..
  &current=temperature_2m,wind_speed_10m,wind_gusts_10m,weather_code,precipitation
  &hourly=temperature_2m,precipitation,wind_gusts_10m,weather_code
  &daily=temperature_2m_max,temperature_2m_min,precipitation_sum,wind_gusts_10m_max,weather_code
  &timezone=Europe%2FWarsaw&forecast_days=3` (baza w `sources.open_meteo_url`).
- MET Norway: `https://api.met.no/weatherapi/locationforecast/2.0/compact?lat=..&lon=..`
  (`sources.met_no_url`), tylko gdy Open-Meteo zawiedzie i tylko dla części
  biuletynu. MET compact nie ma porywów — w zapasie mówimy wiatr średni.
- Wspólny model `Forecast { issued: DateTime<Utc>, provider, current: Option<Now>,
  days: Vec<Day> }`, `Day { date, t_min, t_max, precip_mm, gust_max_kmh:
  Option<f64>, code: Condition }`; `Condition` — nasz wyliczeniowy opis
  (bezchmurnie, małe zachmurzenie, pochmurno, mgła, mżawka, deszcz, przelotny
  deszcz, deszcz ze śniegiem, śnieg, burza) z tabel kodów WMO i `symbol_code`
  MET (przyrostki `_day/_night/_polartwilight` obcinane). Nieznany kod → brak
  opisu słownego (nie błąd), reszta liczb zostaje.
- Pamięć podręczna w źródle: ostatnia udana prognoza (Mutex, odzysk po
  zatruciu jak w swpc) z czasem pobrania; część biuletynu używa jej, gdy
  ≤ 60 min i pozycja ta sama (zaokrąglona do 0,01°), inaczej pobiera.

### Tekst części

- Przed 14:00 lokalnie: „Pogoda dla okolic stacji. Teraz {t} stopni, {opis}.
  Dziś do {t_max} stopni, w nocy {t_min jutro}… opady {mm} milimetra / bez
  opadów, porywy wiatru do {g} kilometrów na godzinę.” Od 14:00: teraz + noc
  + jutro (dzień 1). Dokładne sformułowania w planie; liczby zaokrąglane do
  całości (opady do 1 miejsca, gdy < 10). Ujemne: „minus 3 stopnie” (słownie
  robi normalizer — tekst niesie cyfry i „stopni”, sprawdzić odmianę
  istniejącym normalizerem).
- Koniec: „Dane: Open-Meteo.” albo „Dane: MET Norway.”.
- Limit części 450 znaków. Brak pozycji stacji → `Ok(None)` z powodem „brak
  pozycji stacji” w podglądzie.

### Alerty progowe (DataSource `pogoda`)

- Needs `Position`, interwał domyślny 1800 s (min 900), zakres
  `{lat:.2},{lon:.2}|g={}|f={}|p={}`.
- Ustawienia (`source:pogoda:*`): `gust_kmh` Integer 40–150, domyślnie 75;
  `frost_c` Integer −40–0, domyślnie −15; `precip_mm` Integer 5–200,
  domyślnie 30 (suma dobowa).
- Dla dni 0 i 1 prognozy Open-Meteo (nie MET): każdy przekroczony próg to
  pozycja; klucz `{rodzaj}:{data}` (`gust:2026-11-23`), temat ten sam,
  odcisk = rodzaj + wartość zaokrąglona do progu kroku (porywy co 10 km/h,
  mróz co 5 °C, opady co 10 mm) — drobne wahania prognozy nie są zmianą.
- Tekst: „Prognoza pogody: {jutro/dziś} porywy wiatru do {g} kilometrów na
  godzinę.” / „…spadek temperatury do minus {x} stopni.” / „…opady do {mm}
  milimetrów.” + „Dane: Open-Meteo.”; zmiana „Zmiana prognozy: …”.
  `cancel_text` pusty (prognoza „przestała przewidywać” — bez komunikatu).
  Priorytet `Normal`, bez przerywania, `valid_until` = koniec dnia lokalnego.
- Wyłączone domyślnie jak każde źródło. Część biuletynu `pogoda` działa
  niezależnie od włączenia źródła (wybór w `parts` wystarcza) — ale używa
  tych samych adresów i pamięci.

## Część `propagacja`

Bez źródła w runnerze (nic nie ogłasza samo; burze ogłasza `swpc`).

- Dane, równolegle, każde z limitem wieku; dane za stare albo błąd = to zdanie
  wypada, nie cała część:
  - SFI: `https://services.swpc.noaa.gov/json/f107_cm_flux.json`
    (`sources.swpc_f107_url`) — najnowszy rekord `frequency == 2800`, wiek ≤ 36 h.
  - Kp: `https://services.swpc.noaa.gov/products/noaa-planetary-k-index.json`
    (`sources.swpc_kp_url`) — najnowszy rekord, `time_tag` to początek okna
    3 h, wiek ≤ 6 h od początku okna.
  - Skale: istniejący `swpc::parse::parse_scales` na `sources.swpc_url`.
  - Jonosonda: `https://prop.kc2g.com/api/stations.json` (`sources.kc2g_url`) —
    najbliższa pozycji stacji (bez pozycji stacji to zdanie wypada) w promieniu 1200 km, `time` ≤ 60 min,
    `cs == -1` (skalowanie ręczne) albo `cs ≥ 70`, `fof2` i `mufd` obecne.
- Słowa (progi stałe, w kodzie, opisane w README):
  - SFI: < 90 „niski”, 90–149 „umiarkowany”, ≥ 150 „wysoki”.
  - Kp (zaokrąglony do 1 miejsca): < 3 „spokojne”, 3–4,9 „niespokojne”,
    ≥ 5 „burza geomagnetyczna”.
  - NVIS: pasma 160/80/60/40/30 m (1,8; 3,5; 5,3; 7,0; 10,1 MHz) „dla
    łączności bliskich” — najwyższe pasmo o dolnej krawędzi < foF2 × 1,0
    (bez marginesu: foF2 jest częstotliwością krytyczną dla pionu).
  - DX: najwyższe pasmo z 80…10 m o dolnej krawędzi < MUF(3000) × 0,85
    („pasma do {N} metrów powinny być otwarte na dalsze trasy”).
- Tekst: „Propagacja. Strumień słoneczny {sfi}, {słowo}. Indeks K {kp},
  warunki {słowo}. [Pogoda kosmiczna bez zaburzeń. | Burza geomagnetyczna
  G{n}…] Jonosonda {miasto}: częstotliwość krytyczna {fof2} megaherca, MUF
  {mufd} megaherca. Łączności bliskie do pasma {N} metrów, dalekie do pasma
  {M} metrów. Dane: NOAA, GIRO przez KC2G.” (atrybucja KC2G tylko gdy
  jonosonda użyta). Nazwy miast jonosond: mała tabela polska (Warszawa,
  Pruhonice, Juliusruh, Sopron, Chilton, Dourbes, Rzym…), nieznana → nazwa
  z pliku do przecinka.
- Wszystkie zdania wypadły → `Ok(None)`. Limit 500 znaków.

## Część `ostrzezenia`

- Z `source_state`: aktywne pozycje (`TrackedState::Active`, `aired = true`,
  `valid_until` w przyszłości albo brak) wszystkich źródeł; sortowanie
  priorytet, potem `first_seen_at`.
- Tekst: „Obowiązujące ostrzeżenia: {summary}; {summary}.” (najwyżej 5,
  reszta „i {n} innych”). Brak → „Brak obowiązujących ostrzeżeń.” (to jest
  treść, nie `None` — słuchacz chce to wiedzieć). Limit 500 znaków.
- Wymaga `summary` w pozycjach — sprawdzić, że każde źródło 11a i `imgw-meteo`
  wypełnia je krótkim tekstem do wymówienia; puste → pozycja pominięta.

## Poprawki z 11a

1. Normalizer: „godz.” + godzina → `HourCase::Bare` („godz. dziewiąta
   dwadzieścia dwa”, bez wstawionego „godzina”). Test w `normalize.rs`.
2. RSO: `scope()` nie zależy od pola `atak-z-powietrza` w `categories`
   (kategoria i tak pobierana zawsze) — kanoniczna lista kategorii w zakresie
   z tą kategorią zawsze dodaną. Test: przełączenie pola nie zmienia zakresu.

## Rozstrzygnięcia (moje, do sprawdzenia przez właściciela)

1. Osobny moduł biuletynu zamiast zmiennych w komunikatach — prostszy panel;
   koszt: operator nie ułoży własnego tekstu wokół danych.
2. Pory w czasie lokalnym (nie UTC jak cron) — tak myśli słuchacz.
3. Bez hamqsl (licencja); oceny pasm z liczb własnymi progami — koszt: ocena
   uproszczona, czasem mniej trafna niż N0NBH.
4. KC2G `cs == -1` przyjmowane (połowa rekordów, w tym Warszawa) — koszt:
   ręcznie skalowana wartość może być błędna.
5. MET Norway tylko jako zapas części biuletynu, alerty tylko z Open-Meteo —
   koszt: przy awarii Open-Meteo alerty progowe milkną (źródło przestarzałe
   w panelu).
6. Pusty biuletyn nie jest nadawany; „brak ostrzeżeń” jest treścią.

## Rozstrzygnięcia po przeglądzie (mają pierwszeństwo przed tekstem wyżej)

1. **`ostrzezenia` — tylko żywe źródła.** Pozycja wchodzi, gdy: stan
   `Active`; jej źródło jest włączone w `source_state` i nie jest
   przestarzałe (`SourceStatus::is_stale`); `valid_until` w przyszłości albo
   brak; i poszła na antenę — `aired = true` **albo** jej `queue_item_id`
   ma stan `Done` (leniwe `aired` z runnera). Zapytanie w `aars-core`
   (join z `source_state`), testy na: źródło wyłączone, przestarzałe,
   nienadane, nadane-ale-jeszcze-nie-oznaczone.
2. **Długość a limit nadawania.** Limit znaków biuletynu =
   `min(1500, tx_max_duration_s × 10)` (ok. 10 znaków tekstu na sekundę
   z zapasem na rozwinięcie liczb). Gdy złożony tekst się nie mieści,
   wypadają całe części od końca kolejności (w podglądzie: „pominięta —
   limit czasu nadawania”); wstęp i „Koniec biuletynu.” zostają. Nadal
   jeden element kolejki.
3. **Alerty pogodowe:** temat = klucz (`gust:2026-11-23`) — każdy rodzaj
   i dzień śledzony osobno, bez reguły zastąpienia. `valid_until` = koniec
   **dnia prognozy** (północ lokalna po tej dacie, w UTC). Słowo „dziś”/
   „jutro” liczone przy pobraniu; odcisk go nie zawiera, więc przejście
   przez północ nie jest zmianą ani nowym ogłoszeniem.
4. **Cisza nocna.** Przy odpaleniu, gdy trwa okno ciszy, a `Normal` jest
   poniżej `quiet_min_priority` — wpis `bulletin_fires` z
   `skipped_reason = "cisza nocna"`, nic do kolejki. Pomoc ustawienia
   `times` mówi o tym. Sprawdzenie tą samą funkcją okna ciszy, której
   używa blokada (interlock), nie kopią.
5. **„Godzina {HH:MM}.”** — szablon nie pisze słowa „Godzina”: sama
   godzina „07:00.” (normalizer mówi „godzina siódma”). Poprawka nr 1 z 11a
   bez zmian.
6. (moje) Ta sama instancja `Pogoda` w runnerze i w biuletynie —
   `station.rs` buduje ją raz (konkretny typ), rejestruje jako
   `Arc<dyn DataSource>` i podaje kompozytorowi jako część.
   Znak stacji, `misfire_grace_s`, `tx_max_duration_s` i okna ciszy czytane
   z bazy przy każdym odpaleniu, nie z migawki startowej.
7. (moje) Jonosonda: najpierw filtr (wiek, `cs`, obecne `fof2`/`mufd`),
   potem najbliższa. DX: pasma 80, 40, 30, 20, 17, 15, 12, 10 m; NVIS:
   160, 80, 60, 40, 30 m. Gdy żadne pasmo nie spełnia warunku — zdanie
   o danym rodzaju łączności wypada.
