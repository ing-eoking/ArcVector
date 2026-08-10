//! One error type for every command path.
//!
//! Handlers return `Result<Reply, Error>` and a single place in [`crate::lib`]
//! turns that into an ASCII response. Whether a failure is reported as
//! `CLIENT_ERROR` or `SERVER_ERROR` follows from the error itself, so no call
//! site has to decide.

use std::fmt;

use crate::codec::CodecError;
use crate::filter::ParseError;
use crate::store::StoreError;

/// Who is at fault, which picks the ASCII error prefix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Blame {
    Client,
    Server,
}

impl Blame {
    pub const fn prefix(self) -> &'static str {
        match self {
            Blame::Client => "CLIENT_ERROR",
            Blame::Server => "SERVER_ERROR",
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
    Codec(CodecError),
    Filter(ParseError),
    Store(StoreError),
    /// usearch reported a failure.
    Index(String),
}

impl Error {
    pub fn bad_request(msg: impl Into<String>) -> Error {
        Error::BadRequest(msg.into())
    }

    pub fn blame(&self) -> Blame {
        match self {
            Error::BadRequest(_) | Error::NoSuchIndex | Error::IndexEvicted | Error::Filter(_) => {
                Blame::Client
            }
            // Encoding failures reflect what the client sent; decoding failures
            // mean the stored bytes are wrong, which is ours to answer for.
            Error::Codec(e) => match e {
                CodecError::AttrTooLarge { .. } | CodecError::VectorLenMismatch { .. } => {
                    Blame::Client
                }
                _ => Blame::Server,
            },
            Error::Store(_) | Error::Index(_) => Blame::Server,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadRequest(m) | Error::Index(m) => f.write_str(m),
            Error::NoSuchIndex => f.write_str("index not found"),
            Error::IndexEvicted => f.write_str("index was evicted"),
            Error::Codec(e) => write!(f, "{e}"),
            Error::Filter(e) => write!(f, "{e}"),
            Error::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Codec(e) => Some(e),
            Error::Filter(e) => Some(e),
            Error::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<CodecError> for Error {
    fn from(e: CodecError) -> Error {
        Error::Codec(e)
    }
}

impl From<ParseError> for Error {
    fn from(e: ParseError) -> Error {
        Error::Filter(e)
    }
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Error {
        Error::Store(e)
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
            Reply::Created => "CREATED\r\n",
            Reply::Exists => "EXISTS\r\n",
            Reply::Stored => "STORED\r\n",
            Reply::Deleted => "DELETED\r\n",
            Reply::Dropped => "DROPPED\r\n",
            Reply::NotFound => "NOT_FOUND\r\n",
            Reply::Overflowed => "OVERFLOWED\r\n",
            Reply::Body(s) => s,
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
        assert_eq!(Error::Codec(CodecError::BadMagic).blame(), Blame::Server);
        assert_eq!(Error::Store(StoreError::Unavailable).blame(), Blame::Server);
        assert_eq!(Error::Index("boom".into()).blame(), Blame::Server);
    }

    #[test]
    fn errors_convert_from_their_module_types() {
        let e: Error = CodecError::BadMagic.into();
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
