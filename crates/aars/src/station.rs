//! Stacja w binarce (etap 9): kandydaci konfiguracji sprzętu z powrotem do
//! ostatniej działającej (R5), praca bez nadawania (R6) i restart w procesie
//! (R4), który nigdy nie przerywa nadania.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use tokio::sync::{Notify, oneshot};

use aars_core::config::{AudioConfig, Config, FirstAdmin, PttBackendKind, PttConfig};
use aars_core::dispatcher::{
    Dispatcher, DispatcherConfig, DispatcherControl, StationTest, TestOutcome,
};
use aars_core::hardware::probe::{HardwareProbe, resolve_model};
use aars_core::hardware::{ApplyStatus, ConfigOrigin, HW_KEYS, HardwareRepo, HardwareSettings};
use aars_core::module::{ModuleRegistry, ModuleRepo};
use aars_core::ptt::PttBackend;
use aars_core::queue::QueueRepo;
use aars_core::settings::{SettingsRepo, StationSettings};
use aars_core::sink::{AudioSink, CancelToken};
use aars_core::source_state::SourceStateRepo;
use aars_core::synth::Synthesizer;
use aars_web::state::{NO_RADIO, StationControl, TestFuture};

use crate::radio::{self, RadioStack};
use crate::render;
use crate::service::{self, Notifier};
use crate::update::health::HealthCell;

/// Skąd stacja bierze sprzęt. Jedyna droga do PTT i karty (D2); testy
/// podmieniają ją na fałszywkę.
pub trait HardwareFactory: Send + Sync {
    fn ptt(&self, cfg: &PttConfig) -> Result<Arc<dyn PttBackend>, String>;
    fn sink(&self, cfg: &AudioConfig) -> Result<Arc<dyn AudioSink>, String>;
    /// Synteza z modelem i częstotliwością tej konfiguracji. Nie dotyka
    /// sprzętu, ale musi iść za kandydatem (R19), więc żyje obok PTT i karty.
    fn synth(&self, config: &Config) -> Arc<dyn Synthesizer>;
}

/// Prawdziwy sprzęt: `aars_ptt::build`, `CpalSink::open` i Piper.
pub struct RealHardware;

impl HardwareFactory for RealHardware {
    fn ptt(&self, cfg: &PttConfig) -> Result<Arc<dyn PttBackend>, String> {
        aars_ptt::build(cfg).map_err(|e| e.to_string())
    }

    fn sink(&self, cfg: &AudioConfig) -> Result<Arc<dyn AudioSink>, String> {
        aars_audio::output::CpalSink::open(&cfg.device, cfg.sample_rate)
            .map(|sink| Arc::new(sink) as Arc<dyn AudioSink>)
            .map_err(|e| e.to_string())
    }

    fn synth(&self, config: &Config) -> Arc<dyn Synthesizer> {
        // Po aktualizacji ustawiony model mógł zniknąć z katalogu głosów
        // (np. darkman): wtedy Gosia, żeby stacja nie zamilkła.
        let model = resolve_model(&config.tts.model, &config.tts.voices_dir);
        if let Some(missing) = &model.replaced {
            tracing::warn!(
                brak = %missing,
                uzyty = %model.path,
                "model głosu nie istnieje — używam Gosi; wybierz głos na stronie Sprzęt"
            );
        }
        Arc::new(aars_audio::piper::PiperSynth::new(
            &config.tts.piper_bin,
            &model.path,
            config.audio.sample_rate,
        ))
    }
}

/// Jedna konfiguracja do wypróbowania: `config` to plik z nałożonym sprzętem,
/// `pairs` to pełny zestaw `HW_KEYS` (do zapisu jako ostatnia działająca).
#[derive(Debug, Clone)]
pub struct Candidate {
    pub origin: ConfigOrigin,
    pub config: Config,
    pub pairs: BTreeMap<String, String>,
}

/// Kandydaci w kolejności R5: panel → ostatnia działająca → sam plik.
/// Identyczni są scalani (zostaje wcześniejszy). Drugi element to
/// ostrzeżenia o nieczytelnych wartościach (R1).
pub fn candidates(
    base: &Config,
    panel: &BTreeMap<String, String>,
    last_good: &BTreeMap<String, String>,
) -> (Vec<Candidate>, Vec<String>) {
    let mut out: Vec<Candidate> = Vec::new();
    let mut warnings = Vec::new();
    let mut push = |origin: ConfigOrigin, settings: HardwareSettings| {
        let pairs = settings.to_pairs();
        if !out.iter().any(|c| c.pairs == pairs) {
            out.push(Candidate {
                origin,
                config: settings.apply_to(base),
                pairs,
            });
        }
    };

    for (origin, pairs) in [
        (ConfigOrigin::Panel, panel),
        (ConfigOrigin::LastGood, last_good),
    ] {
        if pairs.is_empty() {
            continue;
        }
        let (settings, w) = HardwareSettings::from_pairs(base, pairs);
        warnings.extend(w.into_iter().map(|w| format!("{}: {w}", origin.label())));
        push(origin, settings);
    }
    push(ConfigOrigin::File, HardwareSettings::from_config(base));
    (out, warnings)
}

fn join_errors(errors: &[(ConfigOrigin, String)]) -> Option<String> {
    (!errors.is_empty()).then(|| {
        errors
            .iter()
            .map(|(origin, err)| format!("{}: {err}", origin.label()))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

async fn audit_system(pool: &SqlitePool, action: &str, detail: &str) {
    if let Err(err) =
        sqlx::query("INSERT INTO audit_log (user_id, action, detail) VALUES (NULL, ?, ?)")
            .bind(action)
            .bind(detail)
            .execute(pool)
            .await
    {
        tracing::error!(%err, action, "zapis do dziennika audytu zawiódł");
    }
}

/// Buduje stos z pierwszego kandydata, z którego wstaną PTT i wyjście audio.
/// Zapisuje stan zastosowania; po sukcesie kandydata z panelu zapamiętuje go
/// jako ostatnią działającą konfigurację, a gdy kandydat z panelu zawiedzie,
/// pisze `hardware_apply_failed` do audytu (bez użytkownika). Wartości `hw:*`
/// zostają nietknięte (R6).
pub async fn build_first(
    cands: Vec<Candidate>,
    factory: &dyn HardwareFactory,
    pool: &SqlitePool,
) -> (Option<(RadioStack, Candidate)>, Vec<(ConfigOrigin, String)>) {
    let mut errors: Vec<(ConfigOrigin, String)> = Vec::new();
    let mut built = None;
    for cand in cands {
        match radio::build_stack(&cand.config, pool, factory).await {
            Ok(stack) => {
                built = Some((stack, cand));
                break;
            }
            Err(err) => {
                let err = format!("{err:#}");
                tracing::error!(origin = cand.origin.as_str(), %err, "konfiguracja sprzętu nie wstała");
                errors.push((cand.origin, err));
            }
        }
    }

    let repo = HardwareRepo::new(pool.clone());
    let origin = built.as_ref().map_or(ConfigOrigin::None, |(_, c)| c.origin);
    let status = ApplyStatus {
        origin,
        error: join_errors(&errors),
    };
    if let Err(err) = repo.set_apply_status(&status).await {
        tracing::error!(%err, "zapis stanu zastosowania sprzętu zawiódł");
    }
    if let Some((_, cand)) = &built
        && cand.origin == ConfigOrigin::Panel
        && let Err(err) = repo.save_last_good(&cand.pairs).await
    {
        tracing::error!(%err, "zapis ostatniej działającej konfiguracji zawiódł");
    }
    if errors
        .iter()
        .any(|(origin, _)| *origin == ConfigOrigin::Panel)
    {
        let detail = status.error.clone().unwrap_or_default();
        audit_system(pool, "hardware_apply_failed", &detail).await;
    }
    (built, errors)
}

/// Konfiguracja, którą stacja spróbuje jako pierwszą (panel, jeśli jest).
/// `aars tx` i `aars info` używają jej, żeby CLI nadawał tym samym sprzętem
/// co stacja.
pub async fn first_candidate(base: &Config, pool: &SqlitePool) -> Result<Candidate> {
    let repo = HardwareRepo::new(pool.clone());
    let panel = repo
        .overrides()
        .await
        .context("odczyt ustawień sprzętu z panelu")?;
    let last_good = repo
        .last_good()
        .await
        .context("odczyt ostatniej działającej konfiguracji")?;
    let (cands, warnings) = candidates(base, &panel, &last_good);
    for warning in warnings {
        tracing::warn!(%warning, "ustawienie sprzętu z panelu pominięte");
    }
    Ok(cands
        .into_iter()
        .next()
        .expect("kandydat z pliku jest zawsze"))
}

/// `aars hardware show`: konfiguracja efektywna z pochodzeniem każdego klucza
/// i stan ostatniego zastosowania (R7).
pub fn hardware_show_lines(
    base: &Config,
    panel: &BTreeMap<String, String>,
    status: Option<&ApplyStatus>,
) -> Vec<String> {
    let (settings, warnings) = HardwareSettings::from_pairs(base, panel);
    let effective = settings.to_pairs();
    let mut lines: Vec<String> = HW_KEYS
        .iter()
        .map(|key| {
            let rejected = warnings.iter().any(|w| w.starts_with(&format!("{key}:")));
            let origin = if panel.contains_key(*key) && !rejected {
                "panel"
            } else {
                "plik"
            };
            format!("{key} = {}  ({origin})", effective[*key])
        })
        .collect();
    lines.extend(warnings.into_iter().map(|w| format!("ostrzeżenie: {w}")));
    match status {
        Some(status) => {
            lines.push(format!("stan zastosowania: {}", status.origin.label()));
            if let Some(error) = &status.error {
                lines.push(format!("błąd: {error}"));
            }
        }
        None => lines.push("stan zastosowania: brak zapisu (stacja nie startowała)".to_owned()),
    }
    lines
}

pub enum Exit {
    Restart,
    /// Koniec procesu po bieżącym nadaniu: aktualizację zainstaluje strażnik
    /// przy następnym starcie (etap 10, R10a).
    Update,
    /// Nowa wersja w próbie dostała werdykt `bad` (R9): koniec procesu po
    /// bieżącym nadaniu, błędem, a strażnik wraca do poprzedniej wersji.
    Unhealthy,
    Stop,
}

/// Uchwyt panelu do działającej stacji (`StationControl`).
pub struct StationHandle {
    control: Option<DispatcherControl>,
    restart: CancelToken,
    update: CancelToken,
    /// Przebieg stacji wyszedł z pętli: prośba o aktualizację do tego
    /// uchwytu już nie zakończy procesu (przegląd 8–9 I-1).
    closed: CancelToken,
    status: ApplyStatus,
    hardware: Option<HardwareSettings>,
    station: StationSettings,
    tx_tail: Duration,
}

impl crate::update::control::UpdateTarget for StationHandle {
    /// Prośba o zakończenie procesu po bieżącym nadaniu (R10a): ta sama
    /// droga co restart, ale `run_once` zwraca `Exit::Update`.
    ///
    /// `false`, gdy ten przebieg już się skończył (np. restart w procesie
    /// wygrał): wtedy nikt nie zakończy procesu, a żądanie trzeba wycofać.
    /// Kolejność (najpierw `update`, potem odczyt `closed`) pasuje do pętli,
    /// która po `closed` sprawdza `update` jeszcze raz.
    fn request_update(&self) -> bool {
        self.update.cancel();
        if self.closed.is_cancelled() {
            tracing::warn!("prośba o aktualizację do zakończonego przebiegu stacji — wycofana");
            return false;
        }
        tracing::info!("zlecono aktualizację — stacja skończy po bieżącym nadaniu");
        true
    }
}

/// Najkrótsze oczekiwanie panelu na test PTT: przy krótszym radio mogłoby
/// zostać włączone już po tym, jak panel powiedział „brak odpowiedzi”.
const TEST_PTT_MIN_WAIT: Duration = Duration::from_secs(15);
/// Zapas ponad czas testu PTT na kolejkę dyspozytora, ogony i zapis wyniku.
const TEST_PTT_MARGIN: Duration = Duration::from_secs(10);
/// Test audio: synteza plus odtwarzanie ucięte po 15 s (R11).
const TEST_AUDIO_TIMEOUT: Duration = Duration::from_secs(60);

impl StationControl for StationHandle {
    fn apply_status(&self) -> ApplyStatus {
        self.status.clone()
    }

    fn running_hardware(&self) -> Option<HardwareSettings> {
        self.hardware.clone()
    }

    fn running_station(&self) -> StationSettings {
        self.station.clone()
    }

    fn request_restart(&self) {
        tracing::info!("zlecono restart stacji");
        self.restart.cancel();
    }

    fn run_test(&self, test: StationTest) -> TestFuture<'_> {
        Box::pin(async move {
            let Some(control) = &self.control else {
                return TestOutcome::Unsupported(NO_RADIO);
            };
            let timeout = match &test {
                StationTest::Ptt { seconds, .. } => {
                    (Duration::from_secs(*seconds) + self.tx_tail * 2 + TEST_PTT_MARGIN)
                        .max(TEST_PTT_MIN_WAIT)
                }
                StationTest::Audio { .. } => TEST_AUDIO_TIMEOUT,
            };
            control.run_test(test, timeout).await
        })
    }
}

/// Źródła nie udało się zbudować: ślad w `source_state`, żeby panel pokazał je
/// jako awaryjne, a nie „włączone” bez prób.
pub async fn source_start_failed(state: &SourceStateRepo, id: &str, reason: &str) {
    tracing::error!(source = id, %reason, "źródło nie wystartuje");
    let message = format!("klient HTTP: {reason}");
    if let Err(err) = state.record_failure(id, chrono::Utc::now(), &message).await {
        tracing::error!(%err, "nie udało się zapisać porażki źródła");
    }
}

/// Instancje źródeł współdzielone z biuletynem (jedna pamięć danych dla
/// runnera i części); `None` po błędzie budowy biuletynu.
#[derive(Default)]
struct SharedSources {
    pogoda: Option<Arc<aars_sources::pogoda::Pogoda>>,
    wodowskazy: Option<Arc<aars_sources::imgw_wodowskazy::ImgwWodowskazy>>,
    powietrze: Option<Arc<aars_sources::powietrze::Powietrze>>,
    /// Instancja `imgw-meteo` dla runnera i mapy ostrzeżeń w panelu.
    meteo: Option<Arc<aars_sources::imgw_meteo::ImgwMeteo>>,
}

impl SharedSources {
    fn from_bulletin(built: Option<&aars_sources::bulletin::BuiltBulletin>) -> Self {
        Self {
            pogoda: built.map(|b| b.pogoda.clone()),
            wodowskazy: built.map(|b| b.wodowskazy.clone()),
            powietrze: built.map(|b| b.powietrze.clone()),
            meteo: None,
        }
    }
}

/// Ostrzeżenia z pamięci `imgw-meteo` w postaci dla mapy: jeden wpis na parę
/// (ostrzeżenie, powiat); brak stopnia = 1, czas końca nieistniejący w
/// Warszawie = wpis pominięty, ostrzeżenie jeszcze nieobowiązujące też.
pub fn warnings_snapshot(
    snapshot: aars_sources::imgw_meteo::MeteoSnapshot,
    now: chrono::DateTime<chrono::Utc>,
) -> aars_web::state::WarningsSnapshot {
    let mut warnings = Vec::new();
    for w in &snapshot.warnings {
        let Some(until) = aars_sources::speech::warsaw_to_utc(w.valid_until) else {
            continue;
        };
        // Ostrzeżenie, które jeszcze nie obowiązuje, nie barwi mapy (popup
        // nie podaje początku). Początek nieistniejący w Warszawie — pokazujemy.
        if aars_sources::speech::warsaw_to_utc(w.valid_from).is_some_and(|from| from > now) {
            continue;
        }
        for teryt in &w.teryt {
            warnings.push(aars_web::state::MapWarning {
                teryt: teryt.clone(),
                level: w.level.unwrap_or(1),
                event: w.event.clone(),
                until,
            });
        }
    }
    aars_web::state::WarningsSnapshot {
        fetched_at: snapshot.fetched_at,
        warnings,
    }
}

/// Źródło dla modułu. `pogoda`, `imgw-wodowskazy`, `powietrze` i `imgw-meteo` to wspólne
/// instancje (biuletyn albo mapa), więc runner źródła i odbiorca dzielą
/// pamięć; bez nich (błąd budowy, już zapisany) `None` — źródło nie startuje.
fn source_for(
    module: &aars_sources::SourceModule,
    config: &Config,
    shared: &SharedSources,
) -> Option<Result<Arc<dyn aars_sources::DataSource>, aars_sources::SourceError>> {
    match module.descriptor.id {
        aars_sources::pogoda::ID => shared
            .pogoda
            .as_ref()
            .map(|p| Ok(p.clone() as Arc<dyn aars_sources::DataSource>)),
        aars_sources::imgw_wodowskazy::ID => shared
            .wodowskazy
            .as_ref()
            .map(|w| Ok(w.clone() as Arc<dyn aars_sources::DataSource>)),
        aars_sources::powietrze::ID => shared
            .powietrze
            .as_ref()
            .map(|p| Ok(p.clone() as Arc<dyn aars_sources::DataSource>)),
        aars_sources::imgw_meteo::ID => shared
            .meteo
            .as_ref()
            .map(|m| Ok(m.clone() as Arc<dyn aars_sources::DataSource>)),
        _ => Some((module.build)(config)),
    }
}

/// Złożony biuletyn w postaci dla panelu i CLI (bez typów z `aars-sources`).
pub fn bulletin_preview_view(
    composed: aars_sources::bulletin::Composed,
) -> aars_web::state::BulletinPreviewView {
    aars_web::state::BulletinPreviewView {
        text: composed.text,
        parts: composed
            .parts
            .iter()
            .map(|(id, outcome)| ((*id).to_owned(), outcome.describe()))
            .collect(),
        skipped_reason: composed.skipped_reason.map(str::to_owned),
        part_chars: composed
            .parts
            .iter()
            .map(|(_, outcome)| match outcome {
                aars_sources::bulletin::PartOutcome::Included(text) => Some(text.chars().count()),
                _ => None,
            })
            .collect(),
    }
}

/// Jak często pętla stacji sprawdza `stop` i żądanie restartu.
const CONTROL_POLL: Duration = Duration::from_millis(50);

/// Jeden przebieg stacji: od zbudowania stosu do zatrzymania albo restartu.
/// `notifier.ready()` idzie w każdym przebiegu po nasłuchu panelu; to, że
/// systemd dostaje `READY=1` raz na proces, zapewnia `service::ReadyOnce`.
pub async fn run_once(
    base: &Config,
    pool: &SqlitePool,
    factory: Arc<dyn HardwareFactory>,
    probe: Arc<dyn HardwareProbe>,
    stop: CancelToken,
    notifier: Arc<dyn Notifier>,
    health: Arc<HealthCell>,
) -> Result<Exit> {
    run_station(base, pool, factory, probe, stop, notifier, health, None).await
}

/// Zapas drenażu ponad limit strażnika TX: zamknięcie strumienia, join
/// wątku sekwencera i zapis wyniku. W testach krótszy, żeby test
/// zawieszonego drenażu nie trwał pół minuty.
const SERVICE_DRAIN_MARGIN: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const DRAIN_MARGIN: Duration = SERVICE_DRAIN_MARGIN;
#[cfg(test)]
const DRAIN_MARGIN: Duration = Duration::from_secs(2);

/// Adresy interfejsów stacji dla kreatora `/setup` (R12). Błąd listy daje
/// pustą listę: zostają stałe sieci lokalne (RFC 1918, link-local, ULA).
fn local_nets() -> Vec<aars_core::netlocal::LocalNet> {
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces
            .into_iter()
            .map(|iface| {
                let prefix = match &iface.addr {
                    if_addrs::IfAddr::V4(v4) => v4.prefixlen,
                    if_addrs::IfAddr::V6(v6) => v6.prefixlen,
                };
                aars_core::netlocal::LocalNet {
                    addr: iface.ip(),
                    prefix,
                }
            })
            .collect(),
        Err(err) => {
            tracing::warn!(%err, "lista interfejsów sieci niedostępna — kreator zna tylko stałe sieci lokalne");
            Vec::new()
        }
    }
}

/// Domeny lokalne, pod którymi router albo DHCP zwykle ogłasza nazwę
/// hosta (przegląd końcowy M-1). Żadna nie jest delegowana w publicznym
/// DNS, więc nie otwiera DNS rebinding. Inne (np. `fritz.box`, który jest
/// w publicznej strefie `.box`) dopisuje się w `server.allowed_hosts`.
const LOCAL_SUFFIXES: [&str; 5] = ["local", "lan", "home", "home.arpa", "localdomain"];

/// Nazwy, pod którymi panel jest stacją (`Host` kreatora, obrona przed DNS
/// rebinding): nazwa hosta, `{nazwa}.{domena lokalna}` (`LOCAL_SUFFIXES`) i
/// nazwy hostów z adresów nasłuchu. Literały IP i `{nazwa}-{N}.local` po
/// kolizji w avahi sprawdza panel sam.
fn station_host_names(host: &str, binds: &[&str]) -> Vec<String> {
    let base = host.trim_end_matches('.').trim_end_matches(".local").to_ascii_lowercase();
    let mut names = vec![base.clone()];
    names.extend(LOCAL_SUFFIXES.iter().map(|suffix| format!("{base}.{suffix}")));
    for bind in binds {
        let name = match bind.rsplit_once(':') {
            Some((name, _)) => name.trim_start_matches('[').trim_end_matches(']'),
            None => bind,
        };
        if !name.is_empty() && name.parse::<std::net::IpAddr>().is_err() {
            names.push(name.to_ascii_lowercase());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Nazwa hosta do komunikatu o kreatorze; `aars` (R14), gdy nieznana.
fn local_host_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: bufor ma podaną długość, a `gethostname` pisze najwyżej tyle.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(name) = std::str::from_utf8(&buf[..end])
                && !name.is_empty()
            {
                return name.to_owned();
            }
        }
    }
    "aars".to_owned()
}

/// Adres kreatora do logu i `STATUS=`: przy nasłuchu na wszystkich adresach
/// `http://{host}.local:{port}/setup` (avahi, R14), inaczej sam adres.
fn setup_url(bound: std::net::SocketAddr, host: &str) -> String {
    if bound.ip().is_unspecified() {
        let host = host.trim_end_matches(".local");
        format!("http://{host}.local:{}/setup", bound.port())
    } else {
        format!("http://{bound}/setup")
    }
}

/// Nasłuch panelu. Obraz słucha na `[::]:8080` (przegląd końcowy M-2):
/// jedno gniazdo przyjmuje IPv6 i IPv4 (jako `::ffff:a.b.c.d`). Gniazdo
/// `[::]` ma jawnie `IPV6_V6ONLY = 0` (przegląd N-2), więc IPv4 działa także
/// przy `net.ipv6.bindv6only = 1`. Gdy jądro nie ma IPv6 (`ipv6.disable=1`),
/// `[::]` nie wstanie — wtedy panel słucha na `0.0.0.0` tego samego portu,
/// żeby właściciel nie stracił dostępu.
async fn bind_panel(bind: &str) -> std::io::Result<tokio::net::TcpListener> {
    match bind_once(bind).await {
        Ok(listener) => Ok(listener),
        Err(err) => match ipv4_fallback(bind, &err) {
            Some(v4) => {
                tracing::warn!(%err, %bind, fallback = %v4, "IPv6 niedostępne — panel słucha tylko na IPv4");
                tokio::net::TcpListener::bind(v4).await
            }
            None => Err(err),
        },
    }
}

/// `[::]:port` przez `socket2` z `IPV6_V6ONLY = 0`; każdy inny adres
/// zwykłym `bind` tokio.
async fn bind_once(bind: &str) -> std::io::Result<tokio::net::TcpListener> {
    match bind.parse::<std::net::SocketAddr>() {
        Ok(addr) if addr.is_ipv6() && addr.ip().is_unspecified() => dual_stack(addr),
        _ => tokio::net::TcpListener::bind(bind).await,
    }
}

fn dual_stack(addr: std::net::SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(false)?;
    // Jak `TcpListener::bind` tokio na Uniksie: szybki restart usługi.
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    tokio::net::TcpListener::from_std(socket.into())
}

/// `[::]:port` → `0.0.0.0:port`, gdy błąd nie dotyczy portu (zajęty albo
/// bez uprawnień — tam IPv4 nic nie zmieni).
fn ipv4_fallback(bind: &str, err: &std::io::Error) -> Option<std::net::SocketAddr> {
    use std::io::ErrorKind;
    let addr: std::net::SocketAddr = bind.parse().ok()?;
    let port_problem = matches!(err.kind(), ErrorKind::AddrInUse | ErrorKind::PermissionDenied);
    (addr.is_ipv6() && addr.ip().is_unspecified() && !port_problem)
        .then(|| std::net::SocketAddr::from(([0, 0, 0, 0], addr.port())))
}

/// Stan dla systemd, gdy stacja działa bez stosu nadawczego (R6).
const RADIO_LESS_STATUS: &str =
    "stacja działa bez nadawania — żadna konfiguracja sprzętu nie wstała";

// Argumenty to niezależne zależności przebiegu (sprzęt, sonda, sygnały,
// systemd, zdrowie, uchwyt dla testów); struktura dla nich nic by nie dała.
#[allow(clippy::too_many_arguments)]
async fn run_station(
    base: &Config,
    pool: &SqlitePool,
    factory: Arc<dyn HardwareFactory>,
    probe: Arc<dyn HardwareProbe>,
    stop: CancelToken,
    notifier: Arc<dyn Notifier>,
    health: Arc<HealthCell>,
    ready: Option<oneshot::Sender<Arc<StationHandle>>>,
) -> Result<Exit> {
    health.set_dispatcher_alive(false);
    // Do wejścia w pętlę ocena próby czeka (przegląd 5–7 I-2).
    health.set_restarting(true);
    let sources = aars_sources::modules();
    let mut descriptors = aars_core::module::builtin();
    descriptors.extend(sources.iter().map(|m| m.descriptor));
    let registry = Arc::new(ModuleRegistry::new(descriptors));
    ModuleRepo::new(pool.clone())
        .register(&registry)
        .await
        .context("rejestracja modułów")?;

    // Kandydaci i stos (R5, R6).
    let hw = HardwareRepo::new(pool.clone());
    let panel = hw
        .overrides()
        .await
        .context("odczyt ustawień sprzętu z panelu")?;
    let last_good = hw
        .last_good()
        .await
        .context("odczyt ostatniej działającej konfiguracji")?;
    let (cands, mut notes) = candidates(base, &panel, &last_good);
    for note in &notes {
        tracing::warn!(%note, "ustawienie sprzętu z panelu pominięte");
    }
    let first = cands[0].clone();
    let (built, errors) = build_first(cands, factory.as_ref(), pool).await;
    let radio_less = built.is_none();
    health.set_radio(!radio_less, join_errors(&errors));
    let (stack, config, origin) = match built {
        Some((stack, cand)) => (Some(stack), cand.config, cand.origin),
        None => {
            tracing::error!("żadna konfiguracja sprzętu nie wstała — stacja działa bez nadawania");
            (None, first.config, ConfigOrigin::None)
        }
    };

    // Panel: `bind` kandydata, a przy błędzie `bind` z pliku (R6).
    let listener = match bind_panel(&config.server.bind).await {
        Ok(listener) => listener,
        Err(err) if config.server.bind != base.server.bind => {
            let note = format!(
                "nasłuch na {}: {err} — panel wrócił na {} z pliku",
                config.server.bind, base.server.bind
            );
            tracing::error!(%note);
            // R6: nieudane zastosowanie ustawienia z panelu idzie do audytu.
            if panel.contains_key("server.bind") {
                audit_system(pool, "hardware_apply_failed", &note).await;
            }
            notes.push(note);
            bind_panel(&base.server.bind)
                .await
                .with_context(|| format!("nasłuch na {}", base.server.bind))?
        }
        Err(err) => {
            return Err(err).with_context(|| format!("nasłuch na {}", config.server.bind));
        }
    };
    let bound = listener
        .local_addr()
        .map_or_else(|_| config.server.bind.clone(), |a| a.to_string());
    let setup_url = listener.local_addr().ok().map(|a| setup_url(a, &local_host_name()));

    let mut status = ApplyStatus {
        origin,
        error: join_errors(&errors),
    };
    if !notes.is_empty() {
        let extra = notes.join("; ");
        status.error = Some(match status.error.take() {
            Some(error) => format!("{error}; {extra}"),
            None => extra,
        });
        if let Err(err) = hw.set_apply_status(&status).await {
            tracing::error!(%err, "zapis stanu zastosowania sprzętu zawiódł");
        }
    }

    let station = SettingsRepo::new(pool.clone())
        .load()
        .await
        .context("odczyt ustawień stacji")?;
    let interlocks = match &stack {
        Some(stack) => stack.interlocks.clone(),
        None => radio::load_interlocks(pool).await?,
    };

    let synth = factory.synth(&config);
    let cache = aars_audio::cache::AudioCache::new(
        pool.clone(),
        config.storage.cache_dir.clone(),
        config.storage.cache_max_bytes,
    );
    // Świeży renderer i podgląd przy każdym przebiegu: po restarcie obowiązują
    // nowe wzmocnienie i głos (R19).
    let preview = Arc::new(render::PiperPreview::new(
        cache.clone(),
        synth.clone(),
        config.audio.gain_db,
        SettingsRepo::new(pool.clone()),
    ));
    let queue = Arc::new(QueueRepo::new(pool.clone()));

    let shutdown = CancelToken::new();
    let drain = CancelToken::new();
    let restart = CancelToken::new();
    let update = CancelToken::new();
    let closed = CancelToken::new();
    // Żądanie sprzed tego przebiegu nie przyszło z tego przebiegu: strażnik
    // już je widział albo poprzedni przebieg nie zakończył procesu
    // (przegląd 8–9 I-1). Nie może czekać na przypadkowy start.
    crate::update::discard_request(
        &crate::update::paths::UpdatePaths::from_config(base),
        "żądanie sprzed startu tego przebiegu stacji",
    );

    let users = aars_core::user::UserRepo::new(pool.clone());
    // R12: tryb pierwszego konta tylko z pliku (`base`). W trybie `wizard`
    // hasła na konsoli nie ma — konto zakłada kreator `/setup`.
    let wizard = base.setup.first_admin == FirstAdmin::Wizard;
    if !wizard
        && let Some(password) = users
            .ensure_bootstrap_admin()
            .await
            .context("konto startowe")?
    {
        // Jedyny moment, w którym to hasło jest widoczne.
        println!("== konto startowe: admin / {password}");
        println!("== zmień je po pierwszym logowaniu");
    }
    if let Err(err) = users.purge_expired_sessions().await {
        tracing::error!(%err, "czyszczenie wygasłych sesji zawiodło");
    }

    // Aktualizacja ze stałych pór: tryb cykliczny jest domyślny, więc
    // stacja bez zapisanego trybu nadaje teraz co 10 minut.
    match aars_core::bulletin_layout::times_without_mode(pool).await {
        Ok(true) => tracing::warn!(
            "biuletyn działa teraz w trybie cyklicznym (domyślnie co {} min), choć zapisane są stałe pory; \
             żeby wrócić do stałych pór, w panelu Biuletyn wybierz tryb „o stałych porach” i zapisz",
            aars_core::bulletin_layout::DEFAULT_INTERVAL_MIN
        ),
        Ok(false) => {}
        Err(err) => tracing::error!(%err, "odczyt trybu biuletynu zawiódł"),
    }

    // Biuletyn i źródła `pogoda`, `imgw-wodowskazy`, `powietrze` dzielą
    // instancje (jedna pamięć danych). Błąd budowy wyłącza biuletyn i te
    // źródła, reszta działa.
    let bulletin = match aars_sources::bulletin::build(&config, pool.clone()) {
        Ok(built) => Some(built),
        Err(err) => {
            tracing::error!(
                source = aars_core::module::BULLETIN.id,
                %err,
                "biuletyn nie wystartuje: {err}"
            );
            for id in [
                aars_sources::pogoda::ID,
                aars_sources::imgw_wodowskazy::ID,
                aars_sources::powietrze::ID,
            ] {
                source_start_failed(&SourceStateRepo::new(pool.clone()), id, &err.to_string())
                    .await;
            }
            None
        }
    };
    let mut shared = SharedSources::from_bulletin(bulletin.as_ref());
    match aars_sources::imgw_meteo::ImgwMeteo::new(&config.sources.imgw_meteo_url) {
        Ok(meteo) => shared.meteo = Some(Arc::new(meteo)),
        Err(err) => {
            source_start_failed(
                &SourceStateRepo::new(pool.clone()),
                aars_sources::imgw_meteo::ID,
                &err.to_string(),
            )
            .await;
        }
    }

    // Źródła są odizolowane: błąd budowy jednego wyłącza tylko je.
    let mut runners = Vec::new();
    let mut refreshers: Vec<(&'static str, Arc<Notify>)> = Vec::new();
    for module in &sources {
        let id = module.descriptor.id;
        let Some(built) = source_for(module, &config, &shared) else {
            continue;
        };
        match built {
            Ok(source) => {
                let runner = aars_sources::SourceRunner::new(
                    source,
                    SourceStateRepo::new(pool.clone()),
                    queue.clone(),
                );
                refreshers.push((id, runner.refresh_handle()));
                runners.push(runner);
            }
            Err(err) => {
                source_start_failed(&SourceStateRepo::new(pool.clone()), id, &err.to_string())
                    .await;
            }
        }
    }

    // Stos nadawczy: dyspozytor i most strażnika tylko, gdy jest sprzęt.
    let tx_tail = Duration::from_millis(config.ptt.tx_tail_ms);
    let mut control = None;
    let mut dispatcher_task = None;
    let mut poison_task = None;
    let mut watchdog = None;
    let mut watchdog_marker = None;
    // Bez stosu nie ma nadań, więc i drenażu; wartość tylko dla porządku.
    let mut drain_limit = Duration::ZERO;
    if let Some(RadioStack {
        sequencer,
        watchdog: wd,
        interlocks: locks,
        watchdog_limit,
    }) = stack
    {
        drain_limit = watchdog_limit + DRAIN_MARGIN;
        health.set_drain_limit(drain_limit);
        let renderer = Arc::new(render::PiperRenderer::new(
            cache.clone(),
            synth.clone(),
            &station.callsign,
            station.id_mode,
            config.audio.sample_rate,
            config.audio.gain_db,
            SettingsRepo::new(pool.clone()),
            station.speech_rate,
        ));
        let mut dispatcher = Dispatcher::new(
            queue.clone(),
            renderer,
            sequencer,
            DispatcherConfig {
                tick: Duration::from_millis(500),
                ptt_backend: match config.ptt.backend {
                    PttBackendKind::Gpio => "gpio",
                    PttBackendKind::Serial => "serial",
                    PttBackendKind::Vox => "vox",
                },
                sample_rate: config.audio.sample_rate,
            },
        );
        control = Some(dispatcher.control());
        let marker = wd.marker();
        poison_task = Some(tokio::spawn(radio::watch_poison(
            marker.clone(),
            locks,
            pool.clone(),
            shutdown.clone(),
        )));
        dispatcher_task = Some(tokio::spawn(
            dispatcher.run(shutdown.clone(), drain.clone()),
        ));
        watchdog_marker = Some(marker);
        watchdog = Some(wd);
    }

    let hardware = control
        .as_ref()
        .map(|_| HardwareSettings::from_config(&config));
    let handle = Arc::new(StationHandle {
        control,
        restart: restart.clone(),
        update: update.clone(),
        closed: closed.clone(),
        status: status.clone(),
        hardware,
        station: station.clone(),
        tx_tail,
    });

    let scheduler = aars_core::scheduler::Scheduler::new(
        aars_core::message::MessageRepo::new(pool.clone()),
        queue.clone(),
        aars_core::scheduler::SchedulerConfig {
            misfire_grace: Duration::from_secs(station.misfire_grace_s),
            tick: Duration::from_secs(1),
        },
    );

    // Aktualizacja z panelu (R10, R11): `releases_url` tylko z pliku, więc
    // z `base`, a nie z konfiguracji z nadpisaniami panelu.
    let update_control: Arc<dyn aars_web::state::UpdateControl> =
        Arc::new(crate::update::control::UpdateHandle::new(
            base,
            crate::update::control::UpdateDeps {
                pool: pool.clone(),
                queue: queue.clone(),
                interlocks: interlocks.clone(),
                station: handle.clone(),
                last: crate::update::health::stored_last_result(pool).await,
            },
        ));
    let mut state = aars_web::AppState::new(pool.clone(), queue.clone(), interlocks.clone(), false)
        .with_preview(preview)
        .with_station(handle.clone())
        .with_update(update_control)
        .with_first_admin(base.setup.first_admin)
        .with_local_nets(local_nets())
        .with_host_names({
            let mut names =
                station_host_names(&local_host_name(), &[&base.server.bind, &config.server.bind]);
            // `server.allowed_hosts` tylko z pliku (`base`), jak ścieżki.
            names.extend(base.server.allowed_hosts.iter().map(|n| n.to_ascii_lowercase()));
            names
        })
        .with_host_check(true)
        // `server.allow_remote` tylko z pliku (`base`), jak `allowed_hosts`.
        .with_lan_only(!base.server.allow_remote)
        .with_probe(probe)
        .with_modules(registry.clone())
        .with_base_config(Arc::new(base.clone()))
        .with_version(service::VERSION);
    if let Some(marker) = watchdog_marker.clone() {
        state = state.with_watchdog_clear(Arc::new(move || marker.clear_poison()));
    }
    if !refreshers.is_empty() {
        state = state.with_source_refresh(Arc::new(move |id: &str| {
            match refreshers.iter().find(|(known, _)| *known == id) {
                Some((_, notify)) => {
                    notify.notify_one();
                    true
                }
                None => false,
            }
        }));
    }
    if let Some(meteo) = shared.meteo.clone() {
        state = state.with_warnings_map(Arc::new(move || {
            meteo
                .snapshot()
                .map(|snap| warnings_snapshot(snap, chrono::Utc::now()))
        }));
    }
    if let Some(built) = &bulletin {
        let bulletin = built.bulletin.clone();
        state = state.with_bulletin_preview(Arc::new(move |layout| {
            let bulletin = bulletin.clone();
            Box::pin(async move {
                bulletin
                    .compose_with(chrono::Utc::now(), layout)
                    .await
                    .map(bulletin_preview_view)
                    .map_err(|e| e.to_string())
            })
        }));
    }
    let app = aars_web::router(state);

    let mut tasks = vec![
        tokio::spawn(radio::quiet_hours_task(
            pool.clone(),
            interlocks.clone(),
            shutdown.clone(),
        )),
        tokio::spawn(scheduler.run(shutdown.clone())),
    ];
    // Runner startuje zawsze i sam sprawdza włączenie przy każdym tiku. Tylko
    // wstawia do kolejki — nadaje dyspozytor (bez stosu nikt).
    tasks.extend(bulletin.map(|built| {
        tokio::spawn(
            aars_sources::bulletin::BulletinRunner::new(built.bulletin, queue.clone())
                .run(shutdown.clone()),
        )
    }));
    tasks.extend(poison_task);
    tasks.extend(
        runners
            .into_iter()
            .map(|runner| tokio::spawn(runner.run(shutdown.clone()))),
    );
    let server_shutdown = shutdown.clone();
    tasks.push(tokio::spawn(async move {
        // Adres klienta dla kreatora `/setup` (sieć lokalna, R12).
        let service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
        let graceful = axum::serve(listener, service).with_graceful_shutdown(async move {
            while !server_shutdown.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
        if let Err(err) = graceful.await {
            tracing::error!(%err, "serwer panelu zakończył się błędem");
        }
    }));

    tracing::info!(
        bind = %bound,
        origin = status.origin.as_str(),
        misfire_grace_s = station.misfire_grace_s,
        "stacja uruchomiona, Ctrl-C kończy"
    );
    // R9: panel nasłuchuje, a stos nadawczy jest zbudowany albo stacja
    // świadomie przeszła w tryb bez nadawania.
    notifier.ready();
    health.mark_ready();
    if radio_less {
        notifier.status(RADIO_LESS_STATUS);
    }
    if wizard && !users.has_enabled_admin().await.unwrap_or(true) {
        let text = format!(
            "brak konta administratora — otwórz {} w sieci lokalnej",
            setup_url.as_deref().unwrap_or("/setup")
        );
        tracing::warn!("{text}");
        notifier.status(&text);
    }
    if let Some(ready) = ready {
        let _ = ready.send(handle.clone());
    }

    // Dyspozytor kończy się sam tylko po panice: stacja przestała nadawać,
    // więc nie udajemy, że działa (I-2).
    let mut dispatcher_failed = false;
    let exit = loop {
        if stop.is_cancelled() {
            break Exit::Stop;
        }
        if dispatcher_task.as_ref().is_some_and(|t| t.is_finished()) {
            dispatcher_failed = true;
            break Exit::Stop;
        }
        // Aktualizacja przed restartem: obie idą tą samą drogą drenażu, a
        // `Update` i tak kończy proces (przegląd 8–9 I-1).
        if update.is_cancelled() {
            break Exit::Update;
        }
        if restart.is_cancelled() {
            break Exit::Restart;
        }
        // Werdykt `bad` nie przerywa nadania (I-2): ta sama droga co restart.
        if health.fatal().is_some() {
            break Exit::Unhealthy;
        }
        // R9: dyspozytor pracuje, gdy jego pętla tyka. W trakcie nadania nie
        // tyka, ale nadanie trwa najwyżej do limitu strażnika.
        let alive = match (&dispatcher_task, &handle.control) {
            (Some(task), Some(control)) => {
                !task.is_finished() && control.since_beat() <= drain_limit
            }
            _ => true,
        };
        health.set_dispatcher_alive(alive);
        health.set_restarting(false);
        tokio::time::sleep(CONTROL_POLL).await;
    };
    // Prośba o aktualizację, która przyszła tuż po odczycie `update` w pętli,
    // zobaczy `closed` albo zostanie zauważona tutaj — nigdy nie przepada.
    closed.cancel();
    let exit = match exit {
        Exit::Restart if update.is_cancelled() => Exit::Update,
        other => other,
    };
    if matches!(exit, Exit::Restart) {
        // Drenaż i budowa następnego przebiegu to nie „dyspozytor stoi”.
        // Najpierw `restarting`, potem `alive = false`: ocena próby czyta je
        // w tej kolejności (przegląd 8–9 M-2).
        health.set_restarting(true);
    }
    health.set_dispatcher_alive(false);
    // `STOPPING=1` tylko przy zatrzymaniu na żądanie (Ctrl-C, SIGTERM).
    // Panika dyspozytora i werdykt „zła wersja” kończą proces błędem, który
    // systemd ma podnieść, więc nie udają zatrzymania (przegląd 1–4, M-2).
    let stop_requested = || stop.is_cancelled() && health.fatal().is_none();
    if matches!(exit, Exit::Stop) && !dispatcher_failed && stop_requested() {
        notifier.stopping();
    }

    // Panika dyspozytora zostawia wątek sekwencera bez joina (I-2): wtedy
    // nie wolno otwierać tego samego PTT drugi raz w tym samym procesie.
    if let (Exit::Restart | Exit::Update | Exit::Unhealthy, Some(task)) =
        (&exit, dispatcher_task.as_mut())
    {
        // R4: dyspozytor nie bierze nic nowego, dokańcza nadanie i robi join
        // wątku sekwencera. Strażnik i most działają dalej. Ctrl-C w tym
        // czasie przerywa nadanie jak zwykle.
        tracing::info!("restart: czekam na koniec bieżącego nadania");
        drain.cancel();
        // Drenaż ma limit (przegląd 1–4, I-1): po limicie strażnika TX i
        // zapasie wątek sekwencera wisi (np. zawieszona karta). Watchdog
        // systemd i tak tyka, więc bez limitu stacja milczałaby bez końca.
        // Kończymy proces błędem; jądro zwolni PTT i kartę, a
        // `Restart=always` podniesie go na czysto.
        let give_up = tokio::time::Instant::now() + drain_limit;
        let mut joined_cleanly = false;
        loop {
            tokio::select! {
                joined = &mut *task => {
                    dispatcher_failed = joined.is_err();
                    joined_cleanly = true;
                    break;
                }
                () = tokio::time::sleep(CONTROL_POLL) => {
                    if stop.is_cancelled() && !shutdown.is_cancelled() {
                        if stop_requested() {
                            notifier.stopping();
                        }
                        shutdown.cancel();
                    }
                    if !stop.is_cancelled() && tokio::time::Instant::now() >= give_up {
                        tracing::error!(
                            limit_s = drain_limit.as_secs(),
                            "nadanie nie skończyło się w limicie drenażu — kończę proces błędem"
                        );
                        dispatcher_failed = true;
                        break;
                    }
                }
            }
        }
        if joined_cleanly {
            dispatcher_task = None;
        }
        // Po zwykłym drenażu wątek sekwencera jest dołączony i znacznik czysty.
        // Znacznik w górze bez Ctrl-C znaczy, że joina nie było (np. nie
        // wystartował wątek `aars-seq-join`): stary wątek trzyma jeszcze PTT
        // i kartę, więc nie otwieramy ich drugi raz w tym procesie (F2-1).
        if !stop.is_cancelled() && watchdog_marker.as_ref().is_some_and(|m| m.is_keyed()) {
            dispatcher_failed = true;
        }
    }

    tracing::info!("zatrzymuję zadania stacji");
    shutdown.cancel();
    if let Some(task) = dispatcher_task
        && task.await.is_err()
    {
        dispatcher_failed = true;
    }
    for task in tasks {
        let _ = task.await;
    }
    if dispatcher_failed {
        tracing::error!("zadanie dyspozytora padło — wątek sekwencera mógł zostać bez joina");
    }
    finish_radio(watchdog, watchdog_marker.as_ref(), &interlocks, pool, dispatcher_failed).await?;
    drop(handle);

    if dispatcher_failed {
        // Zamiast restartu w procesie: proces kończy się błędem, a systemd
        // podnosi go na czysto, z PTT zwolnionym przez jądro.
        anyhow::bail!("dyspozytor padł — kończę proces zamiast restartu w procesie");
    }
    Ok(if stop.is_cancelled() {
        Exit::Stop
    } else {
        exit
    })
}

/// Ile razy próbujemy utrwalić interwencję strażnika przy zamykaniu.
const FINAL_POISON_TRIES: u32 = 10;

/// Zamyka strażnika bezpiecznie i utrwala jego ostatnie słowo.
///
/// Po panice dyspozytora (`dispatcher_failed`) najpierw czeka, aż PTT
/// opadnie: sekwencer może jeszcze nadawać, a strażnik jest wtedy jedynym
/// limitem (I-2). Czekanie ogranicza sam strażnik — po swoim limicie opuszcza
/// PTT i znacznik. Gdy dyspozytor skończył normalnie, PTT już opuścił (także
/// `force_unkey` po zawieszonym wyjściu przy Ctrl-C), a znacznik może zostać
/// „w górze” tylko dlatego, że zawieszony wątek sekwencera go nie zdjął —
/// czekanie dałoby wtedy minuty zamiast ograniczonego joina (F-1).
/// Po zatrzymaniu wątku strażnika flaga już się nie zmieni, więc jeden zapis
/// wystarczy; most odpytywał co 500 ms i mógł nie zdążyć (I-1).
async fn finish_radio(
    watchdog: Option<aars_core::watchdog::WatchdogHandle>,
    marker: Option<&aars_core::watchdog::WatchdogMarker>,
    interlocks: &aars_core::interlock::InMemoryInterlocks,
    pool: &SqlitePool,
    dispatcher_failed: bool,
) -> Result<()> {
    let (Some(watchdog), Some(marker)) = (watchdog, marker) else {
        return Ok(());
    };
    if dispatcher_failed && marker.is_keyed() {
        tracing::warn!("PTT wciąż w górze — czekam, aż opadnie albo zdejmie je strażnik");
        while marker.is_keyed() {
            tokio::time::sleep(CONTROL_POLL).await;
        }
    }
    watchdog.stop();
    if !marker.is_poisoned() {
        return Ok(());
    }
    for _ in 0..FINAL_POISON_TRIES {
        if radio::record_poison(marker, interlocks, pool).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("nie udało się utrwalić interwencji strażnika — kończę proces bez restartu")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use aars_core::priority::Priority;
    use aars_core::ptt::{FakePtt, PttEvent};
    use aars_core::sink::FakeSink;
    use aars_core::synth::FakeSynth;

    use crate::service::{FakeNotifier, Notifier, ReadyOnce};
    use crate::update::control::UpdateTarget;

    /// `TimeoutStopSec` w `aars.service` musi objąć najdłuższe nadanie: po
    /// SIGTERM sekwencer zwykle opuszcza PTT w ~150 ms, ale po panice
    /// dyspozytora stacja czeka na strażnika TX, a drenaż ma zapas
    /// `SERVICE_DRAIN_MARGIN`. SIGKILL wcześniej zostawiłby PTT jądru.
    /// Górne granice z walidacji panelu: `tx_max_duration_s` ≤ 600
    /// (`settings.rs`), `ptt.tx_tail_ms` ≤ 2000 (`hardware/mod.rs`).
    #[test]
    fn the_service_stop_timeout_covers_the_longest_transmission() {
        let unit = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/systemd/aars.service"
        ))
        .unwrap();
        let stop: u64 = unit
            .lines()
            .find_map(|l| l.trim().strip_prefix("TimeoutStopSec="))
            .expect("TimeoutStopSec w aars.service")
            .parse()
            .expect("TimeoutStopSec w sekundach");
        let longest = radio::watchdog_limit(Duration::from_secs(600), Duration::from_millis(2000))
            + SERVICE_DRAIN_MARGIN;
        assert!(
            Duration::from_secs(stop) >= longest,
            "TimeoutStopSec={stop} < {} s",
            longest.as_secs()
        );
    }

    struct FakeHardware {
        fail_devices: Vec<String>,
        seen: Mutex<Vec<(PttBackendKind, String)>>,
        ptt: Arc<FakePtt>,
        last_backend: Mutex<Option<PttBackendKind>>,
        /// Wyjście podstawione przez test; bez niego świeży `FakeSink`.
        sink: Mutex<Option<Arc<dyn AudioSink>>>,
    }

    impl FakeHardware {
        fn new(fail_devices: &[&str]) -> Self {
            Self {
                fail_devices: fail_devices.iter().map(|d| (*d).to_owned()).collect(),
                seen: Mutex::new(Vec::new()),
                ptt: Arc::new(FakePtt::new()),
                last_backend: Mutex::new(None),
                sink: Mutex::new(None),
            }
        }

        fn seen(&self) -> Vec<(PttBackendKind, String)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl HardwareFactory for FakeHardware {
        fn ptt(&self, cfg: &PttConfig) -> Result<Arc<dyn PttBackend>, String> {
            *self.last_backend.lock().unwrap() = Some(cfg.backend);
            Ok(self.ptt.clone())
        }

        fn sink(&self, cfg: &AudioConfig) -> Result<Arc<dyn AudioSink>, String> {
            let backend = self
                .last_backend
                .lock()
                .unwrap()
                .expect("PTT przed wyjściem audio");
            self.seen
                .lock()
                .unwrap()
                .push((backend, cfg.device.clone()));
            if self.fail_devices.contains(&cfg.device) {
                Err(format!("brak urządzenia {}", cfg.device))
            } else if let Some(sink) = self.sink.lock().unwrap().clone() {
                Ok(sink)
            } else {
                Ok(Arc::new(FakeSink::new()))
            }
        }

        fn synth(&self, config: &Config) -> Arc<dyn Synthesizer> {
            Arc::new(FakeSynth::new(config.audio.sample_rate))
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    async fn put(pool: &SqlitePool, key: &str, value: &str) {
        SettingsRepo::new(pool.clone())
            .kv_put(key, value)
            .await
            .unwrap();
    }

    async fn get(pool: &SqlitePool, key: &str) -> Option<String> {
        SettingsRepo::new(pool.clone()).kv_get(key).await.unwrap()
    }

    async fn failed_audits(pool: &SqlitePool) -> (i64, i64) {
        sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(user_id IS NULL), 0) FROM audit_log \
             WHERE action = 'hardware_apply_failed'",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn close(stack: RadioStack) {
        stack.sequencer.shutdown();
        stack.watchdog.stop();
    }

    /// Stos z kandydatów zbudowanych z tego, co leży w bazie (jak `run_once`).
    async fn build_from_db(
        pool: &SqlitePool,
        hardware: &FakeHardware,
    ) -> (Option<(RadioStack, Candidate)>, Vec<(ConfigOrigin, String)>) {
        let repo = HardwareRepo::new(pool.clone());
        let (cands, _) = candidates(
            &Config::default(),
            &repo.overrides().await.unwrap(),
            &repo.last_good().await.unwrap(),
        );
        build_first(cands, hardware, pool).await
    }

    async fn status(pool: &SqlitePool) -> ApplyStatus {
        HardwareRepo::new(pool.clone())
            .apply_status()
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn the_file_config_is_used_when_the_panel_has_nothing() {
        let pool = aars_core::db::test_pool().await;
        let hardware = FakeHardware::new(&[]);

        let (built, errors) = build_from_db(&pool, &hardware).await;

        let (stack, cand) = built.expect("plik musi wstać");
        assert_eq!(cand.origin, ConfigOrigin::File);
        assert!(errors.is_empty());
        assert_eq!(
            status(&pool).await,
            ApplyStatus {
                origin: ConfigOrigin::File,
                error: None
            }
        );
        assert_eq!(failed_audits(&pool).await.0, 0);
        assert!(
            HardwareRepo::new(pool.clone())
                .last_good()
                .await
                .unwrap()
                .is_empty()
        );
        close(stack);
    }

    #[tokio::test]
    async fn panel_settings_win_over_the_file() {
        let pool = aars_core::db::test_pool().await;
        put(&pool, "hw:ptt.backend", "gpio").await;
        put(&pool, "hw:audio.device", "hw:1,0").await;
        let hardware = FakeHardware::new(&[]);

        let (built, _) = build_from_db(&pool, &hardware).await;

        let (stack, cand) = built.unwrap();
        assert_eq!(
            hardware.seen(),
            vec![(PttBackendKind::Gpio, "hw:1,0".to_owned())]
        );
        assert_eq!(cand.origin, ConfigOrigin::Panel);
        assert_eq!(status(&pool).await.origin, ConfigOrigin::Panel);
        assert_eq!(
            get(&pool, "hw_good:audio.device").await.as_deref(),
            Some("hw:1,0")
        );
        assert_eq!(
            get(&pool, "hw_good:ptt.backend").await.as_deref(),
            Some("gpio")
        );
        close(stack);
    }

    #[tokio::test]
    async fn a_failing_panel_config_falls_back_to_the_last_good_one_and_records_the_error() {
        let pool = aars_core::db::test_pool().await;
        put(&pool, "hw:audio.device", "zly").await;
        put(&pool, "hw_good:audio.device", "hw:0,0").await;
        let hardware = FakeHardware::new(&["zly"]);

        let (built, errors) = build_from_db(&pool, &hardware).await;

        let (stack, cand) = built.unwrap();
        assert_eq!(cand.origin, ConfigOrigin::LastGood);
        assert_eq!(cand.config.audio.device, "hw:0,0");
        assert_eq!(errors.len(), 1);
        let state = status(&pool).await;
        assert_eq!(state.origin, ConfigOrigin::LastGood);
        assert!(state.error.as_deref().unwrap().contains("zly"), "{state:?}");
        assert_eq!(
            failed_audits(&pool).await,
            (1, 1),
            "jeden wpis bez użytkownika"
        );
        assert_eq!(
            get(&pool, "hw:audio.device").await.as_deref(),
            Some("zly"),
            "zapis panelu zostaje"
        );
        assert_eq!(
            get(&pool, "hw_good:audio.device").await.as_deref(),
            Some("hw:0,0")
        );
        close(stack);
    }

    #[tokio::test]
    async fn without_any_working_config_the_station_runs_without_radio() {
        let pool = aars_core::db::test_pool().await;
        put(&pool, "hw:audio.device", "zly").await;
        put(&pool, "hw_good:audio.device", "hw:0,0").await;
        let hardware = FakeHardware::new(&["zly", "hw:0,0", "default"]);

        let (built, errors) = build_from_db(&pool, &hardware).await;

        assert!(built.is_none());
        assert_eq!(errors.len(), 3);
        let state = status(&pool).await;
        assert_eq!(state.origin, ConfigOrigin::None);
        let error = state.error.unwrap();
        for part in ["zly", "hw:0,0", "default"] {
            assert!(error.contains(part), "błąd musi wymienić {part}: {error}");
        }
        for origin in [
            ConfigOrigin::Panel,
            ConfigOrigin::LastGood,
            ConfigOrigin::File,
        ] {
            assert!(error.contains(origin.label()), "{error}");
        }
    }

    #[tokio::test]
    async fn identical_candidates_are_tried_once() {
        let pool = aars_core::db::test_pool().await;
        let file = HardwareSettings::from_config(&Config::default()).to_pairs();
        let hardware = FakeHardware::new(&["default"]);

        let (cands, warnings) = candidates(&Config::default(), &file, &file);
        assert_eq!(cands.len(), 1);
        assert!(warnings.is_empty());
        let (built, errors) = build_first(cands, &hardware, &pool).await;

        assert!(built.is_none());
        assert_eq!(errors.len(), 1);
        assert_eq!(hardware.seen().len(), 1);
    }

    #[test]
    fn an_unreadable_panel_value_is_a_warning_and_falls_back_to_the_file() {
        let (cands, warnings) = candidates(
            &Config::default(),
            &map(&[("audio.sample_rate", "123")]),
            &BTreeMap::new(),
        );
        assert_eq!(cands.len(), 1, "panel bez czytelnych zmian = plik");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("audio.sample_rate"), "{warnings:?}");
    }

    #[test]
    fn hardware_show_marks_the_origin_of_each_key() {
        let lines = hardware_show_lines(
            &Config::default(),
            &map(&[("audio.device", "hw:1,0"), ("audio.sample_rate", "123")]),
            Some(&ApplyStatus {
                origin: ConfigOrigin::Panel,
                error: None,
            }),
        );
        assert!(
            lines.contains(&"audio.device = hw:1,0  (panel)".to_owned()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"audio.sample_rate = 48000  (plik)".to_owned()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"ptt.backend = vox  (plik)".to_owned()),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains(ConfigOrigin::Panel.label()))
        );
    }

    fn station_config(tag: &str) -> Config {
        let mut config = Config::default();
        // `hw:server.bind` nie przyjmie portu 0 (R9), więc port losowy idzie z pliku.
        config.server.bind = "127.0.0.1:0".to_owned();
        config.storage.cache_dir =
            std::env::temp_dir().join(format!("aars-station-{tag}-{}", std::process::id()));
        // Testy nie dotykają /var/lib ani /opt.
        let update = std::env::temp_dir().join(format!("aars-update-{tag}-{}", std::process::id()));
        config.update.state_dir = update.join("state");
        config.update.guard_dir = update.join("guard");
        config.update.install_root = update.join("opt");
        config
    }

    async fn start(
        config: Config,
        pool: SqlitePool,
        hardware: Arc<FakeHardware>,
        stop: CancelToken,
    ) -> (tokio::task::JoinHandle<Result<Exit>>, Arc<StationHandle>) {
        start_with(
            config,
            pool,
            hardware,
            stop,
            Arc::new(FakeNotifier::default()),
        )
        .await
    }

    async fn start_with(
        config: Config,
        pool: SqlitePool,
        hardware: Arc<FakeHardware>,
        stop: CancelToken,
        notifier: Arc<dyn Notifier>,
    ) -> (tokio::task::JoinHandle<Result<Exit>>, Arc<StationHandle>) {
        start_full(
            config,
            pool,
            hardware,
            stop,
            notifier,
            Arc::new(HealthCell::default()),
        )
        .await
    }

    async fn start_full(
        config: Config,
        pool: SqlitePool,
        hardware: Arc<FakeHardware>,
        stop: CancelToken,
        notifier: Arc<dyn Notifier>,
        health: Arc<HealthCell>,
    ) -> (tokio::task::JoinHandle<Result<Exit>>, Arc<StationHandle>) {
        let (ready_tx, ready_rx) = oneshot::channel();
        let probe: Arc<dyn HardwareProbe> = Arc::new(aars_core::hardware::probe::FakeProbe {
            audio: Ok(vec![]),
            serial: Ok(vec![]),
            gpio: Ok(vec![]),
        });
        let task = tokio::spawn(async move {
            run_station(
                &config,
                &pool,
                hardware,
                probe,
                stop,
                notifier,
                health,
                Some(ready_tx),
            )
            .await
        });
        let handle = tokio::time::timeout(Duration::from_secs(10), ready_rx)
            .await
            .expect("stacja musi wstać")
            .expect("stacja padła przy starcie");
        (task, handle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restart_requested_mid_transmission_waits_for_the_unkey() {
        let pool = aars_core::db::test_pool().await;
        let ids = QueueRepo::new(pool.clone())
            .enqueue(aars_core::queue::EnqueueRequest {
                message_id: None,
                // FakeSynth: 50 ms na znak, ~1,5 s audio.
                rendered_text: "komunikat testowy trwa sekundy".to_owned(),
                priority: Priority::Normal,
                interrupting: false,
                not_before: None,
                expires_at: None,
                repeat_count: 1,
                repeat_interval: Duration::ZERO,
                created_by: None,
            })
            .await
            .unwrap();
        let hardware = Arc::new(FakeHardware::new(&[]));
        let ptt = hardware.ptt.clone();
        let (task, handle) = start(
            station_config("restart"),
            pool.clone(),
            hardware,
            CancelToken::new(),
        )
        .await;

        tokio::time::timeout(Duration::from_secs(10), async {
            while !ptt.is_keyed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("zadanie musi wejść na antenę");
        handle.request_restart();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !task.is_finished() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "restart musi się zakończyć"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!ptt.is_keyed(), "run_once wraca dopiero po opuszczeniu PTT");
        let exit = task.await.unwrap().unwrap();
        assert!(matches!(exit, Exit::Restart));

        // Wątek sekwencera przy wyjściu woła jeszcze `force_unkey` (stan
        // bezpieczny); liczy się jedna para podniesienie/opuszczenie.
        let transitions: Vec<PttEvent> = ptt
            .events()
            .into_iter()
            .filter(|e| *e != PttEvent::ForceUnkey)
            .collect();
        assert_eq!(
            transitions,
            vec![PttEvent::Key, PttEvent::Unkey],
            "{:?}",
            ptt.events()
        );
        let result: String =
            sqlx::query_scalar("SELECT result FROM tx_log WHERE queue_item_id = ?")
                .bind(ids[0])
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(result, "ok", "restart nie może przerwać nadania");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_update_requested_mid_transmission_waits_for_the_unkey() {
        let pool = aars_core::db::test_pool().await;
        let queue = QueueRepo::new(pool.clone());
        let mut ids = Vec::new();
        for text in [
            "komunikat testowy trwa sekundy",
            "drugi komunikat w kolejce",
        ] {
            ids.extend(
                queue
                    .enqueue(aars_core::queue::EnqueueRequest {
                        message_id: None,
                        // FakeSynth: 50 ms na znak, ~1,5 s audio.
                        rendered_text: text.to_owned(),
                        priority: Priority::Normal,
                        interrupting: false,
                        not_before: None,
                        expires_at: None,
                        repeat_count: 1,
                        repeat_interval: Duration::ZERO,
                        created_by: None,
                    })
                    .await
                    .unwrap(),
            );
        }
        let hardware = Arc::new(FakeHardware::new(&[]));
        let ptt = hardware.ptt.clone();
        let health = Arc::new(HealthCell::default());
        let (task, handle) = start_full(
            station_config("update"),
            pool.clone(),
            hardware,
            CancelToken::new(),
            Arc::new(FakeNotifier::default()),
            health.clone(),
        )
        .await;
        assert!(health.radio_up());

        tokio::time::timeout(Duration::from_secs(10), async {
            while !ptt.is_keyed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("zadanie musi wejść na antenę");
        assert!(health.dispatcher_alive());
        assert!(handle.request_update());

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !task.is_finished() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "aktualizacja musi zakończyć przebieg"
            );
            assert!(
                ptt.is_keyed() || !task.is_finished(),
                "przebieg nie kończy się przy wciśniętym PTT"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!ptt.is_keyed(), "run_once wraca dopiero po opuszczeniu PTT");
        let exit = task.await.unwrap().unwrap();
        assert!(matches!(exit, Exit::Update));
        assert!(
            !health.dispatcher_alive(),
            "po drenażu dyspozytor nie pracuje"
        );

        let transitions: Vec<PttEvent> = ptt
            .events()
            .into_iter()
            .filter(|e| *e != PttEvent::ForceUnkey)
            .collect();
        assert_eq!(
            transitions,
            vec![PttEvent::Key, PttEvent::Unkey],
            "{:?}",
            ptt.events()
        );
        let result: String =
            sqlx::query_scalar("SELECT result FROM tx_log WHERE queue_item_id = ?")
                .bind(ids[0])
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(result, "ok", "aktualizacja nie może przerwać nadania");
        let second: String = sqlx::query_scalar("SELECT state FROM queue_items WHERE id = ?")
            .bind(ids[1])
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(second, "pending", "drugie zadanie czeka na nową wersję");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_drain_stuck_on_a_hung_output_ends_the_process_with_an_error() {
        let pool = aars_core::db::test_pool().await;
        // Strażnik: 1 s + 0 ogona + 2 s zapasu; drenaż: to plus zapas testowy.
        put(&pool, "tx_max_duration_s", "1").await;
        // Krótki tekst: nadanie dłuższe niż limit 1 s nie podniosłoby PTT.
        enqueue_one(&pool, "kom").await;
        let hardware = Arc::new(FakeHardware::new(&[]));
        let ptt = hardware.ptt.clone();
        *hardware.sink.lock().unwrap() = Some(Arc::new(HangsLong));
        let mut config = station_config("hung-drain");
        config.ptt.tx_tail_ms = 0;
        let (task, handle) = start(config, pool.clone(), hardware, CancelToken::new()).await;

        tokio::time::timeout(Duration::from_secs(10), async {
            while !ptt.is_keyed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("zadanie musi wejść na antenę");
        let requested = std::time::Instant::now();
        handle.request_restart();

        // Limit drenażu (3 s + 2 s) i ograniczony join po zatrzymaniu (5 s).
        let result = tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("drenaż nie może czekać bez końca")
            .unwrap();
        assert!(
            result.is_err(),
            "zawieszony drenaż to błąd procesu, nie restart"
        );
        assert!(
            requested.elapsed() >= Duration::from_secs(4),
            "najpierw czekamy na limit"
        );
        assert!(!ptt.is_keyed(), "PTT opuszczone");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_ends_the_loop() {
        let pool = aars_core::db::test_pool().await;
        let stop = CancelToken::new();
        let (task, _handle) = start(
            station_config("stop"),
            pool,
            Arc::new(FakeHardware::new(&[])),
            stop.clone(),
        )
        .await;

        stop.cancel();
        let exit = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("stop kończy przebieg w 2 s")
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Stop));
    }

    #[tokio::test]
    async fn the_weather_source_is_the_bulletin_part_instance() {
        let config = Config::default();
        let pool = aars_core::db::test_pool().await;
        let built = aars_sources::bulletin::build(&config, pool).unwrap();
        let shared = SharedSources::from_bulletin(Some(&built));
        let none = SharedSources::default();
        let modules = aars_sources::modules();
        let by_id = |id: &str| modules.iter().find(|m| m.descriptor.id == id).unwrap();
        let module = by_id(aars_sources::pogoda::ID);
        let source = source_for(module, &config, &shared).unwrap().unwrap();
        // Ta sama instancja = ta sama pamięć prognozy dla runnera i biuletynu.
        assert!(std::ptr::addr_eq(
            Arc::as_ptr(&source),
            Arc::as_ptr(&built.pogoda)
        ));
        let gauges = by_id(aars_sources::imgw_wodowskazy::ID);
        let source = source_for(gauges, &config, &shared).unwrap().unwrap();
        assert!(std::ptr::addr_eq(
            Arc::as_ptr(&source),
            Arc::as_ptr(&built.wodowskazy)
        ));
        let air = by_id(aars_sources::powietrze::ID);
        let source = source_for(air, &config, &shared).unwrap().unwrap();
        assert!(std::ptr::addr_eq(
            Arc::as_ptr(&source),
            Arc::as_ptr(&built.powietrze)
        ));
        // Bez biuletynu (błąd budowy) współdzielone źródła nie startują osobno.
        assert!(source_for(module, &config, &none).is_none());
        assert!(source_for(gauges, &config, &none).is_none());
        assert!(source_for(air, &config, &none).is_none());
        // Inne źródła budują się po staremu.
        assert_eq!(
            source_for(by_id("swpc"), &config, &none).unwrap().unwrap().id(),
            "swpc"
        );
    }

    #[tokio::test]
    async fn the_meteo_source_is_the_map_instance() {
        let config = Config::default();
        let meteo = Arc::new(
            aars_sources::imgw_meteo::ImgwMeteo::new(&config.sources.imgw_meteo_url).unwrap(),
        );
        let shared = SharedSources {
            meteo: Some(meteo.clone()),
            ..SharedSources::default()
        };
        let modules = aars_sources::modules();
        let module = modules
            .iter()
            .find(|m| m.descriptor.id == aars_sources::imgw_meteo::ID)
            .unwrap();
        let source = source_for(module, &config, &shared).unwrap().unwrap();
        // Ta sama instancja = ta sama pamięć ostrzeżeń dla runnera i mapy.
        assert!(std::ptr::addr_eq(Arc::as_ptr(&source), Arc::as_ptr(&meteo)));
        // Bez instancji (błąd budowy, już zapisany) źródło nie startuje.
        assert!(source_for(module, &config, &SharedSources::default()).is_none());
    }

    #[test]
    fn a_meteo_snapshot_becomes_map_warnings() {
        const JULY: &[u8] =
            include_bytes!("../../aars-sources/tests/fixtures/imgw/warningsmeteo-2026-07-23.json");
        let parsed = aars_sources::imgw_meteo::parse::parse_warnings(JULY).unwrap();
        let total: usize = parsed.warnings.iter().map(|w| w.teryt.len()).sum();
        let fetched_at = chrono::Utc::now();
        let snap = warnings_snapshot(
            aars_sources::imgw_meteo::MeteoSnapshot {
                fetched_at,
                warnings: parsed.warnings.clone(),
            },
            fetched_at,
        );
        assert_eq!(snap.fetched_at, fetched_at);
        assert_eq!(snap.warnings.len(), total);
        let until = chrono::DateTime::parse_from_rfc3339("2026-07-23T20:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(snap.warnings.iter().any(|w| w.teryt == "1806"
            && w.level == 1
            && w.event == "Burze"
            && w.until == until));
        // Stopień nieznany → 1; czas nieistniejący w Warszawie → pominięte.
        let mut base = parsed.warnings[0].clone();
        base.teryt = vec!["1801".into()];
        let mut no_level = base.clone();
        no_level.level = None;
        let mut gap = base.clone();
        gap.valid_until =
            chrono::NaiveDateTime::parse_from_str("2026-03-29 02:30:00", "%Y-%m-%d %H:%M:%S")
                .unwrap();
        let snap = warnings_snapshot(
            aars_sources::imgw_meteo::MeteoSnapshot {
                fetched_at,
                warnings: vec![no_level, gap],
            },
            fetched_at,
        );
        assert_eq!(snap.warnings.len(), 1);
        assert_eq!(snap.warnings[0].level, 1);
    }

    #[test]
    fn a_warning_not_yet_in_force_is_left_off_the_map() {
        const JULY: &[u8] =
            include_bytes!("../../aars-sources/tests/fixtures/imgw/warningsmeteo-2026-07-23.json");
        let parsed = aars_sources::imgw_meteo::parse::parse_warnings(JULY).unwrap();
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let mut base = parsed.warnings[0].clone();
        base.teryt = vec!["1801".into()];
        let from = |s: &str| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
        };
        let mut started = base.clone();
        started.valid_from = from("2026-07-23 08:00:00");
        started.event = "Teraz".into();
        let mut later = base.clone();
        later.valid_from = from("2026-07-23 10:00:01");
        later.event = "Później".into();
        let snap = warnings_snapshot(
            aars_sources::imgw_meteo::MeteoSnapshot {
                fetched_at: at("2026-07-23T07:00:00Z"),
                warnings: vec![started, later],
            },
            at("2026-07-23T08:00:00Z"),
        );
        let events: Vec<&str> = snap.warnings.iter().map(|w| w.event.as_str()).collect();
        assert_eq!(events, ["Teraz"]);
    }

    #[test]
    fn a_composed_bulletin_becomes_the_panel_view() {
        use aars_sources::bulletin::{Composed, PartOutcome};
        let view = bulletin_preview_view(Composed {
            text: None,
            parts: vec![
                (
                    "pogoda",
                    PartOutcome::Empty("brak pozycji stacji".to_owned()),
                ),
                ("propagacja", PartOutcome::Failed("x".to_owned())),
            ],
            skipped_reason: Some("brak danych"),
        });
        assert_eq!(view.text, None);
        assert_eq!(view.skipped_reason.as_deref(), Some("brak danych"));
        assert_eq!(
            view.parts,
            vec![
                (
                    "pogoda".to_owned(),
                    "pominięta — brak pozycji stacji".to_owned()
                ),
                ("propagacja".to_owned(), "błąd — x".to_owned()),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_station_starts_with_the_bulletin_enabled_and_without_network() {
        let pool = aars_core::db::test_pool().await;
        let mut config = station_config("bulletin");
        for url in [
            &mut config.sources.open_meteo_url,
            &mut config.sources.met_no_url,
            &mut config.sources.open_meteo_aq_url,
            &mut config.sources.swpc_f107_url,
            &mut config.sources.swpc_kp_url,
            &mut config.sources.kc2g_url,
        ] {
            *url = "http://127.0.0.1:1/x".to_owned();
        }
        // Pora daleko od teraz: UTC + 6 h czytane jako czas warszawski (UTC+1/+2)
        // to za 4–5 h, więc nigdy w oknie teraz ± 15 min, w każdej strefie hosta.
        let far = (chrono::Utc::now() + chrono::TimeDelta::hours(6)).format("%H:%M");
        put(&pool, "module:bulletin:enabled", "true").await;
        put(&pool, "module:bulletin:mode", "pory").await;
        put(&pool, "module:bulletin:times", &far.to_string()).await;
        let locator = aars_core::geo::locator_from(50.04, 21.99, 6).unwrap();
        aars_core::geo::PositionRepo::new(pool.clone())
            .save(Some(&aars_core::geo::StationPosition {
                lat: 50.04,
                lon: 21.99,
                locator,
            }))
            .await
            .unwrap();
        sqlx::query("INSERT INTO source_state (source_id, enabled) VALUES ('pogoda', 1)")
            .execute(&pool)
            .await
            .unwrap();

        let hardware = Arc::new(FakeHardware::new(&[]));
        let ptt = hardware.ptt.clone();
        let stop = CancelToken::new();
        let (task, handle) = start(config, pool.clone(), hardware, stop.clone()).await;
        assert!(handle.running_hardware().is_some(), "stos nadawczy wstał");

        // Źródło `pogoda` działa (wspólna instancja) i bez sieci tylko notuje porażkę.
        let repo = SourceStateRepo::new(pool.clone());
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let failed = repo
                    .statuses()
                    .await
                    .unwrap()
                    .into_iter()
                    .any(|s| s.source_id == "pogoda" && s.consecutive_failures > 0);
                if failed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("źródło pogoda musi spróbować pobrania");
        // Runner wystartował: minione pory (wczoraj, może dziś) zajął jako
        // spóźnione, żadnej nie złożył.
        let fires = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let rows: Vec<(Option<String>, Option<i64>)> =
                    sqlx::query_as("SELECT skipped_reason, queue_item_id FROM bulletin_fires")
                        .fetch_all(&pool)
                        .await
                        .unwrap();
                if !rows.is_empty() {
                    break rows;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("runner biuletynu musi ruszyć");
        for (reason, item) in &fires {
            assert_eq!(reason.as_deref(), Some("spóźnione ponad misfire_grace_s"));
            assert_eq!(*item, None);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;

        let pending = QueueRepo::new(pool.clone()).pending(10).await.unwrap();
        assert!(pending.is_empty(), "nic przed porą: {pending:?}");
        assert!(
            ptt.events().is_empty(),
            "PTT nie kluczowany: {:?}",
            ptt.events()
        );

        stop.cancel();
        let exit = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Stop));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn without_radio_the_tests_are_unsupported_but_the_station_runs() {
        let pool = aars_core::db::test_pool().await;
        let stop = CancelToken::new();
        let (task, handle) = start(
            station_config("noradio"),
            pool,
            Arc::new(FakeHardware::new(&["default"])),
            stop.clone(),
        )
        .await;

        assert_eq!(handle.apply_status().origin, ConfigOrigin::None);
        assert!(handle.running_hardware().is_none());
        assert_eq!(
            handle
                .run_test(StationTest::Audio {
                    by: "admin".to_owned()
                })
                .await,
            TestOutcome::Unsupported(NO_RADIO)
        );

        stop.cancel();
        let exit = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Stop));
    }

    /// Wyjście, które wisi, dopóki strażnik nie opuści PTT, i wraca zaraz
    /// potem — najgorszy moment dla mostu odpytującego co 500 ms (I-1).
    struct HangsUntilWatchdog(Arc<FakePtt>);

    impl AudioSink for HangsUntilWatchdog {
        fn play(
            &self,
            _: &aars_core::synth::Pcm,
            _: &CancelToken,
        ) -> Result<aars_core::sink::PlayReport, aars_core::sink::SinkError> {
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !self.0.events().contains(&PttEvent::ForceUnkey) {
                assert!(std::time::Instant::now() < deadline, "strażnik nie zainterweniował");
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(aars_core::sink::PlayReport {
                played: Duration::from_secs(1),
                cancelled: false,
            })
        }
    }

    async fn enqueue_one(pool: &SqlitePool, text: &str) {
        QueueRepo::new(pool.clone())
            .enqueue(aars_core::queue::EnqueueRequest {
                message_id: None,
                rendered_text: text.to_owned(),
                priority: Priority::Normal,
                interrupting: false,
                not_before: None,
                expires_at: None,
                repeat_count: 1,
                repeat_interval: Duration::ZERO,
                created_by: None,
            })
            .await
            .unwrap();
    }

    fn trial_paths(dir: &std::path::Path) -> crate::update::paths::UpdatePaths {
        let paths = crate::update::paths::UpdatePaths {
            install_root: dir.join("opt"),
            guard_dir: dir.join("guard"),
            state_dir: dir.join("state"),
            db_path: dir.join("aars.db"),
        };
        std::fs::create_dir_all(&paths.guard_dir).unwrap();
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        crate::update::install::write_trial(
            &paths,
            &aars_core::update::Trial {
                format: 1,
                version: "0.11.0".to_owned(),
                previous: "0.10.0".to_owned(),
                installed_at: String::new(),
                attempts: 1,
                radio_was_up: true,
            },
        )
        .unwrap();
        paths
    }

    async fn tx_results(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT result FROM tx_log ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn wait_keyed(ptt: &FakePtt) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !ptt.is_keyed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("zadanie musi wejść na antenę");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restart_during_the_trial_and_a_long_transmission_is_not_a_bad_verdict() {
        // Przegląd 5–7 I-2: chwila oceny trafia w drenaż przed restartem.
        let dir = tempfile::tempdir().unwrap();
        let paths = trial_paths(dir.path());
        let pool = aars_core::db::test_pool().await;
        // FakeSynth: 50 ms na znak, ~3 s audio.
        enqueue_one(&pool, "komunikat testowy trwa kilka dobrych sekund na antenie").await;
        let hardware = Arc::new(FakeHardware::new(&[]));
        let ptt = hardware.ptt.clone();
        let health = Arc::new(HealthCell::default());
        let stop = CancelToken::new();
        let fake = Arc::new(FakeNotifier::default());
        let config = station_config("trial-restart");
        let (task, handle) = start_full(
            config.clone(),
            pool.clone(),
            hardware.clone(),
            stop.clone(),
            fake.clone(),
            health.clone(),
        )
        .await;
        wait_keyed(&ptt).await;
        handle.request_restart();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !health.restarting() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("drenaż oznacza restart");
        // Ocena teraz: czas próby minął, a stacja właśnie drenuje.
        let confirm = tokio::spawn(crate::update::health::confirm_after(
            paths.clone(),
            health.clone(),
            "0.11.0".to_owned(),
            stop.clone(),
            Duration::ZERO,
        ));
        let exit = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Restart));
        assert!(!stop.is_cancelled(), "nadanie nie zostało przerwane przez stop");
        assert_eq!(tx_results(&pool).await, vec!["ok".to_owned()]);
        assert!(!paths.verdict().exists(), "w trakcie restartu nie ma werdyktu");

        // Następny przebieg w tym samym procesie.
        let (task, _handle) = start_full(
            config,
            pool.clone(),
            Arc::new(FakeHardware::new(&[])),
            stop.clone(),
            fake,
            health.clone(),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(10), confirm)
            .await
            .expect("ocena po restarcie")
            .unwrap();
        assert_eq!(std::fs::read_to_string(paths.confirmed()).unwrap(), "0.11.0");
        assert!(!paths.verdict().exists());
        assert!(health.fatal().is_none());
        stop.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bad_verdict_during_a_transmission_waits_for_the_unkey() {
        // Przegląd 5–7 I-2, pkt 2: werdykt przy działającym stosie kończy
        // przez drenaż, a nie przez `stop`.
        let pool = aars_core::db::test_pool().await;
        enqueue_one(&pool, "komunikat testowy trwa kilka sekund").await;
        enqueue_one(&pool, "drugie zadanie nie wchodzi").await;
        let hardware = Arc::new(FakeHardware::new(&[]));
        let ptt = hardware.ptt.clone();
        let health = Arc::new(HealthCell::default());
        let stop = CancelToken::new();
        let fake = Arc::new(FakeNotifier::default());
        let (task, _handle) = start_full(
            station_config("bad-verdict"),
            pool.clone(),
            hardware,
            stop.clone(),
            fake.clone(),
            health.clone(),
        )
        .await;
        wait_keyed(&ptt).await;
        health.set_fatal("bad 0.11.0: dyspozytor nie pracuje".to_owned());
        let exit = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Unhealthy));
        assert!(!ptt.is_keyed());
        assert!(!stop.is_cancelled());
        assert_eq!(tx_results(&pool).await, vec!["ok".to_owned()]);
        assert!(
            !fake.events().contains(&"STOPPING=1".to_owned()),
            "{:?}",
            fake.events()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_watchdog_intervention_during_the_restart_drain_survives_the_restart() {
        let pool = aars_core::db::test_pool().await;
        // Strażnik: 1 s + 0 ogona + 2 s zapasu.
        put(&pool, "tx_max_duration_s", "1").await;
        // Krótki tekst: nadanie dłuższe niż limit 1 s nie podniosłoby PTT.
        enqueue_one(&pool, "kom").await;
        let hardware = Arc::new(FakeHardware::new(&[]));
        let ptt = hardware.ptt.clone();
        *hardware.sink.lock().unwrap() = Some(Arc::new(HangsUntilWatchdog(ptt.clone())));
        let mut config = station_config("drain-poison");
        config.ptt.tx_tail_ms = 0;
        let (task, handle) = start(config, pool.clone(), hardware, CancelToken::new()).await;

        tokio::time::timeout(Duration::from_secs(10), async {
            while !ptt.is_keyed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("zadanie musi wejść na antenę");
        handle.request_restart();

        let exit = tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("restart musi się zakończyć")
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Restart));
        assert_eq!(
            get(&pool, "poisoned").await.as_deref(),
            Some("true"),
            "interwencja w trakcie drenażu musi trafić do bazy przed restartem"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_radio_waits_for_the_watchdog_before_stopping_it() {
        let pool = aars_core::db::test_pool().await;
        let ptt = Arc::new(FakePtt::new());
        let watchdog =
            aars_core::watchdog::Watchdog::spawn(ptt.clone(), Duration::from_millis(200));
        let marker = watchdog.marker();
        // Sekwencer po panice dyspozytora: PTT w górze, nikt go nie opuści.
        ptt.key().unwrap();
        marker.mark_keyed();
        let interlocks = aars_core::interlock::InMemoryInterlocks::permissive();

        tokio::time::timeout(
            Duration::from_secs(5),
            finish_radio(Some(watchdog), Some(&marker), &interlocks, &pool, true),
        )
        .await
        .expect("czekanie ogranicza limit strażnika")
        .unwrap();

        assert!(!ptt.is_keyed(), "strażnik nie może zniknąć przy wciśniętym PTT");
        assert!(aars_core::interlock::Interlocks::snapshot(&interlocks).poisoned);
        assert_eq!(get(&pool, "poisoned").await.as_deref(), Some("true"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_station_poisoned_in_the_database_refuses_the_audio_test_after_restart() {
        let pool = aars_core::db::test_pool().await;
        put(&pool, "poisoned", "true").await;
        let hardware = Arc::new(FakeHardware::new(&[]));
        let sink = Arc::new(FakeSink::new());
        *hardware.sink.lock().unwrap() = Some(sink.clone());
        let stop = CancelToken::new();
        // Przy VOX test audio jest niedostępny z innego powodu; tu PTT osobne.
        let mut config = station_config("poisoned-audio");
        config.ptt.backend = PttBackendKind::Serial;
        let (task, handle) = start(config, pool.clone(), hardware, stop.clone()).await;

        let outcome = handle
            .run_test(StationTest::Audio {
                by: "admin".to_owned(),
            })
            .await;
        assert!(
            !matches!(outcome, TestOutcome::Done { .. }),
            "test audio przy blokadzie z bazy: {outcome:?}"
        );
        assert_eq!(sink.played_samples(), 0, "nic nie może iść na wejście radia");

        stop.cancel();
        let exit = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Stop));
        assert_eq!(get(&pool, "poisoned").await.as_deref(), Some("true"));
    }

    // --- Kryterium wyjścia etapu 9 ---------------------------------------

    fn form_encode(text: &str) -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        body: String,
    ) -> (axum::http::StatusCode, Option<String>, String) {
        use axum::http::{Request, header};
        use tower::ServiceExt;
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if method == "POST" {
            request = request.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        let response = app
            .clone()
            .oneshot(request.body(axum::body::Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or_default().to_owned());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, cookie, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn audit_count(pool: &SqlitePool, action: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE action = ?")
            .bind(action)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Świeża instalacja: w konfiguracji tylko ścieżki, a administrator
    /// ustawia z panelu PTT, wyjście audio, głos, pozycję i moduły. Stacja
    /// buduje się potem z tych ustawień (kryterium wyjścia etapu 9).
    #[tokio::test]
    async fn a_fresh_install_is_configured_entirely_from_the_panel() -> Result<()> {
        use aars_core::geo::PositionRepo;
        use aars_core::hardware::probe::{FakeProbe, GpioChipInfo};
        use aars_core::message::MessageRepo;
        use aars_core::scheduler::{Scheduler, SchedulerConfig};
        use aars_core::user::{Role, UserRepo};
        use aars_web::state::AppState;
        use aars_web::state::testing::FakeStation;
        use axum::http::StatusCode;
        use chrono::Utc;

        let dir = std::env::temp_dir().join(format!("aars-fresh-install-{}", std::process::id()));
        let voices = dir.join("voices");
        std::fs::create_dir_all(&voices)?;
        let model = voices.join("pl_PL-test.onnx");
        std::fs::write(&model, b"model")?;
        std::fs::write(voices.join("pl_PL-test.onnx.json"), b"{}")?;
        let mut base = Config::default();
        base.storage.db_path = dir.join("aars.db");
        base.storage.cache_dir = dir.join("cache");
        base.tts.voices_dir = voices.clone();

        let pool = aars_core::db::test_pool().await;
        let mut descriptors = aars_core::module::builtin();
        descriptors.extend(aars_sources::modules().into_iter().map(|m| m.descriptor));
        let registry = Arc::new(ModuleRegistry::new(descriptors));
        ModuleRepo::new(pool.clone()).register(&registry).await?;
        let probe = FakeProbe {
            audio: Ok(vec!["default".to_owned(), "hw:1,0".to_owned()]),
            serial: Ok(vec![]),
            gpio: Ok(vec![GpioChipInfo {
                path: "/dev/gpiochip0".to_owned(),
                label: "pinctrl".to_owned(),
                lines: 28,
            }]),
        };
        let station = FakeStation::new(Some(HardwareSettings::from_config(&base)));
        let state = AppState::for_tests(pool.clone())
            .with_modules(registry)
            .with_probe(Arc::new(probe))
            .with_station(Arc::new(station))
            .with_base_config(Arc::new(base.clone()));
        UserRepo::new(pool.clone())
            .create("admin", "haslo", Role::Admin)
            .await?;
        let app = aars_web::router(state);

        let (_, cookie, _) = send(
            &app,
            "POST",
            "/login",
            None,
            "username=admin&password=haslo".to_owned(),
        )
        .await;
        let cookie = cookie.expect("logowanie ustawia cookie");
        let (_, _, dashboard) = send(&app, "GET", "/", Some(&cookie), String::new()).await;
        let csrf = dashboard
            .split("name=\"csrf\" value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("token CSRF na pulpicie")
            .to_owned();

        let hardware_form = [
            ("bind", "0.0.0.0:8080".to_owned()),
            ("audio_device", "hw:1,0".to_owned()),
            ("sample_rate", "48000".to_owned()),
            ("gain_db", "-3".to_owned()),
            ("ptt_backend", "gpio".to_owned()),
            ("tx_tail_ms", "200".to_owned()),
            ("gpio_chip", "gpiochip0".to_owned()),
            ("gpio_line", "17".to_owned()),
            ("gpio_inhibit_line", String::new()),
            ("serial_port", "/dev/ttyUSB0".to_owned()),
            ("serial_line", "rts".to_owned()),
            ("voice_model", model.display().to_string()),
        ]
        .iter()
        .map(|(k, v)| format!("{k}={}", form_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
        for (uri, fields, expected) in [
            ("/settings/hardware", hardware_form, StatusCode::SEE_OTHER),
            ("/settings/position", "locator=JO91RS".to_owned(), StatusCode::SEE_OTHER),
            (
                "/modules/imgw-meteo/settings",
                "area=10&interval_s=300".to_owned(),
                StatusCode::SEE_OTHER,
            ),
            ("/modules/imgw-meteo/enabled", "enabled=1".to_owned(), StatusCode::SEE_OTHER),
            ("/modules/scheduler/enabled", "enabled=0".to_owned(), StatusCode::SEE_OTHER),
        ] {
            let (status, _, body) = send(
                &app,
                "POST",
                uri,
                Some(&cookie),
                format!("csrf={csrf}&{fields}"),
            )
            .await;
            assert_eq!(status, expected, "POST {uri}: {body}");
        }

        // Stacja buduje się z tego, co zapisał panel.
        let hardware = FakeHardware::new(&[]);
        let (cands, _) = candidates(
            &base,
            &HardwareRepo::new(pool.clone()).overrides().await?,
            &BTreeMap::new(),
        );
        let (built, errors) = build_first(cands, &hardware, &pool).await;
        let (stack, used) = built.expect("stos z ustawień panelu");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(used.origin, ConfigOrigin::Panel);
        assert_eq!(
            hardware.seen(),
            vec![(PttBackendKind::Gpio, "hw:1,0".to_owned())]
        );
        assert_eq!(used.config.ptt.gpio.line, 17);
        assert_eq!(used.config.audio.sample_rate, 48000);
        assert_eq!(used.config.server.bind, "0.0.0.0:8080");
        assert!(used.config.tts.model.ends_with("pl_PL-test.onnx"));
        close(stack);

        assert_eq!(
            PositionRepo::new(pool.clone()).load().await?.unwrap().locator,
            "JO91RS"
        );
        let imgw = SourceStateRepo::new(pool.clone())
            .config("imgw-meteo")
            .await?
            .unwrap();
        assert!(imgw.enabled);
        assert_eq!(imgw.area.len(), 1);
        let queue = Arc::new(QueueRepo::new(pool.clone()));
        let scheduler = Scheduler::new(
            MessageRepo::new(pool.clone()),
            queue,
            SchedulerConfig {
                misfire_grace: Duration::from_secs(300),
                tick: Duration::from_secs(1),
            },
        );
        assert!(scheduler.tick(Utc::now()).await?.disabled);
        for action in ["hardware", "position", "module_settings", "module_enabled"] {
            assert!(audit_count(&pool, action).await >= 1, "{action}");
        }

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Wyjście, które wisi dłużej niż ograniczony join i ignoruje anulowanie.
    struct HangsLong;

    impl AudioSink for HangsLong {
        fn play(
            &self,
            _: &aars_core::synth::Pcm,
            _: &CancelToken,
        ) -> Result<aars_core::sink::PlayReport, aars_core::sink::SinkError> {
            std::thread::sleep(Duration::from_secs(30));
            Ok(aars_core::sink::PlayReport {
                played: Duration::ZERO,
                cancelled: true,
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_c_with_a_hung_output_ends_within_the_bounded_join() {
        let pool = aars_core::db::test_pool().await;
        enqueue_one(&pool, "komunikat na zawieszoną kartę").await;
        let hardware = Arc::new(FakeHardware::new(&[]));
        let ptt = hardware.ptt.clone();
        *hardware.sink.lock().unwrap() = Some(Arc::new(HangsLong));
        let stop = CancelToken::new();
        let (task, _handle) =
            start(station_config("hung-stop"), pool.clone(), hardware, stop.clone()).await;

        tokio::time::timeout(Duration::from_secs(10), async {
            while !ptt.is_keyed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("zadanie musi wejść na antenę");
        stop.cancel();

        let exit = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("Ctrl-C nie może czekać na limit strażnika (minuty)")
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Stop));
        assert!(!ptt.is_keyed(), "PTT opuszczone przez ograniczony join");
        assert_eq!(get(&pool, "poisoned").await.as_deref(), Some("false"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ready_is_sent_once_after_the_panel_listens() {
        let pool = aars_core::db::test_pool().await;
        let fake = Arc::new(FakeNotifier::default());
        let notifier: Arc<dyn Notifier> = Arc::new(ReadyOnce::new(fake.clone()));
        let config = station_config("ready");

        let (task, handle) = start_with(
            config.clone(),
            pool.clone(),
            Arc::new(FakeHardware::new(&[])),
            CancelToken::new(),
            notifier.clone(),
        )
        .await;
        // `ready` idzie po nasłuchu panelu i zbudowaniu stosu.
        assert_eq!(fake.events(), vec!["READY=1".to_owned()]);

        handle.request_restart();
        let exit = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Restart));
        assert!(
            !fake.events().contains(&"STOPPING=1".to_owned()),
            "restart w procesie to nie zatrzymanie: {:?}",
            fake.events()
        );

        let stop = CancelToken::new();
        let (task, _handle) = start_with(
            config,
            pool,
            Arc::new(FakeHardware::new(&[])),
            stop.clone(),
            notifier,
        )
        .await;
        stop.cancel();
        let exit = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Stop));

        assert_eq!(
            fake.events(),
            vec![
                "READY=1".to_owned(),
                "STATUS=restart w procesie".to_owned(),
                "STOPPING=1".to_owned(),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unhealthy_exit_does_not_announce_stopping() {
        // Werdykt „zła wersja” kończy proces błędem przez ten sam `stop`.
        // systemd nie może dostać `STOPPING=1`, bo to nie jest zatrzymanie
        // na żądanie (przegląd 1–4, M-2).
        let pool = aars_core::db::test_pool().await;
        let fake = Arc::new(FakeNotifier::default());
        let health = Arc::new(HealthCell::default());
        let stop = CancelToken::new();
        let (task, _handle) = start_full(
            station_config("fatal-no-stopping"),
            pool,
            Arc::new(FakeHardware::new(&[])),
            stop.clone(),
            fake.clone(),
            health.clone(),
        )
        .await;
        health.set_fatal("nadawanie nie wstało".to_owned());
        stop.cancel();
        let exit = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Stop));
        assert_eq!(fake.events(), vec!["READY=1".to_owned()]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wizard_mode_prints_no_password_and_points_to_setup() {
        let pool = aars_core::db::test_pool().await;
        let fake = Arc::new(FakeNotifier::default());
        let mut config = station_config("wizard");
        config.setup.first_admin = FirstAdmin::Wizard;
        let stop = CancelToken::new();
        let (task, _handle) = start_with(
            config,
            pool.clone(),
            Arc::new(FakeHardware::new(&[])),
            stop.clone(),
            fake.clone(),
        )
        .await;
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(users, 0, "w trybie wizard nie ma konta startowego");
        let status = fake
            .events()
            .into_iter()
            .find(|e| e.starts_with("STATUS=brak konta administratora"))
            .expect("STATUS z adresem kreatora");
        assert!(status.contains("http://127.0.0.1:"), "{status}");
        assert!(status.ends_with("/setup w sieci lokalnej"), "{status}");
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[test]
    fn station_host_names_cover_the_hostname_mdns_and_the_bind_host() {
        assert_eq!(
            station_host_names("Stacja", &["0.0.0.0:8080", "panel.lan:8080"]),
            vec![
                "panel.lan",
                "stacja",
                "stacja.home",
                "stacja.home.arpa",
                "stacja.lan",
                "stacja.local",
                "stacja.localdomain"
            ]
        );
        assert_eq!(
            station_host_names("aars.local", &["[::]:8080"]),
            vec!["aars", "aars.home", "aars.home.arpa", "aars.lan", "aars.local", "aars.localdomain"]
        );
    }

    #[test]
    fn only_an_unavailable_ipv6_wildcard_falls_back_to_ipv4() {
        use std::io::{Error, ErrorKind};
        let unavailable = Error::from(ErrorKind::AddrNotAvailable);
        assert_eq!(
            ipv4_fallback("[::]:8080", &unavailable),
            Some("0.0.0.0:8080".parse().unwrap())
        );
        assert_eq!(ipv4_fallback("[::]:8080", &Error::from(ErrorKind::AddrInUse)), None);
        assert_eq!(ipv4_fallback("0.0.0.0:8080", &unavailable), None);
        assert_eq!(ipv4_fallback("[fe80::1]:8080", &unavailable), None);
    }

    #[tokio::test]
    async fn the_image_bind_accepts_ipv4_clients() {
        // `[::]` jednym gniazdem (M-2). Bez IPv6 w środowisku testu działa
        // zapasowe `0.0.0.0`; tak samo zachowa się stacja.
        let listener = bind_panel("[::]:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move { listener.accept().await.map(|(_, peer)| peer) });
        let _client = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let peer = accept.await.unwrap().unwrap();
        let ip = match peer.ip() {
            std::net::IpAddr::V6(v6) => std::net::IpAddr::V4(v6.to_ipv4_mapped().expect("IPv4 jako ::ffff:")),
            v4 => v4,
        };
        assert_eq!(ip, std::net::IpAddr::from([127, 0, 0, 1]));
        assert!(aars_core::netlocal::is_local_client(peer.ip(), &[]));
    }

    #[tokio::test]
    async fn the_wildcard_socket_is_dual_stack_regardless_of_bindv6only() {
        // N-2: `IPV6_V6ONLY` ustawione jawnie na 0, nie z sysctl.
        let Ok(listener) = dual_stack("[::]:0".parse().unwrap()) else {
            return; // środowisko bez IPv6: stacja użyje zapasu `0.0.0.0`
        };
        let socket = socket2::SockRef::from(&listener);
        assert!(!socket.only_v6().unwrap());
        assert!(listener.local_addr().unwrap().is_ipv6());
    }

    #[test]
    fn the_setup_url_names_the_host_when_listening_everywhere() {
        let any: std::net::SocketAddr = "0.0.0.0:8080".parse().unwrap();
        assert_eq!(setup_url(any, "aars"), "http://aars.local:8080/setup");
        assert_eq!(setup_url(any, "stacja.local"), "http://stacja.local:8080/setup");
        let one: std::net::SocketAddr = "192.168.1.2:8080".parse().unwrap();
        assert_eq!(setup_url(one, "aars"), "http://192.168.1.2:8080/setup");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ready_is_sent_in_radio_less_mode_too() {
        let pool = aars_core::db::test_pool().await;
        let fake = Arc::new(FakeNotifier::default());
        let stop = CancelToken::new();
        let (task, handle) = start_with(
            station_config("ready-noradio"),
            pool,
            Arc::new(FakeHardware::new(&["default"])),
            stop.clone(),
            fake.clone(),
        )
        .await;
        assert_eq!(handle.apply_status().origin, ConfigOrigin::None);

        let events = fake.events();
        assert_eq!(
            events.first().map(String::as_str),
            Some("READY=1"),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e.starts_with("STATUS=") && e.contains("bez nadawania")),
            "{events:?}"
        );

        stop.cancel();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    /// Przegląd 8–9 I-1: restart i aktualizacja naraz dają `Exit::Update`,
    /// a prośba do zakończonego przebiegu jest odrzucana. Żądanie sprzed
    /// przebiegu znika przy jego starcie.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_update_racing_a_restart_wins_and_a_stale_handle_refuses() {
        let config = station_config("race");
        let paths = crate::update::paths::UpdatePaths::from_config(&config);
        std::fs::create_dir_all(paths.staging().join("0.11.0")).unwrap();
        std::fs::write(paths.request(), "0.11.0\n").unwrap();

        let pool = aars_core::db::test_pool().await;
        let (task, handle) =
            start(config.clone(), pool.clone(), Arc::new(FakeHardware::new(&[])), CancelToken::new()).await;
        assert!(!paths.request().exists(), "stare żądanie wycofane przy starcie przebiegu");
        assert!(!paths.staging().exists());

        handle.restart.cancel();
        assert!(handle.request_update());
        let exit = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Update), "aktualizacja wygrywa z restartem");
        assert!(!handle.request_update(), "przebieg zakończony — prośba odrzucona");

        // Restart sam: prośba do starego uchwytu po końcu przebiegu przepada.
        let (task, handle) =
            start(config, pool, Arc::new(FakeHardware::new(&[])), CancelToken::new()).await;
        handle.restart.cancel();
        let exit = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Restart));
        assert!(!handle.request_update());
    }

    /// Kryterium wyjścia etapu 10 (Task 13): aktualizacja zlecona w trakcie
    /// nadania czeka na `Unkey`, strażnik instaluje, zdrowa wersja zostaje
    /// potwierdzona, a następna, która nie zbudowała stosu nadawczego
    /// (wcześniej działał), wraca do poprzedniej razem z kopią bazy i trafia
    /// do audytu. Czas próby jest symulowany (`judge` z `CONFIRM_AFTER`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_update_installs_after_the_transmission_and_a_broken_one_rolls_back() {
        use aars_core::update::{CONFIRM_AFTER, ResultKind};
        use aars_web::state::UpdateControl;

        use crate::update::control::{UpdateDeps, UpdateHandle};
        use crate::update::fetch::ReleaseClient;
        use crate::update::guard::{GuardAction, run_guard};
        use crate::update::health::{Verdict, audit_last_result, judge};
        use crate::update::install::{NoOwner, SmokeTest};
        use crate::update::paths::{UpdatePaths, installed_version, swap_link, write_atomic_nofollow};
        use crate::update::read_trial;
        use crate::update::testkit::{TestKey, release_archive, release_server};

        /// Wersja z `VERSION` obok binarki: bez uruchamiania świeżo
        /// zapisanego pliku (ETXTBSY przy równoległych testach na Linuksie).
        struct VersionSmoke;
        impl SmokeTest for VersionSmoke {
            fn version_of(&self, binary: &std::path::Path) -> Result<String, String> {
                installed_version(binary.parent().unwrap()).ok_or_else(|| "brak VERSION".to_owned())
            }
        }

        struct NoStation;
        impl UpdateTarget for NoStation {
            fn request_update(&self) -> bool {
                true
            }
        }

        async fn open(path: &std::path::Path) -> SqlitePool {
            let pool = aars_core::db::connect(path).await.unwrap();
            aars_core::db::migrate(&pool).await.unwrap();
            pool
        }

        async fn marker(pool: &SqlitePool) -> Option<String> {
            SettingsRepo::new(pool.clone()).kv_get("test:marker").await.unwrap()
        }

        fn handle(
            paths: &UpdatePaths,
            server: &wiremock::MockServer,
            key: &TestKey,
            running: &str,
            pool: &SqlitePool,
            station: Arc<dyn UpdateTarget>,
        ) -> UpdateHandle {
            UpdateHandle::with_client(
                paths.clone(),
                Some(ReleaseClient::new(&server.uri(), key.trusted()).unwrap()),
                running,
                None,
                UpdateDeps {
                    pool: pool.clone(),
                    queue: Arc::new(QueueRepo::new(pool.clone())),
                    interlocks: Arc::new(aars_core::interlock::InMemoryInterlocks::permissive()),
                    station,
                    last: None,
                },
            )
        }

        let dir = tempfile::tempdir().unwrap();
        let paths = UpdatePaths {
            install_root: dir.path().join("opt/aars"),
            guard_dir: dir.path().join("var/lib/aars-update"),
            state_dir: dir.path().join("var/lib/aars/update"),
            db_path: dir.path().join("var/lib/aars/aars.db"),
        };
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let first = paths.releases().join("0.10.0");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::write(first.join("aars"), "#!/bin/sh\necho \"aars 0.10.0\"\n").unwrap();
        std::fs::write(first.join("VERSION"), "0.10.0\n").unwrap();
        swap_link(&paths.current(), std::path::Path::new("releases/0.10.0")).unwrap();
        let now: chrono::DateTime<chrono::Utc> = "2026-11-21T12:00:00Z".parse().unwrap();
        let key = TestKey::generate();
        let guard = |paths: &UpdatePaths| run_guard(paths, Some(&key.trusted()), &VersionSmoke, &NoOwner, now);

        // 1. Stacja 0.10.0 nadaje ~1,5 s; admin zleca 0.11.0 w trakcie.
        let pool = open(&paths.db_path).await;
        aars_core::user::UserRepo::new(pool.clone())
            .create("admin", "dlugie-haslo-1", aars_core::user::Role::Admin)
            .await
            .unwrap();
        let queue = QueueRepo::new(pool.clone());
        let mut ids = Vec::new();
        for text in ["komunikat testowy trwa sekundy", "drugi komunikat w kolejce"] {
            ids.extend(
                queue
                    .enqueue(aars_core::queue::EnqueueRequest {
                        message_id: None,
                        rendered_text: text.to_owned(),
                        priority: Priority::Normal,
                        interrupting: false,
                        not_before: None,
                        expires_at: None,
                        repeat_count: 1,
                        repeat_interval: Duration::ZERO,
                        created_by: None,
                    })
                    .await
                    .unwrap(),
            );
        }
        let mut config = station_config("exit10");
        config.storage.db_path = paths.db_path.clone();
        config.update.install_root = paths.install_root.clone();
        config.update.guard_dir = paths.guard_dir.clone();
        config.update.state_dir = paths.state_dir.clone();
        let hardware = Arc::new(FakeHardware::new(&[]));
        let ptt = hardware.ptt.clone();
        let (task, station) = start_full(
            config,
            pool.clone(),
            hardware,
            CancelToken::new(),
            Arc::new(FakeNotifier::default()),
            Arc::new(HealthCell::default()),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !ptt.is_keyed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("zadanie musi wejść na antenę");
        let server = release_server(&key, "0.11.0", &release_archive("0.11.0")).await;
        let update = handle(&paths, &server, &key, "0.10.0", &pool, station.clone());
        update.install("0.11.0", "admin").await.unwrap();
        assert!(ptt.is_keyed(), "pobranie i sprawdzenie idą w trakcie nadania (R10d)");
        let exit = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("aktualizacja musi zakończyć przebieg")
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Update));
        assert!(!ptt.is_keyed());
        let transitions: Vec<PttEvent> =
            ptt.events().into_iter().filter(|e| *e != PttEvent::ForceUnkey).collect();
        assert_eq!(transitions, vec![PttEvent::Key, PttEvent::Unkey]);
        let result: String = sqlx::query_scalar("SELECT result FROM tx_log WHERE queue_item_id = ?")
            .bind(ids[0])
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(result, "ok", "aktualizacja nie przerywa nadania");
        let second: String = sqlx::query_scalar("SELECT state FROM queue_items WHERE id = ?")
            .bind(ids[1])
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(second, "pending", "drugie zadanie nie zostało wzięte");
        drop(update);
        pool.close().await;

        // 2. Strażnik (ExecStartPre) instaluje 0.11.0 i otwiera próbę.
        assert_eq!(guard(&paths), vec![GuardAction::Installed("0.11.0".to_owned())]);
        assert_eq!(installed_version(&paths.current()).as_deref(), Some("0.11.0"));
        assert_eq!(installed_version(&paths.previous()).as_deref(), Some("0.10.0"));
        let trial = read_trial(&paths).expect("próba trwa");
        // Pierwszy start liczy się od razu (trzy starty, przegląd 5–7 M-1).
        assert_eq!((trial.version.as_str(), trial.attempts), ("0.11.0", 1));
        assert!(trial.radio_was_up, "0.10.0 zbudowała stos nadawczy");

        // 3. Po 60 s zdrowej pracy: potwierdzenie, strażnik zamyka próbę.
        let healthy = HealthCell::default();
        healthy.set_radio(true, None);
        healthy.set_dispatcher_alive(true);
        assert_eq!(judge(Some(&trial), "0.11.0", &healthy, CONFIRM_AFTER), Verdict::Confirm);
        write_atomic_nofollow(&paths.confirmed(), b"0.11.0").unwrap();
        assert_eq!(guard(&paths), vec![GuardAction::Confirmed("0.11.0".to_owned())]);
        assert!(read_trial(&paths).is_none());

        // 4. Stacja 0.11.0 nadaje (stos z panelu) i dostaje 0.12.0.
        let pool = open(&paths.db_path).await;
        let kv = SettingsRepo::new(pool.clone());
        kv.kv_put("hw_state:origin", "panel").await.unwrap();
        kv.kv_put("test:marker", "przed 0.12.0").await.unwrap();
        let server = release_server(&key, "0.12.0", &release_archive("0.12.0")).await;
        handle(&paths, &server, &key, "0.11.0", &pool, Arc::new(NoStation))
            .install("0.12.0", "admin")
            .await
            .unwrap();
        pool.close().await;
        assert_eq!(guard(&paths), vec![GuardAction::Installed("0.12.0".to_owned())]);
        let trial = read_trial(&paths).expect("próba 0.12.0");
        assert!(trial.radio_was_up, "0.11.0 nadawała");

        // 0.12.0 pisze do bazy, ale nie buduje stosu nadawczego.
        let pool = open(&paths.db_path).await;
        SettingsRepo::new(pool.clone()).kv_put("test:marker", "po 0.12.0").await.unwrap();
        pool.close().await;
        let broken = HealthCell::default();
        broken.set_radio(false, Some("karta nie wstała".to_owned()));
        broken.set_dispatcher_alive(true);
        let Verdict::Bad(reason) = judge(Some(&trial), "0.12.0", &broken, CONFIRM_AFTER) else {
            panic!("wersja bez stosu nadawczego nie może być potwierdzona");
        };
        assert!(reason.starts_with("bad 0.12.0: nadawanie nie wstało"), "{reason}");
        write_atomic_nofollow(&paths.verdict(), reason.as_bytes()).unwrap();

        // Strażnik wraca do 0.11.0 z kopią bazy sprzed instalacji.
        assert_eq!(guard(&paths), vec![GuardAction::RolledBack("0.11.0".to_owned())]);
        assert_eq!(installed_version(&paths.current()).as_deref(), Some("0.11.0"));
        assert!(read_trial(&paths).is_none());
        let pool = open(&paths.db_path).await;
        assert_eq!(marker(&pool).await.as_deref(), Some("przed 0.12.0"), "baza z kopii");
        let last = audit_last_result(&paths, &pool).await.expect("wynik powrotu");
        assert_eq!(last.kind, ResultKind::RolledBack);
        let audits: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'update_rolled_back' AND user_id IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(audits, 1);
        assert!(audit_last_result(&paths, &pool).await.is_none(), "raz");
    }
}
