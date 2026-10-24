//! A small blocking client: one method per request, `subscribe()` for the
//! event stream.

use crate::*;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The daemon answered something we didn't expect.
    Protocol(String),
    /// The daemon answered with an error.
    Daemon { code: ErrorCode, message: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Daemon { message, .. } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    line: String,
}

impl Client {
    /// Connect to [`socket_path`].
    pub fn connect() -> Result<Client> {
        Client::connect_to(&socket_path())
    }

    pub fn connect_to(path: &Path) -> Result<Client> {
        let s = UnixStream::connect(path)?;
        s.set_read_timeout(Some(Duration::from_secs(60)))?;
        Ok(Client { reader: BufReader::new(s.try_clone()?), writer: s, line: String::new() })
    }

    /// Send one request and read one response. Daemon errors become `Err`.
    pub fn call(&mut self, req: Request) -> Result<Response> {
        let mut j = serde_json::to_vec(&Envelope::new(req)).map_err(|e| Error::Protocol(e.to_string()))?;
        j.push(b'\n');
        self.writer.write_all(&j)?;
        self.read()
    }

    fn read(&mut self) -> Result<Response> {
        self.line.clear();
        if self.reader.read_line(&mut self.line)? == 0 {
            return Err(Error::Io(io::Error::from(io::ErrorKind::UnexpectedEof)));
        }
        let env: Envelope<Response> = serde_json::from_str(&self.line).map_err(|e| Error::Protocol(format!("{e}: {}", self.line.trim())))?;
        match env.body {
            Response::Error { code, message } => Err(Error::Daemon { code, message }),
            r => Ok(r),
        }
    }

    pub fn hello(&mut self, client: &str) -> Result<HelloInfo> {
        match self.call(Request::Hello { client: Some(client.into()) })? {
            Response::Hello(h) => Ok(h),
            r => Err(unexpected(r)),
        }
    }

    pub fn filesystems(&mut self) -> Result<Vec<FsInfo>> {
        match self.call(Request::Filesystems)? {
            Response::Filesystems { filesystems } => Ok(filesystems),
            r => Err(unexpected(r)),
        }
    }

    pub fn tree(&mut self, path: &Path, min_bytes: u64, max_depth: u32) -> Result<TreeNode> {
        match self.call(Request::Tree { path: path.into(), min_bytes, max_depth })? {
            Response::Tree { root } => Ok(root),
            r => Err(unexpected(r)),
        }
    }

    pub fn children(&mut self, path: &Path, files: bool, sort: Sort, limit: u32, offset: u32) -> Result<Listing> {
        match self.call(Request::Children { path: path.into(), files, sort, limit, offset })? {
            Response::Children(l) => Ok(l),
            r => Err(unexpected(r)),
        }
    }

    pub fn top(&mut self, path: &Path, n: u32) -> Result<Vec<TopEntry>> {
        match self.call(Request::Top { path: path.into(), n })? {
            Response::Top { folders } => Ok(folders),
            r => Err(unexpected(r)),
        }
    }

    pub fn search(&mut self, query: &str, under: Option<&Path>, limit: u32, kinds: &[Kind]) -> Result<Vec<SearchHit>> {
        let under = under.map(WireUnder::from).unwrap_or_default();
        match self.call(Request::Search { query: query.into(), under, limit, kinds: kinds.to_vec() })? {
            Response::Search { results } => Ok(results),
            r => Err(unexpected(r)),
        }
    }

    /// Start an idle rescan of one filesystem (or all). Returns the ids.
    pub fn rescan(&mut self, fs_id: Option<&str>) -> Result<Vec<String>> {
        match self.call(Request::Rescan { fs_id: fs_id.map(Into::into) })? {
            Response::Rescan { started } => Ok(started),
            r => Err(unexpected(r)),
        }
    }

    /// Turn this connection into an event stream for `paths`.
    pub fn subscribe(mut self, paths: &[&Path]) -> Result<Subscription> {
        let paths = paths.iter().map(|p| SubscribePath { path: (*p).into() }).collect();
        match self.call(Request::Subscribe { paths })? {
            Response::Subscribed => {}
            r => return Err(unexpected(r)),
        }
        self.writer.set_read_timeout(None)?;
        Ok(Subscription { client: self })
    }
}

fn unexpected(r: Response) -> Error {
    Error::Protocol(format!("unexpected answer {r:?}"))
}

/// Events after `Subscribe`: `Changed` and `FsState` responses. Ends when
/// the daemon goes away.
pub struct Subscription {
    client: Client,
}

impl Subscription {
    /// Give up waiting after `timeout` (`next()` then yields an `Io` error
    /// of kind `WouldBlock`/`TimedOut`).
    pub fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.client.writer.set_read_timeout(timeout)
    }
}

impl Iterator for Subscription {
    type Item = Result<Response>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.client.read() {
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => None,
            r => Some(r),
        }
    }
}
