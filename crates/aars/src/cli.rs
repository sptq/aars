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
}
