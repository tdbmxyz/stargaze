use std::collections::HashMap;

use anyhow::anyhow;
use sdl2::audio::AudioQueue;
use sdl2::controller::GameController;
use stargaze_core::decode::{DecodedFrame, DecoderConfig, FramePixels};
use stargaze_core::input::{GamepadAxis, GamepadButton, InputEvent, MouseButton};
use tracing::{info, warn};

use super::audio::create_audio_queue;
use super::input::{InputTracker, PadSlots, ShortcutAction, shortcut_action};
use super::stats::{StatsOverlay, StatsRecorder, draw_overlay};
use crate::transport::NetStats;

/// Window title shown while input is captured ("inside" mode).
const TITLE_CAPTURED: &str = "Stargaze";
/// Window title shown while input is released ("outside" mode).
const TITLE_RELEASED: &str = "Stargaze — input released (Ctrl+Alt+Shift+Z to capture)";

/// Connected game controllers: slot bookkeeping plus the open SDL handles
/// (dropping a handle closes the controller, so they must stay alive).
struct Controllers {
    slots: PadSlots,
    handles: HashMap<u32, GameController>,
}

impl Controllers {
    fn new() -> Self {
        Self {
            slots: PadSlots::new(),
            handles: HashMap::new(),
        }
    }

    /// Opens the controller at `joystick_index` and assigns it a pad slot.
    /// Notifies the server so it creates the matching virtual device.
    fn add(
        &mut self,
        subsystem: &sdl2::GameControllerSubsystem,
        joystick_index: u32,
        input_tx: &std::sync::mpsc::Sender<InputEvent>,
    ) {
        let controller = match subsystem.open(joystick_index) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    index = joystick_index,
                    "Failed to open game controller: {e}"
                );
                return;
            }
        };
        let instance_id = controller.instance_id();
        if self.handles.contains_key(&instance_id) {
            return; // Already open (duplicate hotplug event).
        }
        let Some(pad) = self.slots.allocate(instance_id) else {
            warn!(
                name = controller.name(),
                "All gamepad slots taken, ignoring controller"
            );
            return;
        };
        info!(name = controller.name(), pad, "Game controller connected");
        self.handles.insert(instance_id, controller);
        let _ = input_tx.send(InputEvent::GamepadConnected { pad });
    }

    /// Closes the controller with `instance_id` and frees its pad slot.
    /// Notifies the server so it removes the matching virtual device.
    fn remove(&mut self, instance_id: u32, input_tx: &std::sync::mpsc::Sender<InputEvent>) {
        self.handles.remove(&instance_id);
        if let Some(pad) = self.slots.release(instance_id) {
            info!(pad, "Game controller disconnected");
            let _ = input_tx.send(InputEvent::GamepadDisconnected { pad });
        }
    }

    fn pad_of(&self, instance_id: u32) -> Option<u8> {
        self.slots.get(instance_id)
    }
}

/// Applies the input capture mode to the window.
///
/// "Inside" (`captured == true`): the keyboard is grabbed — on Wayland this
/// inhibits compositor shortcuts (e.g. Hyprland's Super bindings), so every
/// key reaches the remote session — and the mouse is in relative mode.
/// "Outside": grabs are released and the cursor is freed so keyboard and
/// mouse act on the local desktop.
fn apply_capture_mode(
    captured: bool,
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    mouse: &sdl2::mouse::MouseUtil,
) {
    let window = canvas.window_mut();
    window.set_keyboard_grab(captured);
    let title = if captured {
        TITLE_CAPTURED
    } else {
        TITLE_RELEASED
    };
    if let Err(e) = window.set_title(title) {
        warn!("Failed to set window title: {e}");
    }
    mouse.set_relative_mouse_mode(captured);
    mouse.show_cursor(!captured);
    info!(
        captured,
        "Input capture {}",
        if captured {
            "enabled (keys go to the remote session)"
        } else {
            "released (keys stay on the local desktop)"
        }
    );
}

/// Toggles the window between windowed and borderless fullscreen.
fn toggle_fullscreen(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>) {
    let window = canvas.window_mut();
    let target = if window.fullscreen_state() == sdl2::video::FullscreenType::Off {
        sdl2::video::FullscreenType::Desktop
    } else {
        sdl2::video::FullscreenType::Off
    };
    if let Err(e) = window.set_fullscreen(target) {
        warn!("Failed to toggle fullscreen: {e}");
    }
}

#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names
)]
pub(super) fn run_sdl_loop(
    sdl: &sdl2::Sdl,
    config: &DecoderConfig,
    decoded_rx: std::sync::mpsc::Receiver<DecodedFrame>,
    audio_pcm_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    fullscreen: bool,
    input_tx: std::sync::mpsc::Sender<InputEvent>,
    rtt_probe: super::RttProbe,
    net_stats: &NetStats,
    stats_file: Option<&std::path::Path>,
) -> Result<(), anyhow::Error> {
    let audio_queue: AudioQueue<f32> = create_audio_queue(sdl)?;

    // The stream is BT.709 limited range (the server's converter and the
    // encoder both advertise it). SDL's automatic mode picks BT.601 —
    // sdl2-compat does so even for HD — which subtly shifts every color.
    unsafe {
        sdl2::sys::SDL_SetYUVConversionMode(
            sdl2::sys::SDL_YUV_CONVERSION_MODE::SDL_YUV_CONVERSION_BT709,
        );
    }

    let video = sdl
        .video()
        .map_err(|e| anyhow!("SDL2 video init failed: {e}"))?;

    let game_controller_subsystem = sdl
        .game_controller()
        .map_err(|e| anyhow!("SDL2 game controller init failed: {e}"))?;

    let mut window_builder = video.window("Stargaze", config.width, config.height);
    window_builder.position_centered();
    if fullscreen {
        window_builder.fullscreen_desktop();
    }
    let window = window_builder
        .build()
        .map_err(|e| anyhow!("window creation failed: {e}"))?;

    let mut canvas = window
        .into_canvas()
        .accelerated()
        .build()
        .map_err(|e| anyhow!("canvas creation failed: {e}"))?;

    let texture_creator = canvas.texture_creator();
    // The streaming texture is created on the first decoded frame, in the
    // decoder's native pixel layout (NV12 for hardware decode, IYUV for
    // software decode), and recreated if the layout changes mid-session.
    let mut texture: Option<(sdl2::pixels::PixelFormatEnum, sdl2::render::Texture<'_>)> = None;

    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| anyhow!("event pump failed: {e}"))?;

    // Controllers present at startup arrive as ControllerDeviceAdded events
    // on the first event pump iterations, so no manual scan is needed.
    let mut controllers = Controllers::new();

    // Start captured ("inside" mode): all input goes to the remote session.
    let mut captured = true;
    let mut tracker = InputTracker::new();
    apply_capture_mode(captured, &mut canvas, &sdl.mouse());

    let mut overlay = StatsOverlay::new();
    let mut recorder = StatsRecorder::new();
    let video_desc = format!("{}x{}", config.width, config.height);

    info!(
        "Renderer started: {}x{} (fullscreen: {})",
        config.width, config.height, fullscreen
    );

    // Show a black window until the first frame arrives.
    canvas.set_draw_color(sdl2::pixels::Color::BLACK);
    canvas.clear();
    canvas.present();

    'main: loop {
        // Coalesce mouse motion within one event batch: a 1000 Hz mouse
        // delivers ~16 motion events per frame, and sending each as its own
        // control-stream message adds per-event overhead end to end.
        let mut mouse_dx = 0i32;
        let mut mouse_dy = 0i32;

        for event in event_pump.poll_iter() {
            match event {
                sdl2::event::Event::Quit { .. } => break 'main,

                sdl2::event::Event::KeyDown {
                    scancode: Some(sc),
                    keymod,
                    repeat: false,
                    ..
                } => {
                    if let Some(action) = shortcut_action(keymod, sc) {
                        // Release everything held remotely so the chord's
                        // modifiers (already forwarded) don't stay stuck.
                        for ev in tracker.release_all() {
                            let _ = input_tx.send(ev);
                        }
                        match action {
                            ShortcutAction::Quit => break 'main,
                            ShortcutAction::ToggleCapture => {
                                captured = !captured;
                                apply_capture_mode(captured, &mut canvas, &sdl.mouse());
                            }
                            ShortcutAction::ToggleFullscreen => toggle_fullscreen(&mut canvas),
                            ShortcutAction::ToggleStats => {
                                overlay.visible = !overlay.visible;
                            }
                        }
                    } else if captured {
                        tracker.key_down(sc as u32);
                        let _ = input_tx.send(InputEvent::Keyboard {
                            scancode: sc as u32,
                            pressed: true,
                        });
                    }
                }

                sdl2::event::Event::KeyUp {
                    scancode: Some(sc),
                    repeat: false,
                    ..
                } => {
                    if captured {
                        tracker.key_up(sc as u32);
                        let _ = input_tx.send(InputEvent::Keyboard {
                            scancode: sc as u32,
                            pressed: false,
                        });
                    }
                }

                sdl2::event::Event::MouseMotion { xrel, yrel, .. } if captured => {
                    mouse_dx += xrel;
                    mouse_dy += yrel;
                }

                sdl2::event::Event::MouseButtonDown { mouse_btn, .. } if captured => {
                    if let Some(button) = map_mouse_button(mouse_btn) {
                        tracker.mouse_down(button);
                        let _ = input_tx.send(InputEvent::MouseButton {
                            button,
                            pressed: true,
                        });
                    }
                }

                sdl2::event::Event::MouseButtonUp { mouse_btn, .. } if captured => {
                    if let Some(button) = map_mouse_button(mouse_btn) {
                        tracker.mouse_up(button);
                        let _ = input_tx.send(InputEvent::MouseButton {
                            button,
                            pressed: false,
                        });
                    }
                }

                sdl2::event::Event::MouseWheel { x, y, .. } if captured => {
                    let _ = input_tx.send(InputEvent::MouseWheel { dx: x, dy: y });
                }

                // Gamepad input is forwarded regardless of capture mode:
                // the local desktop doesn't compete for it.
                sdl2::event::Event::ControllerAxisMotion {
                    which, axis, value, ..
                } => {
                    if let Some(pad) = controllers.pad_of(which) {
                        let ga = map_gamepad_axis(axis);
                        let _ = input_tx.send(InputEvent::GamepadAxis {
                            pad,
                            axis: ga,
                            value,
                        });
                    }
                }

                sdl2::event::Event::ControllerButtonDown { which, button, .. } => {
                    if let (Some(pad), Some(gb)) =
                        (controllers.pad_of(which), map_gamepad_button(button))
                    {
                        let _ = input_tx.send(InputEvent::GamepadButton {
                            pad,
                            button: gb,
                            pressed: true,
                        });
                    }
                }

                sdl2::event::Event::ControllerButtonUp { which, button, .. } => {
                    if let (Some(pad), Some(gb)) =
                        (controllers.pad_of(which), map_gamepad_button(button))
                    {
                        let _ = input_tx.send(InputEvent::GamepadButton {
                            pad,
                            button: gb,
                            pressed: false,
                        });
                    }
                }

                sdl2::event::Event::ControllerDeviceAdded {
                    which: joystick_index,
                    ..
                } if game_controller_subsystem.is_game_controller(joystick_index) => {
                    controllers.add(&game_controller_subsystem, joystick_index, &input_tx);
                }

                sdl2::event::Event::ControllerDeviceRemoved {
                    which: instance_id, ..
                } => {
                    controllers.remove(instance_id, &input_tx);
                }

                _ => {}
            }
        }

        if mouse_dx != 0 || mouse_dy != 0 {
            let _ = input_tx.send(InputEvent::MouseMove {
                dx: mouse_dx,
                dy: mouse_dy,
            });
        }

        // Wait briefly for a decoded frame so the loop doesn't busy-spin;
        // the short timeout keeps input polling responsive. Then drain any
        // queued frames, keeping only the latest.
        let mut latest_frame: Option<DecodedFrame> =
            match decoded_rx.recv_timeout(std::time::Duration::from_millis(2)) {
                Ok(frame) => {
                    recorder.record(frame.stats);
                    Some(frame)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    info!("Decoded frame channel closed, stopping renderer");
                    break 'main;
                }
            };
        while let Ok(frame) = decoded_rx.try_recv() {
            recorder.record(frame.stats);
            if latest_frame.is_some() {
                overlay.on_frames_dropped(1);
            }
            latest_frame = Some(frame);
        }

        // Drain decoded audio PCM and queue for playback.
        while let Ok(pcm) = audio_pcm_rx.try_recv() {
            if let Err(e) = audio_queue.queue_audio(&pcm) {
                warn!("Failed to queue audio: {e}");
                break;
            }
        }

        // Nothing new to display — keep polling events without re-presenting.
        let Some(frame) = latest_frame else {
            continue;
        };

        let wanted_format = match &frame.pixels {
            FramePixels::I420 { .. } => sdl2::pixels::PixelFormatEnum::IYUV,
            FramePixels::Nv12 { .. } => sdl2::pixels::PixelFormatEnum::NV12,
        };

        static RENDER_DIAG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let rn = RENDER_DIAG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if rn < 5 {
            info!(
                frame_n = rn,
                frame_width = frame.width,
                frame_height = frame.height,
                texture_width = config.width,
                texture_height = config.height,
                texture_format = ?wanted_format,
                "SDL render diagnostics"
            );
        }

        let tex = match &mut texture {
            Some((fmt, tex)) if *fmt == wanted_format => tex,
            slot => {
                let tex = texture_creator
                    .create_texture_streaming(wanted_format, config.width, config.height)
                    .map_err(|e| anyhow!("texture creation failed: {e}"))?;
                info!(format = ?wanted_format, "Created streaming texture");
                &mut slot.insert((wanted_format, tex)).1
            }
        };

        let y_pitch = frame.width as usize;
        match &frame.pixels {
            FramePixels::I420 { y, u, v } => {
                tex.update_yuv(None, y, y_pitch, u, y_pitch / 2, v, y_pitch / 2)
                    .map_err(|e| anyhow!("texture update failed: {e}"))?;
            }
            FramePixels::Nv12 { y, uv } => {
                // rust-sdl2 doesn't wrap SDL_UpdateNVTexture; call it
                // directly. UV pitch equals the Y pitch for NV12.
                let ret = unsafe {
                    sdl2::sys::SDL_UpdateNVTexture(
                        tex.raw(),
                        std::ptr::null(),
                        y.as_ptr(),
                        i32::try_from(y_pitch).unwrap_or(i32::MAX),
                        uv.as_ptr(),
                        i32::try_from(y_pitch).unwrap_or(i32::MAX),
                    )
                };
                if ret != 0 {
                    return Err(anyhow!("SDL_UpdateNVTexture failed: {}", sdl2::get_error()));
                }
            }
        }

        canvas
            .copy(tex, None, None)
            .map_err(|e| anyhow!("canvas copy failed: {e}"))?;

        overlay.on_frame_rendered(frame.stats);
        if overlay.visible {
            let text = overlay
                .text(rtt_probe(), &video_desc, net_stats)
                .to_string();
            if let Err(e) = draw_overlay(&mut canvas, &text) {
                warn!("Stats overlay draw failed: {e}");
            }
        }

        canvas.present();
    }

    // Release any keys/buttons still held so the remote session isn't left
    // with stuck input (best-effort — the transport may already be gone).
    for ev in tracker.release_all() {
        let _ = input_tx.send(ev);
    }

    // Release the keyboard grab and leave relative mouse mode while the
    // window is still alive, then flush the pending Wayland requests.
    // Tearing the window down with relative mode active makes SDL issue a
    // pointer warp against a surface the compositor is already destroying,
    // which crashes Hyprland (SEGV in wp_pointer_warp_v1, seen on 0.55).
    apply_capture_mode(false, &mut canvas, &sdl.mouse());
    event_pump.pump_events();

    if let Some(path) = stats_file {
        match recorder.write_report(
            path,
            &video_desc,
            overlay.rendered(),
            overlay.superseded(),
            net_stats,
        ) {
            Ok(()) => info!(path = %path.display(), "Wrote session stats report"),
            Err(e) => warn!("Failed to write stats report to {}: {e}", path.display()),
        }
    }

    info!("Renderer shutting down");
    Ok(())
}

fn map_mouse_button(btn: sdl2::mouse::MouseButton) -> Option<MouseButton> {
    match btn {
        sdl2::mouse::MouseButton::Left => Some(MouseButton::Left),
        sdl2::mouse::MouseButton::Right => Some(MouseButton::Right),
        sdl2::mouse::MouseButton::Middle => Some(MouseButton::Middle),
        sdl2::mouse::MouseButton::X1 => Some(MouseButton::Side),
        sdl2::mouse::MouseButton::X2 => Some(MouseButton::Extra),
        sdl2::mouse::MouseButton::Unknown => None,
    }
}

fn map_gamepad_axis(axis: sdl2::controller::Axis) -> GamepadAxis {
    match axis {
        sdl2::controller::Axis::LeftX => GamepadAxis::LeftX,
        sdl2::controller::Axis::LeftY => GamepadAxis::LeftY,
        sdl2::controller::Axis::RightX => GamepadAxis::RightX,
        sdl2::controller::Axis::RightY => GamepadAxis::RightY,
        sdl2::controller::Axis::TriggerLeft => GamepadAxis::TriggerLeft,
        sdl2::controller::Axis::TriggerRight => GamepadAxis::TriggerRight,
    }
}

fn map_gamepad_button(btn: sdl2::controller::Button) -> Option<GamepadButton> {
    match btn {
        sdl2::controller::Button::A => Some(GamepadButton::South),
        sdl2::controller::Button::B => Some(GamepadButton::East),
        sdl2::controller::Button::X => Some(GamepadButton::North),
        sdl2::controller::Button::Y => Some(GamepadButton::West),
        sdl2::controller::Button::Start => Some(GamepadButton::Start),
        sdl2::controller::Button::Back => Some(GamepadButton::Back),
        sdl2::controller::Button::Guide => Some(GamepadButton::Guide),
        sdl2::controller::Button::LeftStick => Some(GamepadButton::LeftStick),
        sdl2::controller::Button::RightStick => Some(GamepadButton::RightStick),
        sdl2::controller::Button::LeftShoulder => Some(GamepadButton::LeftShoulder),
        sdl2::controller::Button::RightShoulder => Some(GamepadButton::RightShoulder),
        sdl2::controller::Button::DPadUp => Some(GamepadButton::DPadUp),
        sdl2::controller::Button::DPadDown => Some(GamepadButton::DPadDown),
        sdl2::controller::Button::DPadLeft => Some(GamepadButton::DPadLeft),
        sdl2::controller::Button::DPadRight => Some(GamepadButton::DPadRight),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    /// Renders known BT.709 limited-range YUV data through both an NV12
    /// texture (the VAAPI decode path) and an IYUV texture (the software
    /// decode path) and reads the composited pixels back.
    ///
    /// Catches platform/SDL regressions in NV12 rendering (colorspace,
    /// UV interleave, plane upload) that unit tests can't see.
    #[test]
    #[ignore = "requires a display (Wayland/X11) and a GPU renderer"]
    #[allow(clippy::similar_names)] // y/u/v/uv are the domain names
    fn nv12_and_iyuv_render_the_same_bt709_colors() {
        let (w, h) = (64u32, 608u32);

        // BT.709 limited-range encodings produced by the server's converter:
        // left half pure red, right half pure green.
        let red = (63u8, 102u8, 240u8);
        let green = (172u8, 41u8, 26u8);

        let mut y_plane = vec![0u8; (w * h) as usize];
        let mut u_plane = vec![0u8; (w * h / 4) as usize];
        let mut v_plane = vec![0u8; (w * h / 4) as usize];
        let mut uv_plane = vec![0u8; (w * h / 2) as usize];
        for row in 0..h as usize {
            for col in 0..w as usize {
                let (y, _, _) = if col < (w as usize) / 2 { red } else { green };
                y_plane[row * w as usize + col] = y;
            }
        }
        for row in 0..(h as usize) / 2 {
            for col in 0..(w as usize) / 2 {
                let (_, u, v) = if col < (w as usize) / 4 { red } else { green };
                u_plane[row * (w as usize / 2) + col] = u;
                v_plane[row * (w as usize / 2) + col] = v;
                uv_plane[row * w as usize + col * 2] = u;
                uv_plane[row * w as usize + col * 2 + 1] = v;
            }
        }

        let sdl = sdl2::init().expect("SDL init");
        // The stream is BT.709; SDL must not guess (sdl2-compat defaults
        // to BT.601 regardless of resolution).
        unsafe {
            sdl2::sys::SDL_SetYUVConversionMode(
                sdl2::sys::SDL_YUV_CONVERSION_MODE::SDL_YUV_CONVERSION_BT709,
            );
        }
        let video = sdl.video().expect("SDL video");
        let window = video
            .window("stargaze-nv12-test", w, h)
            .hidden()
            .build()
            .expect("window");
        let mut canvas = window.into_canvas().build().expect("canvas");
        eprintln!("SDL renderer: {}", canvas.info().name);
        let texture_creator = canvas.texture_creator();

        let mut readback = |format: sdl2::pixels::PixelFormatEnum| -> Vec<u8> {
            let mut tex = texture_creator
                .create_texture_streaming(format, w, h)
                .expect("texture");
            match format {
                sdl2::pixels::PixelFormatEnum::NV12 => {
                    let ret = unsafe {
                        sdl2::sys::SDL_UpdateNVTexture(
                            tex.raw(),
                            std::ptr::null(),
                            y_plane.as_ptr(),
                            w.cast_signed(),
                            uv_plane.as_ptr(),
                            w.cast_signed(),
                        )
                    };
                    assert_eq!(ret, 0, "SDL_UpdateNVTexture: {}", sdl2::get_error());
                }
                sdl2::pixels::PixelFormatEnum::IYUV => {
                    tex.update_yuv(
                        None,
                        &y_plane,
                        w as usize,
                        &u_plane,
                        w as usize / 2,
                        &v_plane,
                        w as usize / 2,
                    )
                    .expect("update_yuv");
                }
                _ => unreachable!(),
            }
            canvas.clear();
            canvas.copy(&tex, None, None).expect("copy");
            canvas
                .read_pixels(None, sdl2::pixels::PixelFormatEnum::RGB24)
                .expect("read_pixels")
        };

        let nv12 = readback(sdl2::pixels::PixelFormatEnum::NV12);
        let iyuv = readback(sdl2::pixels::PixelFormatEnum::IYUV);

        let sample = |buf: &[u8], x: usize, y: usize| -> (u8, u8, u8) {
            let o = (y * w as usize + x) * 3;
            (buf[o], buf[o + 1], buf[o + 2])
        };
        // Sample away from the half boundary (chroma filtering bleeds).
        let report = |name: &str, buf: &[u8]| {
            let r = sample(buf, 16, 300);
            let g = sample(buf, 48, 300);
            eprintln!(
                "{name}: red half -> {r:?} (want ~(255,0,0)), green half -> {g:?} (want ~(0,255,0))"
            );
            (r, g)
        };
        let (nv_r, nv_g) = report("NV12", &nv12);
        let (iy_r, iy_g) = report("IYUV", &iyuv);

        let close = |a: (u8, u8, u8), b: (u8, u8, u8)| {
            (i16::from(a.0) - i16::from(b.0)).abs() <= 12
                && (i16::from(a.1) - i16::from(b.1)).abs() <= 12
                && (i16::from(a.2) - i16::from(b.2)).abs() <= 12
        };
        assert!(close(iy_r, (255, 0, 0)), "IYUV red wrong: {iy_r:?}");
        assert!(close(iy_g, (0, 255, 0)), "IYUV green wrong: {iy_g:?}");
        assert!(close(nv_r, (255, 0, 0)), "NV12 red wrong: {nv_r:?}");
        assert!(close(nv_g, (0, 255, 0)), "NV12 green wrong: {nv_g:?}");
    }
}
