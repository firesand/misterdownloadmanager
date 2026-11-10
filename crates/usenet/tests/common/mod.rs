//! A small news server on 127.0.0.1 for the tests: it serves yEnc articles made from random
//! bytes, counts the `BODY` requests of each message id, and can ask for a login, turn away
//! connections beyond a limit, drop one answer half way, and answer slowly.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdm_usenet::{Nzb, Options, Server};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// xorshift: the tests need random-looking bytes, not a random crate.
pub struct Rng(pub u64);

impl Rng {
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n)
            .map(|_| {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                (self.0 >> 24) as u8
            })
            .collect()
    }
}

/// A file as posted: its bytes and the message ids of its articles, in order.
pub struct TestFile {
    pub name: String,
    pub data: Vec<u8>,
    pub ids: Vec<String>,
    /// The encoded articles, by message id.
    pub articles: Vec<Vec<u8>>,
}

pub fn make_file(rng: &mut Rng, name: &str, size: usize, per_article: usize) -> TestFile {
    let data = rng.bytes(size);
    let total = size.div_ceil(per_article);
    let mut ids = Vec::new();
    let mut articles = Vec::new();
    for i in 0..total {
        let (begin, end) = (i * per_article, ((i + 1) * per_article).min(size));
        ids.push(format!("{name}.{}@fake.test", i + 1));
        articles.push(encode(
            name,
            &data,
            begin,
            end,
            (i as u32 + 1, total as u32),
        ));
    }
    TestFile {
        name: name.to_owned(),
        data,
        ids,
        articles,
    }
}

/// yEnc, 128 columns, CRLF; a leading dot is left unescaped so dot-stuffing is exercised.
fn encode(name: &str, file: &[u8], begin: usize, end: usize, (n, total): (u32, u32)) -> Vec<u8> {
    let part = &file[begin..end];
    let mut out = format!(
        "=ybegin part={n} total={total} line=128 size={} name={name}\r\n=ypart begin={} end={end}\r\n",
        file.len(),
        begin + 1
    )
    .into_bytes();
    let mut col = 0;
    for (i, &b) in part.iter().enumerate() {
        let o = b.wrapping_add(42);
        let edge = col == 0 || i == part.len() - 1;
        if matches!(o, 0 | 10 | 13 | b'=') || (edge && matches!(o, b'\t' | b' ')) {
            out.push(b'=');
            out.push(o.wrapping_add(64));
            col += 2;
        } else {
            out.push(o);
            col += 1;
        }
        if col >= 128 {
            out.extend(b"\r\n");
            col = 0;
        }
    }
    if col > 0 {
        out.extend(b"\r\n");
    }
    out.extend(
        format!(
            "=yend size={} part={n} pcrc32={:08x}\r\n",
            part.len(),
            crc32fast::hash(part)
        )
        .bytes(),
    );
    out
}

/// The NZB of `files`, read back through the crate's own parser, named `name`.
pub fn nzb_of(files: &[TestFile], name: &str) -> Nzb {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE nzb PUBLIC \"-//newzBin//DTD NZB 1.1//EN\" \"http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd\">\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n",
    );
    for f in files {
        xml += &format!(
            "<file poster=\"t@t\" date=\"1\" subject=\"[1/1] - &quot;{}&quot; yEnc (1/{})\"><groups><group>alt.test</group></groups><segments>",
            f.name,
            f.ids.len()
        );
        for (i, (id, a)) in f.ids.iter().zip(&f.articles).enumerate() {
            xml += &format!(
                "<segment bytes=\"{}\" number=\"{}\">{id}</segment>",
                a.len(),
                i + 1
            );
        }
        xml += "</segments></file>\n";
    }
    xml += "</nzb>";
    let mut nzb = mdm_usenet::nzb::parse(&xml).expect("the test NZB parses");
    nzb.name = Some(name.to_owned());
    nzb
}

#[derive(Default)]
pub struct Config {
    /// A login the server wants.
    pub login: Option<(String, String)>,
    /// More connections at once are answered 502 and closed.
    pub max_conns: Option<usize>,
    /// Waited before each `BODY` answer, in milliseconds.
    pub delay_ms: AtomicU64,
    /// Message ids the server does not have.
    pub missing: HashSet<String>,
    /// Message ids whose first answer is cut half way, the connection dropped.
    pub drop_once: HashSet<String>,
}

struct Inner {
    articles: HashMap<String, Vec<u8>>,
    cfg: Config,
    counts: Mutex<HashMap<String, u32>>,
    dropped: Mutex<HashSet<String>>,
    active: AtomicUsize,
    peak: AtomicUsize,
    refused: AtomicUsize,
}

pub struct NewsServer {
    pub port: u16,
    inner: Arc<Inner>,
}

impl NewsServer {
    pub async fn start(files: &[TestFile], cfg: Config) -> NewsServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let articles = files
            .iter()
            .flat_map(|f| f.ids.iter().cloned().zip(f.articles.iter().cloned()))
            .collect();
        let inner = Arc::new(Inner {
            articles,
            cfg,
            counts: Mutex::default(),
            dropped: Mutex::default(),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
        });
        let accept = inner.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(accept.clone(), stream));
            }
        });
        NewsServer { port, inner }
    }

    /// How often `BODY` was asked for this message id.
    pub fn count(&self, id: &str) -> u32 {
        self.inner
            .counts
            .lock()
            .unwrap()
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    pub fn counts(&self) -> HashMap<String, u32> {
        self.inner.counts.lock().unwrap().clone()
    }

    pub fn total_requests(&self) -> u32 {
        self.counts().values().sum()
    }

    /// The most connections open at once.
    pub fn peak(&self) -> usize {
        self.inner.peak.load(Ordering::SeqCst)
    }

    pub fn refused(&self) -> usize {
        self.inner.refused.load(Ordering::SeqCst)
    }

    pub fn set_delay(&self, ms: u64) {
        self.inner.cfg.delay_ms.store(ms, Ordering::SeqCst);
    }

    /// Download options for this server.
    pub fn options(&self, connections: u32, login: Option<(&str, &str)>) -> Options {
        Options {
            server: Server {
                host: "127.0.0.1".into(),
                port: self.port,
                tls: false,
                user: login.map(|l| l.0.to_owned()),
                password: login.map(|l| l.1.to_owned()),
                connections,
            },
            read_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
            retry_base: Duration::from_millis(10),
            save_interval: Duration::from_millis(20),
            ..Options::default()
        }
    }
}

struct Active(Arc<Inner>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Lines that start with a dot get another, and the answer ends with a lone dot.
fn stuffed(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 16);
    let mut line_start = true;
    for &b in body {
        if line_start && b == b'.' {
            out.push(b'.');
        }
        out.push(b);
        line_start = b == b'\n';
    }
    out
}

async fn serve(inner: Arc<Inner>, stream: TcpStream) {
    let (read, mut write) = stream.into_split();
    let now = inner.active.fetch_add(1, Ordering::SeqCst) + 1;
    let _active = Active(inner.clone());
    if inner.cfg.max_conns.is_some_and(|max| now > max) {
        inner.refused.fetch_add(1, Ordering::SeqCst);
        let _ = write.write_all(b"502 too many connections\r\n").await;
        return;
    }
    inner.peak.fetch_max(now, Ordering::SeqCst);
    let _ = write.write_all(b"200 fake news server ready\r\n").await;
    let mut lines = BufReader::new(read);
    let mut authed = inner.cfg.login.is_none();
    let mut user = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        match lines.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let cmd = line.trim_end();
        let reply: Vec<u8> = if let Some(u) = cmd.strip_prefix("AUTHINFO USER ") {
            user = u.to_owned();
            b"381 password required\r\n".to_vec()
        } else if let Some(p) = cmd.strip_prefix("AUTHINFO PASS ") {
            match &inner.cfg.login {
                Some((u, pw)) if *u == user && pw == p => {
                    authed = true;
                    b"281 welcome\r\n".to_vec()
                }
                _ => b"481 authentication failed\r\n".to_vec(),
            }
        } else if let Some(id) = cmd.strip_prefix("BODY <").and_then(|r| r.strip_suffix('>')) {
            if !authed {
                b"480 authentication required\r\n".to_vec()
            } else {
                *inner
                    .counts
                    .lock()
                    .unwrap()
                    .entry(id.to_owned())
                    .or_default() += 1;
                let delay = inner.cfg.delay_ms.load(Ordering::SeqCst);
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                match inner.articles.get(id) {
                    Some(_) if inner.cfg.missing.contains(id) => {
                        b"430 no such article\r\n".to_vec()
                    }
                    Some(article) => {
                        let mut out = format!("222 0 <{id}>\r\n").into_bytes();
                        out.extend(stuffed(article));
                        out.extend(b".\r\n");
                        if inner.cfg.drop_once.contains(id)
                            && inner.dropped.lock().unwrap().insert(id.to_owned())
                        {
                            let _ = write.write_all(&out[..out.len() / 2]).await;
                            return;
                        }
                        out
                    }
                    None => b"430 no such article\r\n".to_vec(),
                }
            }
        } else if cmd == "QUIT" {
            let _ = write.write_all(b"205 bye\r\n").await;
            return;
        } else {
            b"500 what?\r\n".to_vec()
        };
        if write.write_all(&reply).await.is_err() {
            return;
        }
    }
}

/// The bytes of `dir/name`.
pub fn read(dir: &Path, name: &str) -> Vec<u8> {
    std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}
