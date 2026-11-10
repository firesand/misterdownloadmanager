//! The one error type of the crate. It is `Clone` so a snapshot can carry it to an interface
//! that words it in its own language.

use std::io;
use std::sync::Arc;

#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    /// The server refused the user name or password, or wants them and has none.
    #[error("the news server did not accept the user name or password ({0})")]
    Auth(String),
    /// The server does not have this article (430): expired, taken down or never fully posted.
    #[error("the news server does not have article <{0}>")]
    MissingArticle(String),
    /// The download ended, but some articles are not on the server.
    #[error("{missing} article(s) are missing on the news server; incomplete: {}", .files.join(", "))]
    Incomplete { missing: u64, files: Vec<String> },
    /// The server turned the connection away (too many connections, or none for now).
    #[error("the news server turned the connection away: {0}")]
    Refused(String),
    /// The server answered with something that is not NNTP, or not what was asked.
    #[error("unexpected answer from the news server: {0}")]
    Protocol(String),
    /// An article that is not valid yEnc, or whose data does not match its checksum.
    #[error("damaged article: {0}")]
    Yenc(String),
    /// A document that is not a usable NZB.
    #[error("this is not a usable NZB file: {0}")]
    Nzb(String),
    /// The host's name could not be resolved.
    #[error("cannot find the news server: {0}")]
    Resolve(String),
    /// The chosen network adapter cannot be used for this connection.
    #[error("{0}")]
    Network(String),
    #[error("connection error: {0}")]
    Io(Arc<io::Error>),
    #[error("timed out {0}")]
    Timeout(&'static str),
    /// The secure connection could not be made (certificate, handshake).
    #[error("secure connection failed: {0}")]
    Tls(String),
    #[error("cancelled")]
    Cancelled,
    /// Writing to the download folder failed.
    #[error("cannot write to the download folder: {0}")]
    Disk(Arc<io::Error>),
}

impl Error {
    /// A failure of the connection itself, which another attempt may get past.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Error::Io(_)
                | Error::Timeout(_)
                | Error::Protocol(_)
                | Error::Resolve(_)
                | Error::Refused(_)
        )
    }

    pub(crate) fn disk(e: io::Error) -> Error {
        Error::Disk(Arc::new(e))
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(Arc::new(e))
    }
}
