//! Proces pod systemd (etap 10, R9, R10e): powiadomienia `sd_notify`
//! (gotowość, watchdog, stan), sygnały zatrzymania i wersja produktu.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Wersja produktu (R17): `version` crate'a `aars`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Powiadomienia dla menedżera usług. Bez systemd nic nie robią.
pub trait Notifier: Send + Sync {
    /// `READY=1` razem z `STATUS=…`.
    fn ready(&self);
    /// `STATUS=…`.
    fn status(&self, text: &str);
    /// `WATCHDOG=1`.
    fn watchdog(&self);
    /// `STOPPING=1`.
    fn stopping(&self);
}

/// Prawdziwe powiadomienia przez gniazdo z `NOTIFY_SOCKET`. Bez tej
/// zmiennej `sd_notify::notify` nic nie wysyła.
pub struct SystemdNotifier;

/// Stan wysyłany razem z gotowością.
pub const READY_STATUS: &str = "stacja uruchomiona";

impl SystemdNotifier {
    fn send(states: &[sd_notify::NotifyState]) {
        if let Err(err) = sd_notify::notify(states) {
            // Powiadomienie to informacja dla systemd, nie warunek pracy stacji.
            tracing::warn!(%err, "powiadomienie systemd zawiodło");
        }
    }
}

impl Notifier for SystemdNotifier {
    fn ready(&self) {
        Self::send(&[
            sd_notify::NotifyState::Ready,
            sd_notify::NotifyState::Status(READY_STATUS),
        ]);
    }

    fn status(&self, text: &str) {
        Self::send(&[sd_notify::NotifyState::Status(text)]);
    }

    fn watchdog(&self) {
        Self::send(&[sd_notify::NotifyState::Watchdog]);
    }

    fn stopping(&self) {
        Self::send(&[sd_notify::NotifyState::Stopping]);
    }
}

/// `READY=1` raz na proces: przy restarcie w procesie systemd dostaje już
/// tylko `STATUS=restart w procesie`.
pub struct ReadyOnce<N> {
    inner: N,
    sent: AtomicBool,
}

impl<N> ReadyOnce<N> {
    pub fn new(inner: N) -> Self {
        Self {
            inner,
            sent: AtomicBool::new(false),
        }
    }
}

impl<N: Notifier> Notifier for ReadyOnce<N> {
    fn ready(&self) {
        if self.sent.swap(true, Ordering::SeqCst) {
            self.inner.status("restart w procesie");
        } else {
            self.inner.ready();
        }
    }

    fn status(&self, text: &str) {
        self.inner.status(text);
    }

    fn watchdog(&self) {
        self.inner.watchdog();
    }

    fn stopping(&self) {
        self.inner.stopping();
    }
}

impl<N: Notifier + ?Sized> Notifier for Arc<N> {
    fn ready(&self) {
        (**self).ready();
    }

    fn status(&self, text: &str) {
        (**self).status(text);
    }

    fn watchdog(&self) {
        (**self).watchdog();
    }

    fn stopping(&self) {
        (**self).stopping();
    }
}

/// Czy proces działa jako usługa systemd z powiadomieniami.
pub fn under_systemd() -> bool {
    std::env::var_os("NOTIFY_SOCKET").is_some()
}

/// Co ile tykać `WATCHDOG=1`: jedna trzecia `WatchdogSec` (20 s przy 60 s,
/// R9), żeby jedno spóźnione tyknięcie nie kończyło się zabiciem procesu.
/// `None`, gdy systemd nie włączył watchdoga.
pub fn watchdog_interval() -> Option<Duration> {
    sd_notify::watchdog_enabled()
        .filter(|d| !d.is_zero())
        .map(|d| d / 3)
}

/// Czeka na SIGINT (Ctrl-C) albo SIGTERM (`systemctl stop`). Oba kończą
/// stację tą samą drogą: nadanie przerwane, PTT opuszczone przez sekwencer,
/// ograniczony join (R10e).
pub async fn wait_for_stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if let Err(err) = result {
                            tracing::error!(%err, "oczekiwanie na Ctrl-C zawiodło");
                            // Bez SIGINT zostaje jeszcze SIGTERM.
                            term.recv().await;
                        }
                    }
                    _ = term.recv() => {}
                }
            }
            Err(err) => {
                tracing::error!(%err, "nasłuch SIGTERM zawiódł — zostaje tylko Ctrl-C");
                if let Err(err) = tokio::signal::ctrl_c().await {
                    tracing::error!(%err, "oczekiwanie na Ctrl-C zawiodło");
                    std::future::pending::<()>().await;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(%err, "oczekiwanie na Ctrl-C zawiodło");
            std::future::pending::<()>().await;
        }
    }
}

/// Zapisuje powiadomienia zamiast je wysyłać.
#[cfg(test)]
#[derive(Default)]
pub struct FakeNotifier {
    pub events: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
impl FakeNotifier {
    pub fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn push(&self, event: String) {
        self.events.lock().unwrap().push(event);
    }
}

#[cfg(test)]
impl Notifier for FakeNotifier {
    fn ready(&self) {
        self.push("READY=1".to_owned());
    }

    fn status(&self, text: &str) {
        self.push(format!("STATUS={text}"));
    }

    fn watchdog(&self) {
        self.push("WATCHDOG=1".to_owned());
    }

    fn stopping(&self) {
        self.push("STOPPING=1".to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nazwa testu-dziecka, uruchamianego w osobnym procesie z
    /// `NOTIFY_SOCKET`, żeby nie zmieniać środowiska innych testów.
    const CHILD: &str = "service::tests::child_sends_ready_and_watchdog";

    #[test]
    #[ignore = "uruchamiany tylko jako podproces testu gniazda"]
    fn child_sends_ready_and_watchdog() {
        assert!(under_systemd(), "dziecko musi dostać NOTIFY_SOCKET");
        SystemdNotifier.ready();
        SystemdNotifier.watchdog();
    }

    #[cfg(unix)]
    #[test]
    fn the_notifier_sends_ready_and_watchdog_to_the_socket() {
        use std::os::unix::net::UnixDatagram;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.sock");
        let socket = UnixDatagram::bind(&path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([CHILD, "--exact", "--ignored", "--test-threads=1"])
            .env("NOTIFY_SOCKET", &path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );

        let mut received = String::new();
        let mut buf = [0u8; 512];
        for _ in 0..2 {
            let n = socket.recv(&mut buf).unwrap();
            received.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(received.contains("READY=1\n"), "{received:?}");
        assert!(
            received.contains(&format!("STATUS={READY_STATUS}\n")),
            "{received:?}"
        );
        assert!(received.contains("WATCHDOG=1\n"), "{received:?}");
    }

    #[test]
    fn without_notify_socket_nothing_happens() {
        // Testy nie ustawiają NOTIFY_SOCKET w swoim procesie; uruchomione
        // spod usługi systemd nie mają czego sprawdzać.
        if under_systemd() {
            eprintln!("NOTIFY_SOCKET ustawione — test pominięty");
            return;
        }
        assert_eq!(watchdog_interval(), None);
        SystemdNotifier.ready();
        SystemdNotifier.status("x");
        SystemdNotifier.watchdog();
        SystemdNotifier.stopping();
    }

    #[test]
    fn ready_goes_out_once_then_only_the_restart_status() {
        let once = ReadyOnce::new(FakeNotifier::default());
        once.ready();
        once.ready();
        assert_eq!(
            once.inner.events(),
            vec!["READY=1".to_owned(), "STATUS=restart w procesie".to_owned()]
        );
    }
}
