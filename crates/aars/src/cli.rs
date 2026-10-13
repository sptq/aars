use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "aars", about = "Automatic Alert Radio Station", version)]
pub struct Cli {
    /// Ścieżka do aars.toml. Pominięta — używane są domyślne wartości i AARS_*.
    #[arg(long, short, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Nakłada migracje na bazę.
    Migrate,
    /// Wypisuje efektywną konfigurację.
    Info,
    /// Syntezuje tekst i zapisuje do pliku WAV.
    Say {
        /// Treść komunikatu.
        text: String,
        /// Plik wyjściowy. Pominięty — audio ląduje w cache.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Tylko wypisz znormalizowany tekst, bez syntezy.
        #[arg(long)]
        dry_run: bool,
    },
    /// Nadaje komunikat natychmiast.
    Tx {
        /// Treść komunikatu.
        text: String,
        /// Priorytet: emergency | high | normal | low.
        #[arg(long, default_value = "normal")]
        priority: String,
        /// Pomiń identyfikację stacji (tylko do testów warsztatowych).
        #[arg(long)]
        no_id: bool,
        /// Sprawdź interlocki i złóż audio, ale nie wchodź na pasmo.
        #[arg(long)]
        dry_run: bool,
    },
}
