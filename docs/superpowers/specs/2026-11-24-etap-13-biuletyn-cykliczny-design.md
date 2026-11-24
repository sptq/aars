# Etap 13 — cykliczny komunikat informacyjny, automatyczny obszar, panel bez przeładowań

## Po co

Właściciel (SP7TQ) przetestował wydanie 0.12.0-rc.2 i ocenił je jako zbyt słabe.
Cytaty z jego opinii:

- „Panel do całkowitego przebudowania — UX jest taki, że jak coś zapiszę, to odświeża
  stronę i nie jestem w stanie nic zrobić.”
- „Konfiguracja TERYT jest zepsuta. Masz koordynaty stacji, więc powinno robić się
  to z automatu — po co mi elementy z innych powiatów lub województw.”
- „Komunikat pogodowy jest bardzo słaby. Nic nie wnosi — nie ma wilgotności czy
  prognozy.”
- „Biuletyny pogodowe czy ostrzeżenia muszą się nadawać do 10 min. Jeśli jest alert
  RCB, to powinien nadawać go co 1 min.”
- „Zobacz, jak to jest w SR0WX… tam komunikat całości idzie co 10 min. U nas jakieś
  pojedyncze rzeczy. Brakuje sklejania całego biuletynu informacyjnego w całość, to
  ma być cykliczne i działać sprawnie.”

Wzorzec: stacje SR0WX (sr0wx.py) — jeden pełny komunikat co 10 minut, sklejony
z modułów, czytelny i bogaty w dane. Analiza SR0WX: `research_notes/SR0WX analiza/`.

Właściciel kazał działać bez pytań; poniższe rozstrzygnięcia są decyzjami Claude
i mają w nawiasie koszt pomyłki.

## Cele

### A. Cykliczny komunikat informacyjny (jak SR0WX)

1. Biuletyn nadawany **cyklicznie**, domyślnie **co 10 minut** (ustawienie 5–60 min),
   wyrównany do pełnych dziesiątek minut czasu lokalnego (…:00, :10, :20).
   Dotychczasowe stałe pory zostają jako drugi tryb („o stałych porach”).
2. Komunikat jest **sklejany w całość** z części, w stałej kolejności:
   wstęp (znak, „automatyczna stacja informacyjna”), godzina, **obowiązujące
   ostrzeżenia pełną treścią** (wszystkie aktywne ze źródeł: IMGW meteo i hydro, RSO,
   GIOŚ, EMSC, NOAA), pogoda bieżąca, prognoza, stany wód, jakość powietrza,
   propagacja, wschód/zachód słońca, zakończenie ze znakiem.
3. Ostrzeżenia **wracają w każdym cyklu, dopóki obowiązują** — więc słychać je
   najwyżej co 10 minut. Pierwsze ogłoszenie nowego ostrzeżenia zostaje natychmiastowe
   (jak dziś), cykl tylko je powtarza.
4. **Alert RCB i atak z powietrza** (RSO, priorytet Alarmowy): **3 razy co minutę**
   (ustawienia: liczba 1–10, odstęp 30–600 s), potem jako obowiązujące ostrzeżenie
   w każdym cyklu biuletynu (zmiana właściciela 2026-11-24).
4a. **Wizualny edytor biuletynu z klocków** (prośba właściciela): kolejność klocków
   (przeciąganie), pauza przed i po każdym, kiedy klocek jest nadawany (zawsze, co N-ty
   cykl, w godzinach), własne teksty, podgląd i odsłuch niezapisanego układu.
   Pauzy realizuje synteza (znacznik `[[pauza:MS]]` w tekście).
5. Długość komunikatu mieści się w limicie czasu nadawania (`tx_max_duration_s`);
   gdy za długi, wypadają części od końca listy priorytetów (najpierw propagacja,
   słońce, powietrze, wody; ostrzeżenia nigdy).
6. Cisza nocna działa jak dziś (komunikat Zwykły wstrzymany; Alarmowy przechodzi).

### B. Pogoda, która coś wnosi

7. Bieżące warunki: temperatura, odczuwalna (gdy różni się o ≥ 2 °C), wilgotność,
   ciśnienie (zredukowane do poziomu morza) z tendencją 3 h (rośnie/spada/bez zmian),
   wiatr: kierunek słownie (np. „północno-zachodni”), prędkość i porywy,
   zachmurzenie/opis, opad bieżący, widzialność gdy < 5 km.
8. Prognoza: najbliższe godziny (do 6 h: zmiana opisu, opad, porywy), reszta dnia albo
   noc i jutro (temperatury min/max, opad sumarycznie, wiatr, opis), wschód
   i zachód słońca.
9. Polszczyzna przez istniejący normalizer (jednostki °C, %, hPa, km/h, m/s, mm);
   brak angielskich słów i surowych kodów.

### C. Obszar z pozycji stacji

10. Powiat (TERYT 4 cyfry) i województwo (2 cyfry) wyliczane **automatycznie**
    z pozycji stacji (punkt w wielokącie na `assets/powiaty.geojson`).
11. Źródła obszarowe dostają obszar automatyczny, gdy ręczny jest pusty:
    IMGW meteo → powiat stacji, IMGW hydro → województwo stacji,
    RSO → województwo stacji. Pole ręczne zostaje jako „nadpisz obszar” (zaawansowane),
    domyślnie puste; panel pokazuje wyliczony obszar słownie.
12. Pozycja poza Polską → brak obszaru automatycznego, panel mówi to wprost.

### D. Panel bez przeładowań

13. Zapisanie ustawień, włączenie/wyłączenie modułu, odświeżenie źródła, akcje
    kolejki i komunikatów **nie przeładowują strony**: wymieniana jest tylko zmieniona
    karta, a wynik pokazuje powiadomienie („Zapisano”, błąd walidacji przy polu).
    Przewinięcie i fokus zostają.
14. Nawigacja między stronami bez pełnego przeładowania (wymiana treści, historia
    przeglądarki działa).
15. Strona Moduły przebudowana: lista modułów z przełącznikami i stanem, szczegóły
    jednego modułu obok/pod spodem (zakładki), ustawienia źródeł obszarowych pokazują
    obszar automatyczny; pola z opisami po polsku.
16. Bez JS panel dalej działa (formularze z przekierowaniem jak dziś).

## Nie-cele

- Nowe źródła danych (poza polami z Open-Meteo/MET Norway, które już pobieramy).
- Pliki dźwiękowe zamiast syntezy (SR0WX sklejał próbki; my mamy Pipera).
- Framework JS z krokiem budowania, Node, CDN.

## Anty-cele

- Cokolwiek nadaje bez świadomej decyzji operatora poza tym, co już nadaje stacja
  (cykl, alerty) — w szczególności panel nie może kliknięciem w odświeżony fragment
  wysłać czegoś innego niż widać.
- Powtórki RCB zapychają kolejkę albo przekraczają limit nadawania.
- Komunikat z angielskimi słowami, surowymi kodami lub liczbami czytanymi źle.
- Automatyczny obszar nadpisuje obszar, który operator świadomie wpisał.

## Podejścia do panelu (D)

1. **htmx: wymiana fragmentów i `hx-boost`** (wybrane). Każdy formularz wysyła się
   przez htmx i dostaje z powrotem swoją kartę oraz powiadomienie (nagłówek
   `HX-Trigger`); linki menu przez `hx-boost`. Zero kroku budowania, działa offline,
   istniejące testy SSR dalej sprawdzają treść, bez JS zostaje dzisiejsze zachowanie.
2. Prawdziwe SPA (np. Preact z dostarczonym plikiem ESM) i API JSON. Odrzucone:
   dubluje każdy ekran (API + klient), wymaga przepisania ~270 testów panelu i macierzy
   ról na API, a wynik dla użytkownika (brak przeładowań, szybkie przejścia) jest ten
   sam. (Koszt pomyłki: jeśli właściciel chce SPA dla samego SPA — przepisanie
   warstwy widoku później; API i tak powstaje częściowo w D.)

## Decyzje projektowe

- Cykl biuletynu to nowy tryb istniejącego `BulletinRunner` (ustawienie
  `module:bulletin:mode` = `cykl`/`pory`, `interval_min`). Domyślnie po aktualizacji:
  `cykl`, 10 min. Idempotencja jak dziś przez `bulletin_fires` (klucz = chwila cyklu).
- Element kolejki biuletynu wygasa po długości cyklu (nie zalega).
- Powtórki alertów: nowy mechanizm w runnerze źródeł — aktywna pozycja z polityką
  powtórki dostaje ponowne zgłoszenie po `repeat_every`, jeśli poprzednie poszło na
  antenę lub wygasło. Polityka z `SourceItem` (nowe pole `repeat_every: Option<Duration>`).
- Obszar automatyczny liczony w `aars-core` (`area::auto_area(position)`), geojson
  wbudowany w binarkę (`include_bytes!`), parsowany raz.
- Pogoda: Open-Meteo dostaje dodatkowe pola (`relative_humidity_2m`,
  `apparent_temperature`, `pressure_msl`, `wind_direction_10m`, `cloud_cover`,
  `visibility`, `daily: sunrise,sunset`, godzinowe `pressure_msl` do tendencji);
  MET Norway — co daje (`air_pressure_at_sea_level`, `relative_humidity`,
  `wind_from_direction`).

## Wnioski z SR0WX (research_notes/SR0WX analiza/sr0wx.md)

- SR0WX: jeden pełny komunikat z crona, moduły sklejane w stałej kolejności, ostrzeżenia
  czytane w każdym cyklu, dopóki obowiązują; bez stanu i bez powtórek alarmowych.
- Pogoda SR0WX: opis, zachmurzenie %, temperatura, ciśnienie, wilgotność, wiatr
  (kierunek słownie, prędkość w m/s i km/h), prognoza 4 h i 12 h; bez porywów,
  opadów, tendencji, dat ważności ostrzeżeń.
- Przejmujemy: pełny komunikat co cykl, kolejność „dane → zakończenie znakiem”,
  wiatr słownie z 8 kierunków, prędkość wiatru w **m/s** (jak IMGW), wilgotność,
  zachmurzenie, prognoza na najbliższe godziny, wschód/zachód słońca.
- Robimy lepiej: ostrzeżenia **na początku** (bezpieczeństwo), z poziomem, obszarem
  i ważnością; porywy, opady, tendencja ciśnienia, prognoza na jutro, data we wstępie;
  część bez danych wypada z krótką informacją w panelu, nie psuje komunikatu.

## Zostawione budowniczemu

- Dokładne sformułowania zdań (wzorowane na SR0WX, po polsku, zwięzłe).
- Układ zakładek strony Moduły.
