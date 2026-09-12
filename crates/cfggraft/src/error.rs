//! Failures that stop a run.
//!
//! An [`Error`] means the run could not be carried out: a file could not be
//! read, a document could not be parsed, a marker has no end. It never means
//! "the target disagreed with the declaration" — disagreement is an ordinary
//! outcome carried by [`Decision::Refuse`](crate::policy::Decision::Refuse), and
//! conflating the two would make a drift check unable to distinguish a
//! difference from a crash.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure that prevents the run from completing.
#[derive(Debug)]
pub enum Error {
    /// A file could not be read or written.
    Io {
        /// The file being accessed.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// A document could not be parsed in the format its adapter expects.
    Parse {
        /// The file being parsed.
        path: PathBuf,
        /// What the parser reported.
        message: String,
    },
    /// The input is well-formed but cannot be operated on — an unterminated
    /// marker region, a declaration naming a path that does not exist in the
    /// fragment, a missing target path where one is required.
    Invalid(String),
}

impl Error {
    /// Attach a path to an I/O failure.
    pub fn io(path: impl AsRef<Path>, source: io::Error) -> Self {
        Error::Io {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }

    /// Attach a path to a parse failure.
    pub fn parse(path: impl AsRef<Path>, message: impl fmt::Display) -> Self {
        Error::Parse {
            path: path.as_ref().to_path_buf(),
            message: message.to_string(),
        }
    }

    /// Report an input this crate cannot act on.
    pub fn invalid(message: impl fmt::Display) -> Self {
        Error::Invalid(message.to_string())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Parse { path, message } => write!(f, "{}: {message}", path.display()),
            Error::Invalid(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_name_the_path() {
        let e = Error::io(
            "settings.json",
            io::Error::new(io::ErrorKind::NotFound, "nope"),
        );
        assert!(e.to_string().starts_with("settings.json: "), "got: {e}");
    }

    #[test]
    fn invalid_errors_are_the_message_alone() {
        assert_eq!(Error::invalid("no end marker").to_string(), "no end marker");
    }
}
