//! Process restart and shutdown-signal handling.
use crate::shutdown::Shutdown;
use std::ffi::OsString;
use std::process::Command;
use std::time::Duration;
use tracing::{info, warn};

pub fn spawn_restart() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut command = Command::new(exe);
    command.env("PHONOLITE_START_DELAY_MS", "1000").args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        return Err(command.exec().to_string());
    }
    #[cfg(not(unix))]
    {
        command.spawn().map_err(|err| err.to_string())?;
        Ok(())
    }
}

/// Allow the small administration response to leave before requesting the
/// common lifecycle. The action itself is executed by main after draining.
pub(crate) fn after_response(shutdown: Shutdown, restart: bool) {
    let monitor = shutdown.clone();
    shutdown.spawn_graceful_named("process-response", async move {
        tokio::select! {
            biased;
            _ = monitor.cancelled() => {},
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                if restart {
                    monitor.request_restart();
                } else {
                    monitor.request_stop();
                }
            }
        }
    });
}

/// The signal monitor is owned by main, outside the work tracker. It must keep
/// receiving an explicit stop while an administration restart is draining.
pub(crate) struct SignalListener(tokio::task::JoinHandle<()>);

impl Drop for SignalListener {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn monitor_signal(
    shutdown: Shutdown,
    signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> SignalListener {
    SignalListener(tokio::spawn(async move {
        signal.await;
        info!("Shutdown signal received; stopping listeners and background work.");
        shutdown.request_stop();
    }))
}

pub(crate) fn listen_for_shutdown(shutdown: Shutdown) -> std::io::Result<SignalListener> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        return Ok(monitor_signal(shutdown, async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = term.recv() => {},
            }
        }));
    }

    #[cfg(not(unix))]
    {
        Ok(monitor_signal(shutdown, async move {
            if let Err(err) = tokio::signal::ctrl_c().await {
                warn!("Failed to listen for ctrl-c: {}", err);
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restart_uses_the_shared_stop_and_does_not_launch_a_process() {
        let shutdown = Shutdown::new();
        after_response(shutdown.clone(), true);
        assert!(!shutdown.is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), shutdown.cancelled())
            .await
            .unwrap();
        assert!(shutdown.restart_requested());
        assert!(shutdown.drain(Duration::from_secs(1)).await);
    }

    #[tokio::test]
    async fn a_system_stop_cancels_a_pending_administration_restart() {
        let shutdown = Shutdown::new();
        after_response(shutdown.clone(), true);
        shutdown.request_stop();
        assert!(shutdown.drain(Duration::from_secs(1)).await);
        assert!(!shutdown.restart_requested());
    }

    #[tokio::test]
    async fn the_owned_signal_monitor_can_override_restart_during_drain() {
        let shutdown = Shutdown::new();
        let (signal, received) = tokio::sync::oneshot::channel();
        let mut listener = monitor_signal(shutdown.clone(), async move {
            let _ = received.await;
        });
        shutdown.request_restart();
        assert!(shutdown.drain(Duration::from_secs(1)).await);
        assert!(shutdown.restart_requested());
        signal.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), &mut listener.0)
            .await
            .unwrap()
            .unwrap();
        assert!(!shutdown.restart_requested());
    }
}
