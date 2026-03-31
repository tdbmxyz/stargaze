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
    #[error("Failed to parse config file {path}: {reason}")]
    ParseError {
        /// Path to the config file.
        path: String,
        /// Description of the parse error.
        reason: String,
    },

    /// The config file could not be read.
    #[error("Failed to read config file {path}: {reason}")]
    ReadError {
        /// Path to the config file.
        path: String,
        /// Description of the read error.
        reason: String,
    },
}

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
            reason: "invalid key".to_string(),
        };
        let display = err.to_string();
        assert!(display.contains("/tmp/test.toml"));
        assert!(display.contains("invalid key"));
    }

    #[test]
    fn test_config_error_display_read_error() {
        let err = ConfigError::ReadError {
            path: "/tmp/test.toml".to_string(),
            reason: "permission denied".to_string(),
        };
        let display = err.to_string();
        assert!(display.contains("/tmp/test.toml"));
        assert!(display.contains("permission denied"));
    }
}
