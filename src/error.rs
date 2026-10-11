//! Error types shared by every layer of grafile.
//!
//! The design mirrors SQLite's result codes: every [`Error`] carries an
//! [`ErrorCode`] that callers can branch on, plus a human-readable message and
//! an optional underlying cause.

use std::fmt;

/// Result type alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Broad error class, modeled after SQLite's extended result codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// A bug or invariant violation inside grafile.
    Internal,
    /// On-disk structure failed validation (checksum, layout, ordering).
    Corrupt,
    /// The file is not a grafile database, or its format version is unknown.
    NotADb,
    /// An operating-system level failure.
    Io,
    /// The database is locked by another writer.
    Busy,
    /// A lock request conflicts with locks held elsewhere.
    Locked,
    /// The database was opened read-only and a write was attempted.
    ReadOnly,
    /// The API was used incorrectly (e.g. stepping a finished statement).
    Misuse,
    /// An argument was outside its accepted range.
    Range,
    /// A uniqueness constraint or MERGE expectation was violated.
    Constraint,
    /// A value had the wrong type for the operation.
    Type,
    /// The Cypher text could not be parsed.
    Syntax,
    /// The feature is valid Cypher or SQL but not implemented by grafile.
    Unsupported,
    /// A referenced entity (label, index, variable) does not exist.
    NotFound,
    /// An entity that must be unique already exists.
    Exists,
}

impl ErrorCode {
    /// Stable short name of the code, used in error messages and tests.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Corrupt => "corrupt",
            Self::NotADb => "notadb",
            Self::Io => "io",
            Self::Busy => "busy",
            Self::Locked => "locked",
            Self::ReadOnly => "readonly",
            Self::Misuse => "misuse",
            Self::Range => "range",
            Self::Constraint => "constraint",
            Self::Type => "type",
            Self::Syntax => "syntax",
            Self::Unsupported => "unsupported",
            Self::NotFound => "notfound",
            Self::Exists => "exists",
        }
    }

    /// Whether retrying the same operation may succeed (locks will be released).
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Busy | Self::Locked)
    }

    /// Whether the failure indicates the database file is damaged.
    pub const fn is_corruption(self) -> bool {
        matches!(self, Self::Corrupt | Self::NotADb)
    }
}

/// The error type for all fallible grafile operations.
#[derive(Debug, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct Error {
    code: ErrorCode,
    message: String,
    #[source]
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl Error {
    /// Create an error with the given code and message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            source: None,
        }
    }

    /// Create a corruption error.
    pub fn corrupt(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Corrupt, message)
    }

    /// Create an internal-invariant error. Used for conditions that should be
    /// impossible; keeps production code free of `panic!`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    /// Create a "feature not implemented" error.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, message)
    }

    /// Create an API-misuse error.
    pub fn misuse(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Misuse, message)
    }

    /// Create a type error.
    pub fn type_error(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Type, message)
    }

    /// Create a constraint error.
    pub fn constraint(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Constraint, message)
    }

    /// Create a syntax error with a message that already embeds position info.
    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Syntax, message)
    }

    /// Attach an underlying cause, preserving the current code and message.
    pub fn with_source(
        mut self,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The error class.
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// The human-readable message (without the code prefix).
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        let code = match err.kind() {
            std::io::ErrorKind::NotFound => ErrorCode::NotFound,
            std::io::ErrorKind::PermissionDenied => ErrorCode::ReadOnly,
            std::io::ErrorKind::UnexpectedEof => ErrorCode::Corrupt,
            _ => ErrorCode::Io,
        };
        Self {
            code,
            message: err.to_string(),
            source: Some(Box::new(err)),
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_maps_and_keeps_source() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let err = Error::from(io);
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn display_contains_code_and_message() {
        let err = Error::corrupt("page 7 checksum mismatch");
        let text = err.to_string();
        assert!(text.contains("Corrupt"));
        assert!(text.contains("page 7 checksum mismatch"));
    }

    #[test]
    fn retryable_and_corruption_classification() {
        assert!(ErrorCode::Busy.is_retryable());
        assert!(ErrorCode::Locked.is_retryable());
        assert!(!ErrorCode::Io.is_retryable());
        assert!(ErrorCode::Corrupt.is_corruption());
        assert!(ErrorCode::NotADb.is_corruption());
        assert!(!ErrorCode::Syntax.is_corruption());
    }
}
