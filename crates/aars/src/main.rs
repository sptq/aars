mod cli;
mod radio;
mod render;

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

            let mut stack = radio::build_stack(&config, &pool).await?;

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
                ) {
                    break;
                }
            }

            stack.sequencer.shutdown();
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
        cli::Command::Run => {
            use aars_core::dispatcher::{Dispatcher, DispatcherConfig};
            use aars_core::ident::IdMode;
            use aars_core::queue::QueueRepo;
            use aars_core::sink::CancelToken;
            use std::sync::Arc;

            let pool = db::connect(&config.storage.db_path).await.context("otwieranie bazy")?;
            db::migrate(&pool).await.context("nakładanie migracji")?;

            let stack = radio::build_stack(&config, &pool).await?;

            let callsign: String =
                sqlx::query_scalar("SELECT value FROM settings WHERE key = 'callsign'")
                    .fetch_one(&pool)
                    .await
                    .context("odczyt znaku wywoławczego")?;
            let id_mode: String =
                sqlx::query_scalar("SELECT value FROM settings WHERE key = 'id_mode'")
                    .fetch_one(&pool)
                    .await
                    .context("odczyt trybu identyfikacji")?;
            let id_mode = if id_mode == "cw" { IdMode::Cw } else { IdMode::Voice };

            let synth: Arc<dyn aars_core::synth::Synthesizer> =
                Arc::new(aars_audio::piper::PiperSynth::new(
                    &config.tts.piper_bin,
                    &config.tts.model,
                    config.audio.sample_rate,
                ));
            let cache = aars_audio::cache::AudioCache::new(
                pool.clone(),
                config.storage.cache_dir.clone(),
                config.storage.cache_max_bytes,
            );
            let renderer = Arc::new(render::PiperRenderer::new(
                cache,
                synth,
                &callsign,
                id_mode,
                config.audio.sample_rate,
            ));

            let shutdown = CancelToken::new();
            let dispatcher = Dispatcher::new(
                Arc::new(QueueRepo::new(pool)),
                renderer,
                stack.sequencer,
                DispatcherConfig { tick: std::time::Duration::from_millis(500) },
            );

            let task = tokio::spawn(dispatcher.run(shutdown.clone()));
            tracing::info!("stacja uruchomiona, Ctrl-C kończy");

            tokio::signal::ctrl_c().await.context("oczekiwanie na sygnał")?;
            tracing::info!("zatrzymuję stację");
            shutdown.cancel();
            let _ = task.await;
            stack.watchdog.stop();
        }
    }

    Ok(())
}
