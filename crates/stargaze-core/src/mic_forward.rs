use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::config::MicForwardConfig;

const STARGAZE_FIFO_PATH: &str = "/tmp/stargaze_mic_pipe";
const STARGAZE_MIC_NAME: &str = "stargaze_virtual_microphone";

/// Spawns `rsonance receiver` on the server to create a virtual microphone.
///
/// # Errors
///
/// Returns an error if the rsonance binary cannot be started.
pub fn spawn_rsonance_receiver(config: &MicForwardConfig) -> anyhow::Result<Child> {
    info!(
        "Spawning rsonance receiver on port {} (binary: {})",
        config.port, config.rsonance_binary
    );

    let child = Command::new(&config.rsonance_binary)
        .arg("receiver")
        .arg("--host")
        .arg("0.0.0.0")
        .arg("--port")
        .arg(config.port.to_string())
        .arg("--fifo-path")
        .arg(STARGAZE_FIFO_PATH)
        .arg("--microphone-name")
        .arg(STARGAZE_MIC_NAME)
        .kill_on_drop(true)
        .spawn()?;

    info!("Rsonance receiver started (pid: {:?})", child.id());
    Ok(child)
}

/// Spawns `rsonance transmitter` on the client to stream mic audio to the server.
///
/// # Errors
///
/// Returns an error if the rsonance binary cannot be started.
pub fn spawn_rsonance_transmitter(
    config: &MicForwardConfig,
    server_address: &str,
) -> anyhow::Result<Child> {
    info!(
        "Spawning rsonance transmitter to {}:{} (binary: {})",
        server_address, config.port, config.rsonance_binary
    );

    let child = Command::new(&config.rsonance_binary)
        .arg("transmitter")
        .arg("--host")
        .arg(server_address)
        .arg("--port")
        .arg(config.port.to_string())
        .kill_on_drop(true)
        .spawn()?;

    info!("Rsonance transmitter started (pid: {:?})", child.id());
    Ok(child)
}

/// Stops a running rsonance subprocess.
///
/// Sends SIGKILL and waits for the process to exit. If the process
/// has already exited, this is a no-op.
pub async fn stop_rsonance(child: &mut Child) {
    let pid = child.id();
    match child.kill().await {
        Ok(()) => {
            info!("Rsonance process (pid: {pid:?}) killed");
        }
        Err(e) => {
            warn!("Failed to kill rsonance process (pid: {pid:?}): {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spawn_receiver_nonexistent_binary() {
        let config = MicForwardConfig {
            enabled: true,
            port: 9001,
            rsonance_binary: "/nonexistent/rsonance_fake_binary".to_string(),
        };
        let result = spawn_rsonance_receiver(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_spawn_transmitter_nonexistent_binary() {
        let config = MicForwardConfig {
            enabled: true,
            port: 9001,
            rsonance_binary: "/nonexistent/rsonance_fake_binary".to_string(),
        };
        let result = spawn_rsonance_transmitter(&config, "127.0.0.1");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_stop_rsonance_already_exited() {
        // Spawn a process that exits immediately.
        let mut child = Command::new("true")
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn 'true'");
        // Wait for it to exit.
        let _ = child.wait().await;
        // stop_rsonance should not panic on an already-exited process.
        stop_rsonance(&mut child).await;
    }

    #[tokio::test]
    #[ignore = "requires rsonance binary and PulseAudio"]
    async fn test_spawn_and_stop_receiver() {
        let config = MicForwardConfig::default();
        let mut child = spawn_rsonance_receiver(&config).expect("should spawn rsonance receiver");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        stop_rsonance(&mut child).await;
    }

    #[tokio::test]
    #[ignore = "requires rsonance binary and microphone"]
    async fn test_spawn_and_stop_transmitter() {
        let config = MicForwardConfig::default();
        let mut child = spawn_rsonance_transmitter(&config, "127.0.0.1")
            .expect("should spawn rsonance transmitter");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        stop_rsonance(&mut child).await;
    }
}
