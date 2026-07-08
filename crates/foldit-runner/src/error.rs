//! Unified error type for the foldit-runner crate

use thiserror::Error;

/// Unified error type for all foldit-runner operations
#[derive(Debug, Error)]
pub enum RunnerError {
    /// Protobuf serialization/deserialization errors
    #[error("Protobuf error: {0}")]
    Protobuf(#[from] prost::DecodeError),

    /// Operation not supported by this implementation
    #[error("Operation not supported")]
    Unsupported,

    /// IO errors (file system, network, etc.)
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Generic error with message
    #[error("{0}")]
    Generic(String),

    /// Plugin returned a structured `proto::Error`. Code is matched
    /// against well-known names (e.g. `"STALE_GEN"`) by the orchestrator
    /// for protocol-level recovery — keeping it structured (not folded
    /// into the message string) is the whole point of the variant.
    #[error("plugin error [{code}] {message}")]
    PluginError {
        /// Machine-readable error code (e.g. `"STALE_GEN"`).
        code: String,
        /// Human-readable error message from the plugin.
        message: String,
    },
}

impl From<anyhow::Error> for RunnerError {
    fn from(err: anyhow::Error) -> Self {
        RunnerError::Generic(err.to_string())
    }
}

impl From<foldit_plugin_sdk::PluginError> for RunnerError {
    fn from(err: foldit_plugin_sdk::PluginError) -> Self {
        use foldit_plugin_sdk::PluginError as Pe;
        match err {
            // Keep the code structured so STALE_GEN recovery still matches.
            Pe::Op { code, message } => {
                RunnerError::PluginError { code, message }
            }
            Pe::Unsupported => RunnerError::Unsupported,
            Pe::Decode(e) => RunnerError::Protobuf(e),
            Pe::Other(s) => RunnerError::Generic(s),
        }
    }
}

impl From<String> for RunnerError {
    fn from(s: String) -> Self {
        RunnerError::Generic(s)
    }
}

impl From<&str> for RunnerError {
    fn from(s: &str) -> Self {
        RunnerError::Generic(String::from(s))
    }
}

/// Convenience type alias for Result with RunnerError
pub type Result<T> = std::result::Result<T, RunnerError>;
