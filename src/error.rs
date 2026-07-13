//! OmniAgent error types.
//!
//! A fixed error enum covering all failure modes across the codebase.
//! Replaces ad-hoc anyhow usage and unwrap() calls with typed, matchable errors.
//! Standard errors (std::io::Error, sqlx::Error, etc.) get automatic From conversions
//! for ergonomic `?` usage.

use std::fmt;

/// Alias for `Result<T, Error>` used throughout the crate.
pub type AppResult<T> = std::result::Result<T, Error>;

/// OmniAgent error enum.
///
/// Every production error path should produce one of these variants.
/// Test code may still use `.unwrap()` freely - panicking on test failure is acceptable.
#[derive(Debug)]
pub enum Error {
    // ── Wrapped external errors ──
    Io(std::io::Error),
    Sqlx(sqlx::Error),
    SerdeJson(serde_json::Error),
    SerdeYaml(serde_yaml::Error),
    Join(tokio::task::JoinError),
    Reqwest(reqwest::Error),
    ChronoParse(chrono::ParseError),
    Regex(regex::Error),

    // ── Domain errors ──
    /// An Option was None / a nullable field was null when a value was expected.
    NotNullExpected,

    /// A named action was not found in the actions file.
    ActionNotFound(String),

    /// A named plugin was not found.
    PluginNotFound(String),

    /// A channel was not found by its numeric id.
    ChannelNotFound(i64),

    /// A cron schedule was not found.
    ScheduleNotFound(String),

    /// Invalid thread cause string (must be "user" or "system").
    InvalidThreadCause(String),

    /// SQL query returned no rows when rows were expected.
    NotFound,

    /// A lock (Mutex, RwLock) is poisoned.
    LockPoisoned,

    /// MCP protocol-level error.
    McpProtocol(String),

    // ── Generic string message ──
    /// Catch-all for string-based error messages.
    Message(String),
}

// ── From impls: convert external error types into our Error ──

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::Sqlx(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::SerdeJson(e)
    }
}

impl From<serde_yaml::Error> for Error {
    fn from(e: serde_yaml::Error) -> Self {
        Error::SerdeYaml(e)
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Self {
        Error::Join(e)
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Reqwest(e)
    }
}

impl From<chrono::ParseError> for Error {
    fn from(e: chrono::ParseError) -> Self {
        Error::ChronoParse(e)
    }
}

impl From<regex::Error> for Error {
    fn from(e: regex::Error) -> Self {
        Error::Regex(e)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::Message(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error::Message(s.to_string())
    }
}

// ── Display: human-readable error messages ──

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {}", e),
            Error::Sqlx(e) => write!(f, "Database error: {}", e),
            Error::SerdeJson(e) => write!(f, "JSON error: {}", e),
            Error::SerdeYaml(e) => write!(f, "YAML error: {}", e),
            Error::Join(e) => write!(f, "Task join error: {}", e),
            Error::Reqwest(e) => write!(f, "HTTP request error: {}", e),
            Error::ChronoParse(e) => write!(f, "Timestamp parse error: {}", e),
            Error::Regex(e) => write!(f, "Regex error: {}", e),
            Error::NotNullExpected => write!(f, "Unexpected null value"),
            Error::ActionNotFound(name) => write!(f, "Action '{}' not found", name),
            Error::PluginNotFound(name) => write!(f, "Plugin '{}' not found", name),
            Error::ChannelNotFound(id) => write!(f, "Channel {} not found", id),
            Error::ScheduleNotFound(id) => write!(f, "Cron job '{}' not found", id),
            Error::InvalidThreadCause(s) => write!(
                f,
                "Invalid thread cause '{}': must be 'user' or 'system'",
                s
            ),
            Error::NotFound => write!(f, "Resource not found"),
            Error::LockPoisoned => write!(f, "Internal lock poisoned"),
            Error::McpProtocol(s) => write!(f, "MCP protocol error: {}", s),
            Error::Message(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Sqlx(e) => Some(e),
            Error::SerdeJson(e) => Some(e),
            Error::SerdeYaml(e) => Some(e),
            Error::Join(e) => Some(e),
            Error::Reqwest(e) => Some(e),
            Error::ChronoParse(e) => Some(e),
            Error::Regex(e) => Some(e),
            _ => None,
        }
    }
}

/// Extension trait: `.ctx()` as a lightweight alternative to `.with_context()`.
/// Works with our Error type where `anyhow::Context` is not available.
pub trait ErrorContext<T> {
    fn ctx(self, msg: impl Into<String>) -> AppResult<T>;
}

impl<T, E: std::fmt::Display> ErrorContext<T> for std::result::Result<T, E> {
    fn ctx(self, msg: impl Into<String>) -> AppResult<T> {
        self.map_err(|e| Error::Message(format!("{}: {}", msg.into(), e)))
    }
}

/// Helper macro: return `Err(Error::Message(...))` with a format string.
/// Replaces `anyhow::bail!("...")` / `bail!("...")`.
#[macro_export]
macro_rules! err_msg {
    ($($arg:tt)*) => {
        return Err($crate::error::Error::Message(format!($($arg)*)))
    };
}

/// Helper macro: create `Error::Message(...)` value.
/// Replaces `anyhow::anyhow!("...")`.
#[macro_export]
macro_rules! err_str {
    ($($arg:tt)*) => {
        $crate::error::Error::Message(format!($($arg)*))
    };
}

/// Helper trait: convert `Option<T>` into `AppResult<T>` with `NotNullExpected`.
pub trait Required<T> {
    fn required(self) -> AppResult<T>;
}

impl<T> Required<T> for Option<T> {
    fn required(self) -> AppResult<T> {
        self.ok_or(Error::NotNullExpected)
    }
}
