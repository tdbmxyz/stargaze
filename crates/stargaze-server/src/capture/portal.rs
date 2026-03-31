use std::os::unix::io::OwnedFd;

use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use ashpd::enumflags2::BitFlags;
use stargaze_core::capture::CaptureError;
use tracing::debug;

/// Creates a portal screencast session and returns the `PipeWire` fd and node id.
///
/// This function:
/// 1. Opens a screencast portal session via D-Bus
/// 2. Requests a monitor source (no cursor overlay)
/// 3. Starts the session (may trigger a user confirmation dialog)
/// 4. Opens the `PipeWire` remote and returns the fd + node id
///
/// # Errors
///
/// Returns `CaptureError::PortalError` if any portal interaction fails
/// (D-Bus unavailable, user denied access, no monitors found).
pub async fn create_screencast_session() -> Result<(OwnedFd, u32), CaptureError> {
    let screencast = Screencast::new().await.map_err(|e| {
        CaptureError::PortalError(format!("failed to create screencast proxy: {e}"))
    })?;

    debug!("Creating portal screencast session");
    let session = screencast
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|e| CaptureError::PortalError(format!("failed to create session: {e}")))?;

    debug!("Selecting sources (monitor, no cursor)");
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Hidden)
                .set_sources(BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await
        .map_err(|e| CaptureError::PortalError(format!("failed to select sources: {e}")))?;

    debug!("Starting portal session");
    let response = screencast
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(|e| CaptureError::PortalError(format!("failed to start session: {e}")))?
        .response()
        .map_err(|e| CaptureError::PortalError(format!("portal start response error: {e}")))?;

    let stream = response
        .streams()
        .first()
        .ok_or_else(|| CaptureError::PortalError("no streams returned by portal".to_string()))?;

    let node_id = stream.pipe_wire_node_id();
    debug!(node_id, "Got PipeWire node from portal");

    let fd = screencast
        .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
        .await
        .map_err(|e| CaptureError::PortalError(format!("failed to open PipeWire remote: {e}")))?;

    Ok((fd, node_id))
}
