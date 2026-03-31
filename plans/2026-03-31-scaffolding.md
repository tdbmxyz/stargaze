# Scaffolding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Set up the Cargo workspace with server/client binaries and shared core crate, with CLI parsing, TOML config loading, and structured logging.

**Architecture:** Three-crate Cargo workspace (stargaze-core, stargaze-server, stargaze-client). Core holds config types and error types. Each binary parses CLI args via clap, loads a TOML config file with CLI overrides, initializes tracing, and logs a startup message before exiting.

**Tech Stack:** Rust 2024 edition (nightly), clap (derive), serde + toml, thiserror, anyhow, tokio, tracing + tracing-subscriber, directories

---

## File Structure

### New files to create

- `Cargo.toml` — rewrite as workspace manifest (replaces current single-package manifest)
- `crates/stargaze-core/Cargo.toml`
- `crates/stargaze-core/src/lib.rs` — re-exports config and error modules
- `crates/stargaze-core/src/config.rs` — `ServerConfig`, `ClientConfig`, `Resolution`, `Codec`, config loading
- `crates/stargaze-core/src/error.rs` — `StargazeError` enum
- `crates/stargaze-server/Cargo.toml`
- `crates/stargaze-server/src/main.rs` — server CLI, config loading, logging init
- `crates/stargaze-client/Cargo.toml`
- `crates/stargaze-client/src/main.rs` — client CLI, config loading, logging init

### Files to remove

- `src/main.rs` — replaced by the two binary crates

---

## Task 1: Convert to Cargo workspace

**Files:**
- Modify: `Cargo.toml`
- Remove: `src/main.rs`
- Create: `crates/stargaze-core/Cargo.toml`
- Create: `crates/stargaze-core/src/lib.rs`
- Create: `crates/stargaze-server/Cargo.toml`
- Create: `crates/stargaze-server/src/main.rs`
- Create: `crates/stargaze-client/Cargo.toml`
- Create: `crates/stargaze-client/src/main.rs`

- [ ] **Step 1: Rewrite root Cargo.toml as workspace manifest**

```toml
[workspace]
members = [
    "crates/stargaze-core",
    "crates/stargaze-server",
    "crates/stargaze-client",
]
resolver = "3"

[workspace.package]
version = "0.1.0"
edition = "2024"
```

- [ ] **Step 2: Create stargaze-core crate**

`crates/stargaze-core/Cargo.toml`:
```toml
[package]
name = "stargaze-core"
version.workspace = true
edition.workspace = true

[dependencies]
serde = { version = "1", features = ["derive"] }
toml = "0.8"
thiserror = "2"
tracing = "0.1"
directories = "6"
```

`crates/stargaze-core/src/lib.rs`:
```rust
pub mod config;
pub mod error;
```

- [ ] **Step 3: Create stargaze-server crate**

`crates/stargaze-server/Cargo.toml`:
```toml
[package]
name = "stargaze-server"
version.workspace = true
edition.workspace = true

[dependencies]
stargaze-core = { path = "../stargaze-core" }
anyhow = "1"
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

`crates/stargaze-server/src/main.rs`:
```rust
fn main() {
    println!("stargaze-server placeholder");
}
```

- [ ] **Step 4: Create stargaze-client crate**

`crates/stargaze-client/Cargo.toml`:
```toml
[package]
name = "stargaze-client"
version.workspace = true
edition.workspace = true

[dependencies]
stargaze-core = { path = "../stargaze-core" }
anyhow = "1"
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

`crates/stargaze-client/src/main.rs`:
```rust
fn main() {
    println!("stargaze-client placeholder");
}
```

- [ ] **Step 5: Remove old src/main.rs**

```bash
rm src/main.rs
rmdir src
```

- [ ] **Step 6: Verify workspace compiles**

Run: `cargo build`
Expected: Compiles all three crates successfully.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat: convert to Cargo workspace with core, server, and client crates"
```

---

## Task 2: Implement error types in stargaze-core

**Files:**
- Create: `crates/stargaze-core/src/error.rs`

- [ ] **Step 1: Write tests for error types**

Add to end of `crates/stargaze-core/src/error.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_error_display_invalid_codec() {
        let err = ConfigError::InvalidCodec("vp9".to_string());
        assert_eq!(err.to_string(), "Invalid codec: vp9. Supported: h265, av1");
    }

    #[test]
    fn test_config_error_display_invalid_resolution() {
        let err = ConfigError::InvalidResolution("bad".to_string());
        assert_eq!(
            err.to_string(),
            "Invalid resolution: bad. Expected format: WIDTHxHEIGHT (e.g. 1920x1080)"
        );
    }

    #[test]
    fn test_config_error_display_parse_error() {
        let err = ConfigError::ParseError {
            path: "/tmp/test.toml".to_string(),
            source: "invalid key".to_string(),
        };
        let display = err.to_string();
        assert!(display.contains("/tmp/test.toml"));
        assert!(display.contains("invalid key"));
    }

    #[test]
    fn test_config_error_display_read_error() {
        let err = ConfigError::ReadError {
            path: "/tmp/test.toml".to_string(),
            source: "permission denied".to_string(),
        };
        let display = err.to_string();
        assert!(display.contains("/tmp/test.toml"));
        assert!(display.contains("permission denied"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p stargaze-core`
Expected: FAIL — `ConfigError` type doesn't exist yet.

- [ ] **Step 3: Implement error types**

Write `crates/stargaze-core/src/error.rs`:

```rust
use thiserror::Error;

/// Errors related to configuration loading and parsing.
#[derive(Error, Debug)]
pub enum ConfigError {
    /// An unsupported codec was specified.
    #[error("Invalid codec: {0}. Supported: h265, av1")]
    InvalidCodec(String),

    /// An invalid resolution string was provided.
    #[error("Invalid resolution: {0}. Expected format: WIDTHxHEIGHT (e.g. 1920x1080)")]
    InvalidResolution(String),

    /// The config file exists but could not be parsed.
    #[error("Failed to parse config file {path}: {source}")]
    ParseError {
        path: String,
        source: String,
    },

    /// The config file could not be read.
    #[error("Failed to read config file {path}: {source}")]
    ReadError {
        path: String,
        source: String,
    },
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p stargaze-core`
Expected: All error tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(core): add error types for config loading"
```

---

## Task 3: Implement config types in stargaze-core

**Files:**
- Create: `crates/stargaze-core/src/config.rs`
- Modify: `crates/stargaze-core/src/lib.rs`

- [ ] **Step 1: Write tests for config types**

Add to end of `crates/stargaze-core/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_defaults() {
        let config = ServerConfig::default();
        assert_eq!(config.bind_address, "0.0.0.0");
        assert_eq!(config.port, 9000);
        assert_eq!(config.resolution, Resolution { width: 1920, height: 1080 });
        assert_eq!(config.framerate, 60);
        assert_eq!(config.bitrate, 20);
        assert_eq!(config.codec, Codec::H265);
    }

    #[test]
    fn test_client_config_defaults() {
        let config = ClientConfig::default();
        assert_eq!(config.server_address, "");
        assert_eq!(config.port, 9000);
        assert!(config.fullscreen);
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
        assert_eq!(config.resolution, Resolution { width: 2560, height: 1440 });
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
    }

    #[test]
    fn test_resolution_display() {
        let res = Resolution { width: 1920, height: 1080 };
        assert_eq!(res.to_string(), "1920x1080");
    }

    #[test]
    fn test_resolution_from_str() {
        let res: Resolution = "2560x1440".parse().unwrap();
        assert_eq!(res, Resolution { width: 2560, height: 1440 });
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
        // Should end with stargaze/server.toml regardless of platform
        let path_str = path.to_string_lossy();
        assert!(path_str.ends_with("stargaze/server.toml") || path_str.ends_with("stargaze\\server.toml"));
    }

    #[test]
    fn test_load_config_missing_file() {
        // Loading from a nonexistent file should return default config
        let config: ServerConfig = load_config(Some("/tmp/nonexistent_stargaze_test.toml")).unwrap();
        assert_eq!(config, ServerConfig::default());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p stargaze-core`
Expected: FAIL — `config` module is empty, types don't exist yet.

- [ ] **Step 3: Implement config types**

Write `crates/stargaze-core/src/config.rs`:

```rust
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::ConfigError;

/// Default port for stargaze server/client communication.
pub const DEFAULT_PORT: u16 = 9000;

/// Video codec selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    /// H.265 / HEVC
    H265,
    /// AV1
    Av1,
}

impl Default for Codec {
    fn default() -> Self {
        Self::H265
    }
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
        match s.to_lowercase().as_str() {
            "h265" | "hevc" => Ok(Self::H265),
            "av1" => Ok(Self::Av1),
            other => Err(ConfigError::InvalidCodec(other.to_string())),
        }
    }
}

/// Video resolution (width x height).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
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

/// Server configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Address to bind to.
    pub bind_address: String,
    /// Port to listen on.
    pub port: u16,
    /// Video resolution.
    pub resolution: Resolution,
    /// Target framerate.
    pub framerate: u32,
    /// Target bitrate in Mbps.
    pub bitrate: u32,
    /// Video codec.
    pub codec: Codec,
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
        }
    }
}

/// Client configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    /// Server address to connect to.
    pub server_address: String,
    /// Server port.
    pub port: u16,
    /// Whether to run in fullscreen mode.
    pub fullscreen: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server_address: String::new(),
            port: DEFAULT_PORT,
            fullscreen: true,
        }
    }
}

/// Returns the platform-appropriate config file path for stargaze.
///
/// Uses the `directories` crate to find `~/.config/stargaze/<name>.toml`
/// (or platform equivalent).
pub fn config_file_path(name: &str) -> PathBuf {
    directories::ProjectDirs::from("", "", "stargaze")
        .map(|dirs| dirs.config_dir().join(format!("{name}.toml")))
        .unwrap_or_else(|| PathBuf::from(format!("{name}.toml")))
}

/// Load a config from a TOML file path, returning defaults if the file doesn't exist.
///
/// # Errors
///
/// Returns `ConfigError::ParseError` if the file exists but contains invalid TOML.
pub fn load_config<T>(path: Option<&str>) -> Result<T, ConfigError>
where
    T: Default + serde::de::DeserializeOwned,
{
    let path = match path {
        Some(p) => PathBuf::from(p),
        None => return Ok(T::default()),
    };

    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            debug!("Loaded config from {}", path.display());
            toml::from_str(&contents).map_err(|e| ConfigError::ParseError {
                path: path.to_string_lossy().to_string(),
                source: e.to_string(),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!("Config file not found at {}, using defaults", path.display());
            Ok(T::default())
        }
        Err(e) => Err(ConfigError::ReadError {
            path: path.to_string_lossy().to_string(),
            source: e.to_string(),
        }),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p stargaze-core`
Expected: All tests pass.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p stargaze-core -- -W clippy::pedantic`
Expected: No warnings.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(core): add config types with TOML deserialization and defaults"
```

---

## Task 4: Implement server binary CLI and startup

**Files:**
- Modify: `crates/stargaze-server/src/main.rs`

- [ ] **Step 1: Implement server main.rs**

Write `crates/stargaze-server/src/main.rs`:

```rust
use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use stargaze_core::config::{self, Codec, Resolution, ServerConfig};

/// Stargaze streaming server.
///
/// Captures screen and audio, encodes them, and streams to a connected client.
#[derive(Parser, Debug)]
#[command(name = "stargaze-server", version, about)]
struct Cli {
    /// Bind address.
    #[arg(long, default_value = "0.0.0.0")]
    bind: Option<String>,

    /// Port to listen on.
    #[arg(long, default_value_t = config::DEFAULT_PORT)]
    port: Option<u16>,

    /// Video resolution (WIDTHxHEIGHT).
    #[arg(long, default_value = "1920x1080")]
    resolution: Option<Resolution>,

    /// Target framerate.
    #[arg(long, default_value_t = 60)]
    framerate: Option<u32>,

    /// Target bitrate in Mbps.
    #[arg(long, default_value_t = 20)]
    bitrate: Option<u32>,

    /// Video codec.
    #[arg(long, default_value = "h265")]
    codec: Option<Codec>,

    /// Path to config file.
    #[arg(long)]
    config: Option<String>,
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

fn build_config(cli: &Cli) -> Result<ServerConfig> {
    let config_path = cli.config.as_deref().or_else(|| {
        let default_path = config::config_file_path("server");
        if default_path.exists() {
            // Leak is acceptable here — this runs once at startup
            Some(&*Box::leak(default_path.to_string_lossy().into_owned().into_boxed_str()))
        } else {
            None
        }
    });

    let mut cfg: ServerConfig = config::load_config(config_path)?;

    // CLI overrides
    if let Some(ref bind) = cli.bind {
        cfg.bind_address = bind.clone();
    }
    if let Some(port) = cli.port {
        cfg.port = port;
    }
    if let Some(resolution) = cli.resolution {
        cfg.resolution = resolution;
    }
    if let Some(framerate) = cli.framerate {
        cfg.framerate = framerate;
    }
    if let Some(bitrate) = cli.bitrate {
        cfg.bitrate = bitrate;
    }
    if let Some(codec) = cli.codec {
        cfg.codec = codec;
    }

    Ok(cfg)
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let config = build_config(&cli)?;

    info!(
        "Starting stargaze server on {}:{} ({}@{}fps, {} Mbps, {})",
        config.bind_address,
        config.port,
        config.resolution,
        config.framerate,
        config.bitrate,
        config.codec,
    );

    // Nothing to do yet — streaming will be added in later sub-projects.

    Ok(())
}
```

- [ ] **Step 2: Verify it compiles and runs**

Run: `cargo build -p stargaze-server`
Expected: Compiles.

Run: `cargo run -p stargaze-server -- --help`
Expected: Prints help text showing all flags.

Run: `cargo run -p stargaze-server`
Expected: Logs "Starting stargaze server on 0.0.0.0:9000 (1920x1080@60fps, 20 Mbps, h265)" and exits.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat(server): add CLI parsing, config loading, and logging"
```

---

## Task 5: Implement client binary CLI and startup

**Files:**
- Modify: `crates/stargaze-client/src/main.rs`

- [ ] **Step 1: Implement client main.rs**

Write `crates/stargaze-client/src/main.rs`:

```rust
use anyhow::{bail, Result};
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use stargaze_core::config::{self, ClientConfig};

/// Stargaze streaming client.
///
/// Connects to a stargaze server, receives and displays video/audio,
/// and forwards local input back to the server.
#[derive(Parser, Debug)]
#[command(name = "stargaze-client", version, about)]
struct Cli {
    /// Server address to connect to.
    #[arg(long)]
    server: Option<String>,

    /// Server port.
    #[arg(long, default_value_t = config::DEFAULT_PORT)]
    port: Option<u16>,

    /// Run in fullscreen mode.
    #[arg(long, default_value_t = true)]
    fullscreen: Option<bool>,

    /// Path to config file.
    #[arg(long)]
    config: Option<String>,
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

fn build_config(cli: &Cli) -> Result<ClientConfig> {
    let config_path = cli.config.as_deref().or_else(|| {
        let default_path = config::config_file_path("client");
        if default_path.exists() {
            Some(&*Box::leak(default_path.to_string_lossy().into_owned().into_boxed_str()))
        } else {
            None
        }
    });

    let mut cfg: ClientConfig = config::load_config(config_path)?;

    // CLI overrides
    if let Some(ref server) = cli.server {
        cfg.server_address = server.clone();
    }
    if let Some(port) = cli.port {
        cfg.port = port;
    }
    if let Some(fullscreen) = cli.fullscreen {
        cfg.fullscreen = fullscreen;
    }

    if cfg.server_address.is_empty() {
        bail!("Server address is required. Use --server <ADDR> or set server_address in config file.");
    }

    Ok(cfg)
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let config = build_config(&cli)?;

    info!(
        "Connecting to {}:{} (fullscreen: {})",
        config.server_address, config.port, config.fullscreen,
    );

    // Nothing to do yet — streaming will be added in later sub-projects.

    Ok(())
}
```

- [ ] **Step 2: Verify it compiles and runs**

Run: `cargo build -p stargaze-client`
Expected: Compiles.

Run: `cargo run -p stargaze-client -- --help`
Expected: Prints help text.

Run: `cargo run -p stargaze-client -- --server 127.0.0.1`
Expected: Logs "Connecting to 127.0.0.1:9000 (fullscreen: true)" and exits.

Run: `cargo run -p stargaze-client`
Expected: Errors with "Server address is required."

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat(client): add CLI parsing, config loading, and logging"
```

---

## Task 6: Final quality checks

**Files:** None (verification only)

- [ ] **Step 1: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 2: Run clippy on entire workspace**

Run: `cargo clippy --workspace -- -W clippy::pedantic`
Expected: No warnings. If there are warnings, fix them.

- [ ] **Step 3: Run formatter**

Run: `cargo fmt --all`
Then: `cargo fmt --all --check`
Expected: No formatting changes needed.

- [ ] **Step 4: Verify server and client run correctly**

Run: `cargo run -p stargaze-server`
Expected: Logs startup message with defaults and exits cleanly.

Run: `cargo run -p stargaze-server -- --port 8080 --codec av1 --resolution 2560x1440`
Expected: Logs "Starting stargaze server on 0.0.0.0:8080 (2560x1440@60fps, 20 Mbps, av1)".

Run: `cargo run -p stargaze-client -- --server 192.168.1.10 --port 8080`
Expected: Logs "Connecting to 192.168.1.10:8080 (fullscreen: true)".

- [ ] **Step 5: Commit any fixes from quality checks**

Only if Step 2 or 3 produced changes:
```bash
git add -A
git commit -m "chore: fix clippy warnings and formatting"
```
