//! Stacja w binarce (etap 9): kandydaci konfiguracji sprzętu z powrotem do
//! ostatniej działającej (R5), praca bez nadawania (R6) i restart w procesie
//! (R4), który nigdy nie przerywa nadania.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use tokio::sync::{Notify, oneshot};

use aars_core::config::{AudioConfig, Config, PttBackendKind, PttConfig};
use aars_core::dispatcher::{
    Dispatcher, DispatcherConfig, DispatcherControl, StationTest, TestOutcome,
};
use aars_core::hardware::probe::HardwareProbe;
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
        Arc::new(aars_audio::piper::PiperSynth::new(
            &config.tts.piper_bin,
            &config.tts.model,
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
    Stop,
}

/// Uchwyt panelu do działającej stacji (`StationControl`).
pub struct StationHandle {
    control: Option<DispatcherControl>,
    restart: CancelToken,
    status: ApplyStatus,
    hardware: Option<HardwareSettings>,
    station: StationSettings,
    tx_tail: Duration,
}

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
                    Duration::from_secs(*seconds) + self.tx_tail * 2 + TEST_PTT_MARGIN
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

/// Jak często pętla stacji sprawdza `stop` i żądanie restartu.
const CONTROL_POLL: Duration = Duration::from_millis(50);

/// Jeden przebieg stacji: od zbudowania stosu do zatrzymania albo restartu.
pub async fn run_once(
    base: &Config,
    pool: &SqlitePool,
    factory: Arc<dyn HardwareFactory>,
    probe: Arc<dyn HardwareProbe>,
    stop: CancelToken,
) -> Result<Exit> {
    run_station(base, pool, factory, probe, stop, None).await
}

async fn run_station(
    base: &Config,
    pool: &SqlitePool,
    factory: Arc<dyn HardwareFactory>,
    // Panel sprzętu dostaje go w Task 8 (`AppState::with_probe`).
    _probe: Arc<dyn HardwareProbe>,
    stop: CancelToken,
    ready: Option<oneshot::Sender<Arc<StationHandle>>>,
) -> Result<Exit> {
    let sources = aars_sources::modules();
    let mut descriptors = aars_core::module::builtin();
    descriptors.extend(sources.iter().map(|m| m.descriptor));
    let registry = ModuleRegistry::new(descriptors);
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
    let (stack, config, origin) = match built {
        Some((stack, cand)) => (Some(stack), cand.config, cand.origin),
        None => {
            tracing::error!("żadna konfiguracja sprzętu nie wstała — stacja działa bez nadawania");
            (None, first.config, ConfigOrigin::None)
        }
    };

    // Panel: `bind` kandydata, a przy błędzie `bind` z pliku (R6).
    let listener = match tokio::net::TcpListener::bind(&config.server.bind).await {
        Ok(listener) => listener,
        Err(err) if config.server.bind != base.server.bind => {
            let note = format!(
                "nasłuch na {}: {err} — panel wrócił na {} z pliku",
                config.server.bind, base.server.bind
            );
            tracing::error!(%note);
            notes.push(note);
            tokio::net::TcpListener::bind(&base.server.bind)
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
    ));
    let queue = Arc::new(QueueRepo::new(pool.clone()));

    let shutdown = CancelToken::new();
    let drain = CancelToken::new();
    let restart = CancelToken::new();

    let users = aars_core::user::UserRepo::new(pool.clone());
    if let Some(password) = users
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

    // Źródła są odizolowane: błąd budowy jednego wyłącza tylko je.
    let mut runners = Vec::new();
    let mut refreshers: Vec<(&'static str, Arc<Notify>)> = Vec::new();
    for module in &sources {
        let id = module.descriptor.id;
        match (module.build)(&config) {
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
    if let Some(RadioStack {
        sequencer,
        watchdog: wd,
        interlocks: locks,
    }) = stack
    {
        let renderer = Arc::new(render::PiperRenderer::new(
            cache.clone(),
            synth.clone(),
            &station.callsign,
            station.id_mode,
            config.audio.sample_rate,
            config.audio.gain_db,
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

    let mut state = aars_web::AppState::new(pool.clone(), queue.clone(), interlocks.clone(), false)
        .with_preview(preview)
        .with_hardware(vec![
            (
                "Pochodzenie konfiguracji".to_owned(),
                status.origin.label().to_owned(),
            ),
            ("Urządzenie audio".to_owned(), config.audio.device.clone()),
            (
                "Częstotliwość próbkowania".to_owned(),
                format!("{} Hz", config.audio.sample_rate),
            ),
            (
                "Backend PTT".to_owned(),
                config.ptt.backend.label().to_owned(),
            ),
            ("Model głosu".to_owned(), config.tts.model.clone()),
        ]);
    if let Some(marker) = watchdog_marker {
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
    let app = aars_web::router(state);

    let mut tasks = vec![
        tokio::spawn(radio::quiet_hours_task(
            pool.clone(),
            interlocks.clone(),
            shutdown.clone(),
        )),
        tokio::spawn(scheduler.run(shutdown.clone())),
    ];
    tasks.extend(poison_task);
    tasks.extend(
        runners
            .into_iter()
            .map(|runner| tokio::spawn(runner.run(shutdown.clone()))),
    );
    let server_shutdown = shutdown.clone();
    tasks.push(tokio::spawn(async move {
        let graceful = axum::serve(listener, app).with_graceful_shutdown(async move {
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
    if let Some(ready) = ready {
        let _ = ready.send(handle.clone());
    }

    let exit = loop {
        if stop.is_cancelled() {
            break Exit::Stop;
        }
        if restart.is_cancelled() {
            break Exit::Restart;
        }
        tokio::time::sleep(CONTROL_POLL).await;
    };

    if let (Exit::Restart, Some(task)) = (&exit, dispatcher_task.as_mut()) {
        // R4: dyspozytor nie bierze nic nowego, dokańcza nadanie i robi join
        // wątku sekwencera. Strażnik i most działają dalej. Ctrl-C w tym
        // czasie przerywa nadanie jak zwykle.
        tracing::info!("restart: czekam na koniec bieżącego nadania");
        drain.cancel();
        loop {
            tokio::select! {
                _ = &mut *task => break,
                () = tokio::time::sleep(CONTROL_POLL) => {
                    if stop.is_cancelled() {
                        shutdown.cancel();
                    }
                }
            }
        }
        dispatcher_task = None;
    }

    tracing::info!("zatrzymuję zadania stacji");
    shutdown.cancel();
    if let Some(task) = dispatcher_task {
        let _ = task.await;
    }
    for task in tasks {
        let _ = task.await;
    }
    // Po joinie sekwencera: PTT już opuszczone, strażnik nie ma czego pilnować.
    if let Some(watchdog) = watchdog {
        watchdog.stop();
    }
    drop(handle);

    Ok(if stop.is_cancelled() {
        Exit::Stop
    } else {
        exit
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use aars_core::priority::Priority;
    use aars_core::ptt::{FakePtt, PttEvent};
    use aars_core::sink::FakeSink;
    use aars_core::synth::FakeSynth;

    struct FakeHardware {
        fail_devices: Vec<String>,
        seen: Mutex<Vec<(PttBackendKind, String)>>,
        ptt: Arc<FakePtt>,
        last_backend: Mutex<Option<PttBackendKind>>,
    }

    impl FakeHardware {
        fn new(fail_devices: &[&str]) -> Self {
            Self {
                fail_devices: fail_devices.iter().map(|d| (*d).to_owned()).collect(),
                seen: Mutex::new(Vec::new()),
                ptt: Arc::new(FakePtt::new()),
                last_backend: Mutex::new(None),
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
        config
    }

    async fn start(
        config: Config,
        pool: SqlitePool,
        hardware: Arc<FakeHardware>,
        stop: CancelToken,
    ) -> (tokio::task::JoinHandle<Result<Exit>>, Arc<StationHandle>) {
        let (ready_tx, ready_rx) = oneshot::channel();
        let probe: Arc<dyn HardwareProbe> = Arc::new(aars_core::hardware::probe::FakeProbe {
            audio: Ok(vec![]),
            serial: Ok(vec![]),
            gpio: Ok(vec![]),
        });
        let task = tokio::spawn(async move {
            run_station(&config, &pool, hardware, probe, stop, Some(ready_tx)).await
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
}
