mod cli;

use anyhow::{Context, Result};
use clap::Parser;

use aars_core::config::Config;
use aars_core::db;

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
    }

    Ok(())
}
