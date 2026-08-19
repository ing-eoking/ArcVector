//! One error type for every command path.
//!
//! Whether a failure is reported as `CLIENT_ERROR` or `SERVER_ERROR` follows from
//! the error itself, so no call site has to decide.

use std::fmt;

use crate::command::filter::ParseError;
use crate::handler::arcus::element::CodecError;
use crate::handler::arcus::engine::StoreError;

/// Who is at fault, which picks the ASCII error prefix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Blame {
    Client,
    Server,
}

impl Blame {
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Client => "CLIENT_ERROR",
            Self::Server => "SERVER_ERROR",
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// Malformed command line, out-of-range argument, unusable payload.
    BadRequest(String),
    /// No such index in the registry.
    NoSuchIndex,
    /// The Map backing the index was evicted or expired out from under us.
    IndexEvicted,
    /// The index exists but its graph is being rebuilt from Map.
    Unreadable,
    Codec(CodecError),
    Filter(ParseError),
    Store(StoreError),
    /// usearch reported a failure.
    Index(String),
}

impl Error {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }

    pub fn blame(&self) -> Blame {
        match self {
            Self::BadRequest(_) | Self::NoSuchIndex | Self::IndexEvicted | Self::Filter(_) => {
                Blame::Client
            }
            // Encoding failures are the client's; decoding failures are ours.
            Self::Codec(e) => match e {
                CodecError::AttrTooLarge { .. } | CodecError::VectorLenMismatch { .. } => {
                    Blame::Client
                }
                _ => Blame::Server,
            },
            // A well-formed request this node cannot serve right now.
            Self::Unreadable => Blame::Server,
            Self::Store(_) | Self::Index(_) => Blame::Server,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest(m) | Self::Index(m) => f.write_str(m),
            Self::NoSuchIndex => f.write_str("index not found"),
            Self::IndexEvicted => f.write_str("index was evicted"),
            Self::Unreadable => f.write_str("index is unreadable while it rebuilds from Map"),
            Self::Codec(e) => write!(f, "{e}"),
            Self::Filter(e) => write!(f, "{e}"),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Codec(e) => Some(e),
            Self::Filter(e) => Some(e),
            Self::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<CodecError> for Error {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

impl From<ParseError> for Error {
    fn from(e: ParseError) -> Self {
        Self::Filter(e)
    }
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// A successful command result, ready to be written to the connection.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Created,
    Exists,
    Stored,
    Deleted,
    Dropped,
    NotFound,
    Overflowed,
    /// Pre-rendered multi-line body, already terminated by `END\r\n`.
    Body(String),
}

impl Reply {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Created => "CREATED\r\n",
            Self::Exists => "EXISTS\r\n",
            Self::Stored => "STORED\r\n",
            Self::Deleted => "DELETED\r\n",
            Self::Dropped => "DROPPED\r\n",
            Self::NotFound => "NOT_FOUND\r\n",
            Self::Overflowed => "OVERFLOWED\r\n",
            Self::Body(s) => s,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blame_picks_the_ascii_prefix() {
        assert_eq!(Blame::Client.prefix(), "CLIENT_ERROR");
        assert_eq!(Blame::Server.prefix(), "SERVER_ERROR");
    }

    #[test]
    fn client_input_errors_are_blamed_on_the_client() {
        assert_eq!(Error::bad_request("nope").blame(), Blame::Client);
        assert_eq!(Error::NoSuchIndex.blame(), Blame::Client);
        assert_eq!(Error::Filter(ParseError::Empty).blame(), Blame::Client);
        // Oversized ATTR: the client sent it.
        assert_eq!(
            Error::Codec(CodecError::AttrTooLarge {
                limit: 128,
                got: 200
            })
            .blame(),
            Blame::Client
        );
    }

    #[test]
    fn corrupt_stored_data_is_blamed_on_the_server() {
        // A bad magic byte means what we stored is wrong, not what was sent.
        assert_eq!(
            Error::Codec(CodecError::LayoutMismatch).blame(),
            Blame::Server
        );
        assert_eq!(Error::Store(StoreError::Unavailable).blame(), Blame::Server);
        assert_eq!(Error::Index("boom".into()).blame(), Blame::Server);
    }

    #[test]
    fn errors_convert_from_their_module_types() {
        let e: Error = CodecError::LayoutMismatch.into();
        assert!(matches!(e, Error::Codec(_)));
        let e: Error = ParseError::Empty.into();
        assert!(matches!(e, Error::Filter(_)));
        let e: Error = StoreError::KeyGone.into();
        assert!(matches!(e, Error::Store(_)));
    }

    #[test]
    fn every_reply_is_crlf_terminated() {
        for r in [
            Reply::Created,
            Reply::Exists,
            Reply::Stored,
            Reply::Deleted,
            Reply::Dropped,
            Reply::NotFound,
            Reply::Overflowed,
            Reply::Body("END\r\n".into()),
        ] {
            assert!(r.as_str().ends_with("\r\n"), "{r:?}");
        }
    }
}
