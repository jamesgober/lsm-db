//! The crate error type.
//!
//! Every fallible operation in `lsm-db` returns [`Result<T>`], whose error is
//! [`Error`]. The type integrates with the portfolio's `error-forge` framework
//! — it implements [`error_forge::ForgeError`], so callers get the stable
//! `kind` / `caption` / `is_fatal` metadata other crates rely on — while still
//! exposing the underlying [`std::io::Error`] through
//! [`std::error::Error::source`] for code that needs the OS error kind directly.

use std::{fmt, io};

use error_forge::ForgeError;

/// A specialised [`Result`](std::result::Result) for storage-engine operations.
///
/// Defaults its error to [`Error`], so most signatures read `Result<T>`.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while opening, reading from, writing to, or
/// flushing an [`Lsm`](crate::Lsm) engine.
///
/// The type is
/// [`#[non_exhaustive]`](https://doc.rust-lang.org/reference/attributes/type_system.html#the-non_exhaustive-attribute):
/// future versions may add variants without a major bump, so a `match` over it
/// must include a wildcard arm.
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    /// An underlying I/O operation failed.
    ///
    /// `context` names the operation that was attempted (for example
    /// `"open database directory"` or `"flush memtable to disk"`) so the
    /// message is actionable without a backtrace. The original [`io::Error`] is
    /// preserved as the [`source`](std::error::Error::source); inspect it when
    /// the OS error kind (disk full, permission denied, not found) drives the
    /// recovery decision.
    Io {
        /// What the engine was trying to do when the I/O error occurred.
        context: &'static str,
        /// The underlying operating-system error.
        source: io::Error,
    },

    /// An on-disk sorted run (SSTable) is not intact.
    ///
    /// Either a length prefix is implausibly large, or the file ends in the
    /// middle of a record. A damaged run cannot be trusted, so the read that
    /// touched it fails rather than returning partial or fabricated data.
    /// `reason` is a short, human-readable description of the inconsistency.
    Corruption {
        /// A short, human-readable reason the run was rejected.
        reason: &'static str,
    },
}

impl Error {
    /// Wrap an [`io::Error`] with the static context describing the operation.
    pub(crate) fn io(context: &'static str, source: io::Error) -> Self {
        Error::Io { context, source }
    }

    /// Build an [`Error::Corruption`] with a static reason.
    pub(crate) fn corruption(reason: &'static str) -> Self {
        Error::Corruption { reason }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { context, source } => {
                write!(f, "i/o error while {context}: {source}")
            }
            Error::Corruption { reason } => {
                write!(f, "sorted-run corruption: {reason}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            Error::Corruption { .. } => None,
        }
    }
}

/// A bare [`io::Error`] converts into [`Error::Io`] with a generic context.
///
/// Call sites that know what they were doing attach a specific context instead;
/// this exists for the `?` ergonomics of code — including doctests and examples
/// — that does not.
impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Error::Io {
            context: "performing a storage i/o operation",
            source,
        }
    }
}

impl ForgeError for Error {
    fn kind(&self) -> &'static str {
        match self {
            Error::Io { .. } => "Io",
            Error::Corruption { .. } => "Corruption",
        }
    }

    fn caption(&self) -> &'static str {
        "lsm storage engine error"
    }

    /// Corruption is unrecoverable by retry: the bytes on disk are already
    /// damaged. I/O errors are left non-fatal for the caller to judge — a
    /// transient `Interrupted` or a recoverable `WouldBlock` may be retried.
    fn is_fatal(&self) -> bool {
        matches!(self, Error::Corruption { .. })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_io_error_exposes_source() {
        let inner = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let err = Error::io("open database directory", inner);
        let source = std::error::Error::source(&err).expect("io error has a source");
        let io_source = source
            .downcast_ref::<io::Error>()
            .expect("source downcasts to io::Error");
        assert_eq!(io_source.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn test_corruption_has_no_source() {
        let err = Error::corruption("length prefix exceeds file size");
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_corruption_is_fatal_io_is_not() {
        assert!(Error::corruption("truncated record").is_fatal());
        let io = Error::io("read run", io::Error::from(io::ErrorKind::UnexpectedEof));
        assert!(!io.is_fatal());
    }

    #[test]
    fn test_kind_matches_variant() {
        assert_eq!(Error::corruption("x").kind(), "Corruption");
        let io = Error::io("x", io::Error::from(io::ErrorKind::Other));
        assert_eq!(io.kind(), "Io");
    }

    #[test]
    fn test_display_is_actionable() {
        let err = Error::corruption("truncated value");
        assert_eq!(err.to_string(), "sorted-run corruption: truncated value");
    }
}
