//! Bridges `FFmpeg`'s `av_log` output into the `tracing` subscriber.
//!
//! By default libavcodec/libavutil write their diagnostics straight to
//! stderr, bypassing `tracing` entirely — the raw `[hevc @ 0x...] Could
//! not find ref with POC N` lines seen during loss recovery come from
//! there. Installing this bridge gives those messages timestamps, a
//! `ffmpeg` target (filterable with `RUST_LOG=ffmpeg=off` or
//! `ffmpeg=error`), and levels consistent with the rest of the logs.
//!
//! Level mapping: `FFmpeg` `ERROR` is deliberately demoted to `WARN`.
//! In a lossy streaming pipeline most libavcodec "errors" (missing
//! reference frames after a dropped packet) are expected, recoverable
//! events — the receiver already detects the loss and requests an IDR.
//! Only `FATAL`/`PANIC` map to `ERROR`.

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_int, c_void};

use ffmpeg_sys_next::{
    __va_list_tag, AV_LOG_DEBUG, AV_LOG_FATAL, AV_LOG_INFO, AV_LOG_VERBOSE, AV_LOG_WARNING,
    av_log_format_line2, av_log_set_callback,
};
use tracing::{Level, debug, error, event_enabled, info, trace, warn};

/// Target used for all forwarded `FFmpeg` log events.
const TARGET: &str = "ffmpeg";

/// Formatted-line buffer size; matches `FFmpeg`'s own default callback.
const LINE_SIZE: usize = 1024;

thread_local! {
    /// Per-thread reassembly state: FFmpeg emits partial lines (no
    /// trailing newline) that continue in the next call, and
    /// `av_log_format_line2` needs a persistent `print_prefix` flag to
    /// know whether the `[hevc @ ...]` prefix has been printed yet.
    static LOG_STATE: RefCell<LogState> = const {
        RefCell::new(LogState {
            buffer: String::new(),
            print_prefix: 1,
        })
    };
}

struct LogState {
    buffer: String,
    print_prefix: c_int,
}

/// Routes `FFmpeg`'s global log output through `tracing`.
///
/// Call once at startup, after the tracing subscriber is initialized
/// and before any `FFmpeg` API is used. Process-wide: covers every
/// `FFmpeg` context (decoder, encoder, hwaccel) in the binary.
pub fn install_ffmpeg_log_bridge() {
    unsafe { av_log_set_callback(Some(tracing_log_callback)) };
}

/// Maps an `FFmpeg` log level to the `tracing` level used when emitting.
fn map_level(level: c_int) -> Level {
    match level {
        l if l <= AV_LOG_FATAL => Level::ERROR,  // PANIC + FATAL
        l if l <= AV_LOG_WARNING => Level::WARN, // ERROR (see module doc) + WARNING
        l if l <= AV_LOG_INFO => Level::INFO,
        l if l <= AV_LOG_VERBOSE => Level::DEBUG,
        _ => Level::TRACE, // DEBUG + TRACE
    }
}

fn emit(level: Level, message: &str) {
    match level {
        Level::ERROR => error!(target: TARGET, "{message}"),
        Level::WARN => warn!(target: TARGET, "{message}"),
        Level::INFO => info!(target: TARGET, "{message}"),
        Level::DEBUG => debug!(target: TARGET, "{message}"),
        Level::TRACE => trace!(target: TARGET, "{message}"),
    }
}

fn level_enabled(level: Level) -> bool {
    match level {
        Level::ERROR => event_enabled!(target: TARGET, Level::ERROR),
        Level::WARN => event_enabled!(target: TARGET, Level::WARN),
        Level::INFO => event_enabled!(target: TARGET, Level::INFO),
        Level::DEBUG => event_enabled!(target: TARGET, Level::DEBUG),
        Level::TRACE => event_enabled!(target: TARGET, Level::TRACE),
    }
}

unsafe extern "C" fn tracing_log_callback(
    avcl: *mut c_void,
    level: c_int,
    fmt: *const c_char,
    vl: *mut __va_list_tag,
) {
    // Silently discarding a message is always safe; never unwind into C.
    let _ = std::panic::catch_unwind(|| {
        if level > AV_LOG_DEBUG || fmt.is_null() {
            return;
        }
        let tracing_level = map_level(level);
        if !level_enabled(tracing_level) {
            return;
        }

        LOG_STATE.with(|state| {
            let Ok(mut state) = state.try_borrow_mut() else {
                return; // re-entrant call (tracing itself logged via FFmpeg?)
            };

            let mut line = [0 as c_char; LINE_SIZE];
            let rc = unsafe {
                av_log_format_line2(
                    avcl,
                    level,
                    fmt,
                    vl,
                    line.as_mut_ptr(),
                    LINE_SIZE.try_into().unwrap_or(c_int::MAX),
                    &raw mut state.print_prefix,
                )
            };
            if rc < 0 {
                return;
            }
            let formatted = unsafe { CStr::from_ptr(line.as_ptr()) }.to_string_lossy();
            state.buffer.push_str(&formatted);

            // Emit only completed lines; keep any partial tail for the
            // next callback invocation on this thread.
            while let Some(newline) = state.buffer.find('\n') {
                let message: String = state.buffer.drain(..=newline).collect();
                let message = message.trim_end();
                if !message.is_empty() {
                    emit(tracing_level, message);
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffmpeg_sys_next::av_log;
    use std::ffi::CString;

    #[test]
    fn map_level_covers_ffmpeg_range() {
        use ffmpeg_sys_next::{AV_LOG_ERROR, AV_LOG_PANIC, AV_LOG_TRACE};
        assert_eq!(map_level(AV_LOG_PANIC), Level::ERROR);
        assert_eq!(map_level(AV_LOG_FATAL), Level::ERROR);
        assert_eq!(map_level(AV_LOG_ERROR), Level::WARN);
        assert_eq!(map_level(AV_LOG_WARNING), Level::WARN);
        assert_eq!(map_level(AV_LOG_INFO), Level::INFO);
        assert_eq!(map_level(AV_LOG_VERBOSE), Level::DEBUG);
        assert_eq!(map_level(AV_LOG_DEBUG), Level::TRACE);
        assert_eq!(map_level(AV_LOG_TRACE), Level::TRACE);
    }

    /// Smoke test: the callback must survive real varargs traffic from
    /// `FFmpeg`, including partial lines, without crashing.
    #[test]
    fn bridge_handles_av_log_calls() {
        install_ffmpeg_log_bridge();
        let full = CString::new("stargaze avlog test %d\n").unwrap();
        let partial = CString::new("partial ").unwrap();
        let rest = CString::new("rest %s\n").unwrap();
        let arg = CString::new("done").unwrap();
        unsafe {
            av_log(std::ptr::null_mut(), AV_LOG_INFO, full.as_ptr(), 42i32);
            av_log(std::ptr::null_mut(), AV_LOG_WARNING, partial.as_ptr());
            av_log(
                std::ptr::null_mut(),
                AV_LOG_WARNING,
                rest.as_ptr(),
                arg.as_ptr(),
            );
        }
    }
}
