use std::fmt;

use crate::engine::StoreError;
use crate::engine::element::CodecError;
use crate::protocol::filter::ParseError;

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
    BadRequest(String),
    NoSuchIndex,

    /// The key exists but is not one of our indexes; indexes share the key space with plain keys.
    TypeMismatch,

    /// The index is locked (`Halt`); it unlocks once the sweeper drains the backlog.
    Busy,
    Codec(CodecError),
    Filter(ParseError),
    Store(StoreError),
    Index(String),
}

impl Error {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }

    pub fn blame(&self) -> Blame {
        match self {
            Self::BadRequest(_) | Self::NoSuchIndex | Self::TypeMismatch | Self::Filter(_) => {
                Blame::Client
            }

            Self::Codec(e) => match e {
                CodecError::VectorLenMismatch { .. } => Blame::Client,
                _ => Blame::Server,
            },

            Self::Busy => Blame::Server,
            Self::Store(_) | Self::Index(_) => Blame::Server,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest(m) | Self::Index(m) => f.write_str(m),
            Self::NoSuchIndex => f.write_str("index not found"),
            Self::TypeMismatch => f.write_str("the key holds something that is not an index"),
            Self::Busy => write!(
                f,
                "the index is releasing memory and cannot take new searches"
            ),
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

#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Created,
    Exists,
    Stored,
    Deleted,
    Dropped,
    NotFound,
    /// The index exists but has no vector with that name.
    NotFoundVector,
    /// The key is not one of our indexes.
    TypeMismatch,
    /// An existing vkey was overwritten (`vop upsert`).
    Replaced,

    Body(String),
}

impl Error {
    /// The wire form of this error.
    ///
    /// A missing index is a **result**, not a client mistake, so it goes out as a bare
    /// `NOT_FOUND` with no prefix. The same holds for a key that is not an index
    /// (`TYPE_MISMATCH`). Everything else is prefixed with who is to blame.
    pub fn wire(&self) -> String {
        match self {
            Self::NoSuchIndex => Reply::NotFound.as_str().to_owned(),
            Self::TypeMismatch => Reply::TypeMismatch.as_str().to_owned(),
            // The message may carry client-sent bytes (filter fragments, etc.). A raw CR/LF
            // would split the reply line and the client would read the rest as the next
            // reply, so control characters are escaped.
            e => {
                let mut line = format!("{} ", e.blame().prefix());
                for c in e.to_string().chars() {
                    if c.is_control() {
                        line.extend(c.escape_default());
                    } else {
                        line.push(c);
                    }
                }
                line.push_str("\r\n");
                line
            }
        }
    }
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
            Self::NotFoundVector => "NOT_FOUND_VECTOR\r\n",
            Self::TypeMismatch => "TYPE_MISMATCH\r\n",
            Self::Replaced => "REPLACED\r\n",
            Self::Body(s) => s,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_in_a_message_cannot_split_the_reply() {
        let e = Error::bad_request("near 'a\r\nEND\r\n'");
        assert_eq!(e.wire(), "CLIENT_ERROR near 'a\\r\\nEND\\r\\n'\r\n");
    }

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
        assert_eq!(
            Error::Codec(CodecError::VectorLenMismatch { need: 4, got: 3 }).blame(),
            Blame::Client
        );
    }

    #[test]
    fn corrupt_stored_data_is_blamed_on_the_server() {
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
    fn a_key_that_is_not_an_index_answers_type_mismatch() {
        // Indexes share the key space with plain keys. A key that exists but isn't ours is a
        // result, not a mistake, so it goes out without a prefix.
        assert_eq!(Error::TypeMismatch.wire(), "TYPE_MISMATCH\r\n");
        assert_eq!(Error::TypeMismatch.blame(), Blame::Client);
    }

    #[test]
    fn replaced_and_not_found_vector_are_wire_lines() {
        assert_eq!(Reply::Replaced.as_str(), "REPLACED\r\n");
        assert_eq!(Reply::NotFoundVector.as_str(), "NOT_FOUND_VECTOR\r\n");
        // Hitting maxcount is a server error, not a dedicated reply.
        assert_eq!(
            Error::Index("index is full".into()).wire(),
            "SERVER_ERROR index is full\r\n"
        );
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
            Reply::Replaced,
            Reply::NotFoundVector,
            Reply::TypeMismatch,
            Reply::Body("END\r\n".into()),
        ] {
            assert!(r.as_str().ends_with("\r\n"), "{r:?}");
        }
    }
}
