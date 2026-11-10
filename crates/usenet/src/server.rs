//! The news server a download reads from, and the check behind a settings page's "Test" button.

use std::time::Duration;

use mdm_engine::Route;
use serde::{Deserialize, Serialize};

use crate::{Error, nntp};

/// A news server and the account to use on it. The caller decides where the password is kept
/// (it is a plain field, so a settings file can skip it with `#[serde(skip)]` in its own type).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    pub host: String,
    pub port: u16,
    /// Secure from the first byte (port 563 style), not STARTTLS.
    pub tls: bool,
    pub user: Option<String>,
    pub password: Option<String>,
    /// The most connections the account allows at once.
    pub connections: u32,
}

impl Default for Server {
    fn default() -> Self {
        Server {
            host: String::new(),
            port: 563,
            tls: true,
            user: None,
            password: None,
            connections: 8,
        }
    }
}

/// Connects to `server` through `route`, signs in and says goodbye: what a "Test" button runs.
pub async fn check(server: &Server, route: &Route) -> Result<(), Error> {
    let connection = nntp::Connection::connect(
        server,
        route,
        Duration::from_secs(15),
        Duration::from_secs(20),
    )
    .await?;
    connection.quit().await;
    Ok(())
}
