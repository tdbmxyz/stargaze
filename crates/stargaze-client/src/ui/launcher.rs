//! The launcher: host list, host editor, settings, and the connecting
//! overlay, run in a loop from `main` between sessions.
//!
//! Split in two layers: a pure [`Model`] that consumes [`NavEvent`]s
//! and emits [`Effect`]s (unit-testable, no SDL), and [`run_launcher`]
//! which owns the SDL window/canvas/event pump, draws the model every
//! frame, and executes effects (saving the config, spawning the async
//! connect, returning to `main`).

use std::path::Path;
use std::time::Duration;

use anyhow::anyhow;
use sdl2::rect::Rect;
use stargaze_core::config::{self, ClientConfig, Codec, HostEntry, Resolution};
use tracing::{info, warn};

use super::InputMapper;
use super::font::TextRenderer;
use super::widgets::{self, FocusList, NavEvent, Ui, palette};
use crate::transport::{self, ConnectedSession};

/// Resolution presets for the host-edit spinner.
const RESOLUTIONS: [(u32, u32); 5] = [
    (1280, 800),
    (1920, 1080),
    (2560, 1440),
    (3440, 1440),
    (3840, 2160),
];

/// Framerate presets for the host-edit spinner.
const FRAMERATES: [u32; 4] = [30, 60, 90, 120];

/// Extra rows below the hosts on the main screen.
const HOST_LIST_EXTRA: usize = 3; // Add host, Settings, Quit

/// Fields on the host-edit screen.
const EDIT_FIELDS: usize = 8; // name, address, port, resolution, fps, codec, save, cancel

/// Rows on the settings screen.
const SETTINGS_ROWS: usize = 5; // 4 toggles + back

/// Row geometry shared by all screens.
const ROW_TOP: i32 = 150;
const ROW_HEIGHT: u32 = 56;
const ROW_GAP: u32 = 10;
const ROW_MARGIN: i32 = 80;

/// What `main` should do when the launcher returns.
pub enum LauncherOutcome {
    /// Start a session on this established connection; `cfg` is the
    /// snapshot (toggles + quality) the session must run with.
    Connect {
        /// Config snapshot for the session (host quality merged in).
        cfg: Box<ClientConfig>,
        /// The established connection.
        conn: Box<ConnectedSession>,
    },
    /// Exit the client.
    Quit,
}

/// The launcher screen being shown.
enum Screen {
    /// Main screen: saved hosts + add/settings/quit rows.
    HostList { focus: FocusList },
    /// Add or edit a host.
    HostEdit {
        /// `None` = adding a new host.
        index: Option<usize>,
        draft: HostEntry,
        /// Port edited as text (digits only).
        port_text: String,
        focus: FocusList,
        /// A text field is in editing mode.
        editing: bool,
    },
    /// Global toggles.
    Settings { focus: FocusList },
}

/// Side effects the SDL loop executes for the model.
#[derive(Debug, PartialEq, Eq)]
enum Effect {
    None,
    /// Persist the config to disk.
    Save,
    /// Open a connection to `hosts[index]`.
    Connect(usize),
    /// Exit the launcher (and the client).
    Quit,
    /// A text field entered/left editing mode (drives SDL text input).
    EditingChanged(bool),
}

/// Pure launcher state: the config being edited plus the screen stack.
struct Model {
    cfg: ClientConfig,
    screen: Screen,
    error: Option<String>,
}

impl Model {
    fn new(mut cfg: ClientConfig, error: Option<String>) -> Self {
        // Migrate a legacy server_address-only config into the host
        // list; persisted on the first save.
        cfg.hosts = config::effective_hosts(&cfg);
        let count = cfg.hosts.len() + HOST_LIST_EXTRA;
        Self {
            cfg,
            screen: Screen::HostList {
                focus: FocusList::new(count),
            },
            error,
        }
    }

    fn host_list_rects(&self) -> Vec<Rect> {
        widgets::layout_rows(
            self.cfg.hosts.len() + HOST_LIST_EXTRA,
            ROW_TOP,
            ROW_HEIGHT,
            ROW_GAP,
            ROW_MARGIN,
        )
    }

    fn screen_rects(&self) -> Vec<Rect> {
        let count = match &self.screen {
            Screen::HostList { .. } => self.cfg.hosts.len() + HOST_LIST_EXTRA,
            Screen::HostEdit { .. } => EDIT_FIELDS,
            Screen::Settings { .. } => SETTINGS_ROWS,
        };
        widgets::layout_rows(count, ROW_TOP, ROW_HEIGHT, ROW_GAP, ROW_MARGIN)
    }

    /// Feeds one navigation event through the state machine.
    fn update(&mut self, event: &NavEvent) -> Effect {
        // Any interaction dismisses a stale error banner.
        if !matches!(event, NavEvent::PointerMove(..)) {
            self.error = None;
        }
        match &mut self.screen {
            Screen::HostList { .. } => self.update_host_list(event),
            Screen::HostEdit { .. } => self.update_host_edit(event),
            Screen::Settings { .. } => self.update_settings(event),
        }
    }

    fn update_host_list(&mut self, event: &NavEvent) -> Effect {
        let hosts = self.cfg.hosts.len();
        let rects = self.host_list_rects();
        let Screen::HostList { focus } = &mut self.screen else {
            unreachable!()
        };
        let row_add = hosts;
        let row_settings = hosts + 1;
        let row_quit = hosts + 2;
        match event {
            NavEvent::Up => focus.up(),
            NavEvent::Down => focus.down(),
            NavEvent::PointerMove(x, y) => {
                focus.focus_at(&rects, *x, *y);
            }
            NavEvent::PointerClick(x, y) => {
                if focus.focus_at(&rects, *x, *y).is_some() {
                    return self.update_host_list(&NavEvent::Activate);
                }
            }
            NavEvent::Activate => {
                let row = focus.focus;
                if row < hosts {
                    return Effect::Connect(row);
                } else if row == row_add {
                    self.screen = new_host_edit(None, HostEntry::default());
                } else if row == row_settings {
                    self.screen = Screen::Settings {
                        focus: FocusList::new(SETTINGS_ROWS),
                    };
                } else if row == row_quit {
                    return Effect::Quit;
                }
            }
            NavEvent::ConnectShortcut => {
                let row = focus.focus;
                if row < hosts {
                    return Effect::Connect(row);
                } else if hosts > 0 {
                    return Effect::Connect(0);
                }
            }
            NavEvent::Edit => {
                let row = focus.focus;
                if row < hosts {
                    self.screen = new_host_edit(Some(row), self.cfg.hosts[row].clone());
                }
            }
            NavEvent::Delete => {
                let row = focus.focus;
                if row < hosts {
                    self.cfg.hosts.remove(row);
                    focus.resize(self.cfg.hosts.len() + HOST_LIST_EXTRA);
                    return Effect::Save;
                }
            }
            NavEvent::Back => return Effect::Quit,
            _ => {}
        }
        Effect::None
    }

    fn update_host_edit(&mut self, event: &NavEvent) -> Effect {
        let rects = self.screen_rects();
        let hosts = self.cfg.hosts.len();
        let Screen::HostEdit {
            index,
            draft,
            port_text,
            focus,
            editing,
        } = &mut self.screen
        else {
            unreachable!()
        };
        const FIELD_NAME: usize = 0;
        const FIELD_ADDRESS: usize = 1;
        const FIELD_PORT: usize = 2;
        const FIELD_RESOLUTION: usize = 3;
        const FIELD_FPS: usize = 4;
        const FIELD_CODEC: usize = 5;
        const FIELD_SAVE: usize = 6;
        const FIELD_CANCEL: usize = 7;

        if *editing {
            // A text field owns the input until Activate/Back.
            match event {
                NavEvent::Text(s) => {
                    let target = match focus.focus {
                        FIELD_NAME => &mut draft.name,
                        FIELD_ADDRESS => &mut draft.address,
                        FIELD_PORT => port_text,
                        _ => unreachable!("editing a non-text field"),
                    };
                    for ch in s.chars() {
                        let ok = match focus.focus {
                            FIELD_PORT => ch.is_ascii_digit() && target.len() < 5,
                            // Addresses/names: printable ASCII is plenty.
                            _ => !ch.is_control() && target.len() < 64,
                        };
                        if ok {
                            target.push(ch);
                        }
                    }
                }
                NavEvent::Backspace => {
                    let target = match focus.focus {
                        FIELD_NAME => &mut draft.name,
                        FIELD_ADDRESS => &mut draft.address,
                        FIELD_PORT => port_text,
                        _ => unreachable!("editing a non-text field"),
                    };
                    target.pop();
                }
                NavEvent::Activate | NavEvent::Back | NavEvent::Up | NavEvent::Down => {
                    *editing = false;
                    match event {
                        NavEvent::Up => focus.up(),
                        NavEvent::Down => focus.down(),
                        _ => {}
                    }
                    return Effect::EditingChanged(false);
                }
                _ => {}
            }
            return Effect::None;
        }

        match event {
            NavEvent::Up => focus.up(),
            NavEvent::Down => focus.down(),
            NavEvent::PointerMove(x, y) => {
                focus.focus_at(&rects, *x, *y);
            }
            NavEvent::PointerClick(x, y) => {
                if focus.focus_at(&rects, *x, *y).is_some() {
                    return self.update_host_edit(&NavEvent::Activate);
                }
            }
            NavEvent::Left | NavEvent::Right => {
                let forward = matches!(event, NavEvent::Right);
                match focus.focus {
                    FIELD_RESOLUTION => cycle_resolution(&mut draft.resolution, forward),
                    FIELD_FPS => cycle_framerate(&mut draft.framerate, forward),
                    FIELD_CODEC => cycle_codec(&mut draft.codec, forward),
                    _ => {}
                }
            }
            NavEvent::Activate => match focus.focus {
                FIELD_NAME | FIELD_ADDRESS | FIELD_PORT => {
                    *editing = true;
                    return Effect::EditingChanged(true);
                }
                FIELD_RESOLUTION => cycle_resolution(&mut draft.resolution, true),
                FIELD_FPS => cycle_framerate(&mut draft.framerate, true),
                FIELD_CODEC => cycle_codec(&mut draft.codec, true),
                FIELD_SAVE => {
                    if draft.address.is_empty() {
                        self.error = Some("Host address must not be empty".to_string());
                        return Effect::None;
                    }
                    let Ok(port) = port_text.parse::<u16>() else {
                        self.error = Some(format!("Invalid port: {port_text}"));
                        return Effect::None;
                    };
                    draft.port = port;
                    let committed = draft.clone();
                    let index = *index;
                    match index {
                        Some(i) => self.cfg.hosts[i] = committed,
                        None => self.cfg.hosts.push(committed),
                    }
                    self.show_host_list(index.unwrap_or(hosts));
                    return Effect::Save;
                }
                FIELD_CANCEL => {
                    let row = index.unwrap_or(hosts);
                    self.show_host_list(row);
                }
                _ => {}
            },
            NavEvent::Back => {
                let row = index.unwrap_or(hosts);
                self.show_host_list(row);
            }
            _ => {}
        }
        Effect::None
    }

    fn update_settings(&mut self, event: &NavEvent) -> Effect {
        let rects = self.screen_rects();
        let Screen::Settings { focus } = &mut self.screen else {
            unreachable!()
        };
        match event {
            NavEvent::Up => focus.up(),
            NavEvent::Down => focus.down(),
            NavEvent::PointerMove(x, y) => {
                focus.focus_at(&rects, *x, *y);
            }
            NavEvent::PointerClick(x, y) => {
                if focus.focus_at(&rects, *x, *y).is_some() {
                    return self.update_settings(&NavEvent::Activate);
                }
            }
            NavEvent::Activate | NavEvent::Left | NavEvent::Right => {
                let toggled = match focus.focus {
                    0 => {
                        self.cfg.fullscreen = !self.cfg.fullscreen;
                        true
                    }
                    1 => {
                        self.cfg.gamepad_passthrough = !self.cfg.gamepad_passthrough;
                        true
                    }
                    2 => {
                        self.cfg.usb_forward = !self.cfg.usb_forward;
                        true
                    }
                    3 => {
                        self.cfg.mic_forward.enabled = !self.cfg.mic_forward.enabled;
                        true
                    }
                    _ => false,
                };
                if toggled {
                    return Effect::Save;
                }
                if matches!(event, NavEvent::Activate) {
                    // The Back row.
                    self.show_host_list(0);
                }
            }
            NavEvent::Back => self.show_host_list(0),
            _ => {}
        }
        Effect::None
    }

    fn show_host_list(&mut self, focus_row: usize) {
        let mut focus = FocusList::new(self.cfg.hosts.len() + HOST_LIST_EXTRA);
        focus.focus = focus_row.min(focus.count.saturating_sub(1));
        self.screen = Screen::HostList { focus };
    }
}

fn new_host_edit(index: Option<usize>, draft: HostEntry) -> Screen {
    let port_text = draft.port.to_string();
    Screen::HostEdit {
        index,
        draft,
        port_text,
        focus: FocusList::new(EDIT_FIELDS),
        editing: false,
    }
}

fn cycle_resolution(resolution: &mut Resolution, forward: bool) {
    let current = RESOLUTIONS
        .iter()
        .position(|&(w, h)| w == resolution.width && h == resolution.height);
    let next = match (current, forward) {
        (Some(i), true) => (i + 1) % RESOLUTIONS.len(),
        (Some(i), false) => (i + RESOLUTIONS.len() - 1) % RESOLUTIONS.len(),
        // Custom value not in the presets: snap to the first.
        (None, _) => 0,
    };
    let (width, height) = RESOLUTIONS[next];
    *resolution = Resolution { width, height };
}

fn cycle_framerate(framerate: &mut u32, forward: bool) {
    let current = FRAMERATES.iter().position(|&f| f == *framerate);
    let next = match (current, forward) {
        (Some(i), true) => (i + 1) % FRAMERATES.len(),
        (Some(i), false) => (i + FRAMERATES.len() - 1) % FRAMERATES.len(),
        (None, _) => 1, // default to 60
    };
    *framerate = FRAMERATES[next];
}

fn cycle_codec(codec: &mut Codec, forward: bool) {
    let _ = forward; // two entries: either direction flips
    *codec = match codec {
        Codec::H265 => Codec::Av1,
        Codec::Av1 => Codec::H265,
    };
}

// --- Drawing ---

fn draw(model: &Model, connecting: Option<&str>, ui: &mut Ui) {
    ui.canvas.set_draw_color(palette::BACKGROUND);
    ui.canvas.clear();

    ui.text("Stargaze", ROW_MARGIN, 48, 40, palette::TEXT);

    match &model.screen {
        Screen::HostList { focus } => draw_host_list(model, focus, ui),
        Screen::HostEdit {
            index,
            draft,
            port_text,
            focus,
            editing,
        } => draw_host_edit(*index, draft, port_text, focus, *editing, ui),
        Screen::Settings { focus } => draw_settings(model, focus, ui),
    }

    if let Some(host) = connecting {
        let rect = Rect::new(0, 0, widgets::UI_WIDTH, 44);
        ui.canvas.set_draw_color(palette::WIDGET_FOCUS);
        let _ = ui.canvas.fill_rect(rect);
        ui.text(
            &format!("Connecting to {host}…  (B / Esc to cancel)"),
            16,
            10,
            18,
            palette::TEXT,
        );
    } else if let Some(error) = &model.error {
        ui.error_banner(error);
    }

    ui.canvas.present();
}

fn draw_host_list(model: &Model, focus: &FocusList, ui: &mut Ui) {
    let rects = model.host_list_rects();
    let hosts = &model.cfg.hosts;
    ui.text("Hosts", ROW_MARGIN, ROW_TOP - 40, 18, palette::TEXT_DIM);
    for (i, host) in hosts.iter().enumerate() {
        let rect = rects[i];
        let focused = focus.focus == i;
        ui.widget_box(rect, focused);
        let y = rect.y() + (rect.height() as i32 - ui.text.line_height(22) as i32) / 2;
        ui.text(host.display_name(), rect.x() + 16, y, 22, palette::TEXT);
        let details = format!(
            "{}:{}   {} @ {}fps {}",
            host.address, host.port, host.resolution, host.framerate, host.codec
        );
        ui.text_right(
            &details,
            rect.x() + rect.width() as i32 - 16,
            y,
            18,
            palette::TEXT_DIM,
        );
    }
    ui.button(rects[hosts.len()], "Add host…", focus.focus == hosts.len());
    ui.button(
        rects[hosts.len() + 1],
        "Settings",
        focus.focus == hosts.len() + 1,
    );
    ui.button(
        rects[hosts.len() + 2],
        "Quit",
        focus.focus == hosts.len() + 2,
    );
    ui.hint_bar("A/Enter connect   X/E edit   Y/Del delete   B/Esc quit");
}

fn draw_host_edit(
    index: Option<usize>,
    draft: &HostEntry,
    port_text: &str,
    focus: &FocusList,
    editing: bool,
    ui: &mut Ui,
) {
    let rects = widgets::layout_rows(EDIT_FIELDS, ROW_TOP, ROW_HEIGHT, ROW_GAP, ROW_MARGIN);
    let title = if index.is_some() {
        "Edit host"
    } else {
        "Add host"
    };
    ui.text(title, ROW_MARGIN, ROW_TOP - 40, 18, palette::TEXT_DIM);
    let f = focus.focus;
    ui.text_field(rects[0], "Name", &draft.name, f == 0, editing && f == 0);
    ui.text_field(
        rects[1],
        "Address",
        &draft.address,
        f == 1,
        editing && f == 1,
    );
    ui.text_field(rects[2], "Port", port_text, f == 2, editing && f == 2);
    ui.choice(
        rects[3],
        "Resolution",
        &draft.resolution.to_string(),
        f == 3,
    );
    ui.choice(rects[4], "Framerate", &draft.framerate.to_string(), f == 4);
    ui.choice(rects[5], "Codec", &draft.codec.to_string(), f == 5);
    ui.button(rects[6], "Save", f == 6);
    ui.button(rects[7], "Cancel", f == 7);
    ui.hint_bar("A/Enter edit or apply   ◄ ► change value   B/Esc back");
}

fn draw_settings(model: &Model, focus: &FocusList, ui: &mut Ui) {
    let rects = widgets::layout_rows(SETTINGS_ROWS, ROW_TOP, ROW_HEIGHT, ROW_GAP, ROW_MARGIN);
    ui.text("Settings", ROW_MARGIN, ROW_TOP - 40, 18, palette::TEXT_DIM);
    let cfg = &model.cfg;
    let f = focus.focus;
    ui.toggle(rects[0], "Fullscreen", cfg.fullscreen, f == 0);
    ui.toggle(
        rects[1],
        "Gamepad pass-through",
        cfg.gamepad_passthrough,
        f == 1,
    );
    ui.toggle(
        rects[2],
        "USB forward (Valve hardware)",
        cfg.usb_forward,
        f == 2,
    );
    ui.toggle(
        rects[3],
        "Microphone forward",
        cfg.mic_forward.enabled,
        f == 3,
    );
    ui.button(rects[4], "Back", f == 4);
    ui.hint_bar("A/Enter toggle   B/Esc back");
}

// --- SDL loop ---

/// Builds the per-session config snapshot for a host: global toggles
/// from `cfg`, address and quality from the host entry.
fn session_config(cfg: &ClientConfig, host: &HostEntry) -> ClientConfig {
    let mut session = cfg.clone();
    session.server_address.clone_from(&host.address);
    session.port = host.port;
    session.resolution = host.resolution;
    session.framerate = host.framerate;
    session.codec = host.codec;
    session
}

/// Runs the launcher until the user connects or quits.
///
/// Owns every SDL resource it creates (window, canvas, event pump,
/// controller handles); all are dropped before this returns, because
/// the session's render loop creates its own (SDL allows only one
/// event pump).
///
/// # Errors
///
/// Returns an error if SDL setup fails; user-facing problems
/// (connection failures, bad input) stay inside the launcher as
/// banners.
pub fn run_launcher(
    sdl: &sdl2::Sdl,
    rt: &tokio::runtime::Handle,
    cfg: &mut ClientConfig,
    config_path: &Path,
    last_error: Option<String>,
) -> anyhow::Result<LauncherOutcome> {
    let video = sdl.video().map_err(|e| anyhow!("SDL video: {e}"))?;
    let controllers = sdl
        .game_controller()
        .map_err(|e| anyhow!("SDL game controller: {e}"))?;
    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| anyhow!("SDL event pump: {e}"))?;

    let mut builder = video.window("Stargaze", widgets::UI_WIDTH, widgets::UI_HEIGHT);
    builder.position_centered();
    if cfg.fullscreen {
        builder.fullscreen_desktop();
    }
    let window = builder.build()?;
    let mut canvas = window.into_canvas().accelerated().present_vsync().build()?;
    canvas
        .set_logical_size(widgets::UI_WIDTH, widgets::UI_HEIGHT)
        .map_err(|e| anyhow!("SDL logical size: {e}"))?;
    let texture_creator = canvas.texture_creator();
    let mut text = TextRenderer::new()?;

    // Text input off until a field enters editing mode (keeps the Deck
    // OSK from popping up over the list screens).
    video.text_input().stop();

    let mut model = Model::new(cfg.clone(), last_error);
    let mut mapper = InputMapper::new();
    let mut pads: Vec<sdl2::controller::GameController> = Vec::new();
    // In-flight connection attempt: result receiver, abort handle, the
    // session config snapshot it was started with, and a display name.
    struct Connecting {
        rx: std::sync::mpsc::Receiver<
            Result<ConnectedSession, stargaze_core::transport::TransportError>,
        >,
        task: tokio::task::JoinHandle<()>,
        session_cfg: ClientConfig,
        host_name: String,
    }
    let mut connecting: Option<Connecting> = None;

    loop {
        let mut events: Vec<NavEvent> = Vec::new();
        for event in event_pump.poll_iter() {
            match &event {
                sdl2::event::Event::Quit { .. } => {
                    save(cfg, &model, config_path);
                    return Ok(LauncherOutcome::Quit);
                }
                sdl2::event::Event::ControllerDeviceAdded { which, .. } => {
                    if let Ok(pad) = controllers.open(*which) {
                        info!("Launcher: controller connected: {}", pad.name());
                        pads.push(pad);
                    }
                }
                sdl2::event::Event::ControllerDeviceRemoved { which, .. } => {
                    pads.retain(|p| p.instance_id() != *which);
                }
                _ => {}
            }
            if let Some(nav) = mapper.map(&event) {
                events.push(nav);
            }
        }
        if let Some(nav) = mapper.tick() {
            events.push(nav);
        }

        if let Some(attempt) = connecting.take() {
            // While connecting: only allow cancel; poll the attempt.
            let cancelled = events.iter().any(|e| matches!(e, NavEvent::Back));
            match attempt.rx.try_recv() {
                Ok(Ok(conn)) if !cancelled => {
                    save(cfg, &model, config_path);
                    return Ok(LauncherOutcome::Connect {
                        cfg: Box::new(attempt.session_cfg),
                        conn: Box::new(conn),
                    });
                }
                Ok(Ok(conn)) => {
                    // Cancelled at the same moment it succeeded: close
                    // the session outright (the receive task holds its
                    // own connection handle, so dropping isn't enough).
                    conn.usb_connection.close(0u32.into(), b"cancelled");
                    conn.transport.abort();
                }
                Ok(Err(e)) => {
                    // A cancel racing the failure should read as the
                    // cancel the user asked for, not a surprise error.
                    model.error = if cancelled {
                        Some("Connection cancelled".to_string())
                    } else {
                        Some(format!("Connection failed: {e}"))
                    };
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if cancelled {
                        attempt.task.abort();
                        model.error = Some("Connection cancelled".to_string());
                    } else {
                        connecting = Some(attempt);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    model.error = Some("Connection task died".to_string());
                }
            }
        } else {
            for nav in &events {
                match model.update(nav) {
                    Effect::None => {}
                    Effect::Save => save(cfg, &model, config_path),
                    Effect::Quit => {
                        save(cfg, &model, config_path);
                        return Ok(LauncherOutcome::Quit);
                    }
                    Effect::EditingChanged(editing) => {
                        if editing {
                            video.text_input().start();
                        } else {
                            video.text_input().stop();
                        }
                    }
                    Effect::Connect(row) => {
                        let host = model.cfg.hosts[row].clone();
                        let session_cfg = session_config(&model.cfg, &host);
                        let request = transport::SessionRequest {
                            width: host.resolution.width,
                            height: host.resolution.height,
                            framerate: host.framerate,
                            codec: host.codec,
                        };
                        let (tx, rx) = std::sync::mpsc::channel();
                        let connect_cfg = session_cfg.clone();
                        let task = rt.spawn(async move {
                            let result = transport::connect(&connect_cfg, request).await;
                            let _ = tx.send(result);
                        });
                        connecting = Some(Connecting {
                            rx,
                            task,
                            session_cfg,
                            host_name: host.display_name().to_string(),
                        });
                        // Drop the rest of this frame's events: a second
                        // Activate must not spawn a competing connect
                        // and leak the first attempt.
                        break;
                    }
                }
            }
        }

        let connecting_label = connecting.as_ref().map(|c| c.host_name.clone());
        let mut ui = Ui {
            canvas: &mut canvas,
            textures: &texture_creator,
            text: &mut text,
        };
        draw(&model, connecting_label.as_deref(), &mut ui);

        // present_vsync paces us; the sleep only matters on drivers
        // that ignore vsync for occluded windows.
        std::thread::sleep(Duration::from_millis(4));
    }
}

/// Persists the model's config, keeping `cfg` (main's copy) in sync.
fn save(cfg: &mut ClientConfig, model: &Model, config_path: &Path) {
    *cfg = model.cfg.clone();
    if let Err(e) = config::save_config(config_path, cfg) {
        warn!("Could not save config: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with_hosts(n: usize) -> Model {
        let cfg = ClientConfig {
            hosts: (0..n)
                .map(|i| HostEntry {
                    name: format!("host{i}"),
                    address: format!("10.0.0.{i}"),
                    ..HostEntry::default()
                })
                .collect(),
            ..ClientConfig::default()
        };
        Model::new(cfg, None)
    }

    #[test]
    fn activate_host_row_connects() {
        let mut model = model_with_hosts(2);
        model.update(&NavEvent::Down);
        assert_eq!(model.update(&NavEvent::Activate), Effect::Connect(1));
    }

    #[test]
    fn activate_quit_row_quits() {
        let mut model = model_with_hosts(1);
        for _ in 0..3 {
            model.update(&NavEvent::Down);
        }
        assert_eq!(model.update(&NavEvent::Activate), Effect::Quit);
    }

    #[test]
    fn delete_removes_host_and_saves() {
        let mut model = model_with_hosts(2);
        assert_eq!(model.update(&NavEvent::Delete), Effect::Save);
        assert_eq!(model.cfg.hosts.len(), 1);
        assert_eq!(model.cfg.hosts[0].name, "host1");
    }

    #[test]
    fn add_edit_save_flow() {
        let mut model = model_with_hosts(0);
        // Focus starts on "Add host…" when there are no hosts.
        assert_eq!(model.update(&NavEvent::Activate), Effect::None);
        assert!(matches!(model.screen, Screen::HostEdit { .. }));

        // Down to Address, enter editing, type an address.
        model.update(&NavEvent::Down);
        assert_eq!(
            model.update(&NavEvent::Activate),
            Effect::EditingChanged(true)
        );
        model.update(&NavEvent::Text("zeus.lan".to_string()));
        assert_eq!(
            model.update(&NavEvent::Activate),
            Effect::EditingChanged(false)
        );

        // Down to Save (address=1 → save=6).
        for _ in 0..5 {
            model.update(&NavEvent::Down);
        }
        assert_eq!(model.update(&NavEvent::Activate), Effect::Save);
        assert_eq!(model.cfg.hosts.len(), 1);
        assert_eq!(model.cfg.hosts[0].address, "zeus.lan");
        assert!(matches!(model.screen, Screen::HostList { .. }));
    }

    #[test]
    fn save_with_empty_address_shows_error() {
        let mut model = model_with_hosts(0);
        model.update(&NavEvent::Activate); // open Add host
        for _ in 0..6 {
            model.update(&NavEvent::Down);
        }
        assert_eq!(model.update(&NavEvent::Activate), Effect::None);
        assert!(model.error.is_some());
        assert!(matches!(model.screen, Screen::HostEdit { .. }));
    }

    #[test]
    fn edit_existing_host_updates_in_place() {
        let mut model = model_with_hosts(2);
        model.update(&NavEvent::Down);
        model.update(&NavEvent::Edit);
        assert!(matches!(
            model.screen,
            Screen::HostEdit { index: Some(1), .. }
        ));
        // Cycle codec (field 5) then save.
        for _ in 0..5 {
            model.update(&NavEvent::Down);
        }
        model.update(&NavEvent::Right);
        model.update(&NavEvent::Down);
        assert_eq!(model.update(&NavEvent::Activate), Effect::Save);
        assert_eq!(model.cfg.hosts[1].codec, Codec::Av1);
        assert_eq!(model.cfg.hosts.len(), 2);
    }

    #[test]
    fn settings_toggle_saves() {
        let mut model = model_with_hosts(1);
        model.update(&NavEvent::Down); // Add host
        model.update(&NavEvent::Down); // Settings
        model.update(&NavEvent::Activate);
        assert!(matches!(model.screen, Screen::Settings { .. }));
        assert!(model.cfg.fullscreen);
        assert_eq!(model.update(&NavEvent::Activate), Effect::Save);
        assert!(!model.cfg.fullscreen);
        // Back returns to the host list.
        model.update(&NavEvent::Back);
        assert!(matches!(model.screen, Screen::HostList { .. }));
    }

    #[test]
    fn back_on_host_list_quits() {
        let mut model = model_with_hosts(1);
        assert_eq!(model.update(&NavEvent::Back), Effect::Quit);
    }

    #[test]
    fn port_field_accepts_digits_only() {
        let mut model = model_with_hosts(0);
        model.update(&NavEvent::Activate); // Add host
        model.update(&NavEvent::Down);
        model.update(&NavEvent::Down); // Port
        model.update(&NavEvent::Activate); // edit
        // Clear the default port.
        for _ in 0..5 {
            model.update(&NavEvent::Backspace);
        }
        model.update(&NavEvent::Text("9a1x".to_string()));
        let Screen::HostEdit { port_text, .. } = &model.screen else {
            panic!("expected host edit screen");
        };
        assert_eq!(port_text, "91");
    }

    #[test]
    fn legacy_config_migrates_to_host_row() {
        let cfg = ClientConfig {
            server_address: "100.64.0.3".to_string(),
            port: 9000,
            ..ClientConfig::default()
        };
        let model = Model::new(cfg, None);
        assert_eq!(model.cfg.hosts.len(), 1);
        assert_eq!(model.cfg.hosts[0].address, "100.64.0.3");
    }

    #[test]
    fn session_config_merges_host_into_toggles() {
        let cfg = ClientConfig {
            fullscreen: false,
            usb_forward: false,
            ..ClientConfig::default()
        };
        let host = HostEntry {
            name: "zeus".to_string(),
            address: "zeus.lan".to_string(),
            port: 9100,
            resolution: Resolution {
                width: 2560,
                height: 1440,
            },
            framerate: 90,
            codec: Codec::Av1,
        };
        let session = session_config(&cfg, &host);
        assert_eq!(session.server_address, "zeus.lan");
        assert_eq!(session.port, 9100);
        assert_eq!(session.framerate, 90);
        assert_eq!(session.codec, Codec::Av1);
        assert!(!session.fullscreen);
        assert!(!session.usb_forward);
    }
}
