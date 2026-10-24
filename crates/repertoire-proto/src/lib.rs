//! The répertoire protocol: newline-delimited JSON over a Unix socket.
//!
//! One request per line, one response per line, in order. `Subscribe` turns
//! the connection into a stream of events. Every message carries `v`, the
//! protocol version. See `docs/PROTOCOL.md` for the full description with
//! examples.
//!
//! The [`Client`] is blocking and uses plain `std` sockets.

mod client;
mod wire;

pub use client::{Client, Error, Subscription};
pub use wire::{base64_decode, base64_encode, WireName, WirePath, WireUnder};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PROTOCOL_VERSION: u32 = 1;

/// Where the daemon listens unless told otherwise (`REPERTOIRE_SOCKET`).
pub const DEFAULT_SOCKET: &str = "/run/repertoire/repertoire.sock";

pub fn socket_path() -> PathBuf {
    std::env::var_os("REPERTOIRE_SOCKET").map(PathBuf::from).unwrap_or_else(|| DEFAULT_SOCKET.into())
}

/// A message with its protocol version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub v: u32,
    #[serde(flatten)]
    pub body: T,
}

impl<T> Envelope<T> {
    pub fn new(body: T) -> Self {
        Envelope { v: PROTOCOL_VERSION, body }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    #[default]
    Name,
    /// Largest first.
    Size,
    /// Newest first.
    Mtime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Dir,
    File,
    Symlink,
    /// Sockets, fifos, devices.
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    Hello {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },
    Filesystems,
    /// Folders at least `min_bytes` big, `max_depth` levels down.
    Tree {
        #[serde(flatten)]
        path: WirePath,
        #[serde(default)]
        min_bytes: u64,
        #[serde(default = "default_depth")]
        max_depth: u32,
    },
    Children {
        #[serde(flatten)]
        path: WirePath,
        #[serde(default)]
        files: bool,
        #[serde(default)]
        sort: Sort,
        #[serde(default = "default_limit")]
        limit: u32,
        #[serde(default)]
        offset: u32,
    },
    Top {
        #[serde(flatten)]
        path: WirePath,
        #[serde(default = "default_top")]
        n: u32,
    },
    Search {
        query: String,
        #[serde(flatten)]
        under: WireUnder,
        #[serde(default = "default_search_limit")]
        limit: u32,
        /// Empty means every kind.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        kinds: Vec<Kind>,
    },
    Rescan {
        /// Empty or absent: every filesystem.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fs_id: Option<String>,
    },
    Subscribe {
        paths: Vec<SubscribePath>,
    },
}

fn default_depth() -> u32 {
    64
}
fn default_limit() -> u32 {
    1000
}
fn default_top() -> u32 {
    20
}
fn default_search_limit() -> u32 {
    50
}

/// One path to watch (an object, so it can carry `path_b64`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubscribePath {
    #[serde(flatten)]
    pub path: WirePath,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    Hello(HelloInfo),
    Filesystems { filesystems: Vec<FsInfo> },
    Tree { root: TreeNode },
    Children(Listing),
    Top { folders: Vec<TopEntry> },
    Search { results: Vec<SearchHit> },
    Rescan { started: Vec<String> },
    /// Acknowledges a `Subscribe`; events follow.
    Subscribed,
    /// Streamed after `Subscribe`: a watched folder's total changed.
    Changed {
        #[serde(flatten)]
        path: WirePath,
        total_bytes: u64,
    },
    /// Streamed after `Subscribe`: a filesystem changed state.
    FsState { fs_id: String, state: FsState },
    Error { code: ErrorCode, message: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HelloInfo {
    /// `repertoire 0.1.0`
    pub daemon: String,
    pub protocol: u32,
    pub capabilities: Vec<String>,
    /// The uid the daemon filters answers for.
    pub uid: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FsState {
    /// No data yet: the first scan is running.
    Scanning {
        /// 0..=1, bytes seen over bytes used.
        progress: f32,
        bytes_seen: u64,
    },
    /// Serving a snapshot (or the previous scan) while a scan checks it.
    Reconciling {
        progress: f32,
        bytes_seen: u64,
    },
    /// Kept current by kernel events.
    Live,
    /// Events were lost; numbers may be off until the rescan finishes.
    Stale {
        progress: f32,
        bytes_seen: u64,
    },
    /// Indexed once, not watched (no fanotify: macOS, or not root).
    Static,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FsInfo {
    pub id: String,
    pub device: String,
    pub fs_type: String,
    pub mount_points: Vec<MountPoint>,
    /// From statvfs, as `df` shows them.
    pub total: u64,
    pub used: u64,
    /// What the index can attribute to paths. `used - indexed` is hidden
    /// (unreadable folders, filesystem metadata).
    pub indexed: u64,
    pub files: u64,
    pub dirs: u64,
    pub state: FsState,
    /// Unix seconds of the last completed full scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_full_scan: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MountPoint {
    #[serde(flatten)]
    pub path: WirePath,
}

/// Same shape as sentinelle's `disk::Node`: `size == own + sum(children)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeNode {
    #[serde(flatten)]
    pub path: WirePath,
    pub size: u64,
    /// Bytes directly in this folder plus folders folded into it.
    pub own: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TreeNode>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    #[serde(flatten)]
    pub path: WirePath,
    pub total: u64,
    /// Space below this folder in entries the client may not see.
    pub hidden_bytes: u64,
    /// Before `limit`/`offset`.
    pub dir_count: u32,
    pub file_count: u32,
    pub dirs: Vec<DirEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    #[serde(flatten)]
    pub name: WireName,
    pub total: u64,
    pub files: u64,
    pub dirs: u64,
    pub mtime: i64,
    /// The folder couldn't be read; its size is only its own inode.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unreadable: bool,
    /// Another filesystem is mounted here (its own totals).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    #[serde(flatten)]
    pub name: WireName,
    pub kind: Kind,
    /// Allocated bytes (0 for a hard link counted elsewhere).
    pub bytes: u64,
    pub apparent: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TopEntry {
    #[serde(flatten)]
    pub path: WirePath,
    pub total: u64,
    pub files: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rank {
    Exact,
    Prefix,
    Word,
    Substring,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub path: WirePath,
    pub kind: Kind,
    /// Folder total or file bytes.
    pub bytes: u64,
    pub rank: Rank,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed JSON or unknown request.
    BadRequest,
    /// The client speaks a protocol version the daemon doesn't.
    Version,
    /// The path isn't indexed (or the client may not see it).
    NotFound,
    /// The answer would be too large; narrow the request.
    TooLarge,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(r: &Request) {
        let j = serde_json::to_string(&Envelope::new(r.clone())).unwrap();
        let back: Envelope<Request> = serde_json::from_str(&j).unwrap();
        assert_eq!(&back.body, r, "{j}");
    }

    #[test]
    fn requests_round_trip() {
        round(&Request::Hello { client: Some("sentinelle".into()) });
        round(&Request::Filesystems);
        round(&Request::Tree { path: "/".into(), min_bytes: 5, max_depth: 3 });
        round(&Request::Children { path: WirePath::new(b"/\xff".to_vec()), files: true, sort: Sort::Size, limit: 10, offset: 2 });
        round(&Request::Top { path: "/home".into(), n: 7 });
        round(&Request::Search { query: "steam".into(), under: WireUnder::default(), limit: 5, kinds: vec![Kind::Dir] });
        round(&Request::Search { query: "x".into(), under: "/home".into(), limit: 5, kinds: vec![] });
        round(&Request::Rescan { fs_id: None });
        round(&Request::Subscribe { paths: vec![SubscribePath { path: "/".into() }] });
    }

    #[test]
    fn wire_format_is_readable() {
        let j = serde_json::to_string(&Envelope::new(Request::Tree { path: "/".into(), min_bytes: 1, max_depth: 2 })).unwrap();
        assert_eq!(j, r#"{"v":1,"type":"Tree","path":"/","min_bytes":1,"max_depth":2}"#);
        // Defaults fill in.
        let r: Envelope<Request> = serde_json::from_str(r#"{"v":1,"type":"Search","query":"x"}"#).unwrap();
        assert_eq!(r.body, Request::Search { query: "x".into(), under: WireUnder(None), limit: 50, kinds: vec![] });
        let s = serde_json::to_string(&Envelope::new(Response::FsState { fs_id: "a".into(), state: FsState::Scanning { progress: 0.5, bytes_seen: 3 } })).unwrap();
        assert_eq!(s, r#"{"v":1,"type":"FsState","fs_id":"a","state":{"state":"scanning","progress":0.5,"bytes_seen":3}}"#);
    }
}
