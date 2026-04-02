use anyhow::anyhow;
use stargaze_core::decode::{DecodedFrame, DecoderConfig};
use tracing::info;

#[allow(clippy::needless_pass_by_value)]
pub(super) fn run_sdl_loop(
    config: &DecoderConfig,
    decoded_rx: std::sync::mpsc::Receiver<DecodedFrame>,
    fullscreen: bool,
) -> Result<(), anyhow::Error> {
    let sdl = sdl2::init().map_err(|e| anyhow!("SDL2 init failed: {e}"))?;
    let video = sdl
        .video()
        .map_err(|e| anyhow!("SDL2 video init failed: {e}"))?;

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
            sdl2::pixels::PixelFormatEnum::NV12,
            config.width,
            config.height,
        )
        .map_err(|e| anyhow!("texture creation failed: {e}"))?;

    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| anyhow!("event pump failed: {e}"))?;

    info!(
        "Renderer started: {}x{} (fullscreen: {})",
        config.width, config.height, fullscreen
    );

    'main: loop {
        for event in event_pump.poll_iter() {
            match event {
                sdl2::event::Event::Quit { .. }
                | sdl2::event::Event::KeyDown {
                    keycode: Some(sdl2::keyboard::Keycode::Escape),
                    ..
                } => break 'main,
                _ => {}
            }
        }

        let mut latest_frame: Option<DecodedFrame> = None;
        while let Ok(frame) = decoded_rx.try_recv() {
            latest_frame = Some(frame);
        }

        if let Some(frame) = latest_frame {
            let width = frame.width as usize;

            texture
                .update(None, &frame.data, width)
                .map_err(|e| anyhow!("texture update failed: {e}"))?;

            canvas
                .copy(&texture, None, None)
                .map_err(|e| anyhow!("canvas copy failed: {e}"))?;
        }

        canvas.present();
    }

    info!("Renderer shutting down");
    Ok(())
}
