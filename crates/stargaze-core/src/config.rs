use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use directories::ProjectDirs;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Default port for stargaze server/client communication.
pub const DEFAULT_PORT: u16 = 9000;

// --- Codec ---

/// Supported video codecs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    /// H.265 / HEVC codec.
    #[default]
    H265,
    /// AV1 codec.
    Av1,
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::H265 => write!(f, "h265"),
            Self::Av1 => write!(f, "av1"),
        }
    }
}

impl FromStr for Codec {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "h265" => Ok(Self::H265),
            "av1" => Ok(Self::Av1),
            other => Err(ConfigError::InvalidCodec(other.to_string())),
        }
    }
}

// --- Resolution ---

/// Display resolution (width x height).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Default for Resolution {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
        }
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

impl FromStr for Resolution {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('x').collect();
        if parts.len() != 2 {
            return Err(ConfigError::InvalidResolution(s.to_string()));
        }
        let width = parts[0]
            .parse::<u32>()
            .map_err(|_| ConfigError::InvalidResolution(s.to_string()))?;
        let height = parts[1]
            .parse::<u32>()
            .map_err(|_| ConfigError::InvalidResolution(s.to_string()))?;
        Ok(Self { width, height })
    }
}

// --- CursorConfig ---

/// Configuration for cursor rendering in the captured stream.
///
/// Controls whether the cursor is composited into captured video frames
/// by the Wayland compositor (via `CursorMode::Embedded`).
///
/// When `show_cursor` is `true` (default), the compositor embeds the cursor
/// into each captured frame. When `false`, frames contain no cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CursorConfig {
    /// Whether the cursor should be visible in the captured stream.
    ///
    /// - `true` (default): cursor is composited into video frames
    ///   (`CursorMode::Embedded`)
    /// - `false`: cursor is hidden from capture (`CursorMode::Hidden`)
    pub show_cursor: bool,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self { show_cursor: true }
    }
}

// --- MicForwardConfig ---

/// Default port for rsonance mic forwarding (separate from stargaze's QUIC port).
pub const DEFAULT_MIC_FORWARD_PORT: u16 = 9001;

/// Configuration for microphone forwarding via rsonance subprocess.
///
/// When enabled, stargaze spawns `rsonance receiver` on the server and
/// `rsonance transmitter` on the client to forward the client's microphone
/// audio to a virtual `PulseAudio` microphone on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MicForwardConfig {
    /// Whether mic forwarding is enabled.
    pub enabled: bool,
    /// TCP port for rsonance audio streaming.
    pub port: u16,
    /// Path to the rsonance binary.
    pub rsonance_binary: String,
}

impl Default for MicForwardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_MIC_FORWARD_PORT,
            rsonance_binary: "rsonance".to_string(),
        }
    }
}

// --- EncoderTuning ---

/// NVENC encoder tuning knobs.
///
/// The defaults balance quality and speed for 1440p-class streaming; drop
/// the preset (e.g. `p3`) or set `multipass = "disabled"` if the encoder
/// becomes the pipeline bottleneck (visible as high `encode` time in the
/// client stats overlay).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EncoderTuning {
    /// NVENC preset: `p1` (fastest) … `p7` (best quality).
    pub preset: String,
    /// NVENC multipass mode: `disabled`, `qres`, or `fullres`.
    pub multipass: String,
}

impl Default for EncoderTuning {
    fn default() -> Self {
        Self {
            preset: "p4".to_string(),
            multipass: "qres".to_string(),
        }
    }
}

// --- ServerConfig ---

/// Configuration for the stargaze server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Address to bind the server to.
    pub bind_address: String,
    /// Port to listen on.
    pub port: u16,
    /// Capture/stream resolution.
    pub resolution: Resolution,
    /// Target framerate.
    pub framerate: u32,
    /// Target bitrate in Mbps.
    pub bitrate: u32,
    /// Video codec to use.
    pub codec: Codec,
    /// NVENC encoder tuning.
    pub encoder: EncoderTuning,
    /// Mic forwarding configuration.
    pub mic_forward: MicForwardConfig,
    /// Cursor rendering configuration.
    pub cursor: CursorConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".to_string(),
            port: DEFAULT_PORT,
            resolution: Resolution::default(),
            framerate: 60,
            bitrate: 20,
            codec: Codec::default(),
            encoder: EncoderTuning::default(),
            mic_forward: MicForwardConfig::default(),
            cursor: CursorConfig::default(),
        }
    }
}

// --- ClientConfig ---

/// Configuration for the stargaze client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    /// Address of the server to connect to.
    pub server_address: String,
    /// Port to connect on.
    pub port: u16,
    /// Whether to start in fullscreen mode.
    pub fullscreen: bool,
    /// Mic forwarding configuration.
    pub mic_forward: MicForwardConfig,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server_address: String::new(),
            port: DEFAULT_PORT,
            fullscreen: true,
            mic_forward: MicForwardConfig::default(),
        }
    }
}

// --- Helper functions ---

/// Returns the path to a config file for the given component name.
///
/// The file is located in the system config directory under `stargaze/`,
/// e.g. `~/.config/stargaze/server.toml` on Linux.
///
/// # Panics
///
/// Panics if the system has no valid home directory.
#[must_use]
pub fn config_file_path(name: &str) -> PathBuf {
    let proj_dirs =
        ProjectDirs::from("", "", "stargaze").expect("could not determine config directory");
    proj_dirs.config_dir().join(format!("{name}.toml"))
}

/// Loads a configuration from a TOML file.
///
/// If `path` is `Some`, loads from that path. If `None`, uses the default
/// config file path for the type. If the file does not exist, returns the
/// default configuration.
///
/// # Errors
///
/// Returns [`ConfigError::ReadError`] if the file exists but cannot be read,
/// or [`ConfigError::ParseError`] if the file contents are invalid TOML.
pub fn load_config<T>(path: Option<&str>) -> Result<T, ConfigError>
where
    T: Default + DeserializeOwned,
{
    let path_str = match path {
        Some(p) => p.to_string(),
        None => return Ok(T::default()),
    };

    let contents = match std::fs::read_to_string(&path_str) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(e) => {
            return Err(ConfigError::ReadError {
                path: path_str,
                reason: e.to_string(),
            });
        }
    };

    toml::from_str(&contents).map_err(|e| ConfigError::ParseError {
        path: path_str,
        reason: e.to_string(),
    })
}

/// Returns this process's command line with network-identifying arguments
/// (`--bind`, `--server`, `--port`, `--mic-forward-port` and their values)
/// removed and the binary path reduced to its file name.
///
/// Used to annotate session diagnostics (stats reports) without leaking
/// addresses or ports.
#[must_use]
pub fn sanitized_command_line() -> String {
    sanitize_command_line(std::env::args())
}

fn sanitize_command_line(args: impl Iterator<Item = String>) -> String {
    const OMITTED: &[&str] = &["--bind", "--server", "--port", "--mic-forward-port"];

    let mut out: Vec<String> = Vec::new();
    let mut skip_value = false;
    for (i, arg) in args.enumerate() {
        if skip_value {
            skip_value = false;
            continue;
        }
        if i == 0 {
            let name = std::path::Path::new(&arg)
                .file_name()
                .map_or_else(|| arg.clone(), |n| n.to_string_lossy().into_owned());
            out.push(name);
            continue;
        }
        // Match both `--flag value` and `--flag=value` forms.
        let flag = arg.split('=').next().unwrap_or(&arg);
        if OMITTED.contains(&flag) {
            skip_value = !arg.contains('=');
            continue;
        }
        out.push(arg);
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_defaults() {
        let config = ServerConfig::default();
        assert_eq!(config.bind_address, "0.0.0.0");
        assert_eq!(config.port, 9000);
        assert_eq!(
            config.resolution,
            Resolution {
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(config.framerate, 60);
        assert_eq!(config.bitrate, 20);
        assert_eq!(config.codec, Codec::H265);
        assert!(!config.mic_forward.enabled);
        assert_eq!(config.mic_forward.port, 9001);
        assert_eq!(config.mic_forward.rsonance_binary, "rsonance");
        assert!(config.cursor.show_cursor);
        assert_eq!(config.encoder.preset, "p4");
        assert_eq!(config.encoder.multipass, "qres");
    }

    #[test]
    fn test_encoder_tuning_from_toml() {
        let toml = r#"
            [encoder]
            preset = "p2"
            multipass = "disabled"
        "#;
        let config: ServerConfig = toml::from_str(toml).expect("parse");
        assert_eq!(config.encoder.preset, "p2");
        assert_eq!(config.encoder.multipass, "disabled");
        // Unspecified fields keep their defaults.
        assert_eq!(config.framerate, 60);
    }

    #[test]
    fn test_encoder_tuning_defaults_when_absent() {
        let toml = "port = 1234";
        let config: ServerConfig = toml::from_str(toml).expect("parse");
        assert_eq!(config.port, 1234);
        assert_eq!(config.encoder.preset, "p4");
        assert_eq!(config.encoder.multipass, "qres");
    }

    #[test]
    fn test_client_config_defaults() {
        let config = ClientConfig::default();
        assert_eq!(config.server_address, "");
        assert_eq!(config.port, 9000);
        assert!(config.fullscreen);
        assert!(!config.mic_forward.enabled);
        assert_eq!(config.mic_forward.port, 9001);
    }

    #[test]
    fn test_server_config_from_toml() {
        let toml_str = r#"
            bind_address = "192.168.1.1"
            port = 8080
            framerate = 30
            bitrate = 50
            codec = "av1"

            [resolution]
            width = 2560
            height = 1440
        "#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.bind_address, "192.168.1.1");
        assert_eq!(config.port, 8080);
        assert_eq!(
            config.resolution,
            Resolution {
                width: 2560,
                height: 1440
            }
        );
        assert_eq!(config.framerate, 30);
        assert_eq!(config.bitrate, 50);
        assert_eq!(config.codec, Codec::Av1);
    }

    #[test]
    fn test_client_config_from_toml() {
        let toml_str = r#"
            server_address = "10.0.0.5"
            port = 7000
            fullscreen = false
        "#;
        let config: ClientConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server_address, "10.0.0.5");
        assert_eq!(config.port, 7000);
        assert!(!config.fullscreen);
        // mic_forward should use defaults when absent from TOML.
        assert!(!config.mic_forward.enabled);
        assert_eq!(config.mic_forward.port, 9001);
    }

    #[test]
    fn test_server_config_partial_toml_uses_defaults() {
        let toml_str = r#"
            port = 3000
        "#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.bind_address, "0.0.0.0");
        assert_eq!(config.port, 3000);
        assert_eq!(config.framerate, 60);
        assert!(!config.mic_forward.enabled);
        assert!(config.cursor.show_cursor);
    }

    #[test]
    fn test_mic_forward_config_defaults() {
        let config = MicForwardConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.port, DEFAULT_MIC_FORWARD_PORT);
        assert_eq!(config.rsonance_binary, "rsonance");
    }

    #[test]
    fn test_server_config_with_mic_forward_toml() {
        let toml_str = r#"
            bind_address = "0.0.0.0"
            port = 9000

            [mic_forward]
            enabled = true
            port = 8888
            rsonance_binary = "/usr/local/bin/rsonance"
        "#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert!(config.mic_forward.enabled);
        assert_eq!(config.mic_forward.port, 8888);
        assert_eq!(
            config.mic_forward.rsonance_binary,
            "/usr/local/bin/rsonance"
        );
    }

    #[test]
    fn test_client_config_with_mic_forward_toml() {
        let toml_str = r#"
            server_address = "192.168.1.10"
            port = 9000

            [mic_forward]
            enabled = true
            port = 7777
        "#;
        let config: ClientConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server_address, "192.168.1.10");
        assert!(config.mic_forward.enabled);
        assert_eq!(config.mic_forward.port, 7777);
        // rsonance_binary should default when not specified.
        assert_eq!(config.mic_forward.rsonance_binary, "rsonance");
    }

    #[test]
    fn test_resolution_display() {
        let res = Resolution {
            width: 1920,
            height: 1080,
        };
        assert_eq!(res.to_string(), "1920x1080");
    }

    #[test]
    fn test_resolution_from_str() {
        let res: Resolution = "2560x1440".parse().unwrap();
        assert_eq!(
            res,
            Resolution {
                width: 2560,
                height: 1440
            }
        );
    }

    #[test]
    fn test_resolution_from_str_invalid() {
        assert!("not_a_resolution".parse::<Resolution>().is_err());
        assert!("1920".parse::<Resolution>().is_err());
        assert!("1920xabc".parse::<Resolution>().is_err());
    }

    #[test]
    fn test_codec_display() {
        assert_eq!(Codec::H265.to_string(), "h265");
        assert_eq!(Codec::Av1.to_string(), "av1");
    }

    #[test]
    fn test_codec_from_str() {
        assert_eq!("h265".parse::<Codec>().unwrap(), Codec::H265);
        assert_eq!("av1".parse::<Codec>().unwrap(), Codec::Av1);
        assert!("vp9".parse::<Codec>().is_err());
    }

    #[test]
    fn test_config_file_path_server() {
        let path = config_file_path("server");
        let path_str = path.to_string_lossy();
        assert!(
            path_str.ends_with("stargaze/server.toml")
                || path_str.ends_with("stargaze\\server.toml")
        );
    }

    #[test]
    fn test_load_config_missing_file() {
        let config: ServerConfig =
            load_config(Some("/tmp/nonexistent_stargaze_test.toml")).unwrap();
        assert_eq!(config, ServerConfig::default());
    }

    #[test]
    fn test_cursor_config_defaults() {
        let config = CursorConfig::default();
        assert!(config.show_cursor);
    }

    #[test]
    fn test_server_config_with_cursor_toml() {
        let toml_str = r#"
            bind_address = "0.0.0.0"
            port = 9000

            [cursor]
            show_cursor = false
        "#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.cursor.show_cursor);
    }

    #[test]
    fn test_server_config_cursor_default_when_absent() {
        let toml_str = r#"
            port = 3000
        "#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert!(config.cursor.show_cursor);
    }

    #[test]
    fn sanitize_command_line_strips_addresses_and_ports() {
        let args = [
            "/nix/store/abc/bin/stargaze-server",
            "--bind",
            "0.0.0.0",
            "--port",
            "60003",
            "--resolution",
            "3440x1440",
            "--bitrate",
            "60",
            "--preset",
            "p1",
        ];
        let line = sanitize_command_line(args.iter().map(ToString::to_string));
        assert_eq!(
            line,
            "stargaze-server --resolution 3440x1440 --bitrate 60 --preset p1"
        );
    }

    #[test]
    fn sanitize_command_line_strips_equals_form_and_trailing_flag() {
        let args = [
            "stargaze-client",
            "--server=192.168.1.10",
            "--fullscreen",
            "true",
            "--port",
        ];
        let line = sanitize_command_line(args.iter().map(ToString::to_string));
        assert_eq!(line, "stargaze-client --fullscreen true");
    }
}
