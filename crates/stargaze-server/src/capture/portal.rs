use std::os::unix::io::OwnedFd;
use std::path::PathBuf;

use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use ashpd::enumflags2::BitFlags;
use stargaze_core::capture::CaptureError;
use tracing::{debug, info, warn};

/// Returns the path where the screencast restore token is persisted,
/// e.g. `~/.local/state/stargaze/screencast-restore-token` on Linux.
fn restore_token_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "stargaze")?;
    let dir = dirs.state_dir().unwrap_or_else(|| dirs.data_local_dir());
    Some(dir.join("screencast-restore-token"))
}

/// Loads the restore token persisted by a previous session, if any.
fn load_restore_token() -> Option<String> {
    let path = restore_token_path()?;
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Persists the restore token for the next session.
///
/// Tokens are single-use: the portal returns a fresh one on every start,
/// so this must be called after each successful `start`.
fn save_restore_token(token: &str) {
    let Some(path) = restore_token_path() else {
        warn!("Could not determine state directory, screencast token not persisted");
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        warn!("Failed to create {}: {e}", parent.display());
        return;
    }
    match std::fs::write(&path, token) {
        Ok(()) => debug!(path = %path.display(), "Saved screencast restore token"),
        Err(e) => warn!(
            "Failed to save screencast restore token to {}: {e}",
            path.display()
        ),
    }
}

/// Creates a portal screencast session and returns the `PipeWire` fd and node id.
///
/// This function:
/// 1. Opens a screencast portal session via D-Bus
/// 2. Requests a monitor source with the specified cursor mode, restoring
///    the previous grant via a persisted restore token when available
/// 3. Starts the session (triggers a user confirmation dialog only on the
///    first run, or if the persisted token was revoked/expired)
/// 4. Opens the `PipeWire` remote and returns the fd + node id
///
/// # Arguments
///
/// * `show_cursor` - When `true`, the compositor embeds the cursor into
///   captured frames (`CursorMode::Embedded`). When `false`, the cursor
///   is excluded (`CursorMode::Hidden`).
///
/// # Errors
///
/// Returns `CaptureError::PortalError` if any portal interaction fails
/// (D-Bus unavailable, user denied access, no monitors found).
pub async fn create_screencast_session(show_cursor: bool) -> Result<(OwnedFd, u32), CaptureError> {
    let screencast = Screencast::new().await.map_err(|e| {
        CaptureError::PortalError(format!("failed to create screencast proxy: {e}"))
    })?;

    debug!("Creating portal screencast session");
    let session = screencast
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|e| CaptureError::PortalError(format!("failed to create session: {e}")))?;

    let cursor_mode: Option<CursorMode> = if show_cursor {
        let available = screencast.available_cursor_modes().await.ok();

        if available.is_some_and(|m| m.contains(CursorMode::Embedded)) {
            Some(CursorMode::Embedded)
        } else if available.is_some_and(|m| m.contains(CursorMode::Metadata)) {
            debug!("Embedded cursor unavailable, falling back to Metadata");
            Some(CursorMode::Metadata)
        } else {
            debug!("Could not determine available cursor modes, using portal default");
            None
        }
    } else {
        let available = screencast.available_cursor_modes().await.ok();

        if available.is_some_and(|m| m.contains(CursorMode::Hidden)) {
            Some(CursorMode::Hidden)
        } else {
            debug!("Hidden cursor mode unavailable, using portal default");
            None
        }
    };

    // Restore the previous grant if we have a token, and ask the portal to
    // persist the new grant until explicitly revoked. With a valid token the
    // compositor skips the source-picker dialog entirely; with a missing or
    // stale token it falls back to showing the dialog once.
    let restore_token = load_restore_token();
    if restore_token.is_some() {
        debug!("Restoring previous screencast session (no dialog expected)");
    } else {
        info!("No screencast restore token found, the portal may show a source-picker dialog");
    }

    debug!(?cursor_mode, "Selecting sources (monitor)");
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor_mode)
                .set_sources(BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_restore_token(restore_token.as_deref())
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
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

    // The portal hands back a fresh single-use token on every start; persist
    // it so the next launch can skip the dialog.
    if let Some(token) = response.restore_token() {
        save_restore_token(token);
    }

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
