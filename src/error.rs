use std::fmt;

use crate::command::filter::ParseError;
use crate::handler::arcus::element::CodecError;
use crate::handler::arcus::engine::StoreError;

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

    /// 키는 있는데 우리 인덱스가 아니다. 인덱스와 일반 키가 한 키 공간을 쓴다.
    TypeMismatch,

    IndexEvicted,

    Unreadable,
    Rebuilding,
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
            Self::BadRequest(_)
            | Self::NoSuchIndex
            | Self::TypeMismatch
            | Self::IndexEvicted
            | Self::Filter(_) => Blame::Client,

            Self::Codec(e) => match e {
                CodecError::VectorLenMismatch { .. } => Blame::Client,
                _ => Blame::Server,
            },

            Self::Unreadable | Self::Rebuilding | Self::Busy => Blame::Server,
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
            Self::IndexEvicted => f.write_str("index was evicted"),
            Self::Unreadable => f.write_str("index is unreadable while it rebuilds from Map"),
            Self::Rebuilding => f.write_str("index is being released for a rebuild; retry"),
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
    /// 인덱스는 있는데 그 벡터 이름이 없다.
    NotFoundVector,
    /// 그 키가 우리 인덱스가 아니다.
    TypeMismatch,
    /// 있던 vkey를 덮어썼다(`vop upsert`).
    Replaced,

    Body(String),
}

impl Error {
    /// 이 오류가 회선에 나가는 모양.
    ///
    /// 인덱스가 없다는 것은 클라이언트의 잘못이 아니라 **결과**라, 접두사 없이
    /// `NOT_FOUND`로 나간다. 키가 인덱스가 아닌 것(`TYPE_MISMATCH`)도 같다.
    /// 나머지는 책임 소재를 앞세운다.
    pub fn wire(&self) -> String {
        match self {
            Self::NoSuchIndex | Self::IndexEvicted => Reply::NotFound.as_str().to_owned(),
            Self::TypeMismatch => Reply::TypeMismatch.as_str().to_owned(),
            e => format!("{} {e}\r\n", e.blame().prefix()),
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
        // 인덱스와 일반 키가 한 키 공간을 쓴다. 키가 있는데 우리 것이 아니면
        // 결과이지 잘못이 아니라, 접두사 없이 나간다.
        assert_eq!(Error::TypeMismatch.wire(), "TYPE_MISMATCH\r\n");
        assert_eq!(Error::TypeMismatch.blame(), Blame::Client);
    }

    #[test]
    fn replaced_and_not_found_vector_are_wire_lines() {
        assert_eq!(Reply::Replaced.as_str(), "REPLACED\r\n");
        assert_eq!(Reply::NotFoundVector.as_str(), "NOT_FOUND_VECTOR\r\n");
        // maxcount에 닿은 것은 별도 응답이 아니라 서버 오류다.
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
