mod cli;
mod probe;
mod radio;
mod render;
mod service;
mod setup;
mod station;
mod update;

use anyhow::{Context, Result};
use clap::Parser;

use aars_core::config::Config;
use aars_core::db;

fn parse_priority(value: &str) -> Result<aars_core::priority::Priority> {
    use aars_core::priority::Priority;
    match value {
        "emergency" => Ok(Priority::Emergency),
        "high" => Ok(Priority::High),
        "normal" => Ok(Priority::Normal),
        "low" => Ok(Priority::Low),
        other => anyhow::bail!("nieznany priorytet: {other}"),
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_env("AARS_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = cli::Cli::parse();
    let config = Config::load(cli.config.as_deref()).context("wczytywanie konfiguracji")?;

    match cli.command {
        cli::Command::Migrate => {
            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;
            tracing::info!(path = %config.storage.db_path.display(), "migracje nałożone");
        }
        cli::Command::Info => {
            // Z panelem, jeśli baza już istnieje: CLI pokazuje sprzęt, którym
            // stacja spróbuje nadawać jako pierwszym. Bez bazy — sam plik.
            let (config, origin) = if config.storage.db_path.exists() {
                match cli_candidate(&config).await {
                    Ok(cand) => (cand.config, cand.origin),
                    Err(err) => {
                        eprintln!("ustawienia sprzętu z panelu niedostępne ({err:#}), pokazuję plik");
                        (config, aars_core::hardware::ConfigOrigin::File)
                    }
                }
            } else {
                (config, aars_core::hardware::ConfigOrigin::File)
            };
            println!("sprzęt z:    {}", origin.label());
            println!("bind:        {}", config.server.bind);
            println!("baza:        {}", config.storage.db_path.display());
            println!("cache:       {}", config.storage.cache_dir.display());
            println!("audio:       {} @ {} Hz", config.audio.device, config.audio.sample_rate);
            println!("PTT:         {:?}", config.ptt.backend);
            println!("model głosu: {}", config.tts.model);
        }
        cli::Command::Say { text, out, dry_run } => {
            let spoken = aars_audio::text::normalize(&text);
            println!("{spoken}");

            if dry_run {
                return Ok(());
            }

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;

            let synth = aars_audio::piper::PiperSynth::new(
                &config.tts.piper_bin,
                &config.tts.model,
                config.audio.sample_rate,
            );
            let cache = aars_audio::cache::AudioCache::new(
                pool,
                config.storage.cache_dir.clone(),
                config.storage.cache_max_bytes,
            );

            let voice = aars_core::synth::VoiceParams::default();
            let (path, pcm) =
                cache.render(&synth, &text, &voice).await.context("synteza komunikatu")?;

            if let Some(out) = out {
                std::fs::copy(&path, &out).context("zapis pliku wyjściowego")?;
                println!("zapisano {} ({:?})", out.display(), pcm.duration());
            } else {
                println!("cache {} ({:?})", path.display(), pcm.duration());
            }

            cache.prune().await.context("czyszczenie cache")?;
        }
        cli::Command::Tx { text, priority, no_id, dry_run } => {
            use aars_core::interlock::Interlocks;

            let priority = parse_priority(&priority)?;

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;

            // Ten sam sprzęt, którym nadaje stacja (panel, jeśli jest).
            let config = station::first_candidate(&config, &pool).await?.config;
            let mut stack = radio::build_stack(&config, &pool, &station::RealHardware).await?;

            // Interlocki sprawdzamy tu, zanim wydamy czas na syntezę: odmowa
            // ma być szybka i czytelna, a sekwencer sprawdzi je i tak.
            if let Err(reason) = stack.interlocks.check(priority) {
                println!("odmowa nadania: {reason:?}");
                return Ok(());
            }

            let synth = aars_audio::piper::PiperSynth::new(
                &config.tts.piper_bin,
                &config.tts.model,
                config.audio.sample_rate,
            );
            let cache = aars_audio::cache::AudioCache::new(
                pool.clone(),
                config.storage.cache_dir.clone(),
                config.storage.cache_max_bytes,
            );
            let voice = aars_core::synth::VoiceParams::default();
            let (_, body) = cache.render(&synth, &text, &voice).await.context("synteza treści")?;

            let callsign: String =
                sqlx::query_scalar("SELECT value FROM settings WHERE key = 'callsign'")
                    .fetch_one(&pool)
                    .await
                    .context("odczyt znaku wywoławczego")?;

            let id_audio = if no_id || callsign.is_empty() {
                if !no_id {
                    tracing::warn!("znak wywoławczy nieustawiony — nadanie bez identyfikacji");
                }
                None
            } else {
                Some(aars_core::ident::morse_pcm(
                    &callsign,
                    &aars_core::ident::MorseParams::default(),
                    config.audio.sample_rate,
                ))
            };

            if dry_run {
                println!("gotowe do nadania: {:?} treści, priorytet {priority:?}", body.duration());
                return Ok(());
            }

            stack
                .sequencer
                .transmit(aars_core::sequencer::TransmitRequest {
                    id: 0,
                    priority,
                    interrupting: false,
                    body,
                    id_audio,
                })
                .await
                .map_err(anyhow::Error::msg)?;

            while let Some(event) = stack.sequencer.next_event().await {
                println!("{event:?}");
                if matches!(
                    event,
                    aars_core::sequencer::SeqEvent::Finished { .. }
                        | aars_core::sequencer::SeqEvent::Refused { .. }
                        | aars_core::sequencer::SeqEvent::Skipped { .. }
                ) {
                    break;
                }
            }

            stack.sequencer.shutdown();
            // Jednorazowe nadanie też musi zostawić ślad interwencji w bazie.
            if stack.watchdog.is_poisoned() && !radio::record_poison(&stack.watchdog.marker(), &stack.interlocks, &pool).await
            {
                eprintln!("UWAGA: watchdog interweniował, ale flagi nie udało się zapisać w bazie");
            }
            stack.watchdog.stop();
        }
        cli::Command::Queue { action } => {
            use aars_core::queue::{EnqueueRequest, QueueRepo};

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;
            let repo = QueueRepo::new(pool);

            match action {
                cli::QueueAction::Add { text, priority, interrupting, repeat, every } => {
                    let ids = repo
                        .enqueue(EnqueueRequest {
                            message_id: None,
                            rendered_text: text,
                            priority: parse_priority(&priority)?,
                            interrupting,
                            not_before: None,
                            expires_at: None,
                            repeat_count: repeat,
                            repeat_interval: std::time::Duration::from_secs(every),
                            created_by: None,
                        })
                        .await
                        .context("dodanie do kolejki")?;
                    println!("dodano zadania: {ids:?}");
                }
                cli::QueueAction::List => {
                    let items = repo.pending(50).await.context("odczyt kolejki")?;
                    if items.is_empty() {
                        println!("kolejka pusta");
                    }
                    for item in items {
                        println!(
                            "{:>5}  {:?}  {}  próba {}  {}{}",
                            item.id,
                            item.priority,
                            item.not_before.format("%Y-%m-%d %H:%M:%S"),
                            item.attempt,
                            if item.interrupting { "[przerywa] " } else { "" },
                            item.rendered_text
                        );
                    }
                }
                cli::QueueAction::Cancel { id, group } => match (id, group) {
                    (Some(id), _) => {
                        let count = repo.cancel(id).await.context("anulowanie zadania")?;
                        println!("anulowano {count}");
                    }
                    (_, Some(group)) => {
                        let count = repo.cancel_group(&group).await.context("anulowanie grupy")?;
                        println!("anulowano {count}");
                    }
                    (None, None) => anyhow::bail!("podaj --id albo --group"),
                },
            }
        }
        cli::Command::Source { action } => match action {
            cli::SourceAction::Preview { id } => source_preview(&config, &id).await?,
        },
        cli::Command::Run => {
            use aars_core::sink::CancelToken;
            use std::sync::Arc;

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;

            // R13: reset kont admina plikiem z partycji boot. Tylko w trybie
            // kreatora — przy haśle z konsoli nikt nie założyłby nowego admina.
            let reset_flag = setup::reset_flag_path(&config.storage.db_path);
            if config.setup.first_admin == aars_core::config::FirstAdmin::Wizard {
                match setup::reset_admins(&reset_flag, &pool).await {
                    Ok(Some(n)) => tracing::warn!(
                        disabled = n,
                        "reset kont admina z partycji boot — załóż konto w kreatorze /setup"
                    ),
                    Ok(None) => {}
                    Err(err) => tracing::error!("reset kont admina nie powiódł się: {err}"),
                }
            } else if reset_flag.exists() {
                tracing::warn!(
                    flag = %reset_flag.display(),
                    "prośba o reset admina pominięta: działa tylko z setup.first_admin = \"wizard\""
                );
            }

            // Jeden `stop` na cały proces: Ctrl-C i SIGTERM (`systemctl stop`)
            // kończą także stację, która właśnie czeka na koniec nadania
            // przed restartem. Bez SIGTERM systemd zabiłby proces, zanim
            // sekwencer opuści PTT (R10e).
            let stop = CancelToken::new();
            let on_signal = stop.clone();
            let signals = service::StopSignals::register().context("nasłuch SIGINT i SIGTERM")?;
            tokio::spawn(async move {
                signals.wait().await;
                tracing::info!("zatrzymuję stację");
                on_signal.cancel();
            });

            if service::under_systemd() {
                tracing::info!(
                    version = service::VERSION,
                    "praca pod systemd — powiadomienia włączone"
                );
            }
            let notifier: Arc<dyn service::Notifier> =
                Arc::new(service::ReadyOnce::new(service::SystemdNotifier));
            // Watchdog systemd tyka przez całe życie procesu, także podczas
            // drenażu przed restartem: wykrywa zatrzymany runtime, nie stan
            // nadawania (strażnik TX ma własny wątek).
            if let Some(interval) = service::watchdog_interval() {
                let notifier = notifier.clone();
                tokio::spawn(async move {
                    loop {
                        notifier.watchdog();
                        tokio::time::sleep(interval).await;
                    }
                });
            }

            let factory: Arc<dyn station::HardwareFactory> = Arc::new(station::RealHardware);
            let probe: Arc<dyn aars_core::hardware::probe::HardwareProbe> = Arc::new(probe::HostProbe);
            // Wynik instalacji albo powrotu z poprzedniego startu idzie do
            // audytu raz, zanim stacja wstanie (R18).
            let update_paths = update::paths::UpdatePaths::from_config(&config);
            if let Some(last) = update::health::audit_last_result(&update_paths, &pool).await {
                tracing::info!(
                    action = last.audit_action(),
                    version = %last.version,
                    "wynik aktualizacji zapisany w audycie"
                );
            }
            // Działające `aars run` trzyma blokadę, więc ręczny powrót
            // (`aars update rollback`) odmówi (przegląd 5–7 M-7).
            // Pod systemd brak blokady kończy start: bez niej ręczny powrót
            // nie wie, czy usługa działa (przegląd 8–9 M-5). Poza usługą
            // (praca deweloperska) tylko ostrzeżenie; `rollback` odmówi wtedy
            // bez `--force`.
            let _service_lock = match update::paths::hold_service_lock(&update_paths) {
                Ok(lock) => Some(lock),
                Err(err) if service::under_systemd() => {
                    anyhow::bail!("blokada usługi: {err}");
                }
                Err(err) => {
                    tracing::warn!(%err, "blokada usługi niedostępna (poza systemd)");
                    None
                }
            };
            let health = Arc::new(update::health::HealthCell::default());
            // R9: po 60 s od gotowości próba nowej wersji dostaje
            // potwierdzenie albo werdykt.
            tokio::spawn(update::health::confirm_task(
                update_paths,
                health.clone(),
                service::VERSION.to_owned(),
                stop.clone(),
            ));

            let mut updating = false;
            let run_result: Result<()> = async {
                loop {
                    match station::run_once(
                        &config,
                        &pool,
                        factory.clone(),
                        probe.clone(),
                        stop.clone(),
                        notifier.clone(),
                        health.clone(),
                    )
                    .await?
                    {
                        station::Exit::Restart => tracing::info!("restart stacji w procesie"),
                        station::Exit::Update => {
                            notifier.stopping();
                            tracing::info!(
                                "kończę proces — aktualizację zainstaluje strażnik przy starcie"
                            );
                            updating = true;
                            break;
                        }
                        // Werdykt `bad` po bieżącym nadaniu: błąd procesu niżej.
                        station::Exit::Unhealthy | station::Exit::Stop => break,
                    }
                }
                Ok(())
            }
            .await;
            // Żądanie przeżywa proces tylko przy końcu „aktualizacja”: przy
            // zatrzymaniu, werdykcie albo błędzie instalacja odpaliłaby się
            // przy przypadkowym starcie, bez admina (przegląd 8–9 I-1).
            if !updating {
                update::discard_request(
                    &update::paths::UpdatePaths::from_config(&config),
                    "proces kończy się bez aktualizacji",
                );
            }
            run_result?;
            if let Some(reason) = health.fatal() {
                anyhow::bail!("nowa wersja niezdrowa, strażnik wróci do poprzedniej: {reason}");
            }
        }
        cli::Command::Update { action } => update::cli(&config, action).await?,
        cli::Command::Hardware { action } => {
            use aars_core::hardware::HardwareRepo;

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;
            let repo = HardwareRepo::new(pool);
            match action {
                cli::HardwareAction::Show => {
                    let panel = repo.overrides().await.context("odczyt ustawień sprzętu")?;
                    let status = repo.apply_status().await.context("odczyt stanu zastosowania")?;
                    for line in station::hardware_show_lines(&config, &panel, status.as_ref()) {
                        println!("{line}");
                    }
                }
                cli::HardwareAction::Reset => {
                    let n = repo.clear().await.context("kasowanie ustawień sprzętu")?;
                    println!(
                        "usunięto {n} wpisów sprzętu (ustawienia z panelu, ostatnia działająca, stan zastosowania); przy następnym starcie obowiązuje aars.toml"
                    );
                }
            }
        }
        cli::Command::Message { action } => {
            use aars_core::message::{MessageRepo, NewMessage, ScheduleKind};

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;
            let repo = MessageRepo::new(pool);

            match action {
                cli::MessageAction::Add {
                    text,
                    title,
                    cron,
                    at,
                    priority,
                    interrupting,
                    repeat,
                    every,
                } => {
                    let schedule = match (cron, at) {
                        (Some(expr), _) => {
                            // Zepsuty wzorzec odrzucamy tutaj, nie w nocy przy
                            // pierwszym przebiegu harmonogramu.
                            <croner::Cron as std::str::FromStr>::from_str(&expr)
                                .with_context(|| format!("niepoprawny wzorzec cron: {expr}"))?;
                            ScheduleKind::Cron(expr)
                        }
                        (_, Some(when)) => {
                            let parsed = chrono::DateTime::parse_from_rfc3339(&when)
                                .with_context(|| format!("niepoprawna data: {when}"))?;
                            ScheduleKind::At(parsed.with_timezone(&chrono::Utc))
                        }
                        (None, None) => ScheduleKind::Immediate,
                    };

                    let id = repo
                        .create(NewMessage {
                            title,
                            body_template: text,
                            priority: parse_priority(&priority)?,
                            interrupting,
                            repeat_count: repeat,
                            repeat_interval: std::time::Duration::from_secs(every),
                            schedule,
                            valid_from: None,
                            valid_until: None,
                            source_id: None,
                            created_by: None,
                        })
                        .await
                        .context("zapis definicji komunikatu")?;
                    println!("dodano definicję {id}");
                }
                cli::MessageAction::List => {
                    let messages = repo.list().await.context("odczyt definicji")?;
                    if messages.is_empty() {
                        println!("brak definicji");
                    }
                    for message in messages {
                        let schedule = match &message.schedule {
                            ScheduleKind::Immediate => "na żądanie".to_owned(),
                            ScheduleKind::At(at) => at.format("%Y-%m-%d %H:%M:%S").to_string(),
                            ScheduleKind::Cron(expr) => expr.clone(),
                        };
                        println!(
                            "{:>4}  {:<9} {:<20} {:<16} {}{}",
                            message.id,
                            format!("{:?}", message.priority),
                            schedule,
                            message.title,
                            if message.enabled { "" } else { "[wyłączony] " },
                            message.body_template
                        );
                    }
                }
                cli::MessageAction::Enable { id } => {
                    repo.set_enabled(id, true).await.context("włączanie definicji")?;
                    println!("włączono {id}");
                }
                cli::MessageAction::Disable { id } => {
                    repo.set_enabled(id, false).await.context("wyłączanie definicji")?;
                    println!("wyłączono {id}");
                }
                cli::MessageAction::Remove { id } => {
                    let count = repo.delete(id).await.context("usuwanie definicji")?;
                    println!("usunięto {count}");
                }
            }
        }
    }

    Ok(())
}

/// Pierwszy kandydat konfiguracji dla CLI, z bazy, która już istnieje.
async fn cli_candidate(config: &Config) -> Result<station::Candidate> {
    let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
    db::migrate(&pool).await.context("nakładanie migracji")?;
    station::first_candidate(config, &pool).await
}

/// `aars source preview`: jedno pobranie, bez zapisu i bez kolejki.
async fn source_preview(config: &Config, id: &str) -> Result<()> {
    let pool = db::connect(&config.storage.db_path)
        .await
        .context("otwieranie bazy")?;
    db::migrate(&pool).await.context("nakładanie migracji")?;

    let module = aars_sources::modules()
        .into_iter()
        .find(|m| m.descriptor.id == id)
        .ok_or_else(|| anyhow::anyhow!("nieznane źródło: {id}"))?;
    let source = (module.build)(config).map_err(|e| anyhow::anyhow!("klient HTTP: {e}"))?;
    let needs = source.needs();
    // `SourceStateRepo::config` zamienia zepsuty obszar na pusty, a tu trzeba
    // je odróżnić, więc czytamy surowy zapis.
    let raw: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(format!("source:{}:area", source.id()))
        .fetch_optional(&pool)
        .await
        .context("odczyt obszaru źródła")?;
    let area = match aars_core::area::parse_area(raw.as_deref().unwrap_or_default()) {
        Ok(area) => area,
        Err(reason) => {
            println!("nieprawidłowy obszar: {reason}");
            return Ok(());
        }
    };
    if needs == aars_core::module::Needs::Area && area.is_empty() {
        println!("brak obszaru — ustaw go w panelu (/sources) albo w tabeli settings");
        return Ok(());
    }

    println!("obszar: {}", aars_core::area::area_text(&area));
    let position = if needs == aars_core::module::Needs::Position {
        let found = aars_core::geo::PositionRepo::new(pool.clone())
            .load()
            .await
            .context("odczyt pozycji stacji")?;
        if found.is_none() {
            println!("brak pozycji stacji — ustaw ją w panelu (/settings)");
            return Ok(());
        }
        found
    } else {
        None
    };
    let ctx = aars_sources::SourceCtx {
        now: chrono::Utc::now(),
        area_text: aars_core::area::area_text(&area),
        area,
        position,
        settings: std::collections::BTreeMap::new(),
        tracked_keys: Default::default(),
    };
    let output = source
        .fetch(&ctx)
        .await
        .map_err(|e| anyhow::anyhow!("pobieranie: {e}"))?;
    if output.items.is_empty() {
        println!("brak pozycji ze źródła");
    }
    for item in &output.items {
        println!(
            "[{}{}] {}",
            aars_web::chrome::priority_label(item.priority),
            if item.interrupting { ", przerywa" } else { "" },
            item.summary
        );
        println!("  tekst:        {}", item.text);
        println!(
            "  znormalizowany: {}",
            aars_audio::text::normalize(&item.text)
        );
    }
    if !output.complete {
        println!("UWAGA: część odpowiedzi była nieczytelna");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn a_source_that_cannot_start_is_recorded_as_failing() {
        let pool = aars_core::db::test_pool().await;
        let repo = aars_core::source_state::SourceStateRepo::new(pool);
        crate::station::source_start_failed(&repo, "imgw-meteo", "boom").await;

        let statuses = repo.statuses().await.unwrap();
        let status = statuses.iter().find(|s| s.source_id == "imgw-meteo").unwrap();
        assert_eq!(status.consecutive_failures, 1);
        assert!(status.last_error.as_deref().unwrap().contains("boom"));
        assert!(status.last_run_at.is_some());
    }
}
