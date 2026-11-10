//! A client for one NNTP connection, speaking just what downloading articles needs: the
//! greeting, `AUTHINFO` (RFC 4643), `BODY` by message id and `QUIT` (RFC 3977). One command at
//! a time.
//!
//! Everything read is bytes, never text: article bodies are binary and lines can be long. Every
//! read has a timeout, so a server that goes silent mid-answer drops the connection as a
//! stalled HTTP response would.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdm_engine::net::{self, Route};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::{Error, Server};

/// A status line longer than this is not one: the peer is not a news server.
const MAX_LINE: usize = 4096;
/// An article is a few hundred kilobytes; one far beyond that is not an article.
const MAX_BODY: usize = 64 << 20;
const CHUNK: usize = 64 << 10;
/// What ends a multi-line answer: a line holding a single dot.
const TERMINATOR: &[u8] = b"\r\n.\r\n";

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// One signed-in connection to a news server.
pub struct Connection {
    io: Box<dyn Stream>,
    /// Bytes read but not used yet.
    buf: Vec<u8>,
    read_timeout: Duration,
}

impl Connection {
    /// Connects to `server` through `route`, secures the connection when the server says so,
    /// reads the greeting and signs in. `Error::Refused` when the server turns the connection
    /// away, `Error::Auth` when it does not take the login.
    pub async fn connect(
        server: &Server,
        route: &Route,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Connection, Error> {
        let tcp = open_tcp(server, route, connect_timeout).await?;
        let io: Box<dyn Stream> = if server.tls {
            Box::new(handshake(server, tcp, connect_timeout).await?)
        } else {
            Box::new(tcp)
        };
        let mut c = Connection {
            io,
            buf: Vec::new(),
            read_timeout,
        };
        let (code, text) = c.status().await?;
        match code {
            200 | 201 => {}
            400 | 502 => return Err(Error::Refused(reply(code, &text))),
            _ => return Err(Error::Protocol(reply(code, &text))),
        }
        if let Some(user) = server.user.as_deref().filter(|u| !u.is_empty()) {
            c.authenticate(user, server.password.as_deref().unwrap_or_default())
                .await?;
        }
        Ok(c)
    }

    async fn authenticate(&mut self, user: &str, password: &str) -> Result<(), Error> {
        for secret in [user, password] {
            if secret.contains(['\r', '\n']) {
                return Err(Error::Auth("the login holds a line break".into()));
            }
        }
        let (mut code, mut text) = self.command(&format!("AUTHINFO USER {user}")).await?;
        if code == 381 {
            (code, text) = self.command(&format!("AUTHINFO PASS {password}")).await?;
        }
        match code {
            281 => Ok(()),
            _ => Err(sign_in_error(code, text)),
        }
    }

    /// An article's body by message id (without angle brackets), dot-unstuffed: its lines
    /// joined by CRLF, as posted. `on_bytes` is told each chunk's size as it arrives, for the
    /// speed meters and limiters; the next read waits until its future is done.
    /// `Error::MissingArticle` when the server does not have the article; the connection is
    /// still good then.
    pub async fn body<F, Fut>(
        &mut self,
        message_id: &str,
        mut on_bytes: F,
    ) -> Result<Vec<u8>, Error>
    where
        F: FnMut(usize) -> Fut,
        Fut: Future<Output = ()>,
    {
        if message_id.contains(|c: char| c.is_whitespace() || "<>".contains(c)) {
            return Err(Error::Protocol(format!("bad message id {message_id:?}")));
        }
        let (code, text) = self.command(&format!("BODY <{message_id}>")).await?;
        match code {
            222 => {}
            423 | 430 => return Err(Error::MissingArticle(message_id.to_owned())),
            480 | 481 => return Err(Error::Auth(reply(code, &text))),
            400 | 502 => return Err(Error::Refused(reply(code, &text))),
            _ => return Err(Error::Protocol(reply(code, &text))),
        }
        // The body starts after the status line's CRLF: an empty one is just ".\r\n", so the
        // search sees that CRLF too.
        let mut raw = b"\r\n".to_vec();
        raw.append(&mut self.buf);
        if raw.len() > 2 {
            on_bytes(raw.len() - 2).await;
        }
        let mut from = 0;
        loop {
            if let Some(p) = raw[from..]
                .windows(TERMINATOR.len())
                .position(|w| w == TERMINATOR)
            {
                let p = from + p;
                self.buf = raw.split_off(p + TERMINATOR.len());
                // The CRLF ending the last line belongs to the body.
                raw.truncate(p + 2);
                raw.drain(..2);
                unstuff(&mut raw);
                return Ok(raw);
            }
            if raw.len() > MAX_BODY {
                return Err(Error::Protocol("the article is far too large".into()));
            }
            from = raw.len().saturating_sub(TERMINATOR.len() - 1);
            let n = self.fill().await?;
            raw.append(&mut self.buf);
            on_bytes(n).await;
        }
    }

    /// Says goodbye, as servers like, and closes.
    pub async fn quit(mut self) {
        let _ = timeout(Duration::from_secs(2), async {
            self.io.write_all(b"QUIT\r\n").await?;
            self.io.flush().await?;
            self.io.shutdown().await
        })
        .await;
    }

    async fn command(&mut self, line: &str) -> Result<(u16, String), Error> {
        let mut out = Vec::with_capacity(line.len() + 2);
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
        match timeout(self.read_timeout, async {
            self.io.write_all(&out).await?;
            self.io.flush().await
        })
        .await
        {
            Err(_) => return Err(Error::Timeout("sending to the news server")),
            Ok(r) => r?,
        }
        self.status().await
    }

    /// The next status line: a three-digit code and its text.
    async fn status(&mut self) -> Result<(u16, String), Error> {
        let line = self.read_line().await?;
        let digits = line.get(..3).filter(|d| d.iter().all(u8::is_ascii_digit));
        let rest = line.get(3..).unwrap_or_default();
        match digits {
            Some(d) if rest.is_empty() || rest[0] == b' ' => {
                let code = d.iter().fold(0u16, |n, b| n * 10 + u16::from(b - b'0'));
                Ok((code, String::from_utf8_lossy(rest).trim().to_owned()))
            }
            _ => Err(Error::Protocol("the server is not a news server".into())),
        }
    }

    async fn read_line(&mut self) -> Result<Vec<u8>, Error> {
        loop {
            if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=i).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            if self.buf.len() > MAX_LINE {
                return Err(Error::Protocol("the server is not a news server".into()));
            }
            self.fill().await?;
        }
    }

    /// Reads more into `buf`; how much came.
    async fn fill(&mut self) -> Result<usize, Error> {
        self.buf.reserve(CHUNK);
        match timeout(self.read_timeout, self.io.read_buf(&mut self.buf)).await {
            Err(_) => Err(Error::Timeout("waiting for the news server")),
            Ok(Err(e)) => Err(e.into()),
            Ok(Ok(0)) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the news server closed the connection",
            )
            .into()),
            Ok(Ok(n)) => Ok(n),
        }
    }
}

fn reply(code: u16, text: &str) -> String {
    if text.is_empty() {
        code.to_string()
    } else {
        format!("{code} {text}")
    }
}

/// Some servers answer a login over the account's connection limit with 481 or 502 too: their
/// text says so.
fn sign_in_error(code: u16, text: String) -> Error {
    let t = text.to_ascii_lowercase();
    if t.contains("connection") && !["password", "user", "auth"].iter().any(|w| t.contains(w)) {
        return Error::Refused(reply(code, &text));
    }
    match code {
        481 | 482 | 502 | 281 | 381 => Error::Auth(reply(code, &text)),
        _ => Error::Protocol(reply(code, &text)),
    }
}

/// Lines that start with a dot were sent with another one before them (RFC 3977 section 3.1.1).
fn unstuff(data: &mut Vec<u8>) {
    let mut w = 0;
    let mut line_start = true;
    for r in 0..data.len() {
        let b = data[r];
        if line_start && b == b'.' && data.get(r + 1) == Some(&b'.') {
            line_start = false;
            continue;
        }
        data[w] = b;
        w += 1;
        line_start = b == b'\n';
    }
    data.truncate(w);
}

async fn open_tcp(
    server: &Server,
    route: &Route,
    connect_timeout: Duration,
) -> Result<tokio::net::TcpStream, Error> {
    let host = server.host.trim();
    let found: Vec<SocketAddr> = match timeout(
        connect_timeout,
        tokio::net::lookup_host((host, server.port)),
    )
    .await
    {
        Err(_) => return Err(Error::Timeout("looking up the news server")),
        Ok(Err(e)) => return Err(Error::Resolve(format!("{host}: {e}"))),
        Ok(Ok(addrs)) => addrs.collect(),
    };
    let usable = route.keep(found.iter().map(|a| a.ip()).collect());
    let addrs: Vec<SocketAddr> = found
        .into_iter()
        .filter(|a| usable.contains(&a.ip()))
        .collect();
    if addrs.is_empty() {
        return Err(Error::Network(format!(
            "{host} has no address the chosen network adapter can reach"
        )));
    }
    let mut last = Error::Resolve(host.to_owned());
    for addr in addrs {
        match timeout(connect_timeout, net::tcp_connect(route, addr)).await {
            Err(_) => last = Error::Timeout("connecting to the news server"),
            Ok(Err(e)) if e.get_ref().is_some_and(|i| i.is::<net::NetError>()) => {
                return Err(Error::Network(e.to_string()));
            }
            Ok(Err(e)) => last = e.into(),
            Ok(Ok(tcp)) => {
                let _ = tcp.set_nodelay(true);
                return Ok(tcp);
            }
        }
    }
    Err(last)
}

/// MDM's TLS client configuration, built once: the system's verifier with the aws-lc-rs
/// provider, as the engine's FTPS and HTTPS use.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, Error> {
    static CONFIG: Mutex<Option<Arc<rustls::ClientConfig>>> = Mutex::new(None);
    let mut cached = CONFIG.lock().unwrap();
    if let Some(c) = cached.as_ref() {
        return Ok(c.clone());
    }
    if !mdm_engine::tls::system_has_roots() {
        return Err(Error::Tls(mdm_engine::tls::NO_ROOTS.to_owned()));
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
        .map_err(|e| Error::Tls(e.to_string()))?;
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    let config = Arc::new(config);
    *cached = Some(config.clone());
    Ok(config)
}

async fn handshake(
    server: &Server,
    tcp: tokio::net::TcpStream,
    connect_timeout: Duration,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, Error> {
    let name = rustls::pki_types::ServerName::try_from(server.host.trim().to_owned())
        .map_err(|e| Error::Tls(format!("{}: {e}", server.host)))?;
    let connector = tokio_rustls::TlsConnector::from(tls_config()?);
    match timeout(connect_timeout, connector.connect(name, tcp)).await {
        Err(_) => Err(Error::Timeout("making the secure connection")),
        Ok(Err(e)) => Err(Error::Tls(e.to_string())),
        Ok(Ok(stream)) => Ok(stream),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unstuffing_drops_one_leading_dot_per_line() {
        let mut d = b"..dot\r\nplain\r\n...two\r\n.x\r\na..b\r\n".to_vec();
        unstuff(&mut d);
        assert_eq!(d, b".dot\r\nplain\r\n..two\r\n.x\r\na..b\r\n");
        let mut empty = Vec::new();
        unstuff(&mut empty);
        assert!(empty.is_empty());
    }

    #[test]
    fn a_login_over_the_limit_is_a_refusal_not_a_bad_password() {
        assert!(matches!(
            sign_in_error(481, "too many connections for this account".into()),
            Error::Refused(_)
        ));
        assert!(matches!(
            sign_in_error(481, "authentication failed".into()),
            Error::Auth(_)
        ));
    }
}
