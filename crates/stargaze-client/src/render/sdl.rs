use anyhow::anyhow;
use sdl2::audio::AudioQueue;
use sdl2::controller::GameController;
use stargaze_core::decode::{DecodedFrame, DecoderConfig};
use stargaze_core::input::{GamepadAxis, GamepadButton, InputEvent, MouseButton};
use tracing::{info, warn};

use super::audio::create_audio_queue;

#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
pub(super) fn run_sdl_loop(
    sdl: &sdl2::Sdl,
    config: &DecoderConfig,
    decoded_rx: std::sync::mpsc::Receiver<DecodedFrame>,
    audio_pcm_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    fullscreen: bool,
    input_tx: std::sync::mpsc::Sender<InputEvent>,
) -> Result<(), anyhow::Error> {
    let audio_queue: AudioQueue<f32> = create_audio_queue(sdl)?;

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
        .present_vsync()
        .build()
        .map_err(|e| anyhow!("canvas creation failed: {e}"))?;

    let texture_creator = canvas.texture_creator();
    let mut texture = texture_creator
        .create_texture_streaming(
            sdl2::pixels::PixelFormatEnum::IYUV,
            config.width,
            config.height,
        )
        .map_err(|e| anyhow!("texture creation failed: {e}"))?;

    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| anyhow!("event pump failed: {e}"))?;

    sdl.mouse().set_relative_mouse_mode(true);

    let mut controllers: Vec<GameController> = Vec::new();
    let num_joysticks = game_controller_subsystem.num_joysticks().unwrap_or(0);
    for i in 0..num_joysticks {
        if game_controller_subsystem.is_game_controller(i) {
            match game_controller_subsystem.open(i) {
                Ok(controller) => {
                    info!(
                        name = controller.name(),
                        index = i,
                        "Opened game controller"
                    );
                    controllers.push(controller);
                }
                Err(e) => warn!(index = i, "Failed to open game controller: {e}"),
            }
        }
    }

    info!(
        "Renderer started: {}x{} (fullscreen: {}, controllers: {})",
        config.width,
        config.height,
        fullscreen,
        controllers.len()
    );

    'main: loop {
        for event in event_pump.poll_iter() {
            match event {
                sdl2::event::Event::Quit { .. } => break 'main,

                sdl2::event::Event::KeyDown {
                    scancode: Some(sc),
                    repeat: false,
                    ..
                } => {
                    if sc == sdl2::keyboard::Scancode::Escape {
                        break 'main;
                    }
                    let _ = input_tx.send(InputEvent::Keyboard {
                        scancode: sc as u32,
                        pressed: true,
                    });
                }

                sdl2::event::Event::KeyUp {
                    scancode: Some(sc),
                    repeat: false,
                    ..
                } => {
                    let _ = input_tx.send(InputEvent::Keyboard {
                        scancode: sc as u32,
                        pressed: false,
                    });
                }

                sdl2::event::Event::MouseMotion { xrel, yrel, .. } => {
                    let _ = input_tx.send(InputEvent::MouseMove { dx: xrel, dy: yrel });
                }

                sdl2::event::Event::MouseButtonDown { mouse_btn, .. } => {
                    if let Some(button) = map_mouse_button(mouse_btn) {
                        let _ = input_tx.send(InputEvent::MouseButton {
                            button,
                            pressed: true,
                        });
                    }
                }

                sdl2::event::Event::MouseButtonUp { mouse_btn, .. } => {
                    if let Some(button) = map_mouse_button(mouse_btn) {
                        let _ = input_tx.send(InputEvent::MouseButton {
                            button,
                            pressed: false,
                        });
                    }
                }

                sdl2::event::Event::MouseWheel { x, y, .. } => {
                    let _ = input_tx.send(InputEvent::MouseWheel { dx: x, dy: y });
                }

                sdl2::event::Event::ControllerAxisMotion { axis, value, .. } => {
                    let ga = map_gamepad_axis(axis);
                    let _ = input_tx.send(InputEvent::GamepadAxis { axis: ga, value });
                }

                sdl2::event::Event::ControllerButtonDown { button, .. } => {
                    if let Some(gb) = map_gamepad_button(button) {
                        let _ = input_tx.send(InputEvent::GamepadButton {
                            button: gb,
                            pressed: true,
                        });
                    }
                }

                sdl2::event::Event::ControllerButtonUp { button, .. } => {
                    if let Some(gb) = map_gamepad_button(button) {
                        let _ = input_tx.send(InputEvent::GamepadButton {
                            button: gb,
                            pressed: false,
                        });
                    }
                }

                sdl2::event::Event::ControllerDeviceAdded {
                    which: joystick_index,
                    ..
                } => {
                    if game_controller_subsystem.is_game_controller(joystick_index) {
                        match game_controller_subsystem.open(joystick_index) {
                            Ok(controller) => {
                                info!(
                                    name = controller.name(),
                                    index = joystick_index,
                                    "Game controller connected"
                                );
                                controllers.push(controller);
                            }
                            Err(e) => {
                                warn!(index = joystick_index, "Failed to open new controller: {e}");
                            }
                        }
                    }
                }

                _ => {}
            }
        }

        // Drain decoded video frames, keeping only the latest.
        let mut latest_frame: Option<DecodedFrame> = None;
        while let Ok(frame) = decoded_rx.try_recv() {
            latest_frame = Some(frame);
        }

        if let Some(frame) = latest_frame {
            static RENDER_DIAG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let rn = RENDER_DIAG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if rn < 5 {
                info!(
                    frame_n = rn,
                    frame_width = frame.width,
                    frame_height = frame.height,
                    texture_width = config.width,
                    texture_height = config.height,
                    y_plane_len = frame.y_plane.len(),
                    u_plane_len = frame.u_plane.len(),
                    v_plane_len = frame.v_plane.len(),
                    y_pitch = frame.width as usize,
                    chroma_pitch = frame.width as usize / 2,
                    "SDL render diagnostics"
                );
            }

            let y_pitch = frame.width as usize;
            let chroma_pitch = frame.width as usize / 2;

            texture
                .update_yuv(
                    None,
                    &frame.y_plane,
                    y_pitch,
                    &frame.u_plane,
                    chroma_pitch,
                    &frame.v_plane,
                    chroma_pitch,
                )
                .map_err(|e| anyhow!("texture update failed: {e}"))?;
        }

        // Always copy the texture to the canvas — the texture retains its
        // last-uploaded content, but the back buffer is undefined after
        // present() with double buffering, so we must re-copy every frame.
        canvas
            .copy(&texture, None, None)
            .map_err(|e| anyhow!("canvas copy failed: {e}"))?;

        // Drain decoded audio PCM and queue for playback.
        while let Ok(pcm) = audio_pcm_rx.try_recv() {
            if let Err(e) = audio_queue.queue_audio(&pcm) {
                warn!("Failed to queue audio: {e}");
                break;
            }
        }

        canvas.present();
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
