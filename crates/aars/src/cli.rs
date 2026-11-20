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
    /// Operacje na kolejce nadań.
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
    /// Definicje komunikatów i ich harmonogram.
    Message {
        #[command(subcommand)]
        action: MessageAction,
    },
    /// Uruchamia stację: harmonogram i dyspozytor pracują aż do przerwania.
    Run,
}

#[derive(Debug, Subcommand)]
pub enum MessageAction {
    /// Dodaje definicję komunikatu.
    Add {
        /// Treść komunikatu.
        text: String,
        #[arg(long)]
        title: String,
        /// Wzorzec cron, np. "0 18 * * *".
        #[arg(long, group = "schedule")]
        cron: Option<String>,
        /// Jednorazowo o zadanej chwili (RFC 3339).
        #[arg(long, group = "schedule")]
        at: Option<String>,
        #[arg(long, default_value = "normal")]
        priority: String,
        #[arg(long)]
        interrupting: bool,
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        #[arg(long, default_value_t = 600)]
        every: u64,
    },
    /// Wypisuje definicje komunikatów.
    List,
    /// Włącza definicję.
    Enable { id: i64 },
    /// Wyłącza definicję.
    Disable { id: i64 },
    /// Usuwa definicję.
    Remove { id: i64 },
}

#[derive(Debug, Subcommand)]
pub enum QueueAction {
    /// Dodaje komunikat do kolejki.
    Add {
        text: String,
        #[arg(long, default_value = "normal")]
        priority: String,
        /// Przerywa trwające nadanie o niższym priorytecie.
        #[arg(long)]
        interrupting: bool,
        /// Ile razy nadać.
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        /// Odstęp między powtórzeniami w sekundach.
        #[arg(long, default_value_t = 600)]
        every: u64,
    },
    /// Wypisuje zadania czekające w kolejce.
    List,
    /// Anuluje zadanie albo całą grupę powtórzeń.
    Cancel {
        #[arg(long, group = "target")]
        id: Option<i64>,
        #[arg(long, group = "target")]
        group: Option<String>,
    },
}
