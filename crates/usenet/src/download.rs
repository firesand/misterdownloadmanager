//! One NZB download: connections pull articles from a shared queue, decode them and write each
//! at its place in the file's `.mdm.part`; a file whose articles are all in is renamed to its
//! name. Progress is saved to `<nzb name>.mdm.nzbstate` so a stop, a crash or a failed run
//! continues where it left off.
//!
//! The shape mirrors `mdm_engine::Download` (`start`, `snapshot`, `cancel`, `wait`).
//!
//! The server allows its account only so many connections at once, for every network adapter
//! together: they are spread round-robin over the routes. A server that turns an extra
//! connection away gets one connection fewer for the rest of the run. An article the server
//! does not have (or keeps sending damaged) makes its file incomplete; the other files finish,
//! and the run then ends with [`Error::Incomplete`], the part files kept (a repair with par2
//! files is left to the user's tools).

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use mdm_engine::limiter::Limiter;
use mdm_engine::{Route, naming};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, OnceCell};
use tokio_util::sync::CancellationToken;

use crate::{Error, Nzb, Server, nntp, yenc};

/// Events kept per download.
pub const EVENTS: usize = 50;
/// Longest wait between two attempts of a failing connection.
const BACKOFF_MAX: Duration = Duration::from_secs(15);
/// A damaged article is fetched this many times before it counts as missing: a server sends the
/// same copy each time, but a connection can garble one in passing.
const DAMAGED_TRIES: u8 = 2;
/// How often an idle connection looks for work again.
const IDLE_POLL: Duration = Duration::from_millis(100);
/// How long a connection's speed is averaged over.
const SPEED_WINDOW: Duration = Duration::from_millis(500);
const STATE_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub struct Options {
    pub server: Server,
    /// The network adapters to use; empty means [`Route::Any`]. Connections are spread
    /// round-robin over them, so several adapters can be combined.
    pub routes: Vec<Route>,
    /// Every received body byte is paid to each.
    pub limiters: Vec<Arc<Limiter>>,
    /// How long a silent server is waited for before its connection is dropped.
    pub read_timeout: Duration,
    pub connect_timeout: Duration,
    /// Failed attempts in a row after which a connection gives up (it reconnects between).
    pub max_retries: u32,
    /// The wait after a first failure; it doubles up to a quarter minute.
    pub retry_base: Duration,
    /// How often the resume state is saved.
    pub save_interval: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            server: Server::default(),
            routes: Vec::new(),
            limiters: Vec::new(),
            read_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(15),
            max_retries: 5,
            retry_base: Duration::from_millis(500),
            save_interval: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Connecting,
    Downloading,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug)]
pub enum ConnStatus {
    Connecting,
    Receiving,
    /// Connected, nothing to fetch right now.
    Idle,
    /// Waiting before another attempt, with the last error.
    Retrying(Error),
    Finished,
    Failed(Error),
    /// Closed for good: the server allows no more connections than the others use.
    Retired,
}

#[derive(Clone, Debug)]
pub struct ConnView {
    pub id: usize,
    /// Its place in `Options::routes` (0 for the default route).
    pub route: usize,
    pub status: ConnStatus,
    /// Bytes per second over the last moments (0 while not receiving).
    pub speed: u64,
}

#[derive(Clone, Debug)]
pub struct FileView {
    pub name: String,
    /// The file's size: from its yEnc headers when an article of it was read, else the NZB's.
    pub size: u64,
    /// Decoded bytes of it written.
    pub downloaded: u64,
    pub done: bool,
    /// Where it is: the final path once done, else its part file.
    pub path: PathBuf,
}

/// One entry of the event log; `seq` counts up within one download.
#[derive(Clone, Debug)]
pub struct Event {
    pub seq: u64,
    pub at: SystemTime,
    pub text: String,
}

/// A point-in-time view for progress displays.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub state: State,
    /// The files' sizes added up: their yEnc sizes when known, else what the NZB lists.
    pub total: u64,
    /// Decoded bytes written.
    pub downloaded: u64,
    pub files: Vec<FileView>,
    pub connections: Vec<ConnView>,
    /// Raw body bytes received through each route, for per-network accounting.
    pub per_route: Vec<u64>,
    /// Why it failed (state `Failed`), in English.
    pub error: Option<String>,
    /// The same failure as data.
    pub failure: Option<Error>,
    /// Oldest first, the last [`EVENTS`].
    pub events: Vec<Event>,
}

/// What a finished download made.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The files, in the NZB's order, at their final paths.
    pub files: Vec<PathBuf>,
    /// Their bytes added up.
    pub size: u64,
}

/// A running download. Dropping it does not stop it; call [`Download::cancel`].
pub struct Download {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<Result<Outcome, Error>>,
}

impl Download {
    /// Starts downloading in the background; must be called inside a Tokio runtime. A resume
    /// state of the same NZB in `dir` is picked up: finished files and articles are skipped.
    pub fn start(nzb: Nzb, dir: PathBuf, opts: Options) -> Download {
        let shared = Arc::new(Shared::new(nzb, dir, opts));
        let task = tokio::spawn(run(shared.clone()));
        Download { shared, task }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared.snapshot()
    }

    /// Stops it; the progress is saved, so starting again resumes.
    pub fn cancel(&self) {
        self.shared.cancel.cancel();
    }

    pub async fn wait(self) -> Result<Outcome, Error> {
        match self.task.await {
            Ok(r) => r,
            Err(e) if e.is_cancelled() => Err(Error::Cancelled),
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Resume state

#[derive(Serialize, Deserialize)]
struct StateFile {
    version: u32,
    /// Of the NZB's files and message ids: a state of another NZB is not used.
    fingerprint: u64,
    files: Vec<StateEntry>,
}

#[derive(Serialize, Deserialize)]
struct StateEntry {
    name: String,
    size: Option<u64>,
    /// Decoded bytes of the done articles.
    downloaded: u64,
    /// Indexes into the file's segments (sorted by number) whose data is on disk.
    done: Vec<u32>,
    /// The name it was finished under, once renamed.
    finished: Option<String>,
}

fn fingerprint(nzb: &Nzb) -> u64 {
    // FNV-1a: only to tell one NZB from another.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes.iter().chain(&[0]) {
            h = (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
        }
    };
    for f in &nzb.files {
        feed(f.name.as_bytes());
        for s in &f.segments {
            feed(s.message_id.as_bytes());
        }
        feed(b"\x01");
    }
    h
}

fn state_path(dir: &Path, nzb: &Nzb) -> PathBuf {
    let name = naming::sanitize(nzb.name.as_deref().unwrap_or("usenet"));
    dir.join(format!("{name}.mdm.nzbstate"))
}

fn load_state(path: &Path, nzb: &Nzb) -> Option<StateFile> {
    let state: StateFile = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let fits = state.version == STATE_VERSION
        && state.fingerprint == fingerprint(nzb)
        && state.files.len() == nzb.files.len()
        && state.files.iter().zip(&nzb.files).all(|(s, f)| {
            s.name == f.name && s.done.iter().all(|i| (*i as usize) < f.segments.len())
        });
    fits.then_some(state)
}

// ---------------------------------------------------------------------------------------------
// Shared state

struct FileRt {
    name: String,
    /// What the NZB lists for it.
    declared: u64,
    /// From the yEnc headers, once an article of it has been read.
    size: Option<u64>,
    done: Vec<bool>,
    missing: Vec<bool>,
    /// Segments not yet done.
    remaining: usize,
    downloaded: u64,
    /// Its final path, once renamed.
    finished: Option<PathBuf>,
    /// Opened by the first article written; every connection writes through it.
    handle: Arc<OnceCell<Arc<File>>>,
    /// Written to since the last save.
    dirty: bool,
}

struct Meter {
    since: Instant,
    bytes: u64,
}

struct Core {
    files: Vec<FileRt>,
    /// Articles nobody holds, as (file, segment); the front is next.
    queue: VecDeque<(usize, usize)>,
    in_flight: usize,
    /// Connections running.
    live: usize,
    /// Connections the server lets this run keep, once it has said no.
    allowed: usize,
    state: State,
    conns: Vec<ConnView>,
    meters: Vec<Meter>,
    per_route: Vec<u64>,
    /// Damaged copies received, by article.
    damaged: HashMap<(usize, usize), u8>,
    /// Ends the run for every connection.
    fatal: Option<Error>,
    /// What the last connection that gave up died of.
    last_error: Option<Error>,
    error: Option<String>,
    failure: Option<Error>,
    events: Vec<Event>,
    seq: u64,
}

impl Core {
    fn event(&mut self, text: impl Into<String>) {
        self.seq += 1;
        self.events.push(Event {
            seq: self.seq,
            at: SystemTime::now(),
            text: text.into(),
        });
        if self.events.len() > EVENTS {
            self.events.remove(0);
        }
    }

    fn requeue(&mut self, f: usize, s: usize) {
        self.in_flight -= 1;
        self.queue.push_front((f, s));
    }
}

struct Shared {
    nzb: Nzb,
    dir: PathBuf,
    state_path: PathBuf,
    opts: Options,
    routes: Vec<Route>,
    core: Mutex<Core>,
    /// Told when work comes back to the queue or the run is over.
    wake: Notify,
    /// Stops the connections: a cancel, or a failure no connection can get past.
    stop: CancellationToken,
    cancel: CancellationToken,
}

/// What a connection does next.
enum Next {
    Continue,
    Exit,
}

struct Worker {
    id: usize,
    route: usize,
    conn: Option<nntp::Connection>,
    /// Failed attempts in a row.
    failures: u32,
    /// It has already taken itself off `Core::live`.
    left: bool,
}

/// par2 files last, the small index file first among them: what is fetched first is what the
/// user can start on.
fn rank(name: &str) -> u8 {
    let n = name.to_ascii_lowercase();
    if !n.ends_with(".par2") {
        0
    } else if n.contains(".vol") {
        2
    } else {
        1
    }
}

impl Shared {
    fn new(nzb: Nzb, dir: PathBuf, mut opts: Options) -> Shared {
        let routes = if opts.routes.is_empty() {
            vec![Route::Any]
        } else {
            opts.routes.clone()
        };
        opts.server.connections = opts.server.connections.max(1);
        let state_path = state_path(&dir, &nzb);
        let saved = load_state(&state_path, &nzb);
        let mut files: Vec<FileRt> = Vec::new();
        for (i, f) in nzb.files.iter().enumerate() {
            let mut rt = FileRt {
                name: f.name.clone(),
                declared: f.size(),
                size: None,
                done: vec![false; f.segments.len()],
                missing: vec![false; f.segments.len()],
                remaining: f.segments.len(),
                downloaded: 0,
                finished: None,
                handle: Arc::default(),
                dirty: false,
            };
            if let Some(entry) = saved.as_ref().map(|s| &s.files[i]) {
                resume_file(&mut rt, entry, &dir);
            }
            files.push(rt);
        }
        let mut order: Vec<usize> = (0..files.len()).collect();
        order.sort_by_key(|i| rank(&files[*i].name));
        let queue: VecDeque<(usize, usize)> = order
            .into_iter()
            .flat_map(|f| {
                let done = files[f].done.clone();
                (0..done.len())
                    .filter(move |s| !done[*s])
                    .map(move |s| (f, s))
            })
            .collect();
        let cancel = CancellationToken::new();
        let mut core = Core {
            files,
            queue,
            in_flight: 0,
            live: 0,
            allowed: opts.server.connections as usize,
            state: State::Connecting,
            conns: Vec::new(),
            meters: Vec::new(),
            per_route: vec![0; routes.len()],
            damaged: HashMap::new(),
            fatal: None,
            last_error: None,
            error: None,
            failure: None,
            events: Vec::new(),
            seq: 0,
        };
        if saved.is_some() {
            let kept = core
                .files
                .iter()
                .map(|f| f.done.iter().filter(|d| **d).count())
                .sum::<usize>();
            core.event(format!("Resuming: {kept} articles are already on disk"));
        }
        Shared {
            nzb,
            dir,
            state_path,
            opts,
            routes,
            core: Mutex::new(core),
            wake: Notify::new(),
            stop: cancel.child_token(),
            cancel,
        }
    }

    fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap()
    }

    fn part_path(&self, f: usize) -> PathBuf {
        naming::part_path(&self.dir.join(&self.nzb.files[f].name))
    }

    fn snapshot(&self) -> Snapshot {
        let c = self.core();
        let files: Vec<FileView> = c
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let size = f.size.unwrap_or(f.declared);
                FileView {
                    name: f.name.clone(),
                    size,
                    downloaded: f.downloaded,
                    done: f.finished.is_some(),
                    path: f.finished.clone().unwrap_or_else(|| self.part_path(i)),
                }
            })
            .collect();
        Snapshot {
            state: c.state,
            total: files.iter().map(|f| f.size).sum(),
            downloaded: files.iter().map(|f| f.downloaded).sum(),
            files,
            connections: c.conns.clone(),
            per_route: c.per_route.clone(),
            error: c.error.clone(),
            failure: c.failure.clone(),
            events: c.events.clone(),
        }
    }

    fn set_status(&self, id: usize, status: ConnStatus) {
        let mut c = self.core();
        if !matches!(status, ConnStatus::Receiving) {
            c.conns[id].speed = 0;
            c.meters[id] = Meter {
                since: Instant::now(),
                bytes: 0,
            };
        }
        c.conns[id].status = status;
    }

    /// A connection is up and signed in.
    fn connected(&self) {
        let mut c = self.core();
        if c.state == State::Connecting {
            c.state = State::Downloading;
            c.event("Connected to the news server");
        }
    }

    /// Bytes of an article came in: counted, metered, and paid to the limiters.
    async fn received(&self, id: usize, route: usize, n: usize) {
        {
            let mut c = self.core();
            c.per_route[route] += n as u64;
            let m = &mut c.meters[id];
            m.bytes += n as u64;
            let elapsed = m.since.elapsed();
            if elapsed >= SPEED_WINDOW {
                let speed = (m.bytes as f64 / elapsed.as_secs_f64()) as u64;
                m.since = Instant::now();
                m.bytes = 0;
                c.conns[id].speed = speed;
            }
        }
        for limiter in &self.opts.limiters {
            limiter.acquire(n as u64).await;
        }
    }

    fn claim(&self) -> Option<Option<(usize, usize)>> {
        let mut c = self.core();
        if let Some(job) = c.queue.pop_front() {
            c.in_flight += 1;
            Some(Some(job))
        } else if c.in_flight > 0 {
            // Someone holds an article that may come back.
            Some(None)
        } else {
            None
        }
    }

    /// Ends the run for every connection.
    fn fail_run(&self, e: Error) {
        {
            let mut c = self.core();
            c.event(format!("Stopped: {e}"));
            c.fatal.get_or_insert(e);
        }
        self.stop.cancel();
        self.wake.notify_waiters();
    }

    fn mark_missing(&self, f: usize, s: usize, why: &str) {
        let mut c = self.core();
        c.in_flight -= 1;
        c.damaged.remove(&(f, s));
        c.files[f].missing[s] = true;
        let text = format!(
            "Article {} of {} is not available ({why})",
            self.nzb.files[f].segments[s].number, self.nzb.files[f].name
        );
        c.event(text);
        drop(c);
        self.wake.notify_waiters();
    }

    fn requeue(&self, f: usize, s: usize) {
        self.core().requeue(f, s);
        self.wake.notify_waiters();
    }

    async fn worker(self: Arc<Self>, id: usize) {
        let mut w = Worker {
            id,
            route: id % self.routes.len(),
            conn: None,
            failures: 0,
            left: false,
        };
        while !self.stop.is_cancelled() {
            let Some(job) = self.claim() else { break };
            let Some((f, s)) = job else {
                self.set_status(id, ConnStatus::Idle);
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = tokio::time::sleep(IDLE_POLL) => {}
                    _ = self.stop.cancelled() => {}
                }
                continue;
            };
            match self.attempt(&mut w, f, s).await {
                Ok(()) => w.failures = 0,
                Err(e) => {
                    if let Next::Exit = self.failed(&mut w, f, s, e).await {
                        break;
                    }
                }
            }
        }
        if !w.left {
            self.core().live -= 1;
        }
        {
            let mut c = self.core();
            let status = &mut c.conns[id].status;
            if !matches!(status, ConnStatus::Failed(_) | ConnStatus::Retired) {
                *status = ConnStatus::Finished;
            }
            c.conns[id].speed = 0;
        }
        if let Some(conn) = w.conn.take() {
            conn.quit().await;
        }
        self.wake.notify_waiters();
    }

    /// Fetches, decodes and writes one article.
    async fn attempt(&self, w: &mut Worker, f: usize, s: usize) -> Result<(), Error> {
        let seg = &self.nzb.files[f].segments[s];
        if w.conn.is_none() {
            self.set_status(w.id, ConnStatus::Connecting);
            let route = &self.routes[w.route];
            let connect = nntp::Connection::connect(
                &self.opts.server,
                route,
                self.opts.connect_timeout,
                self.opts.read_timeout,
            );
            let conn = tokio::select! {
                biased;
                _ = self.stop.cancelled() => return Err(Error::Cancelled),
                r = connect => r?,
            };
            w.conn = Some(conn);
            self.connected();
        }
        self.set_status(w.id, ConnStatus::Receiving);
        let (id, route) = (w.id, w.route);
        let conn = w.conn.as_mut().expect("connected above");
        let body = tokio::select! {
            biased;
            _ = self.stop.cancelled() => return Err(Error::Cancelled),
            r = conn.body(&seg.message_id, |n| self.received(id, route, n)) => r?,
        };
        let part = yenc::decode(&body)?;
        drop(body);

        let (cell, fresh, path) = {
            let c = self.core();
            let file = &c.files[f];
            (
                file.handle.clone(),
                file.remaining == file.done.len(),
                self.part_path(f),
            )
        };
        let size = part.file_size;
        let file = cell
            .get_or_try_init(|| open_part(path, fresh, size))
            .await?
            .clone();
        let (begin, len, file_size) = (part.begin, part.data.len() as u64, part.file_size);
        let data = part.data;
        tokio::task::spawn_blocking(move || write_all_at(&file, begin, &data))
            .await
            .map_err(|e| Error::disk(io::Error::other(e)))?
            .map_err(Error::disk)?;

        let last = {
            let mut c = self.core();
            let file = &mut c.files[f];
            if !file.done[s] {
                file.done[s] = true;
                file.remaining -= 1;
                file.downloaded += len;
            }
            file.size.get_or_insert(file_size);
            file.dirty = true;
            let last = file.remaining == 0;
            c.in_flight -= 1;
            c.damaged.remove(&(f, s));
            last
        };
        self.wake.notify_waiters();
        w.failures = 0;
        if last && let Err(e) = self.finish_file(f).await {
            self.fail_run(e);
        }
        Ok(())
    }

    /// What became of an article that did not come in.
    async fn failed(&self, w: &mut Worker, f: usize, s: usize, e: Error) -> Next {
        if self.stop.is_cancelled() {
            self.requeue(f, s);
            return Next::Exit;
        }
        match e {
            Error::MissingArticle(_) => {
                self.mark_missing(f, s, "not on the server");
                Next::Continue
            }
            Error::Yenc(why) => {
                let tries = {
                    let mut c = self.core();
                    let t = c.damaged.entry((f, s)).or_default();
                    *t += 1;
                    *t
                };
                if tries >= DAMAGED_TRIES {
                    self.mark_missing(f, s, &why);
                } else {
                    self.requeue(f, s);
                }
                Next::Continue
            }
            e if !e.is_transient() => {
                w.conn = None;
                self.requeue(f, s);
                self.set_status(w.id, ConnStatus::Failed(e.clone()));
                self.fail_run(e);
                Next::Exit
            }
            e => self.retry(w, f, s, e).await,
        }
    }

    /// A failure another attempt may get past: back off and reconnect, or give up after
    /// `max_retries` in a row.
    async fn retry(&self, w: &mut Worker, f: usize, s: usize, e: Error) -> Next {
        w.conn = None;
        if matches!(e, Error::Refused(_)) {
            let mut c = self.core();
            if c.live > 1 {
                // Over the account's limit: one connection fewer from now on.
                c.requeue(f, s);
                c.live -= 1;
                c.allowed = c.allowed.min(c.live);
                let n = c.allowed;
                c.event(format!("The server allows only {n} connections ({e})"));
                c.conns[w.id].status = ConnStatus::Retired;
                w.left = true;
                drop(c);
                self.wake.notify_waiters();
                return Next::Exit;
            }
        }
        self.requeue(f, s);
        w.failures += 1;
        if w.failures > self.opts.max_retries {
            let mut c = self.core();
            c.event(format!("Connection {} gave up: {e}", w.id + 1));
            c.last_error = Some(e.clone());
            c.conns[w.id].status = ConnStatus::Failed(e);
            return Next::Exit;
        }
        let wait = self
            .opts
            .retry_base
            .saturating_mul(1 << (w.failures - 1).min(10))
            .min(BACKOFF_MAX);
        tracing::debug!(
            connection = w.id,
            failures = w.failures,
            "retrying after: {e}"
        );
        self.core()
            .event(format!("Connection {}: {e}; trying again", w.id + 1));
        self.set_status(w.id, ConnStatus::Retrying(e));
        tokio::select! {
            _ = tokio::time::sleep(wait) => Next::Continue,
            _ = self.stop.cancelled() => Next::Exit,
        }
    }

    /// Every article of file `f` is in: cut it to its real size, sync it, and give it its name
    /// (numbered if a file of that name is there already).
    async fn finish_file(&self, f: usize) -> Result<(), Error> {
        let (cell, size, name) = {
            let mut c = self.core();
            let file = &mut c.files[f];
            let cell = std::mem::take(&mut file.handle);
            (cell, file.size, file.name.clone())
        };
        let dir = self.dir.clone();
        let part = self.part_path(f);
        let dest = tokio::task::spawn_blocking(move || -> io::Result<PathBuf> {
            let file = std::fs::OpenOptions::new().write(true).open(&part)?;
            if let Some(size) = size {
                file.set_len(size)?;
            }
            file.sync_data()?;
            drop(file);
            // The handle the connections wrote through must be closed before the rename
            // (Windows refuses otherwise).
            drop(cell);
            let wanted = dir.join(&name);
            let dest = if wanted.exists() {
                naming::unique_path(&dir, &name)
            } else {
                wanted
            };
            craft_atomic::retry_rename(craft_atomic::RenameRetry::platform(), || {
                std::fs::rename(&part, &dest)
            })?;
            Ok(dest)
        })
        .await
        .map_err(|e| Error::disk(io::Error::other(e)))?
        .map_err(Error::disk)?;
        let mut c = self.core();
        let text = format!("Finished {}", c.files[f].name);
        c.event(text);
        let file = &mut c.files[f];
        file.finished = Some(dest);
        file.dirty = false;
        Ok(())
    }

    /// Writes the resume state: the files' articles are synced first, so what the state lists
    /// is on disk.
    async fn save(&self) {
        let (bytes, handles) = {
            let mut c = self.core();
            let mut handles = Vec::new();
            let mut files = Vec::new();
            for f in &mut c.files {
                if f.dirty {
                    f.dirty = false;
                    if let Some(h) = f.handle.get() {
                        handles.push(h.clone());
                    }
                }
                files.push(StateEntry {
                    name: f.name.clone(),
                    size: f.size,
                    downloaded: f.downloaded,
                    done: (0..f.done.len() as u32)
                        .filter(|i| f.done[*i as usize])
                        .collect(),
                    finished: f
                        .finished
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned()),
                });
            }
            let state = StateFile {
                version: STATE_VERSION,
                fingerprint: fingerprint(&self.nzb),
                files,
            };
            (serde_json::to_vec_pretty(&state), handles)
        };
        let Ok(bytes) = bytes else { return };
        let path = self.state_path.clone();
        let written = tokio::task::spawn_blocking(move || {
            for h in &handles {
                h.sync_data()?;
            }
            craft_atomic::atomic_write(&path, &bytes)
        })
        .await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("cannot save the download's progress: {e}"),
            Err(e) => tracing::warn!("cannot save the download's progress: {e}"),
        }
    }
}

/// A file's saved progress, when what it says is still true on disk.
fn resume_file(rt: &mut FileRt, entry: &StateEntry, dir: &Path) {
    let part = naming::part_path(&dir.join(&rt.name));
    if let Some(name) = &entry.finished {
        let path = dir.join(name);
        if path.is_file() {
            rt.done.fill(true);
            rt.remaining = 0;
            rt.size = entry.size;
            rt.downloaded = entry.downloaded;
            rt.finished = Some(path);
        }
        return;
    }
    if entry.done.is_empty() || !part.is_file() {
        return;
    }
    for i in &entry.done {
        rt.done[*i as usize] = true;
    }
    rt.remaining = rt.done.iter().filter(|d| !**d).count();
    rt.size = entry.size;
    rt.downloaded = entry.downloaded;
}

/// The part file, created with its parent folders. `fresh`: no article of it is on disk, so
/// whatever is there is not ours and goes. The file is made its full size at once when known.
async fn open_part(path: PathBuf, fresh: bool, size: u64) -> Result<Arc<File>, Error> {
    tokio::task::spawn_blocking(move || -> io::Result<Arc<File>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(fresh)
            .open(&path)?;
        if file.metadata()?.len() < size {
            file.set_len(size)?;
        }
        Ok(Arc::new(file))
    })
    .await
    .map_err(|e| Error::disk(io::Error::other(e)))?
    .map_err(Error::disk)
}

/// A positional write: connections write their articles to the same file at once.
fn write_all_at(file: &File, mut at: u64, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_write(file, data, at)?;
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::write_at(file, data, at)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        at += n as u64;
        data = &data[n..];
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The run

async fn run(sh: Arc<Shared>) -> Result<Outcome, Error> {
    let result = run_inner(&sh).await;
    let mut c = sh.core();
    match &result {
        Ok(_) => {
            c.state = State::Done;
            c.event("Download complete");
        }
        Err(Error::Cancelled) => {
            c.state = State::Cancelled;
            c.event("Cancelled");
        }
        Err(e) => {
            c.state = State::Failed;
            c.error = Some(e.to_string());
            c.failure = Some(e.clone());
            c.event(format!("Failed: {e}"));
        }
    }
    drop(c);
    result
}

async fn run_inner(sh: &Arc<Shared>) -> Result<Outcome, Error> {
    tokio::fs::create_dir_all(&sh.dir)
        .await
        .map_err(Error::disk)?;
    // Files whose articles were all in when the last run stopped, before their rename.
    let complete: Vec<usize> = {
        let c = sh.core();
        (0..c.files.len())
            .filter(|f| c.files[*f].remaining == 0 && c.files[*f].finished.is_none())
            .collect()
    };
    for f in complete {
        sh.finish_file(f).await?;
    }

    let workers = {
        let mut c = sh.core();
        let n = (sh.opts.server.connections as usize).min(c.queue.len());
        c.live = n;
        c.conns = (0..n)
            .map(|id| ConnView {
                id,
                route: id % sh.routes.len(),
                status: ConnStatus::Connecting,
                speed: 0,
            })
            .collect();
        c.meters = (0..n)
            .map(|_| Meter {
                since: Instant::now(),
                bytes: 0,
            })
            .collect();
        n
    };
    if workers > 0 {
        let mut set = tokio::task::JoinSet::new();
        for id in 0..workers {
            set.spawn(sh.clone().worker(id));
        }
        // Stopped by its token, never aborted: a save in progress finishes (its blocking write
        // cannot be cancelled), so none lands after the state file is removed.
        let quit = CancellationToken::new();
        let saver = tokio::spawn({
            let (sh, quit) = (sh.clone(), quit.clone());
            async move {
                let mut tick =
                    tokio::time::interval(sh.opts.save_interval.max(Duration::from_millis(10)));
                tick.tick().await;
                loop {
                    tokio::select! {
                        _ = tick.tick() => sh.save().await,
                        _ = quit.cancelled() => break,
                    }
                }
            }
        });
        while let Some(joined) = set.join_next().await {
            if let Err(e) = joined {
                tracing::error!("a connection of the download panicked: {e}");
            }
        }
        quit.cancel();
        let _ = saver.await;
    }
    sh.core().state = if sh.cancel.is_cancelled() {
        State::Cancelled
    } else {
        State::Downloading
    };

    let outcome = outcome(sh);
    if outcome.is_ok() {
        let _ = std::fs::remove_file(&sh.state_path);
    } else {
        sh.save().await;
    }
    outcome
}

fn outcome(sh: &Shared) -> Result<Outcome, Error> {
    let c = sh.core();
    if sh.cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if let Some(e) = &c.fatal {
        return Err(e.clone());
    }
    let unfinished: Vec<usize> = (0..c.files.len())
        .filter(|f| c.files[*f].finished.is_none())
        .collect();
    if unfinished.is_empty() {
        let files = c.files.iter().filter_map(|f| f.finished.clone()).collect();
        return Ok(Outcome {
            files,
            size: c.files.iter().map(|f| f.downloaded).sum(),
        });
    }
    // Articles nobody got and nobody refused: the connections gave up.
    let unfetched = unfinished.iter().any(|f| {
        let file = &c.files[*f];
        (0..file.done.len()).any(|s| !file.done[s] && !file.missing[s])
    });
    if unfetched {
        return Err(c
            .last_error
            .clone()
            .unwrap_or_else(|| Error::Protocol("the connections stopped before the end".into())));
    }
    let missing = unfinished
        .iter()
        .map(|f| c.files[*f].missing.iter().filter(|m| **m).count() as u64)
        .sum();
    Err(Error::Incomplete {
        missing,
        files: unfinished
            .iter()
            .map(|f| c.files[*f].name.clone())
            .collect(),
    })
}
