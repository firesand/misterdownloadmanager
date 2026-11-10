//! Usenet downloads: the files an NZB describes, fetched from a news server over NNTP and
//! decoded from yEnc.
//!
//! - [`nzb`] reads the NZB and names its files.
//! - [`yenc`] decodes one article.
//! - [`nntp`] is the client of one connection.
//! - [`Server`] and [`check`] describe and test the news server.
//! - [`Download`] runs it all: several connections, files written in place, resume.

mod download;
mod error;
pub mod nntp;
pub mod nzb;
mod server;
pub mod yenc;

pub use download::{
    ConnStatus, ConnView, Download, EVENTS, Event, FileView, Options, Outcome, Snapshot, State,
};
pub use error::Error;
pub use nzb::{Nzb, NzbFile, Segment};
pub use server::{Server, check};
