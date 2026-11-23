# Etap 12 — profesjonalny panel

## Po co

Właściciel (SP7TQ) chce, żeby panel wyglądał i działał jak narzędzie operatorskie,
a nie jak strona z kartami. Panel jest używany na stacji bez internetu (Raspberry Pi
w sieci lokalnej), przez operatora, który w każdej chwili musi widzieć, czy stacja
nadaje, i móc ją zatrzymać. Kierunek zatwierdzony przez właściciela („Ok działaj”).

## Cele

1. **Menu boczne w grupach** zamiast 12 linków w nagłówku:
   - *Nadawanie*: Pulpit, Nadaj, Komunikaty, Kolejka
   - *Dane*: Źródła, Moduły, Mapa
   - *System*: Ustawienia, Sprzęt, Aktualizacja, Użytkownicy, Dzienniki
   - widoczność pozycji jak dziś (`can_operate`, `is_admin`); bieżąca strona wyróżniona
     (`aria-current="page"`); na wąskim ekranie (< 900 px) menu chowa się pod przyciskiem
     „Menu” (bez JS: `<details>` albo checkbox-hack — wybór budowniczego).
2. **Stały pasek stanu** u góry każdej strony po zalogowaniu:
   - stan nadawania (włączone / wyłączone / watchdog / niedostępne) jako wyraźna plakietka,
   - cisza nocna teraz tak/nie (gdy skonfigurowana),
   - znak stacji (`station.callsign` z konfiguracji), zegar lokalny (czas renderu, odświeżany
     z fragmentem), użytkownik i rola, Wyloguj,
   - **wyłącznik nadawania zawsze pod ręką** dla `can_operate`: ten sam POST `/tx-enabled`
     co dziś, z CSRF; przy włączonym nadawaniu przycisk „Zatrzymaj nadawanie” (danger).
     Włączanie z paska też dozwolone, ale wyłącznie tym samym formularzem co na pulpicie.
   - pasek odświeża się fragmentem htmx (`GET /fragments/status`, co 5 s), a pierwszy
     render jest po stronie serwera (pasek nie może być pusty przy wczytaniu strony).
3. **Pulpit „rzut oka”**: rząd kafli — Nadawanie, Kolejka (liczba zadań, najbliższe),
   Źródła (ile ok / ile z problemem), Najbliższy biuletyn (godzina albo „wyłączony”);
   pod spodem: aktywne ostrzeżenia (z istniejących danych źródeł), ostatnie nadania
   (z istniejącego dziennika/kolejki), kolejka na żywo jak dziś. Wszystkie komunikaty
   błędów z obecnego pulpitu (`no_admin`, `tx_unavailable`, `last_good_note`,
   blokada watchdoga z przyciskiem) zostają, w tej samej treści.
4. **Spójny system wyglądu** w `assets/app.css`: tokeny kolorów, odstępów, typografii;
   tabele, formularze z tekstem pomocy pod polem, plakietki stanu ok/warn/error/disabled,
   przyciski primary/ghost/danger, puste stany („Brak zadań w kolejce”).
5. **Ikony** jako wbudowany SVG (sprite w `assets/icons.svg` albo inline w szablonie),
   bez fontów ikon i bez zasobów z sieci.
6. **Jasny i ciemny motyw**: domyślnie wg `prefers-color-scheme`, ręczny przełącznik
   w menu zapamiętany w `localStorage` (mały plik `assets/theme.js`; brak JS = motyw systemowy).
7. **Podgląd panelu bez sprzętu**: przykład `cargo run -p aars-web --example panel_demo`
   uruchamia panel na `127.0.0.1` z bazą w pamięci/tymczasowej, danymi przykładowymi
   (użytkownik admin z hasłem wypisanym na konsolę, kilka komunikatów, zadania w kolejce,
   ostrzeżenia na mapie z fałszywego `WarningsMap`, podgląd biuletynu z fałszywego
   `BulletinPreview`). Bez `StationControl`, bez probe, bez audio, bez PTT, bez portu
   szeregowego. Służy do zrzutów ekranu i do poradnika.

## Nie-cele

- Żadnych nowych funkcji stacji, nowych ustawień ani zmian logiki nadawania.
- Żadnego frameworka JS, bundlera, Node, CDN, fontów z sieci.
- Bez zmiany adresów URL, metod, nazw pól formularzy i kodów odpowiedzi.

## Anty-cele (porażka, nawet jeśli „działa”)

- Cokolwiek w nowym panelu może włączyć nadawanie bez świadomego kliknięcia
  (np. auto-submit, przycisk przełącznika bez potwierdzenia stanu, formularz w fragmencie
  odświeżanym tak, że kliknięcie trafia w inny stan niż widoczny).
  Formularz wyłącznika w pasku niesie jawną wartość docelową (`enabled=0`/`1`), jak dziś.
- Rola widzi pozycję menu albo przycisk, do którego nie ma prawa (macierz ról musi przejść).
- Demo dotyka sprzętu albo jest wkompilowane w binarkę `aars`.
- Panel przestaje działać na telefonie (szerokość 360 px) albo bez JS (poza mapą).

## Ograniczenia

- SSR askama + htmx (już w `assets/vendor`), zasoby przez rust-embed z `assets/`.
- `aars-web` nie zależy od `aars-sources`; dane do kafli tylko przez istniejące
  repozytoria `aars-core` i callbacki `AppState`.
- Wszystkie istniejące testy (w tym `role_matrix`) przechodzą; zmiany tekstów,
  na których opierają się testy, tylko gdy test sprawdza wygląd, nie zachowanie —
  każda taka zmiana opisana w raporcie zadania.
- Nie formatować całych plików `cargo fmt`; tylko nowe/zmienione fragmenty.
- Dostępność: kontrast WCAG AA w obu motywach, widoczny focus, etykiety formularzy,
  `<nav aria-label>`, przyciski z tekstem (nie same ikony).
- Polskie teksty interfejsu, jak dziś.

## Podejścia

1. **Przepisanie CSS + nowa rama `base.html`, strony dostosowane po kolei** (wybrane).
   Najmniej ryzyka: logika handlerów zostaje, zmiany głównie w szablonach.
2. Komponenty askama (makra) dla kart/tabel/formularzy i przepisanie wszystkich stron
   na makra — spójniej, ale dużo większy diff i ryzyko pomyłek w polach formularzy.
   Makra wolno użyć tam, gdzie powtórzenie jest oczywiste, bez przymusu.
3. Framework CSS (Pico, Tailwind) — odrzucone: zasób zewnętrzny albo krok budowania.

## Decyzje projektowe

- Dane paska stanu: nowa struktura (np. `StatusBar`) liczona w jednym miejscu
  z `AppState` (interlocki/ustawienia/konfiguracja) i dołączana do `Chrome`
  albo renderowana obok niego; ten sam kod dla pierwszego renderu i fragmentu.
- `/fragments/status` wymaga zalogowania jak inne fragmenty; dostępny dla każdej roli,
  wyłącznik tylko dla `can_operate`.
- Ikony: tylko te, które są potrzebne (ok. 15), rysowane prosto (stroke 1.5–2 px).
- Demo: `crates/aars-web/examples/panel_demo.rs`, zależności tylko dev-dependencies.

## Zostawione budowniczemu

- Konkretna paleta, krój (systemowy stos), siatka kafli, wygląd zegara.
- Czy menu na mobile to `<details>` czy checkbox.
- Kolejność dostosowania stron i podział na zadania.
- Dokładny zestaw danych przykładowych w demo.

## Rozstrzygnięcia po przeglądzie (decyzje Claude, do sprawdzenia przez właściciela)

1. **Dane paska i kafli tylko z odczytów lokalnych**, nigdy z sieci. Cisza nocna:
   `StationSettings::quiet_now` na czasie lokalnym. Najbliższy biuletyn: z ustawień modułu
   `bulletin` (włączony + lista godzin, `SettingKind::TimeList`) — czysta funkcja w aars-core
   lub aars-web; gdy moduł wyłączony albo brak godzin — „wyłączony”. Stan źródeł: z tych
   samych danych co dziś na pulpicie (`source_state`). `bulletin_preview` NIE jest wołany
   przez pulpit ani pasek.
2. **Jedna plakietka w pasku, pierwszeństwo:** watchdog (zablokowane) > niedostępne
   (`tx_unavailable`) > wyłączone > włączone. Cisza nocna to osobna, mniejsza plakietka.
3. **Wyłącznik w pasku:** gdy nadawanie włączone — „Zatrzymaj nadawanie” (`enabled=0`);
   gdy wyłączone — „Włącz nadawanie” (`enabled=1`) tylko gdy nie ma watchdoga ani
   `tx_unavailable`; inaczej przycisk Włącz ukryty, a zdjęcie blokady watchdoga zostaje
   tylko na pulpicie (admin). Wartość docelowa jest jawna, więc POST jest idempotentny:
   kliknięcie „Zatrzymaj” na nieświeżym pasku zawsze kończy się wyłączeniem.
4. **Przekierowanie po `/tx-enabled`:** formularz niesie ukryte pole `back` ze ścieżką
   bieżącej strony; handler przekierowuje tam tylko gdy to ścieżka lokalna zaczynająca się
   od `/` (nie `//`, bez schematu), inaczej na `/`. Brak pola = `/` jak dziś.
5. **Fragment `/fragments/status`:** za tym samym uwierzytelnieniem co `/fragments/queue`,
   renderowany per sesja z jej CSRF; wpis w `role_matrix` dla trzech ról (Podgląd widzi
   stan bez wyłącznika). Polling co 5 s z `hx-sync`, który nie podmienia paska w trakcie
   wysyłki formularza (`hx-sync="this:drop"` lub równoważne); odpytywanie wstrzymane,
   gdy fokus jest w pasku — wybór mechanizmu budowniczego.
6. **Pulpit zachowuje własny wyłącznik** (duplikat formularza jest dopuszczalny), więc
   testy pulpitu nie tracą tekstów. Testy, które sprawdzają *brak* „Zatrzymaj nadawanie”
   dla Podglądu, nadal mają przejść — Podgląd nie dostaje wyłącznika nigdzie.
7. **Motyw:** `assets/theme.js` w `<head>` (blokujący, kilka linii) żeby nie migało;
   bez JS — motyw systemowy. Brak CSP w kodzie; nie dodajemy skryptów inline.
8. **Demo:** `crates/aars-web/examples/panel_demo.rs`, `required-features = ["testing"]`
   jeśli potrzebuje `for_tests`; station/probe/update = `None` (strony pokażą swoje
   komunikaty „niedostępne” — to też jest do obejrzenia). Budowane w zwykłym
   `cargo test --workspace --all-features`/`cargo build --examples`, żeby nie gniło.
   Hasło admina stałe dla demo (`demo`), wypisane na konsolę; nasłuch tylko 127.0.0.1.
9. **Weryfikacja wyglądu:** zrzuty ekranu z demo (szerokość 1280 i 360 px, oba motywy)
   robione przez kontrolera; kontrast — tokeny kolorów sprawdzone skryptem liczącym
   współczynnik WCAG (≥ 4.5 dla tekstu) w testach zadania z CSS albo w raporcie.
10. **Znak stacji:** z działającej konfiguracji (`base_config` / ta sama wartość co
    strona sprzętu jako „running”); pusty — pasek pokazuje „bez znaku”.
    Zegar: czas lokalny serwera `HH:MM`, odświeżany z fragmentem.
11. **Mapa** zostaje w nowej ramie z obecnym układem, na pełną szerokość treści.
