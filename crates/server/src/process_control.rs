//! Process restart and shutdown-signal handling.
use std::ffi::OsString;
use std::process::Command;
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

pub(crate) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(err) => {
                warn!("Failed to install terminate signal handler: {}", err);
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(err) = tokio::signal::ctrl_c().await {
            warn!("Failed to listen for ctrl-c: {}", err);
        }
    }

    info!("Shutdown signal received.");
}
