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
    /// Źródła danych (ostrzeżenia IMGW).
    Source {
        #[command(subcommand)]
        action: SourceAction,
    },
    /// Uruchamia stację: harmonogram i dyspozytor pracują aż do przerwania.
    Run,
    /// Ustawienia sprzętu z panelu: podgląd i ratunek bez panelu.
    Hardware {
        #[command(subcommand)]
        action: HardwareAction,
    },
    /// Aktualizacja z wydań GitHub: stan, strażnik startu, powrót.
    Update {
        #[command(subcommand)]
        action: UpdateAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum UpdateAction {
    /// Wersja bieżąca i poprzednia, trwająca próba i ostatni wynik.
    Status,
    /// Strażnik startu (root, z `ExecStartPre` usługi); zawsze kończy się kodem 0.
    Guard,
    /// Ręczny powrót do poprzedniej wersji razem z kopią bazy (root).
    Rollback {
        /// Powrót mimo nieznanego stanu usługi (brak pliku blokady). Tylko
        /// po sprawdzeniu, że `systemctl is-active aars` mówi `inactive`.
        #[arg(long)]
        force: bool,
    },
    /// Sprawdza najnowsze wydanie; niczego nie pobiera poza manifestem.
    Check,
}

#[derive(Debug, Subcommand)]
pub enum HardwareAction {
    /// Konfiguracja efektywna z pochodzeniem każdego klucza i stan zastosowania.
    Show,
    /// Kasuje ustawienia sprzętu z panelu; przy następnym starcie obowiązuje aars.toml.
    Reset,
}

#[derive(Debug, Subcommand)]
pub enum SourceAction {
    /// Jedno pobranie z obszarem z bazy; nic nie zapisuje ani nie nadaje.
    Preview,
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
        #[arg(long, default_value_t = 600,
              value_parser = clap::value_parser!(u64).range(0..=aars_core::message::MAX_REPEAT_INTERVAL_S))]
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
        #[arg(long, default_value_t = 600,
              value_parser = clap::value_parser!(u64).range(0..=aars_core::message::MAX_REPEAT_INTERVAL_S))]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeat_interval_over_a_day_is_refused_by_the_cli() {
        assert!(Cli::try_parse_from(["aars", "queue", "add", "x", "--every", "86401"]).is_err());
        assert!(
            Cli::try_parse_from(["aars", "message", "add", "x", "--title", "t", "--every", "86401"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["aars", "queue", "add", "x", "--every", "86400"]).is_ok());
    }
}
