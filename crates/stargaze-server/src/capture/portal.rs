use std::future::Future;
use std::os::unix::io::OwnedFd;
use std::path::PathBuf;
use std::time::Duration;

use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use ashpd::enumflags2::BitFlags;
use stargaze_core::capture::CaptureError;
use tracing::{debug, info, warn};

/// Non-interactive portal requests answer within moments on a healthy
/// stack; longer silence means `xdg-desktop-portal` or its compositor
/// backend is absent or wedged, so fail fast instead of hanging the
/// server forever with no output.
const PORTAL_REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// Interactive stages (source selection, start) may legitimately sit
/// waiting for someone to approve the portal's picker dialog.
const PORTAL_DIALOG_TIMEOUT: Duration = Duration::from_secs(180);
/// While an interactive stage is pending, remind at this interval that
/// a dialog may be waiting on the server's display.
const PORTAL_DIALOG_WARN_EVERY: Duration = Duration::from_secs(15);

/// Awaits a portal reply that never involves user interaction, mapping
/// both portal errors and unresponsiveness to a clear `CaptureError`.
async fn portal_reply<T>(
    stage: &str,
    fut: impl Future<Output = ashpd::Result<T>>,
) -> Result<T, CaptureError> {
    match tokio::time::timeout(PORTAL_REPLY_TIMEOUT, fut).await {
        Ok(result) => result.map_err(|e| CaptureError::PortalError(format!("{stage} failed: {e}"))),
        Err(_) => Err(CaptureError::PortalError(format!(
            "{stage} timed out after {}s: xdg-desktop-portal (or its compositor \
             backend, e.g. xdg-desktop-portal-hyprland) is not answering; check \
             `systemctl --user status xdg-desktop-portal*` and restart the portal \
             services",
            PORTAL_REPLY_TIMEOUT.as_secs()
        ))),
    }
}

/// Awaits a portal request that may show an approval dialog: warns
/// periodically while pending (the dialog may be sitting on a display
/// nobody is watching) and gives up after [`PORTAL_DIALOG_TIMEOUT`].
async fn portal_dialog<T>(
    stage: &str,
    fut: impl Future<Output = ashpd::Result<T>>,
) -> Result<T, CaptureError> {
    let mut fut = std::pin::pin!(fut);
    let started = tokio::time::Instant::now();
    loop {
        match tokio::time::timeout(PORTAL_DIALOG_WARN_EVERY, &mut fut).await {
            Ok(result) => {
                return result
                    .map_err(|e| CaptureError::PortalError(format!("{stage} failed: {e}")));
            }
            Err(_) if started.elapsed() >= PORTAL_DIALOG_TIMEOUT => {
                return Err(CaptureError::PortalError(format!(
                    "{stage} timed out after {}s: most likely an approval dialog is \
                     waiting on the server's display. On a headless host, configure \
                     an auto-approving picker — see docs/headless-screencast.md \
                     (xdg-desktop-portal-hyprland screencopy:custom_picker_binary)",
                    PORTAL_DIALOG_TIMEOUT.as_secs()
                )));
            }
            Err(_) => warn!(
                "Still waiting for {stage} after {}s — an approval dialog may be \
                 waiting on the server's display",
                started.elapsed().as_secs()
            ),
        }
    }
}

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
/// (D-Bus unavailable, user denied access, no monitors found) or does
/// not answer in time (portal service wedged, unattended dialog).
pub async fn create_screencast_session(show_cursor: bool) -> Result<(OwnedFd, u32), CaptureError> {
    let screencast = portal_reply("creating the screencast proxy", Screencast::new()).await?;

    debug!("Creating portal screencast session");
    let session = portal_reply(
        "creating the portal session",
        screencast.create_session(CreateSessionOptions::default()),
    )
    .await?;

    let available = portal_reply(
        "querying available cursor modes",
        screencast.available_cursor_modes(),
    )
    .await
    .ok();
    let cursor_mode: Option<CursorMode> = if show_cursor {
        if available.is_some_and(|m| m.contains(CursorMode::Embedded)) {
            Some(CursorMode::Embedded)
        } else if available.is_some_and(|m| m.contains(CursorMode::Metadata)) {
            debug!("Embedded cursor unavailable, falling back to Metadata");
            Some(CursorMode::Metadata)
        } else {
            debug!("Could not determine available cursor modes, using portal default");
            None
        }
    } else if available.is_some_and(|m| m.contains(CursorMode::Hidden)) {
        Some(CursorMode::Hidden)
    } else {
        debug!("Hidden cursor mode unavailable, using portal default");
        None
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
    portal_dialog(
        "portal source selection",
        screencast.select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor_mode)
                .set_sources(BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_restore_token(restore_token.as_deref())
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        ),
    )
    .await?;

    debug!("Starting portal session");
    let response = portal_dialog(
        "starting the portal session",
        screencast.start(&session, None, StartCastOptions::default()),
    )
    .await?
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

    let fd = portal_reply(
        "opening the PipeWire remote",
        screencast.open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default()),
    )
    .await?;

    Ok((fd, node_id))
}
