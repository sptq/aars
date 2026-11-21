# Etap 10: obraz Raspberry Pi i autoaktualizacja

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Właściciel nagrywa obraz `.img.xz` z wydania GitHub na kartę (Raspberry Pi Imager, z Wi-Fi i hasłem albo bez), wkłada kartę do Raspberry Pi i bez SSH wchodzi na `http://aars.local:8080`. Panel prowadzi go przez założenie pierwszego admina, chyba że konto wzięło się z hasła ustawionego w Imagerze. Potem admin jednym przyciskiem instaluje nowe wydanie: stacja sprawdza podpis i sumę, czeka na koniec bieżącego nadania, podmienia binarkę i wstaje na nowej wersji, a gdy nowa nie wstanie, wraca do poprzedniej razem z bazą.

**Architecture:** Binarka jest budowana natywnie na runnerze `ubuntu-24.04-arm`. Obraz powstaje z oficjalnego Raspberry Pi OS Lite 64-bit (Trixie, przypięta suma), który skrypt rozszerza w chroocie na tym samym runnerze: Piper (`piper-tts` z OHF-Voice w venv, przypięte hashe), polski głos, użytkownik `aars`, usługa systemd i konfiguracja. Wydanie ma manifest z sumami SHA-256 wszystkich plików. Manifest podpisuje właściciel kluczem minisign, który trzyma poza GitHubem, a klucz publiczny jest wkompilowany w binarkę. Aktualizacja ma trzy fazy. (1) Proces `aars` (użytkownik bez uprawnień) na żądanie admina pobiera manifest, podpis i archiwum do katalogu roboczego, sprawdza je, zapisuje żądanie i kończy pracę tą samą ścieżką co restart z etapu 9 (R4: dyspozytor dokańcza nadanie, join sekwencera). (2) systemd uruchamia usługę ponownie, a `ExecStartPre=-+` (root) woła strażnika: ten sprawdza podpis jeszcze raz, rozpakowuje wydanie do `/opt/aars/releases/<wersja>`, kopiuje bazę, przełącza dowiązania `current`/`previous` i zakłada plik próby. (3) Nowa wersja po 60 s zdrowej pracy zapisuje potwierdzenie. Po trzech nieudanych startach albo po werdykcie „zła” strażnik, uruchamiany wtedy z binarki **poprzedniej**, przywraca dowiązanie i kopię bazy. Panel steruje aktualizacją przez trait `UpdateControl` (jak `StationControl`), więc `aars-web` dalej zależy tylko od `aars-core`.

**Tech Stack:** GitHub Actions (`ubuntu-24.04-arm`, `macos-latest`), Raspberry Pi OS Lite 64-bit 2026-10-06 (Trixie, `init_format: cloudinit-rpi`), systemd (`Type=notify`, `WatchdogSec`), `piper-tts` 1.8.0 (OHF-Voice/piper1-gpl, GPL-3.0, osobny proces), głos `pl_PL-darkman-medium` (CC0), minisign (`minisign-verify` 0.3 w binarce, `minisign` 0.10 tylko w testach), `reqwest` 0.12 (rustls), `sha2`, `flate2`, `tar`, `semver`, `sd-notify` 0.5, `if-addrs` 0.15, `sha-crypt` 0.6, `yescrypt` 0.1, `wiremock` (fałszywy serwer wydań).

**Spec:** `docs/superpowers/specs/2026-10-13-aars-design.md` (§2 D1/D2/D6/D7, §3, §4.2–4.3, §8 role i pierwsze konto, §9, §10, §11, §12 „Wdrożenie”, §13 pkt 10)

## Słowa właściciela (2026-11-20)

„Rób od razu obraz na raspberry pi… moduł autoupdate…”. Spec §13 pkt 10: „gotowy obraz `.img` (Raspberry Pi OS Lite 64-bit, Piper z polskim głosem, usługa systemd) w wydaniach GitHub; pierwsze uruchomienie bez SSH (Wi-Fi i hasło z Raspberry Pi Imager albo kreator pierwszego admina w panelu); autoaktualizacja z GitHub Releases na żądanie admina: suma kontrolna, podmiana binarki, restart, powrót do poprzedniej wersji, gdy nowa nie wstanie; nigdy w trakcie nadawania”.

## Decyzje etapu 10

- **R1 Budowa binarki: natywny runner `ubuntu-24.04-arm`, nie cargo-zigbuild.** Spec §12 przewiduje cargo-zigbuild, ale `cpal` potrzebuje `alsa-sys`, czyli nagłówków i `libasound.so` dla aarch64, a `libsqlite3-sys` (bundled) kompilatora C dla aarch64. Zig nie ma `libasound`, więc trzeba by budować sysroot. Na runnerze arm64 wystarczy `apt install libasound2-dev pkg-config`. Do tego na tym samym runnerze chodzą `cargo test` i `cargo clippy` dla Linuksa. To zamyka odłożoną sprawę z etapu 9: kod `cfg(target_os = "linux")` (gpiocdev) nigdy nie był sprawdzany kompilatorem. Runnery arm64 są darmowe dla repozytoriów publicznych od sierpnia 2025 (`sptq/aars` jest publiczne). Ubuntu 24.04 ma glibc 2.39, a Trixie 2.41, więc binarka zbudowana na runnerze działa na obrazie. **Bookworm (glibc 2.36) nie jest wspierany.** Lokalnie na macOS kompilacja na Linuksa idzie przez `docker run --platform linux/arm64 rust:1.93-trixie` (tylko opisane w README, bez skryptu). Odstępstwo od §12 trafia do „Odstępstw od speca” w `plans/README.md`. *Koszt pomyłki:* gdyby GitHub odebrał darmowe runnery arm64, trzeba dołożyć cargo-zigbuild z sysrootem Debiana. To jedno zadanie CI, kod się nie zmienia.
- **R2 Obraz: rozszerzenie oficjalnego obrazu w chroocie, nie pi-gen ani rpi-image-gen.** Bazą jest `2026-10-06-raspios-trixie-arm64-lite.img.xz` z `downloads.raspberrypi.com`. Przypinamy sumy: archiwum `483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843`, obraz rozpakowany `b8a393dc1c3701450b5fffbfe74368a98fb6eab7415ce456cc0cead886ea8780` (sprawdzone 2026-11-21 w `os_list_imagingutility_v4.json`, tam `init_format: cloudinit-rpi`). Skrypt `packaging/image/build-image.sh`: weryfikuje sumę, rozpakowuje, powiększa partycję główną o 1 GiB (`truncate`, `parted resizepart`, `resize2fs`), montuje przez `losetup -P` i w chroocie (natywnie arm64, bez qemu) instaluje pakiety i pliki z Task 11. Na końcu `zerofree` i `xz -T0 -9`. Uzasadnienie: bity bazy są identyczne z oficjalnymi, więc cloud-init, pierwsze uruchomienie, powiększenie partycji i personalizacja w Imagerze działają tak jak w oficjalnym obrazie. pi-gen buduje system od zera (długo, Docker, na x86 qemu), a rpi-image-gen (od 2025, profile YAML) zaleca budowanie na samym Pi i przykłady ma jeszcze dla Bookworm. *Koszt pomyłki:* gdyby chroot na runnerze zawiódł (montowania, `apt` w chroocie), jedno zadanie przepisujemy na etap pi-gen. Pliki z Task 11 zostają bez zmian.
- **R3 Piper i głos.** Silnik to `piper-tts` **1.8.0** z OHF-Voice/piper1-gpl (następca archiwalnego `rhasspy/piper`, zarchiwizowanego 2025-10-06). Instalacja w venv `/opt/aars/piper` przez `pip install --require-hashes -r packaging/piper/requirements.txt`, a binarka to `/opt/aars/piper/bin/piper`. Hash koła aarch64 to `sha256:3f60c1917de6d8e8033f395878ad3f88f6dfee88a8b05f98971a275f76a38484` (z GitHub Release v1.8.0). Hashe zależności (`onnxruntime` i reszta) generuje implementer przez `pip download --only-binary :all: --platform manylinux_2_28_aarch64 --python-version 3.13` i wpisuje do pliku. Czysty `pip` bez sieci w testach nie występuje. CLI 1.8.0 przyjmuje te same flagi, których używa `PiperSynth`: `--model`, `--output_file`, `--length_scale` i tekst ze stdin (sprawdzone w `src/piper/__main__.py` 2026-11-21). Stara binarka C++ (2023.11.14-2) **odpada**, bo karta modelu `darkman` mówi „Requires Piper 1.5 or higher”. Głos to `pl_PL-darkman-medium` (zbiór danych CC0) z `huggingface.co/rhasspy/piper-voices`, **przypięty do rewizji** `e9c8a39444359388adaa94255becb37a5a2b8e51`, bo repozytorium głosów jest właśnie przetrenowywane: `.onnx` sha256 `cf757ee307e3a7c23d5d8f69934deab51683d01c1c6be376d3e13146020f1530` (63 221 984 B), `.onnx.json` sha256 `1208a7d257daba8d0a421e9f55ec8210db009d785cd4f7e0b0c9936b3168f1a0`. Wszystko to opisuje `packaging/voices.lock`. Licencje: obraz zawiera `/usr/share/doc/aars/THIRD-PARTY.md` (piper-tts GPL-3.0, onnxruntime MIT, espeak-ng GPL-3.0 w kole piper, głos CC0, AARS MIT). Wydanie ma dołączony `piper_tts-1.8.0.tar.gz` (źródło GPL, sha256 `830588aded347df579c91a32703e0fc2a3685d84f1e3533b14f2de69135d4904`). AARS woła Pipera jako osobny proces (D4), więc GPL nie obejmuje kodu AARS. *Koszt pomyłki:* gdy `onnxruntime` nie ma koła cp313 aarch64, venv dostaje Pythona z `uv python install 3.12` (jedna linia w skrypcie obrazu).
- **R4 Zaufanie do wydania: podpis minisign nad manifestem, klucz poza GitHubem.** Sama suma SHA-256 z tego samego wydania chroni tylko przed uszkodzeniem: kto przejmie konto GitHub albo token CI, podmieni plik i sumę naraz. Atestacja GitHub (Sigstore) w Ruście offline oznacza `sigstore-rs` z korzeniem TUF, więc to za ciężko na Pi. Dlatego: `aars-manifest.json` wymienia każdy plik wydania z sumą i rozmiarem, a `aars-manifest.json.minisig` to podpis Ed25519. **Komentarz zaufany podpisu** musi brzmieć `aars {wersja}`, co broni przed podsunięciem starszego, prawdziwie podpisanego manifestu jako „najnowszego”. Klucz publiczny leży w repo w `keys/aars-release.pub` i jest wkompilowany (`include_str!`). Gdy plik zawiera `UNSET`, aktualizacja jest wyłączona z komunikatem `Brak klucza wydań w tej kompilacji — aktualizacja z panelu wyłączona.` Klucz prywatny trzyma właściciel offline. CI tworzy **szkic** wydania, a właściciel uruchamia `scripts/sign-release.sh v{wersja}` (minisign + `gh release upload` + `gh release edit --draft=false`). Szkic nie jest widoczny pod `releases/latest`, więc stacje go nie widzą. Zmiana klucza: wydanie podpisane starym kluczem wnosi nowy `keys/aars-release.pub`. *Koszt pomyłki:* gdy właściciel woli podpis w CI (sekret `MINISIGN_SECRET_KEY` w środowisku z ręcznym zatwierdzeniem), zmienia się tylko `release.yml` i `sign-release.sh`. Zaufanie spada wtedy do poziomu „konto GitHub”.
- **R5 Gdzie i jak sprawdzamy dostępność.** Tylko na żądanie admina (przycisk), bez odpytywania w tle, bo stacja nie dzwoni do domu. Adres: `{update.releases_url}/latest/download/aars-manifest.json` oraz `.minisig`, a archiwum `{releases_url}/download/v{wersja}/{nazwa}`. Bez API GitHuba, więc bez limitu 60 zapytań na godzinę i bez parsowania JSON API. `update.releases_url` ma domyślnie `https://github.com/sptq/aars/releases` i ustawia się **tylko w pliku** (jak ścieżki w etapie 9 R2: kto zmienia źródło aktualizacji, ten wybiera kod do uruchomienia). Limity: manifest ≤ 64 KiB, podpis ≤ 4 KiB, archiwum ≤ 64 MiB, każde żądanie ≤ 120 s, User-Agent `aars/{wersja}`. Odrzucamy: wersję ≤ bieżącej (`semver`), `target` ≠ `aarch64-unknown-linux-gnu` (na innej platformie aktualizacja jest wyłączona), `format` ≠ 1 i każdy plik spoza manifestu. Zły zegar Pi bez RTC daje błąd TLS, a komunikat brzmi `{błąd} — sprawdź datę i godzinę stacji (synchronizacja NTP)`.
- **R6 Układ na dysku i podmiana.** `/opt/aars/releases/{wersja}/{aars, VERSION, LICENSE}` (root:root, 0755), dowiązania `/opt/aars/current` i `/opt/aars/previous`. Podmiana to nowe dowiązanie pod nazwą tymczasową i `rename(2)`, czyli atomowo. Stan strażnika żyje w `/var/lib/aars-update/` (root, 0755): `trial.json`, `snapshots/`, `incoming/`. Katalog roboczy usługi to `/var/lib/aars/update/` (użytkownik `aars`): `staging/{wersja}/`, `request`, `confirmed`, `verdict`, `last-result.json`. Strażnik (root) czyta z katalogu `aars` wyłącznie przez `O_NOFOLLOW`, z limitem 4 KiB. Archiwum kopiuje najpierw do `incoming/` i dopiero tam je sprawdza (bez TOCTOU). Do katalogu `aars` pisze tylko plikiem tymczasowym `O_CREAT|O_EXCL|O_NOFOLLOW` i `rename`. Z archiwum przyjmujemy tylko zwykłe pliki `aars`, `VERSION`, `LICENSE`, bez `/` i `..` w nazwie. Trzymamy najwyżej trzy katalogi w `releases/`: bieżący, poprzedni i nowy.
- **R7 Kopia bazy zamiast migracji w dół.** Nowa wersja może nałożyć migracje, których stara nie zna (sqlx odmawia startu przy nieznanej zastosowanej migracji). Strażnik przed przełączeniem robi więc kopię `aars.db` i `aars.db-wal` (jeśli jest) do `snapshots/aars-{stara_wersja}.db`. Usługa jest wtedy zatrzymana, a to `ExecStartPre`, więc zwykła kopia pliku jest spójna. Strażnik nie otwiera bazy przez sqlx jako root, bo powstałyby pliki `-shm`/`-wal` roota. Powrót przywraca kopię, usuwa `-wal`/`-shm` i robi `chown aars:aars`. **Dane zapisane przez nową wersję w czasie próby (najwyżej kilka minut) przepadają**, a panel mówi to przy wyniku. *Koszt pomyłki:* kolejka i dziennik nadań z okresu próby giną po nieudanej aktualizacji. To świadoma cena, bo bez kopii powrót mógłby trwale unieruchomić stację.
- **R8 Strażnik startu (`aars update guard`).** `aars.service` ma `ExecStartPre=-+/opt/aars/bin/aars-update-guard --config /etc/aars/aars.toml`. `+` znaczy root, a `-` znaczy, że porażka strażnika nie blokuje startu. Czterolinijkowy skrypt woła `/opt/aars/previous/aars update guard`, gdy istnieje `trial.json` (próba trwa, więc poprzednia binarka jest tą sprawdzoną), a w przeciwnym razie `/opt/aars/current/aars update guard`. Kolejność kroków strażnika:
  1. Samonaprawa: gdy `current` nie wskazuje działającego katalogu, a `previous` tak, `current := previous`.
  2. Gdy jest próba: `confirmed` z tą wersją zamyka próbę (usuwa `trial.json`, kopia bazy zostaje do ręcznego powrotu). `verdict` z tą wersją albo `attempts ≥ 3` daje powrót (R7). W pozostałych przypadkach `attempts += 1`.
  3. Gdy jest `request` i nie ma próby: instalacja (R6), w tym test dymny `releases/{nowa}/aars --version` (musi wypisać `aars {nowa}`; łapie złą architekturę i glibc **przed** przełączeniem), potem kopia bazy, przełączenie i `trial.json { format: 1, version, previous, installed_at, attempts: 0, radio_was_up }`. `radio_was_up` to odczyt `hw_state:origin` ≠ `none` sprzed instalacji, przez `sqlite` w trybie `immutable=1` (bez plików pomocniczych).
  4. Wynik każdej instalacji, odmowy i powrotu idzie do `last-result.json` (`{ format, at, kind: installed|rolled_back|failed, version, from, detail }`).
  Strażnik zawsze kończy się kodem 0 i nigdy nie dotyka PTT ani audio.
- **R9 Kiedy nowa wersja „wstała”.** Usługa ma `Type=notify` i `TimeoutStartSec=180`. `READY=1` idzie, gdy panel nasłuchuje i stos nadawczy został zbudowany albo stacja świadomie przeszła w tryb bez nadawania (etap 9 R6). Zawieszony start zabija więc systemd, a to liczy się jako próba. `WatchdogSec=60`, a `WATCHDOG=1` co 20 s z zadania tokio: wykrywa zatrzymany runtime. Strażnik TX (D7) działa niezależnie w swoim wątku. Potwierdzenie: po **60 s** pracy od `READY=1` proces z trwającą próbą swojej wersji zapisuje `confirmed`, jeśli jest zdrowy. Zdrowy znaczy: dyspozytor żyje, a gdy `radio_was_up`, stos nadawczy działa. Gdy `radio_was_up`, a nowa wersja nie zbudowała stosu, który stara budowała, proces zapisuje `verdict` (`bad {wersja}: nadawanie nie wstało: {błąd}`) i kończy się błędem. Bez stosu nic nie nadaje, więc wyjście jest bezpieczne, a strażnik od razu wraca do poprzedniej wersji. *Koszt pomyłki:* gdy właściciel w czasie próby zmieni sprzęt w panelu na zepsuty i zrestartuje, aktualizacja się cofnie. Panel ostrzega o tym przy próbie.
- **R10 Nigdy w trakcie nadawania.** (a) Prośba o instalację przechodzi tę samą ścieżkę co restart z etapu 9 (R4): `drain` w dyspozytorze, dokończenie bieżącego nadania (strażnik TX działa dalej), join wątku sekwencera, zatrzymanie reszty. `run_once` zwraca nowe `Exit::Update`, a `main` kończy proces kodem 0, który `Restart=always` podnosi. (b) Instalacja dzieje się w `ExecStartPre`, gdy procesu `aars` już nie ma, a jądro zwolniło linie GPIO i port. (c) Odmowa przyjęcia prośby, gdy w kolejce czeka zadanie `Emergency` albo `High` z `not_before ≤ teraz + 60 s`: `Czeka pilny komunikat — aktualizacja możliwa po jego nadaniu.` (d) Pobieranie i sprawdzanie dzieje się, zanim stacja zacznie się zatrzymywać. Nadawanie w tym czasie idzie normalnie. (e) `SIGTERM` od `systemctl stop` działa jak Ctrl-C: przerywa nadanie (PTT w dół w ~150 ms) i czeka na ograniczony join. Dziś proces reaguje tylko na SIGINT, a SIGTERM zabija go bez opuszczenia PTT przez sekwencer (Task 2).
- **R11 Aktualizacja tylko pod systemd i tylko na Linuksie aarch64.** `supported` = `NOTIFY_SOCKET` ustawione, `target_os = "linux"`, `target_arch = "aarch64"` i klucz ≠ `UNSET`. W pozostałych przypadkach strona aktualizacji pokazuje wersję i powód, bez przycisków: `Aktualizacja z panelu działa tylko w obrazie Raspberry Pi (usługa systemd).`
- **R12 Pierwsze konto bez SSH.** Nowy klucz pliku `setup.first_admin = "console" | "wizard"`. Domyślnie `console` (dzisiejsze zachowanie, hasło na konsoli), a obraz ma `wizard`. W trybie `wizard`, gdy nie ma żadnego włączonego admina, `GET /` i `/login` przekierowują na `/setup`. Formularz: nazwa (3–32 znaki `[a-z0-9._-]`), hasło (≥ 10 znaków) i jego powtórzenie. CSRF działa jako double-submit: losowe ciasteczko `aars_setup` (`HttpOnly`, `SameSite=Strict`, 15 min) i to samo w polu formularza. Kreator jest dostępny **tylko z adresów lokalnych**: pętla zwrotna, RFC 1918, link-local v4/v6, ULA `fc00::/7` oraz IPv6 z tym samym prefiksem `/64` co dowolny adres interfejsu stacji. Polski operator często daje w LAN adresy globalne IPv6, stąd ten ostatni przypadek. Listę sieci binarka liczy przy starcie (`if-addrs`) i podaje do `AppState`. Inny adres dostaje `403` `Kreator pierwszego konta działa tylko z sieci lokalnej stacji.` Po założeniu konta `/setup` daje `404`, a audyt dostaje `first_admin` (`{nazwa} z {adres}`). Bez punktu dostępowego Wi-Fi: sieć bierze się z Imagera, z kabla Ethernet albo z ręcznie edytowanego `network-config` na partycji boot (README). *Koszt pomyłki:* ktoś w tej samej sieci lokalnej może zająć stację pierwszy. Widać to od razu, bo właściciel nie zaloguje się i dostanie `404` na `/setup`, a ratunkiem jest R13.
- **R13 Hasło z Imagera i ratunek z partycji boot** (zadanie do cięcia, Task 10). Jednorazowa usługa `aars-firstboot.service` (root, `After=cloud-final.service userconfig.service`, `Before=aars.service`) woła `aars setup import-os-user`. Polecenie bierze pierwszego użytkownika z UID 1000–59999 i jego hash z `/etc/shadow`. Gdy hash ma postać `$y$` (yescrypt, którego Imager używa dla obrazów od 2023), `$6$` albo `$5$`, polecenie zapisuje `/var/lib/aars/bootstrap-admin` (0600, `aars`). AARS przy starcie bez żadnego użytkownika zakłada z tego pliku admina o tej samej nazwie i z tym hashem, plik usuwa, a do audytu idzie `first_admin` (`{nazwa} z Raspberry Pi Imager`). `UserRepo::authenticate` umie zweryfikować hash crypt i przy pierwszym udanym logowaniu przepisuje go na Argon2id. Ratunek: pusty plik `aars-reset-admin` na partycji boot (`/boot/firmware/`). `aars-firstboot` (też przy każdym starcie, `ConditionPathExists`) wyłącza wtedy wszystkie konta admina, kończy ich sesje, usuwa plik i zapisuje audyt `admin_reset`, więc kreator otwiera się znowu. *Koszt pomyłki:* gdy Imager zapisze hash w nieznanym formacie, zostaje kreator (R12). Nic nie przestaje działać.
- **R14 Sieć i nazwa.** Obraz ma `hostname = aars` (Imager może to nadpisać), `avahi-daemon` z bazy Lite daje `aars.local`, a panel słucha na `0.0.0.0:8080`, bo port < 1024 wymagałby uprawnień (etap 9 R9). Bez HTTPS: panel działa w LAN, a README to mówi. Wi-Fi tylko z Imagera albo z `network-config` (R12).
- **R15 Usługa i uprawnienia.** `aars.service`: `User=aars`, `Group=aars`, `SupplementaryGroups=audio gpio dialout`, `StateDirectory=aars`, `ExecStart=/opt/aars/current/aars --config /etc/aars/aars.toml run`, `Restart=always`, `RestartSec=5`, `StartLimitIntervalSec=0` (stacja ostrzegawcza się nie poddaje), `KillMode=mixed` (Piper dostaje sygnał razem z procesem), `TimeoutStopSec=30`, `NoNewPrivileges=yes`, `ProtectSystem=strict`, `ReadWritePaths=/var/lib/aars`, `ProtectHome=yes`, `PrivateTmp=yes`. Bez `DeviceAllow`, bo urządzenia GPIO, ALSA i tty dają grupy. `ExecStartPre=-+` działa mimo `ProtectSystem` (`+` znosi ograniczenia). Użytkownik przez `sysusers.d`. Od etapu 9 panika dyspozytora kończy proces błędem, a `Restart=always` go podnosi (sprawa odłożona z etapu 9).
- **R16 Dźwięk na Pi (M-1 z etapu 9).** `CpalSink::open` otwiera dziś tylko mono. Karty USB (CM108) i `hw:X,Y` bez `plug` często mają tylko stereo, więc z panelu wybrana karta nie wstanie, choć jest dobra. Nowa czysta funkcja `choose_channels(&[ConfigRange], rate) -> Result<u16, String>` wybiera 1 kanał, gdy jest, potem 2, potem najmniej kanałów ≥ 1 przy danej częstotliwości. Wyjście kopiuje próbkę mono na wszystkie kanały. Gdy żadna konfiguracja nie ma tej częstotliwości, błąd `urządzenie {x} nie obsługuje {rate} Hz (obsługuje: {lista})` wraca przy budowie stosu, więc taka konfiguracja nie staje się `hw_good` (obawa M-1).
- **R17 Wersja produktu** to `version` crate'a `aars`, a znacznik wydania to `v{wersja}`. Workflow wydania sprawdza, czy znacznik zgadza się z `aars --version`. Proponowane pierwsze wydanie: `v0.10.0`, czyli numer etapu (pytanie do właściciela). Wersja widnieje w stopce panelu (każda rola), w `/healthz` (`ok {wersja}`) i w `aars update status`.
- **R18 Role i audyt.** `/settings/update` oraz `POST /settings/update/check` i `/settings/update/install`: **admin**, z CSRF. Viewer i operator widzą tylko wersję w stopce. `/setup` jest bez logowania, ale tylko gdy nie ma admina, z sieci lokalnej i z CSRF double-submit (R12). Audyt: `update_install` (`{z} → {do}: {użytkownik}`), systemowe (bez użytkownika) `update_installed`, `update_rolled_back`, `update_failed` (z `last-result.json`, raz, przy starcie następnej wersji), `first_admin`, `admin_reset`. Każde ma polską etykietę w `audit_action_label`.

## Global Constraints

- Kierunek zależności (spec §3): `aars` → {`aars-web`, `aars-sources`, `aars-audio`, `aars-ptt`} → `aars-core`. `aars-web` nie zależy od `aars-sources`, `aars-audio`, `aars-ptt` ani od kodu aktualizacji. Pobieranie, weryfikacja, instalacja i strażnik żyją w `crates/aars/src/update/`, a w `aars-core::update` są tylko czyste typy (manifest, stan próby, wynik, stan dla panelu). Bez nowego crate'a.
- Sekwencer jest jedynym właścicielem PTT i audio (D2). Kod aktualizacji, strażnik i kreator nie otwierają PTT, karty dźwiękowej ani portu szeregowego. Walidacja nigdy nie otwiera portu (DTR/RTS kluczuje radio), karty ani linii GPIO.
- Niezmienniki TX z etapów 3–9 zostają: wyłącznik nadawania dostępny operatorowi, limit TX w sekwencerze, watchdog w osobnym wątku, lepka flaga `poisoned`, kolejność interlocków, strażnik RAII, restart i aktualizacja nigdy w trakcie nadania (R10).
- Role (spec §8) i CSRF na każdym formularzu zmieniającym stan. Pola formularzy jako `String` z `#[serde(default)]` (macierz z etapu 7).
- Żaden test nie dotyka sieci (fałszywy serwer wydań to `wiremock` na `127.0.0.1`), karty dźwiękowej, GPIO, portu szeregowego, `/opt`, `/var/lib` ani `/etc`. Wszystkie ścieżki aktualizacji są wstrzykiwane (`UpdatePaths`), a klucz wydań w testach to para wygenerowana w teście (`minisign` jako dev-dependency). Testy nie wymagają roota ani systemd (`NOTIFY_SOCKET` w testach to gniazdo w `tempdir`).
- Wszystko ma działać na macOS (praca) i na Linuksie arm64 (CI). Kod tylko linuksowy jest za `cfg(target_os = "linux")` i sprawdzany w CI na `ubuntu-24.04-arm`.
- Nowe zależności tylko z listy w Tech Stack. Wersje są przypięte w `Cargo.lock`, a `cargo build --locked` w CI.
- `cargo test --workspace` zielone, `cargo clippy --workspace --all-targets -- -D warnings` czyste na obu platformach. Nie uruchamiaj `cargo fmt` na całym repo.
- Teksty panelu, komunikaty CLI dla użytkownika i ten plan po polsku. Commity bez atrybucji AI (zasada właściciela).
- TDD, commit po każdym zielonym cyklu.

## Review Focus

- **Prośba o aktualizację w trakcie długiego nadania:** nadanie kończy się `ok`, PTT opada raz, proces kończy się dopiero po joinie sekwencera, a drugie zadanie z kolejki nie zostaje wzięte. Testy w Task 7 (`an_update_requested_mid_transmission_waits_for_the_unkey`, `an_update_is_refused_while_an_emergency_is_due`).
- **Nowa wersja nie wstaje:** trzy starty bez potwierdzenia albo werdykt `bad` → `current` wskazuje starą wersję, baza wraca z kopii z właścicielem `aars`, `last-result.json` mówi `rolled_back`, a stara wersja zapisuje to w audycie. Testy w Task 6 (`three_unconfirmed_starts_roll_back_with_the_database`, `a_bad_verdict_rolls_back_at_once`) i Task 7 (`the_last_result_is_audited_once`).
- **Podrobione albo stare wydanie:** zła suma, zły podpis, podpis innym kluczem, komentarz zaufany z inną wersją, wersja ≤ bieżącej, plik spoza manifestu, archiwum ze ścieżką `../` albo dowiązaniem, za duży plik. Nic nie trafia do `releases/`, a `current` się nie zmienia. Testy w Task 5 i Task 6.
- **Strażnik jako root czyta i pisze w katalogu użytkownika `aars`:** podłożone dowiązanie zamiast `confirmed` albo `last-result.json` nie powoduje odczytu ani zapisu poza katalogiem. Testy w Task 6 (`the_guard_does_not_follow_symlinks_planted_by_the_service`).
- **SIGTERM w trakcie nadania:** PTT opada przez sekwencer, proces kończy się w ograniczonym czasie (Task 2, `sigterm_stops_the_station_like_ctrl_c`).
- **Kreator pierwszego konta:** niedostępny z adresu spoza sieci lokalnej, po założeniu admina, bez ciasteczka CSRF i przy trybie `console`. Testy w Task 9.

---

### Task 1: CI na Linuksie arm64 i macOS

**Files:**
- Create: `.github/workflows/ci.yml`
- Modify (gdy kompilator coś znajdzie): `crates/aars-ptt/src/gpio.rs`, `crates/aars-ptt/src/probe.rs`, `crates/aars-ptt/src/lib.rs`

**Interfaces:**
- `ci.yml`: `on: [push, pull_request]`, macierz `os: [macos-latest, ubuntu-24.04-arm]`. Kroki: `actions/checkout@v4`, `dtolnay/rust-toolchain@stable` z `components: clippy` (wersja ≥ `rust-version` 1.93), `Swatinem/rust-cache@v2`, na Linuksie `sudo apt-get install -y libasound2-dev pkg-config shellcheck`, potem `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, a na Linuksie dodatkowo `shellcheck packaging/**/*.sh scripts/*.sh packaging/aars-update-guard` (krok pomijany, dopóki plików nie ma: `if: hashFiles('packaging/**') != ''`).
- Akcje przypięte do znacznika głównej wersji. Hash commita jest opcjonalny (pytanie do właściciela, jak w R4).

- [ ] **Step 1: Test:** lokalnie nie ma czego uruchomić. „Test” to pierwszy przebieg CI na gałęzi. Zanim go wypchniesz, sprawdź, czy kod linuksowy w ogóle się kompiluje: `docker run --rm --platform linux/arm64 -v "$PWD":/w -w /w rust:1.93-trixie sh -c 'apt-get update && apt-get install -y libasound2-dev pkg-config && cargo clippy --workspace --all-targets -- -D warnings'` (gdy Docker jest dostępny; inaczej od razu CI).
- [ ] **Step 2: FAIL:** spodziewane błędy w `cfg(target_os = "linux")` (gpiocdev 0.8 API, nieużywane importy). Zapisz je w raporcie.
- [ ] **Step 3: Implementacja:** popraw kod linuksowy tak, żeby przeszedł clippy i testy. Bez zmiany zachowania na macOS. Gdy poprawka zmienia semantykę PTT GPIO (np. `active_low`, wartość po zwolnieniu linii), dopisz test jednostkowy za `cfg(target_os = "linux")`, który nie żąda linii (tylko logika wartości), i opisz zmianę w raporcie dla przeglądu TX.
- [ ] **Step 4: PASS:** zielone CI na obu systemach (właściciel wypycha gałąź albo daje zgodę na push; patrz pytania).
- [ ] **Step 5: Commit**: `ci: testy i clippy na macOS i Linuksie arm64`

---

### Task 2: Proces pod systemd — SIGTERM, sd_notify, wersja

**Files:**
- Create: `crates/aars/src/service.rs`
- Modify: `crates/aars/src/main.rs` (sygnały, `Exit`), `crates/aars/src/station.rs` (`ready` → notifier), `crates/aars/Cargo.toml` (`sd-notify`), `crates/aars-web/src/lib.rs` (`/healthz`), `crates/aars-web/src/state.rs` (`version`), `crates/aars-web/templates/base.html` (stopka), `crates/aars/tests/cli.rs`

**Interfaces:**
- Produces (`aars::service`):
```rust
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub trait Notifier: Send + Sync {
    fn ready(&self);                 // READY=1 + STATUS=…
    fn status(&self, text: &str);    // STATUS=
    fn watchdog(&self);              // WATCHDOG=1
    fn stopping(&self);              // STOPPING=1
}
pub struct SystemdNotifier;          // sd_notify::notify(false, …); bez NOTIFY_SOCKET → nic
pub fn under_systemd() -> bool;      // std::env::var_os("NOTIFY_SOCKET").is_some()
pub fn watchdog_interval() -> Option<Duration>;  // sd_notify::watchdog_enabled → połowa WatchdogSec
pub async fn wait_for_stop_signal(); // SIGINT albo SIGTERM (tokio::signal::unix na unix)
#[cfg(test)] pub struct FakeNotifier { pub events: Mutex<Vec<String>> }
```
- `run_once(…, notifier: Arc<dyn Notifier>)`: `ready()` raz na proces, po zbudowaniu stosu (albo trybie bez nadawania) i nasłuchu panelu. Przy kolejnych restartach w procesie tylko `status("restart w procesie")`. `stopping()` przy `Exit::Stop`. Zadanie `watchdog` co `watchdog_interval()` tyka przez cały czas życia procesu (w `main`, poza `run_once`), także podczas drenażu.
- `main`: `stop` od `wait_for_stop_signal()` zamiast samego `ctrl_c`.
- `AppState.version: &'static str` (builder `with_version`; `for_tests` = `"0.0.0-test"`). `/healthz` → `ok {version}`. Stopka `base.html`: `AARS {version}` dla każdej roli i na `/login`.

- [ ] **Step 1: Testy**
  - `service.rs`: `the_notifier_sends_ready_and_watchdog_to_the_socket` (gniazdo `UnixDatagram` w `tempdir`, `NOTIFY_SOCKET` ustawione **tylko** w podprocesie testowym przez `std::process::Command` z `current_exe()` i filtrem testu, żeby nie zmieniać środowiska innych testów; odebrane `READY=1`, `WATCHDOG=1`); `without_notify_socket_nothing_happens`.
  - `station.rs`: `ready_is_sent_once_after_the_panel_listens` (`FakeNotifier`, restart w procesie → jeden `READY`, potem `STATUS=restart w procesie`); `ready_is_sent_in_radio_less_mode_too`.
  - `tests/cli.rs`: `sigterm_stops_the_station_like_ctrl_c` (tylko `cfg(unix)`). Uruchom `aars run` z tymczasowym `aars.toml`: baza w `tempdir`, `server.bind = "127.0.0.1:0"`, `ptt.backend = "serial"`, `ptt.serial.port = "/dev/aars-test-nie-ma"`. PTT pada przy budowie przed otwarciem karty, więc stacja idzie w tryb bez nadawania i żaden test nie dotyka dźwięku. Czekaj na `stacja uruchomiona` w stderr (≤ 10 s), wyślij `SIGTERM` (`libc::kill` z `nix` **nie** dokładamy: `Command::new("kill").args(["-TERM", pid])`) i sprawdź wyjście z kodem 0 w ≤ 5 s.
  - `aars-web`: `healthz_reports_the_version`, `the_footer_shows_the_version_on_login`.
- [ ] **Step 2: FAIL** (`cargo test -p aars service station -p aars-web healthz`)
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test --workspace`)
- [ ] **Step 5: Commit**: `feat: SIGTERM jak Ctrl-C, powiadomienia systemd (READY, watchdog) i wersja w panelu`

---

### Task 3: Dźwięk na Pi — liczba kanałów wyjścia (M-1)

**Files:**
- Modify: `crates/aars-audio/src/output.rs`

**Interfaces:**
- Produces:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigRange { pub channels: u16, pub min_rate: u32, pub max_rate: u32 }
pub fn choose_channels(ranges: &[ConfigRange], rate: u32) -> Result<u16, String>;   // R16
pub fn upmix_into(mono: &[f32], channels: u16, out: &mut [f32]);                     // ta sama próbka na każdy kanał
```
- `CpalSink::open`: `device.supported_output_configs()` → `ConfigRange` → `choose_channels`. Gdy lista się nie da pobrać, zostaje dzisiejsze mono (błąd otwarcia wyjdzie jak dziś). Strumień z wybraną liczbą kanałów, a w callbacku `upmix_into`. `PlayReport` liczy czas po próbkach mono.
- Komunikat: `urządzenie {x} nie obsługuje {rate} Hz (obsługuje: {kanały} kan. {min}–{max} Hz, …)`.

- [ ] **Step 1: Testy** (czyste funkcje, bez cpal)
  - `mono_is_preferred_when_offered` (`[2ch 8k–192k, 1ch 8k–48k]`, 48000 → 1).
  - `stereo_only_cards_get_two_channels` (CM108: `[2ch 44100–48000]` → 2).
  - `a_rate_outside_every_range_is_an_error_listing_the_ranges` (22050 przy `[2ch 44100–48000]` → błąd zawiera `44100–48000`).
  - `more_channels_only_when_nothing_smaller_fits` (`[8ch 48k]` → 8).
  - `upmix_copies_each_sample_to_every_channel` (`[0.5, -0.5]`, 2 kan. → `[0.5, 0.5, -0.5, -0.5]`).
- [ ] **Step 2: FAIL** (`cargo test -p aars-audio output`)
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars-audio`). Ręcznie na Pi (lista w kryterium wyjścia): karta USB stereo wstaje z panelu.
- [ ] **Step 5: Commit**: `fix(audio): wyjście na kartach bez mono — wybór liczby kanałów i powielenie próbek`

---

### Task 4: Rdzeń aktualizacji — wersja, manifest, stan próby (`aars_core::update`)

**Files:**
- Create: `crates/aars-core/src/update.rs`
- Modify: `crates/aars-core/src/lib.rs`, `crates/aars-core/src/config.rs` (`UpdateConfig`, `SetupConfig`), `crates/aars-core/Cargo.toml` (`semver`, `serde_json`)

**Interfaces:**
- Produces (`config.rs`): `Config.update: UpdateConfig { releases_url: String /* domyślnie https://github.com/sptq/aars/releases */, state_dir: PathBuf /* /var/lib/aars/update */, guard_dir: PathBuf /* /var/lib/aars-update */, install_root: PathBuf /* /opt/aars */ }` oraz `Config.setup: SetupConfig { first_admin: FirstAdmin /* Console | Wizard, domyślnie Console */ }`. Stary `aars.toml` bez tych sekcji dalej się wczytuje.
- Produces (`update.rs`):
```rust
pub const MANIFEST_NAME: &str = "aars-manifest.json";
pub const SIGNATURE_NAME: &str = "aars-manifest.json.minisig";
pub const TARGET: &str = "aarch64-unknown-linux-gnu";
pub const MAX_ATTEMPTS: u32 = 3;
pub const CONFIRM_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset { pub name: String, pub sha256: String, pub size: u64, pub kind: AssetKind }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "kebab-case")]
pub enum AssetKind { Binary, Image, ImagerList, Source }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest { pub format: u32, pub version: String, pub target: String, pub published_at: String,
                      pub notes: String, pub assets: Vec<Asset> }
impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, String>;     // format == 1, version semver, dokładnie jeden Binary, nazwy bez '/', sha256 64 znaki hex małe
    pub fn binary(&self) -> &Asset;
    pub fn trusted_comment(&self) -> String;                // "aars {version}"
}
pub fn is_newer(candidate: &str, current: &str) -> Result<bool, String>;   // semver, prerelease nigdy nie jest nowsze od wydania
pub fn binary_archive_name(version: &str) -> String;                       // "aars-{v}-aarch64-linux-gnu.tar.gz"

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trial { pub format: u32, pub version: String, pub previous: String, pub installed_at: String,
                   pub attempts: u32, pub radio_was_up: bool }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum ResultKind { Installed, RolledBack, Failed }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastResult { pub format: u32, pub at: String, pub kind: ResultKind, pub version: String,
                        pub from: String, pub detail: String }
impl LastResult { pub fn audit_action(&self) -> &'static str; }  // update_installed | update_rolled_back | update_failed

/// Co panel pokazuje (R11, R18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStatus { pub current: String, pub previous: Option<String>, pub trial: Option<Trial>,
                          pub unsupported: Option<String>, pub last: Option<LastResult> }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available { pub version: String, pub notes: String, pub size: u64 }
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateRefusal {
    #[error("Aktualizacja z panelu działa tylko w obrazie Raspberry Pi (usługa systemd).")] Unsupported,
    #[error("Czeka pilny komunikat — aktualizacja możliwa po jego nadaniu.")] UrgentPending,
    #[error("Trwa próba wersji {0} — poczekaj na jej potwierdzenie.")] TrialRunning(String),
    #[error("Wersja {0} nie jest nowsza od działającej.")] NotNewer(String),
    #[error("Aktualizacja już trwa.")] InProgress,
    #[error("{0}")] Failed(String),
}
```
- Produces (`queue.rs`): `QueueRepo::urgent_due(&self, horizon: Duration) -> Result<bool, DbError>` (`pending`, `priority <= High`, `not_before IS NULL OR not_before <= now + horizon`).

- [ ] **Step 1: Testy**
  - `a_valid_manifest_parses` (fixture w teście: binary + image + imager-list + source).
  - `manifests_with_two_binaries_bad_hashes_or_paths_are_refused` (`../aars`, `a/b`, sha 63 znaki, wielkie litery, `format: 2`, wersja `abc`).
  - `newer_is_semver_not_string_order` (`0.10.0 > 0.9.9`, `0.10.0` vs `0.10.0` → false, `0.11.0-rc.1` vs `0.10.0` → false wg R5: prerelease nigdy).
  - `trial_and_last_result_round_trip_as_json` (`format: 1`; nieznane pole ignorowane, żeby starsza binarka czytała nowszy plik).
  - `audit_actions_per_result_kind`.
  - `config.rs`: `a_config_without_update_and_setup_uses_the_defaults`.
  - `queue.rs`: `urgent_due_sees_high_and_emergency_only_within_the_horizon` (Normal teraz → false; High za 30 s → true przy 60 s; Emergency za 10 min → false).
- [ ] **Step 2: FAIL** (`cargo test -p aars-core update queue config`)
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars-core`)
- [ ] **Step 5: Commit**: `feat(core): typy aktualizacji — manifest, wersje, stan próby i wynik`

---

### Task 5: Pobranie i weryfikacja wydania (`aars::update::fetch`)

**Files:**
- Create: `crates/aars/src/update/mod.rs`, `crates/aars/src/update/verify.rs`, `crates/aars/src/update/fetch.rs`, `keys/aars-release.pub` (zawartość `UNSET` do czasu, aż właściciel wygeneruje klucz)
- Modify: `crates/aars/src/main.rs` (`mod update`), `crates/aars/Cargo.toml` (`reqwest`, `minisign-verify`, `sha2`, `serde_json`, `semver`; dev: `minisign`, `wiremock`, `tempfile`)

**Interfaces:**
- Consumes: `Manifest`, `is_newer`, `binary_archive_name`, `UpdateConfig` (Task 4).
- Produces:
```rust
// verify.rs
pub const RELEASE_KEY: &str = include_str!("../../../../keys/aars-release.pub");
pub struct TrustedKey(minisign_verify::PublicKey);
impl TrustedKey {
    pub fn embedded() -> Option<Self>;                   // None, gdy plik zawiera "UNSET"
    pub fn from_text(text: &str) -> Result<Self, String>;
}
pub fn verify_manifest(key: &TrustedKey, manifest: &[u8], signature: &[u8]) -> Result<Manifest, String>;
// podpis + komentarz zaufany == manifest.trusted_comment(); błędy: „podpis nie pasuje do klucza wydań”, „komentarz podpisu „{x}” nie pasuje do wersji {v}”
pub fn sha256_file(path: &Path) -> std::io::Result<String>;
pub fn check_asset(path: &Path, asset: &Asset) -> Result<(), String>;  // rozmiar, potem suma; „{nazwa}: suma się nie zgadza”

// fetch.rs
pub struct ReleaseClient { /* reqwest::Client, base: String, key: TrustedKey */ }
pub struct Staged { pub manifest: Manifest, pub dir: PathBuf }   // dir = {state_dir}/staging/{wersja}
impl ReleaseClient {
    pub fn new(base: &str, key: TrustedKey) -> Result<Self, String>;     // User-Agent aars/{VERSION}, timeout 120 s
    pub async fn latest(&self) -> Result<Manifest, String>;              // R5, z weryfikacją podpisu
    pub async fn stage(&self, manifest: &Manifest, current: &str, state_dir: &Path) -> Result<Staged, String>;
    // is_newer albo „Wersja … nie jest nowsza”; pobiera archiwum strumieniowo z limitem 64 MiB do pliku .part,
    // check_asset, rename; zapisuje manifest i podpis obok; poprzednie katalogi staging/* usuwa
}
```
- Błędy sieci dostają dopisek z R5 o zegarze, gdy `reqwest` zgłasza błąd certyfikatu.

- [ ] **Step 1: Testy** (`wiremock` na `127.0.0.1`, klucz z `minisign::KeyPair::generate_unencrypted_keypair()` w teście, pliki w `tempdir`)
  - `a_signed_manifest_from_latest_is_accepted`.
  - `a_signature_by_another_key_is_refused`; `a_tampered_manifest_is_refused` (jeden bajt `notes` zmieniony po podpisie).
  - `an_old_manifest_signed_for_another_version_is_refused` (komentarz `aars 0.9.0` przy `version: 0.11.0`).
  - `staging_checks_size_and_hash` (archiwum o złej sumie → błąd, w `staging/` nie ma pliku, `.part` usunięty).
  - `an_oversized_download_is_cut_off` (serwer podaje 65 MiB bez `Content-Length` → błąd, plik `.part` usunięty; test trzyma dane w pamięci serwera, ~65 MiB to akceptowalne, inaczej limit wstrzykiwany: `ReleaseClient::with_limits`).
  - `a_version_not_newer_is_not_downloaded` (serwer nie dostaje żądania o archiwum).
  - `http_errors_are_reported_in_polish` (404 na manifest → `nie znaleziono wydania (HTTP 404)`).
  - `the_embedded_unset_key_disables_updates` (`TrustedKey::from_text("UNSET")` → błąd; `embedded()` zwraca `None`, dopóki plik w repo to `UNSET`).
- [ ] **Step 2: FAIL** (`cargo test -p aars update::fetch update::verify`)
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars`)
- [ ] **Step 5: Commit**: `feat(update): pobranie wydania z GitHub Releases, podpis minisign i sumy SHA-256`

---

### Task 6: Instalacja, strażnik startu i powrót (`aars::update::{install, guard}`)

**Files:**
- Create: `crates/aars/src/update/paths.rs`, `crates/aars/src/update/install.rs`, `crates/aars/src/update/guard.rs`, `packaging/aars-update-guard`
- Modify: `crates/aars/src/cli.rs` (`update status|guard|rollback|check`), `crates/aars/src/main.rs`, `crates/aars/Cargo.toml` (`flate2`, `tar`)

**Interfaces:**
- Consumes: `Trial`, `LastResult`, `MAX_ATTEMPTS`, `Manifest` (Task 4); `verify_manifest`, `check_asset`, `TrustedKey` (Task 5).
- Produces:
```rust
// paths.rs — wszystko wstrzykiwane (testy: tempdir)
pub struct UpdatePaths { pub install_root: PathBuf, pub guard_dir: PathBuf, pub state_dir: PathBuf, pub db_path: PathBuf }
impl UpdatePaths {
    pub fn from_config(c: &Config) -> Self;
    pub fn current(&self) -> PathBuf; pub fn previous(&self) -> PathBuf; pub fn releases(&self) -> PathBuf;
    pub fn trial(&self) -> PathBuf; pub fn request(&self) -> PathBuf; pub fn confirmed(&self) -> PathBuf;
    pub fn verdict(&self) -> PathBuf; pub fn last_result(&self) -> PathBuf;
}
pub fn read_small_nofollow(path: &Path, max: usize) -> std::io::Result<Option<String>>;  // O_NOFOLLOW; dowiązanie → Err
pub fn write_atomic_nofollow(path: &Path, data: &[u8]) -> std::io::Result<()>;           // tmp O_EXCL|O_NOFOLLOW + rename
pub fn swap_link(link: &Path, target: &Path) -> std::io::Result<()>;                     // symlink tmp + rename
pub fn installed_version(dir: &Path) -> Option<String>;                                  // VERSION z katalogu

// install.rs (uruchamiane przez strażnika jako root)
pub trait SmokeTest { fn version_of(&self, binary: &Path) -> Result<String, String>; } // „aars X” z `--version`, timeout 10 s
pub struct ProcessSmoke;
pub trait Owner { fn chown_to_service(&self, path: &Path) -> std::io::Result<()>; }    // Linux: uid/gid użytkownika aars; testy: no-op
pub fn install_requested(p: &UpdatePaths, key: &TrustedKey, smoke: &dyn SmokeTest, current: &str, radio_was_up: bool)
    -> Result<Trial, String>;   // R6 + R7 + R8 krok 3
pub fn radio_was_up(db_path: &Path) -> bool;   // sqlite `immutable=1`, hw_state:origin != "none"; błąd → false

// guard.rs
#[derive(Debug, PartialEq, Eq)]
pub enum GuardAction { Nothing, Healed, Counted(u32), Confirmed(String), RolledBack(String), Installed(String), InstallFailed(String) }
pub fn run_guard(p: &UpdatePaths, key: Option<&TrustedKey>, smoke: &dyn SmokeTest, owner: &dyn Owner, now: DateTime<Utc>)
    -> Vec<GuardAction>;        // nigdy nie panikuje i nie zwraca błędu (R8)
pub fn rollback(p: &UpdatePaths, owner: &dyn Owner, reason: &str, now: DateTime<Utc>) -> Result<String, String>; // R7
```
- `packaging/aars-update-guard` (POSIX sh, `set -u`, korzeń ścieżek z `AARS_UPDATE_PREFIX`, domyślnie pusty, tylko dla testów):
```sh
#!/bin/sh
set -u
P="${AARS_UPDATE_PREFIX:-}"
if [ -e "$P/var/lib/aars-update/trial.json" ] && [ -x "$P/opt/aars/previous/aars" ]; then
  exec "$P/opt/aars/previous/aars" update guard "$@"
fi
exec "$P/opt/aars/current/aars" update guard "$@"
```
- CLI: `aars update status` wypisuje bieżącą, poprzednią, próbę i ostatni wynik. `aars update guard` (root, z unitu) wypisuje każdą `GuardAction` do dziennika i kończy się kodem 0. `aars update rollback` (root, ręcznie) wymaga `previous`, wypisuje `Przywracam {prev} i kopię bazy z {data} — zmiany zapisane od tego czasu przepadną.` i robi `rollback`. `aars update check` pobiera manifest i wypisuje dostępną wersję, niczego nie pobierając.

- [ ] **Step 1: Testy** (`tempdir` jako korzeń, fałszywy `SmokeTest`, `Owner` no-op, wydanie zbudowane w teście: `tar.gz` z `aars` jako skryptem sh wypisującym `aars 0.11.0`)
  - `install_puts_the_release_in_place_and_starts_a_trial` (`current → releases/0.11.0`, `previous → releases/0.10.0`, `trial.attempts == 0`, kopia bazy w `snapshots/`, `request` usunięte, `last-result` = `Installed`).
  - `a_smoke_test_failure_leaves_current_untouched` (`version_of` → `Err("Exec format error")` → `InstallFailed`, `current` bez zmian, `releases/0.11.0` usunięte).
  - `the_guard_re_verifies_the_signature` (podpis podmieniony w `staging/` po weryfikacji przez usługę → `InstallFailed`, nic nie rozpakowane).
  - `archives_with_paths_links_or_extra_files_are_refused` (`../aars`, dowiązanie `aars → /bin/sh`, dodatkowy plik `x`).
  - `a_confirmed_trial_is_closed` (`confirmed = 0.11.0` → `Confirmed`, brak `trial.json`, `previous` zostaje).
  - `three_unconfirmed_starts_roll_back_with_the_database` (po instalacji baza zmieniona w teście, trzy wywołania `run_guard` → `Counted(1..3)`, czwarte → `RolledBack`; `current → 0.10.0`; zawartość bazy = kopia; `-wal`/`-shm` usunięte; `last-result` = `RolledBack` z `nie wstała w 3 próbach`).
  - `a_bad_verdict_rolls_back_at_once` (`verdict = "bad 0.11.0: nadawanie nie wstało: …"` → `RolledBack` przy pierwszym wywołaniu, szczegół zawiera powód).
  - `a_verdict_for_another_version_is_ignored`.
  - `a_missing_current_link_is_healed_from_previous` (power-cut między dowiązaniami).
  - `the_guard_does_not_follow_symlinks_planted_by_the_service` (`confirmed` jako dowiązanie do pliku z treścią `0.11.0` poza katalogiem → nie potwierdza; `last-result.json` jako dowiązanie do pliku-ofiary → ofiara nietknięta, wynik zapisany jako zwykły plik).
  - `the_guard_never_fails` (brak katalogów, zepsuty `trial.json` → `Nothing`/`Healed`, bez paniki; zepsuty `trial.json` jest przenoszony do `trial.json.bad`).
  - `only_three_releases_are_kept`.
  - `radio_was_up_reads_the_apply_status_without_writing` (baza testowa z `hw_state:origin=panel` → true; po odczycie brak pliku `-shm` utworzonego przez test; brak bazy → false).
  - `crates/aars/tests/guard_script.rs` (`cfg(unix)`): `the_script_runs_previous_during_a_trial_and_current_otherwise` (`AARS_UPDATE_PREFIX` = `tempdir`, `current/aars` i `previous/aars` to skrypty dopisujące swoją nazwę do pliku; z `trial.json` i bez).
- [ ] **Step 2: FAIL** (`cargo test -p aars update`)
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars`)
- [ ] **Step 5: Commit**: `feat(update): instalacja wydania, strażnik startu z licznikiem prób i powrót z kopią bazy`

---

### Task 7: Stacja — aktualizacja po bieżącym nadaniu, potwierdzenie zdrowia, wynik w audycie

**Files:**
- Create: `crates/aars/src/update/control.rs`, `crates/aars/src/update/health.rs`
- Modify: `crates/aars/src/station.rs` (`Exit::Update`, żądanie aktualizacji, zdrowie), `crates/aars/src/main.rs` (pętla), `crates/aars-web/src/state.rs` (trait `UpdateControl` — tylko definicja, użycie w Task 8)

**Interfaces:**
- Consumes: `ReleaseClient`, `Staged` (Task 5); `UpdatePaths`, `write_atomic_nofollow` (Task 6); `UpdateStatus`, `Available`, `UpdateRefusal`, `LastResult`, `CONFIRM_AFTER`, `QueueRepo::urgent_due` (Task 4); `Notifier`, `under_systemd` (Task 2).
- Produces (`aars_web::state`):
```rust
pub type UpdateFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub trait UpdateControl: Send + Sync {
    fn status(&self) -> UpdateStatus;
    fn check(&self) -> UpdateFuture<'_, Result<Option<Available>, UpdateRefusal>>;   // None = brak nowszej
    fn install(&self, version: &str, by: &str) -> UpdateFuture<'_, Result<(), UpdateRefusal>>;
}
```
- Produces (`aars::station`): `Exit::{Restart, Update, Stop}`, `StationHandle::request_update(&self)` (osobny `CancelToken update`). Ścieżka drenażu R4 obsługuje `Restart | Update` tak samo. Gdy po drenażu znacznik PTT jest wciąż w górze bez Ctrl-C (F2-1 z etapu 9), wynik to błąd procesu, a nie `Exit::Update`, i żądanie zostaje (instalacja przy następnym starcie i tak przechodzi przez strażnika).
- Produces (`update::control`):
```rust
pub struct UpdateHandle { /* paths, client: Option<ReleaseClient>, station: Weak/Arc<StationHandle>, queue, pool, supported: Option<String>, in_progress: AtomicBool */ }
impl UpdateControl for UpdateHandle { … }
// install: kolejno R11 (unsupported) → trial trwa → InProgress → urgent_due(60 s) → latest() i wersja == żądana →
// stage() → write_atomic(request, wersja) → audyt update_install → station.request_update().
// Błąd na każdym etapie przed request_update: nic nie zostaje zażądane, staging usunięty.
```
- Produces (`update::health`):
```rust
pub struct HealthCell { /* radio_up: AtomicBool, dispatcher_alive: AtomicBool, last_error: Mutex<Option<String>> */ }
pub enum Verdict { Confirm, Bad(String), Wait }
pub fn judge(trial: Option<&Trial>, running: &str, cell: &HealthCell, since_ready: Duration) -> Verdict;   // R9
pub async fn confirm_task(paths: UpdatePaths, cell: Arc<HealthCell>, ready_at: Instant, stop: CancelToken);
// raz na proces: po CONFIRM_AFTER judge → Confirm: write_atomic(confirmed); Bad: write_atomic(verdict) i stop.cancel() z błędem procesu
pub async fn audit_last_result(paths: &UpdatePaths, pool: &SqlitePool);   // raz przy starcie: audyt systemowy wg LastResult::audit_action, plik usunięty; klucz settings update:last_result = JSON (dla panelu)
```
- `main`: `audit_last_result` przed pierwszym `run_once`, `confirm_task` po pierwszym `READY`. Pętla: `Restart` → kolejny przebieg, `Update` → `notifier.stopping()`, log `kończę proces — aktualizację zainstaluje strażnik przy starcie` i `return Ok(())`, `Stop` → koniec.

- [ ] **Step 1: Testy** (`station.rs::tests` z `FakeHardware` z etapu 9; `update/control.rs` z `wiremock` i kluczem testowym; `update/health.rs` z czasem wstrzykiwanym)
  - `an_update_requested_mid_transmission_waits_for_the_unkey` (jak `a_restart_requested_mid_transmission_waits_for_the_unkey`: zadanie ~1,5 s, `request_update()` przy `is_keyed()` → `Exit::Update` dopiero po `Unkey`, `tx_log` = `ok`, jedna para `Key`/`Unkey`, drugie zadanie `pending`).
  - `an_update_is_refused_while_an_emergency_is_due` (zadanie `Emergency` w kolejce, `install` → `UrgentPending`, brak `request`, `request_update` nie wywołane).
  - `install_stages_writes_the_request_and_audits` (fałszywy serwer z wydaniem 0.11.0 → `request == "0.11.0"`, `staging/0.11.0/aars-0.11.0-aarch64-linux-gnu.tar.gz` z dobrą sumą, audyt `update_install` z `0.10.0 → 0.11.0: admin`, licznik restartów w fałszywej stacji = 1).
  - `a_failed_download_requests_nothing` (zły podpis → `Failed`, brak `request`, `staging/` pusty).
  - `install_is_refused_during_a_trial_and_when_unsupported`.
  - `a_second_install_while_one_runs_is_in_progress`.
  - `health.rs`: `judge_waits_for_sixty_seconds`; `judge_confirms_a_healthy_trial_of_its_own_version`; `judge_is_bad_when_the_radio_was_up_before_and_is_not_now` (powód zawiera błąd stosu); `judge_ignores_a_trial_of_another_version`; `radio_less_is_fine_when_it_was_radio_less_before`.
  - `the_last_result_is_audited_once` (`last-result.json` `RolledBack` → jeden wpis `update_rolled_back` z `user_id IS NULL`; drugi start nic nie dokłada).
- [ ] **Step 2: FAIL** (`cargo test -p aars station update`)
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test --workspace`)
- [ ] **Step 5: Commit**: `feat: aktualizacja po bieżącym nadaniu, potwierdzenie zdrowia nowej wersji i wynik w audycie`

---

### Task 8: Panel — strona „Aktualizacja”

**Files:**
- Create: `crates/aars-web/src/update_page.rs`, `crates/aars-web/templates/update.html`
- Modify: `crates/aars-web/src/state.rs` (`update: Option<Arc<dyn UpdateControl>>`, `with_update`, `testing::FakeUpdate`), `crates/aars-web/src/lib.rs` (trasy), `crates/aars-web/src/chrome.rs` (etykiety audytu), `crates/aars-web/templates/base.html` (link „Aktualizacja” dla admina), `crates/aars-web/templates/settings.html` (link), `crates/aars/src/station.rs` (`with_update`)

**Interfaces:**
- Consumes: `UpdateControl` (Task 7); `UpdateStatus`, `Available`, `UpdateRefusal`, `LastResult` (Task 4).
- `state::testing::FakeUpdate { pub status: Mutex<UpdateStatus>, pub available: Mutex<Result<Option<Available>, UpdateRefusal>>, pub install_result: Mutex<Result<(), UpdateRefusal>>, pub installs: Mutex<Vec<(String, String)>> }`.
- Produces:
  - `GET /settings/update` (admin): karta **Wersja** (`Działa: {current}`, `Poprzednia: {previous albo „brak”}`). Gdy trwa próba: `Próba wersji {v}: start {attempts} z 3. Stacja potwierdzi ją po minucie pracy; gdy nie wstanie, wróci do {previous}.` oraz ostrzeżenie `Nie zmieniaj teraz sprzętu — nieudany start w czasie próby cofnie aktualizację.` Karta **Ostatni wynik** (z `last`): `Zainstalowano {v}`, `Cofnięto {v} → {from}: {detail}. Zmiany z czasu próby przepadły (kopia bazy sprzed aktualizacji).` albo `Aktualizacja nie powiodła się: {detail}`. Gdy `unsupported`, karta z powodem, bez formularzy. Przycisk `Sprawdź aktualizacje` jako formularz `POST`.
  - `POST /settings/update/check` (admin, CSRF): `200` ze stroną. `Ok(None)` → `Działa najnowsza wersja ({current}).`, `Ok(Some(a))` → `Dostępna wersja {v} ({rozmiar} MB)` + notatki jako tekst (escapowane, `white-space: pre-wrap`) + formularz instalacji, `Err(e)` → `Nie udało się sprawdzić: {e}`.
  - Formularz instalacji: ukryte `version`, checkbox `confirm=tak` z etykietą `Rozumiem: stacja dokończy bieżące nadanie, zatrzyma się na około minutę i wstanie na wersji {v}. Gdy nowa wersja nie wstanie, wróci do {current}.`, przycisk `Zainstaluj {v}`.
  - `POST /settings/update/install` (admin, CSRF; `version`, `confirm` z `#[serde(default)]`): bez `confirm == "tak"` → `400` `Zaznacz potwierdzenie — aktualizacja restartuje stację.`, bez `update` w stanie → `503` `aktualizacja nie działa w tym procesie`, `Err(r)` → `409` z tekstem `r`, `Ok` → `200` `Aktualizacja do {v} zlecona. Stacja dokończy bieżące nadanie i uruchomi się ponownie — odśwież stronę za minutę.` (audyt pisze `UpdateHandle`, nie strona).
  - `audit_action_label`: `update_install` → „aktualizacja zlecona”, `update_installed` → „aktualizacja zainstalowana”, `update_rolled_back` → „aktualizacja cofnięta”, `update_failed` → „aktualizacja nieudana”, `first_admin` → „pierwsze konto admina”, `admin_reset` → „reset kont admina”.

- [ ] **Step 1: Testy** (router z `FakeUpdate`)
  - `the_page_shows_the_running_and_previous_version`.
  - `a_check_with_a_newer_version_offers_the_install_form` (zawiera `Zainstaluj 0.11.0` i `name="confirm"`); `a_check_without_news_says_so`; `a_check_error_is_shown`.
  - `release_notes_are_escaped` (`<script>` w notatkach → `&lt;script&gt;`).
  - `install_needs_the_confirmation` (`400`, `installs` puste).
  - `a_confirmed_install_reaches_the_controller` (`installs == [("0.11.0", "admin")]`, tekst zlecenia).
  - `a_refusal_is_409_with_its_text` (`UrgentPending` → `Czeka pilny komunikat`).
  - `an_unsupported_station_shows_the_reason_without_forms`.
  - `a_trial_warns_against_hardware_changes`; `a_rollback_result_says_trial_data_was_lost`.
  - `an_operator_cannot_open_or_post_update` (`403`, `installs` puste); `without_an_update_controller_install_is_503`.
- [ ] **Step 2: FAIL** (`cargo test -p aars-web update`)
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test -p aars-web`)
- [ ] **Step 5: Commit**: `feat(web): strona aktualizacji — sprawdzenie, instalacja z potwierdzeniem, próba i wynik`

---

### Task 9: Kreator pierwszego admina (`/setup`)

**Files:**
- Create: `crates/aars-web/src/setup_page.rs`, `crates/aars-web/templates/setup.html`, `crates/aars-core/src/netlocal.rs`
- Modify: `crates/aars-core/src/user.rs` (`has_enabled_admin`, `create_first_admin`), `crates/aars-core/src/lib.rs`, `crates/aars-web/src/state.rs` (`first_admin: FirstAdmin`, `local_nets: Arc<Vec<LocalNet>>`), `crates/aars-web/src/lib.rs` (trasy, przekierowanie, `MockConnectInfo` w testach), `crates/aars/src/station.rs` (`into_make_service_with_connect_info::<SocketAddr>()`, lista sieci, tryb `wizard` bez hasła na konsoli), `crates/aars/Cargo.toml` (`if-addrs`)

**Interfaces:**
- Produces (`aars_core::netlocal`, czyste):
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct LocalNet { pub addr: IpAddr, pub prefix: u8 }
pub fn is_local_client(ip: IpAddr, nets: &[LocalNet]) -> bool;   // R12; IPv4-mapped IPv6 traktowane jak IPv4
```
- Produces (`user.rs`): `UserRepo::has_enabled_admin(&self) -> Result<bool, DbError>`; `UserRepo::create_first_admin(&self, username: &str, password: &str) -> Result<i64, FirstAdminError>` z sprawdzeniem i wstawieniem w **jednej transakcji** (`BEGIN IMMEDIATE`; gdy admin już jest → `FirstAdminError::AlreadyExists`), `must_change_password = 0`. Gdy nazwa należy do wyłączonego admina (po resecie R13), konto dostaje nowe hasło i jest włączane.
- Produces (`setup_page`): `GET /setup`: `404`, gdy `first_admin == Console` albo admin już jest. Adres spoza sieci lokalnej → `403` z tekstem R12. W przeciwnym razie formularz i ciasteczko `aars_setup`. `POST /setup`: te same warunki, ciasteczko == pole `csrf` (inaczej `400` `Sesja kreatora wygasła — odśwież stronę.`), walidacja (`nazwa: 3–32 znaki: małe litery, cyfry, . _ -`, `hasło: co najmniej 10 znaków`, `hasła się różnią`) → `400` z wpisaną nazwą. Sukces: `create_first_admin`, audyt `first_admin`, od razu sesja (jak `/login`), `303 /`. Wyścig dwóch przeglądarek kończy się `AlreadyExists` → `409` `Konto administratora już założono.`
- Przekierowanie: w trybie `wizard` bez admina `GET /` i `GET /login` → `303 /setup`. Strona `setup.html` mówi: `Pierwsze uruchomienie stacji AARS. Załóż konto administratora. Kreator działa tylko z sieci lokalnej i znika po założeniu konta.`
- `station.rs`: przy `Wizard` nie wołamy `ensure_bootstrap_admin`. Log `brak konta administratora — otwórz http://{host}:{port}/setup w sieci lokalnej` i `STATUS=` z tym tekstem.

- [ ] **Step 1: Testy**
  - `netlocal.rs`: tabela `is_local_client` (`127.0.0.1`, `10.1.2.3`, `172.16.0.1`, `172.32.0.1` ✗, `192.168.1.5`, `169.254.1.1`, `100.64.0.1` ✗, `8.8.8.8` ✗, `::1`, `fe80::1`, `fd00::1`, `2a01:110::5` ✓ przy sieci `2a01:110::1/64`, `2a01:111::5` ✗, `::ffff:192.168.1.5` ✓).
  - `user.rs`: `the_first_admin_is_created_once` (drugie wywołanie → `AlreadyExists`); `a_disabled_admin_name_is_reused_after_a_reset`.
  - `setup_page.rs`: `the_wizard_is_hidden_in_console_mode` (`404`); `without_an_admin_root_redirects_to_setup`; `a_local_client_creates_the_admin_and_is_logged_in` (`MockConnectInfo(192.168.1.10:5555)`, `303 /`, ciasteczko sesji, audyt `first_admin` z `192.168.1.10`); `a_remote_client_is_refused` (`8.8.8.8` → `403`, brak użytkowników); `a_post_without_the_setup_cookie_is_refused`; `short_passwords_and_mismatches_are_refused`; `after_the_admin_exists_setup_is_404`.
  - `role_matrix.rs`: `/setup` dopisany jako wyjątek (bez sesji, opisany w komentarzu), żeby macierz nie wymagała logowania.
- [ ] **Step 2: FAIL** (`cargo test -p aars-core netlocal user -p aars-web setup`)
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test --workspace`)
- [ ] **Step 5: Commit**: `feat: kreator pierwszego konta administratora z sieci lokalnej`

---

### Task 10: Hasło z Raspberry Pi Imager i ratunek z partycji boot (do cięcia)

Zadanie do cięcia: bez niego zostaje kreator (Task 9), który spełnia kryterium „albo kreator”.

**Files:**
- Create: `crates/aars/src/setup.rs`
- Modify: `crates/aars-core/src/user.rs` (hash crypt), `crates/aars-core/Cargo.toml` (`sha-crypt`, `yescrypt`), `crates/aars/src/cli.rs` (`setup import-os-user|reset-admin`), `crates/aars/src/station.rs` (import z pliku przy starcie)

**Interfaces:**
- Produces (`user.rs`): `pub fn is_supported_crypt(hash: &str) -> bool` (`$y$`, `$6$`, `$5$`); `UserRepo::create_admin_with_crypt(&self, username, hash) -> Result<i64, FirstAdminError>` (jak `create_first_admin`). `authenticate` rozpoznaje prefiks: Argon2id jak dziś, a crypt przez `yescrypt`/`sha-crypt`. Po udanym logowaniu crypt hash jest przepisywany na Argon2id (`UPDATE … WHERE password_hash = ?`, żeby nie nadpisać równoległej zmiany).
- Produces (`aars::setup`):
```rust
pub fn pick_os_user(passwd: &str, shadow: &str) -> Option<(String, String)>;  // pierwszy UID 1000–59999 z obsługiwanym hashem
pub fn import_os_user(passwd: &Path, shadow: &Path, out: &Path, owner: &dyn Owner) -> Result<Option<String>, String>; // 0600
pub async fn import_bootstrap_admin(path: &Path, pool: &SqlitePool) -> Result<Option<String>, String>; // przy starcie, gdy brak użytkowników; plik usuwany
pub async fn reset_admins(flag: &Path, pool: &SqlitePool) -> Result<u64, String>; // R13: wyłącza adminów, kończy sesje, audyt admin_reset, usuwa flagę
```
- CLI: `aars setup import-os-user` (root, `/etc/passwd`, `/etc/shadow`, `/var/lib/aars/bootstrap-admin`), `aars setup reset-admin` (root, flaga `/boot/firmware/aars-reset-admin`). Ścieżki są argumentami z wartościami domyślnymi, testy podają `tempdir`.

- [ ] **Step 1: Testy**
  - `crypt_hashes_from_imager_verify_and_are_upgraded` (hash `$y$` i `$6$` wygenerowany w teście tymi samymi crate'ami; `authenticate` → OK; w bazie potem `$argon2id$`; złe hasło → błąd bez przepisania).
  - `pick_os_user_skips_system_and_locked_accounts` (`root`, `nobody`, użytkownik z `!` i `*` pominięci).
  - `import_writes_a_private_file_and_unknown_formats_are_skipped` (`$1$` → `None`, plik nie powstaje).
  - `the_bootstrap_file_creates_the_admin_once_and_is_removed`; `with_users_present_the_file_is_ignored_and_removed`.
  - `reset_disables_admins_ends_sessions_and_reopens_the_wizard` (`has_enabled_admin` → false, operatorzy nietknięci, audyt `admin_reset`).
- [ ] **Step 2: FAIL** (`cargo test -p aars-core user -p aars setup`)
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** (`cargo test --workspace`)
- [ ] **Step 5: Commit**: `feat: konto admina z hasła ustawionego w Raspberry Pi Imager i reset admina z partycji boot`

---

### Task 11: Pakowanie — systemd, użytkownik, konfiguracja obrazu, Piper i głos

**Files:**
- Create: `packaging/systemd/aars.service`, `packaging/systemd/aars-firstboot.service`, `packaging/sysusers.d/aars.conf`, `packaging/tmpfiles.d/aars.conf`, `packaging/etc/aars.toml`, `packaging/piper/requirements.txt`, `packaging/voices.lock`, `packaging/THIRD-PARTY.md`, `crates/aars/tests/packaging.rs`

**Interfaces:**
- `aars.service`: dokładnie R15 + R8 + R9 (`Type=notify`, `NotifyAccess=main`, `TimeoutStartSec=180`, `WatchdogSec=60`, `ExecStartPre=-+/opt/aars/bin/aars-update-guard --config /etc/aars/aars.toml`), `After=network-online.target sound.target aars-firstboot.service`, `Wants=network-online.target`, `[Install] WantedBy=multi-user.target`.
- `aars-firstboot.service`: `Type=oneshot`, root, `ExecStart=-/opt/aars/current/aars setup import-os-user`, `ExecStart=-/opt/aars/current/aars --config /etc/aars/aars.toml setup reset-admin`, `After=cloud-final.service userconfig.service`, `Before=aars.service`, `WantedBy=multi-user.target`.
- `sysusers.d/aars.conf`: `u aars - "AARS" /var/lib/aars` oraz `m aars audio`, `m aars gpio`, `m aars dialout`.
- `tmpfiles.d/aars.conf`: `d /var/lib/aars-update 0755 root root -`, `d /var/lib/aars/update 0750 aars aars -`, `d /var/lib/aars/voices 0755 aars aars -`.
- `packaging/etc/aars.toml`: `[storage] db_path = "/var/lib/aars/aars.db"`, `cache_dir = "/var/lib/aars/cache"`; `[tts] piper_bin = "/opt/aars/piper/bin/piper"`, `voices_dir = "/var/lib/aars/voices"`, `model = "/var/lib/aars/voices/pl_PL-darkman-medium.onnx"`; `[server] bind = "0.0.0.0:8080"`; `[ptt] backend = "vox"` (bezpieczny start: nic nie kluczuje, dopóki admin nie wybierze sterowania); `[setup] first_admin = "wizard"`; `[update]` z wartościami domyślnymi wpisanymi jawnie; `[sources]` jak w `aars.example.toml`.
- `packaging/piper/requirements.txt`: `piper-tts==1.8.0 --hash=sha256:3f60c1917de6d8e8033f395878ad3f88f6dfee88a8b05f98971a275f76a38484` + zależności z hashami (R3).
- `packaging/voices.lock` (format `nazwa url sha256 rozmiar`, jedna linia na plik, URL z `resolve/e9c8a39444359388adaa94255becb37a5a2b8e51/…`).

- [ ] **Step 1: Testy** (`tests/packaging.rs`, czytają pliki z repo, bez systemd)
  - `the_service_runs_unprivileged_and_restarts_always` (`User=aars`, `Restart=always`, `StartLimitIntervalSec=0`, `Type=notify`, `WatchdogSec=`, `KillMode=mixed`, `NoNewPrivileges=yes`; żadna linia `ExecStart=` nie zaczyna się od `+`, a jedyna linia `+` to `ExecStartPre=-+/opt/aars/bin/aars-update-guard`).
  - `the_image_config_loads_and_uses_the_wizard_and_vox` (`Config::load(packaging/etc/aars.toml)` → `FirstAdmin::Wizard`, `PttBackendKind::Vox`, `bind` port ≥ 1024, `tts.model` w `voices_dir`).
  - `the_voice_lock_pins_a_revision_and_hashes` (każdy URL zawiera `/resolve/` + 40 znaków hex, sha256 64 znaki, model ma `.onnx` i `.onnx.json`).
  - `every_python_requirement_has_a_hash`.
  - `the_guard_script_is_posix_and_executable` (`sh -n packaging/aars-update-guard`, bit `x`).
- [ ] **Step 2: FAIL** (`cargo test -p aars --test packaging`)
- [ ] **Step 3: Implementacja** (hashe zależności Pipera według R3; implementer potrzebuje do tego sieci, testy już nie)
- [ ] **Step 4: PASS** (`cargo test -p aars --test packaging`)
- [ ] **Step 5: Commit**: `feat(packaging): usługa systemd, użytkownik aars, konfiguracja obrazu, przypięty Piper i głos`

---

### Task 12: Wydanie i obraz w GitHub Actions

**Files:**
- Create: `.github/workflows/release.yml`, `packaging/image/build-image.sh`, `packaging/image/base.lock`, `scripts/sign-release.sh`, `crates/aars/examples/make_manifest.rs`
- Modify: `crates/aars/Cargo.toml` (wersja według R17, gdy właściciel potwierdzi)

**Interfaces:**
- `release.yml`: `on: push: tags: ['v*']`, `permissions: contents: write`. Job `binary` (`ubuntu-24.04-arm`): sprawdzenie `Cargo.toml` == znacznik, `cargo test --workspace --locked`, `cargo build -p aars --release --locked`, `strip`, `aars --version` == znacznik, archiwum `aars-{v}-aarch64-linux-gnu.tar.gz` (`aars`, `VERSION`, `LICENSE`, `--owner=0 --group=0 --mtime=@$SOURCE_DATE_EPOCH`). Job `image` (`ubuntu-24.04-arm`, `needs: binary`, `timeout-minutes: 90`): `sudo packaging/image/build-image.sh --binary … --out aars-{v}-rpi.img.xz`, potem `aars-os-list.json`. Job `publish`: `cargo run -p aars --example make_manifest -- --version {v} --notes-from CHANGELOG-or-tag-message dist/*` → `aars-manifest.json`, `gh release create v{v} --draft` ze wszystkimi plikami i `piper_tts-1.8.0.tar.gz`. **Bez podpisu w CI** (R4).
- `make_manifest.rs`: buduje `aars_core::update::Manifest` (sumy, rozmiary, rodzaje wg nazwy), `Manifest::parse` na własnym wyjściu jako kontrola, wypisuje JSON.
- `build-image.sh` (bash, `set -euo pipefail`, root): czyta `base.lock` (URL + dwie sumy z R2), pobiera z cache `actions/cache`, sprawdza sumy, rozszerza obraz (R2), w chroocie robi: `apt-get install -y --no-install-recommends python3-venv libasound2 avahi-daemon` (avahi już jest, tu tylko dla pewności), venv Pipera z `requirements.txt`, głos z `voices.lock` (sprawdzenie sum), `/opt/aars/releases/{v}`, dowiązania `current` (bez `previous`), `/opt/aars/bin/aars-update-guard`, unity, `systemd-sysusers`, `systemd-tmpfiles --create`, `systemctl enable aars.service aars-firstboot.service`, `/etc/hostname = aars`, `THIRD-PARTY.md`. Potem `umount`, `zerofree`, `xz`, `.sha256`, rozmiar i suma rozpakowanego obrazu dla `aars-os-list.json`. Każdy krok loguje się czytelnie, a `trap` zawsze odmontowuje i zwalnia urządzenie pętli.
- `aars-os-list.json` (format v4 Imagera): `{ "os_list": [ { "name": "AARS {v} (Raspberry Pi OS Lite 64-bit)", "description": "Automatyczna stacja ostrzegawcza …", "url": ".../download/v{v}/aars-{v}-rpi.img.xz", "extract_size", "extract_sha256", "image_download_size", "image_download_sha256", "release_date", "init_format": "cloudinit-rpi", "devices": ["pi5-64bit","pi4-64bit","pi3-64bit"] } ] }`.
- `sign-release.sh v{v}`: `gh release download v{v} -p aars-manifest.json`, `minisign -S -s ~/.minisign/aars.key -t "aars {v}" -m aars-manifest.json`, `minisign -V -p keys/aars-release.pub …` (kontrola), `gh release upload v{v} aars-manifest.json.minisig`, `gh release edit v{v} --draft=false --latest`.

- [ ] **Step 1: Testy**
  - `crates/aars/examples/make_manifest.rs` ma moduł `#[cfg(test)]`, a `cargo test --examples` jest częścią `cargo test --workspace --all-targets`: `the_manifest_lists_every_file_with_its_hash` (tempdir z trzema plikami).
  - `tests/packaging.rs`: `the_base_image_lock_pins_both_hashes` (`base.lock` ma URL `downloads.raspberrypi.com` i dwie sumy 64 hex); `release_workflow_does_not_sign_in_ci` (w `release.yml` nie ma `minisign` ani `secrets.MINISIGN`; jest `--draft`).
  - Skrypty: `shellcheck` w CI (Task 1) i `bash -n` w `tests/packaging.rs`.
- [ ] **Step 2: FAIL**
- [ ] **Step 3: Implementacja**
- [ ] **Step 4: PASS** lokalnie. Pierwsze prawdziwe uruchomienie to wypchnięcie znacznika testowego `v0.10.0-rc.1` (wydanie robocze, R5 nie pokazuje go stacjom). Tylko po zgodzie właściciela.
- [ ] **Step 5: Commit**: `ci: wydanie z binarką aarch64, obrazem Raspberry Pi, listą dla Imagera i manifestem do podpisu`

---

### Task 13: Macierz ról, kryterium wyjścia, dokumentacja

**Files:**
- Modify: `crates/aars-web/src/role_matrix.rs`, `crates/aars/src/station.rs` albo `crates/aars/src/update/mod.rs` (test kryterium wyjścia)
- Modify: `README.md`, `aars.example.toml`, `docs/superpowers/plans/README.md` (wiersz 10 → ✓, odstępstwo R1), `docs/superpowers/specs/2026-10-13-aars-design.md` (§12: dopisek o natywnym runnerze arm64 zamiast cargo-zigbuild, Trixie)

**Interfaces:**
- Macierz: `get("/settings/update", ADMIN)`; `POSTS`: `/settings/update/check` (ADMIN, `503` bez kontrolera), `/settings/update/install` (ADMIN, `400`: brak `confirm`). `/setup` opisany jako wyjątek (Task 9).
- Test kryterium wyjścia `an_update_installs_after_the_transmission_and_a_broken_one_rolls_back` (`update/mod.rs::tests`, `tempdir` jako korzeń, fałszywy serwer z dwoma wydaniami podpisanymi kluczem testowym):
  1. Stacja z `FakeHardware` nadaje zadanie ~1,5 s. `UpdateHandle::install("0.11.0", "admin")` w trakcie nadania → `run_once` zwraca `Exit::Update` dopiero po `Unkey`, `tx_log` = `ok`.
  2. `run_guard` → `Installed("0.11.0")`, `current → 0.11.0`, `trial.attempts == 0`.
  3. `judge` przy zdrowym stosie po 60 s symulowanego czasu → `Confirm`, a `run_guard` → `Confirmed`.
  4. Druga aktualizacja do `0.12.0`, której test dymny przechodzi, ale `judge` daje `Bad` (stos nie wstał, a wcześniej działał) → `run_guard` → `RolledBack`, `current → 0.11.0`, baza = kopia, a `audit_last_result` zapisuje `update_rolled_back`.
- Dokumentacja:
  - README, nowa sekcja „Raspberry Pi: obraz i aktualizacja”: nagranie w Imagerze (repozytorium z `aars-os-list.json`: `https://github.com/sptq/aars/releases/latest/download/aars-os-list.json`, albo „Use custom”), Wi-Fi i hasło z Imagera, bez Imagera Ethernet albo `network-config`, `http://aars.local:8080`, kreator, reset z partycji boot, aktualizacja z panelu (R10, R7: co przepada przy powrocie), `aars update status|rollback`, ograniczenia (Trixie, Pi 3/4/5/Zero 2 W, bez HTTPS), licencje (R3), wydawanie (`git tag`, CI, `sign-release.sh`). Status: etapy 1–10 z 12.
  - `aars.example.toml`: sekcje `[setup]` i `[update]` jako komentarz z opisem.
- Ręczna lista kontrolna na Raspberry Pi (wypełnia właściciel albo implementer z dostępem do Pi; wynik w raporcie etapu) — pozycje w kryterium wyjścia.

- [ ] **Step 1: Testy** (macierz + kryterium wyjścia)
- [ ] **Step 2: Uruchom**: powinny przejść od razu. Czerwony wynik to błąd w Task 1–12, nie w teście.
- [ ] **Step 3: Pełna weryfikacja:** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` na macOS i w CI na Linuksie arm64. Wydanie robocze `v0.10.0-rc.1` zbudowane przez `release.yml` (po zgodzie właściciela).
- [ ] **Step 4: Commit**: `test: kryterium wyjścia etapu 10 i macierz ról z aktualizacją; dokumentacja obrazu`

---

## Kryterium wyjścia z etapu

Automatycznie (CI, bez sieci, karty dźwiękowej i GPIO):

- Aktualizacja zlecona w trakcie nadania czeka na `Unkey` i join sekwencera. Przy pilnym komunikacie w kolejce jest odrzucana (Task 7, Task 13).
- Podrobione, stare albo uszkodzone wydanie niczego nie instaluje (Task 5, Task 6).
- Nowa wersja, która nie wstaje (trzy starty bez potwierdzenia albo werdykt `bad`), wraca do poprzedniej razem z kopią bazy, a wynik trafia do audytu (Task 6, Task 7, Task 13).
- Pierwsze konto zakłada kreator wyłącznie z sieci lokalnej i tylko raz (Task 9).
- Macierz rola × endpoint przechodzi z trasami aktualizacji (Task 13).
- `cargo test --workspace` zielone i `cargo clippy --workspace --all-targets -- -D warnings` czyste na macOS i na `ubuntu-24.04-arm`. Kod `cfg(target_os = "linux")` jest skompilowany i sprawdzony (Task 1).
- `release.yml` publikuje szkic wydania z binarką, obrazem, `aars-os-list.json`, manifestem i źródłem Pipera (Task 12).

Ręcznie na Raspberry Pi (nie da się sprawdzić w CI; każdy punkt z wynikiem w raporcie etapu):

1. Obraz nagrany przez Imager z repozytorium `aars-os-list.json`, z Wi-Fi, nazwą i hasłem: Pi wstaje bez monitora i SSH, `http://aars.local:8080` odpowiada, a konto admina z hasła z Imagera działa (gdy Task 10 jest zrobione). Personalizacja przy „Use custom” sprawdzona osobno (R2, niepewne).
2. Obraz bez personalizacji, na kablu Ethernet: `aars.service` startuje mimo czekającego `userconfig`, kreator `/setup` działa z laptopa w LAN i z adresu IPv6 globalnego, ale nie spoza LAN.
3. Partycja główna powiększona do rozmiaru karty, `systemctl status aars` = `active (running)` z `STATUS=`, `journalctl -u aars` bez błędów.
4. Piper: `Test audio stacji.` z panelu na Pi 3, 4 i 5 (czas syntezy zanotowany), głos `darkman` słyszalny.
5. M-1: karta USB z samym stereo (CM108) wstaje z panelu, a karta z błędną częstotliwością nie staje się `hw_good`.
6. PTT GPIO na prawdziwej linii: test PTT 2 s, potem restart w procesie z panelu i test PTT ponownie (linia zwolniona i żądana od nowa, brak `EBUSY`). To samo dla portu szeregowego DTR/RTS: radio **nie** kluczuje się przy starcie ani restarcie.
7. `sudo systemctl stop aars` w trakcie nadania: PTT opada od razu (R10 e). `sudo kill -9 $(pidof aars)` w trakcie nadania: systemd podnosi usługę w ≤ 10 s, a stan linii GPIO po zabiciu procesu jest zapisany w raporcie (ryzyko jest znane i niezależne od tego etapu).
8. Aktualizacja `rc.1 → rc.2` z panelu w trakcie długiego nadania: nadanie dokończone, usługa wraca w ≤ 2 min na nowej wersji, `/settings/update` pokazuje próbę, a po minucie potwierdzenie.
9. Wymuszony powrót: wydanie `rc.3` z celowo psującym startem (np. `panic!` po `READY` za flagą kompilacji tylko w tym wydaniu testowym) → po trzech startach usługa działa na `rc.2`, panel pokazuje „Cofnięto”, a dane sprzed aktualizacji są na miejscu.
10. Odcięcie zasilania w trakcie strażnika (między kopią bazy a przełączeniem): po włączeniu stacja wstaje na jednej z dwóch wersji, nigdy bez `current`.
11. Reset admina z partycji boot (plik `aars-reset-admin`) otwiera kreator znowu.

Uwaga przy wdrożeniu: etap nie dokłada migracji. Istniejące instalacje spoza obrazu (`aars run` z ręki) działają jak dotąd: `setup.first_admin = "console"`, aktualizacja z panelu wyłączona (R11).
