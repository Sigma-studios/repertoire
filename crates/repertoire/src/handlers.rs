//! Answering requests. Every answer goes through the viewer's permissions:
//! a client sees only the entries it could list itself (see
//! `repertoire_core::access`). Aggregate sizes of folders it may see still
//! include what's inside unreadable subfolders; names never leak.

use crate::engine::{request_scan, Speed};
use crate::index::{Fs, Phase, Shared};
use repertoire_core::sys::lstat;
use repertoire_core::{Id, Kind, Rank, Sort, Tree, Viewer, Visibility, ROOT};
use repertoire_proto as p;
use repertoire_proto::{ErrorCode, Request, Response};
use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Largest pruned tree we send in one answer.
const MAX_TREE_NODES: usize = 200_000;

pub fn answer(shared: &Shared, viewer: &Viewer, req: Request) -> Response {
    match handle(shared, viewer, req) {
        Ok(r) => r,
        Err((code, message)) => Response::Error { code, message },
    }
}

type Answer = Result<Response, (ErrorCode, String)>;

fn not_found(path: &Path) -> (ErrorCode, String) {
    (ErrorCode::NotFound, format!("{} is not indexed (or not visible to you)", path.display()))
}

fn handle(shared: &Shared, viewer: &Viewer, req: Request) -> Answer {
    match req {
        Request::Hello { client } => {
            if let Some(c) = client {
                shared.debug(format!("hello from {c} (uid {})", viewer.uid));
            }
            let mut capabilities: Vec<String> =
                ["filesystems", "tree", "children", "top", "search", "rescan", "subscribe"].iter().map(|s| s.to_string()).collect();
            if shared.watching() {
                capabilities.push("live".into());
            }
            Ok(Response::Hello(p::HelloInfo {
                daemon: format!("repertoire {}", env!("CARGO_PKG_VERSION")),
                protocol: p::PROTOCOL_VERSION,
                capabilities,
                uid: viewer.uid,
            }))
        }
        Request::Filesystems => Ok(Response::Filesystems { filesystems: shared.filesystems().iter().map(|f| fs_info(f)).collect() }),
        Request::Tree { path, min_bytes, max_depth } => {
            let path = clean(&path.to_path())?;
            let (fs, above) = locate(shared, viewer, &path)?;
            let ix = fs.index.read().unwrap();
            let tree = &ix.tree;
            let check = |m: Id| above.get(&m).copied().unwrap_or(false);
            let mut vis = Visibility::new(tree, viewer, &check);
            let id = tree.lookup(&path).filter(|&id| tree.node(id).is_dir() && vis.reachable(id)).ok_or_else(|| not_found(&path))?;
            let pruned = tree.pruned(id, min_bytes, max_depth, &mut vis);
            let mut count = 0;
            let root = to_tree_node(tree, &pruned, &mut count);
            if count > MAX_TREE_NODES {
                return Err((ErrorCode::TooLarge, format!("{count} folders; raise min_bytes or lower max_depth")));
            }
            Ok(Response::Tree { root })
        }
        Request::Children { path, files, sort, limit, offset } => {
            let path = clean(&path.to_path())?;
            children(shared, viewer, &path, files, sort, limit as usize, offset as usize)
        }
        Request::Top { path, n } => {
            let path = clean(&path.to_path())?;
            let (fs, above) = locate(shared, viewer, &path)?;
            let ix = fs.index.read().unwrap();
            let tree = &ix.tree;
            let check = |m: Id| above.get(&m).copied().unwrap_or(false);
            let mut vis = Visibility::new(tree, viewer, &check);
            let id = tree.lookup(&path).filter(|&id| tree.node(id).is_dir() && vis.reachable(id)).ok_or_else(|| not_found(&path))?;
            let folders = tree
                .top(id, n.min(10_000) as usize, &mut vis)
                .into_iter()
                .map(|d| p::TopEntry { path: tree.path(d).into(), total: tree.size(d), files: tree.dir(d).map_or(0, |x| x.files) })
                .collect();
            Ok(Response::Top { folders })
        }
        Request::Search { query, under, limit, kinds } => search(shared, viewer, &query, under.0.is_some().then(|| under.to_path()), limit.min(10_000) as usize, &kinds),
        Request::Rescan { fs_id } => {
            let mut started = Vec::new();
            for fs in shared.filesystems() {
                if fs_id.as_deref().is_none_or(|id| id == fs.id()) {
                    request_scan(shared, &fs, Speed::Idle);
                    started.push(fs.id().to_string());
                }
            }
            if started.is_empty() {
                return Err((ErrorCode::NotFound, format!("no filesystem {}", fs_id.unwrap_or_default())));
            }
            Ok(Response::Rescan { started })
        }
        Request::Subscribe { .. } => Err((ErrorCode::BadRequest, "Subscribe is handled by the connection".into())),
    }
}

/// Absolute, no `..`, no trailing slash.
fn clean(path: &Path) -> Result<PathBuf, (ErrorCode, String)> {
    if !path.is_absolute() {
        return Err((ErrorCode::BadRequest, format!("{} is not absolute", path.display())));
    }
    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::RootDir | Component::CurDir => {}
            _ => return Err((ErrorCode::BadRequest, format!("{} contains ..", path.display()))),
        }
    }
    Ok(out)
}

/// The filesystem holding `path`, and for each of its mount nodes whether
/// the viewer can get through the folders above it (checked live: they're
/// on other filesystems, or not indexed at all).
type Located = (Arc<Fs>, HashMap<Id, bool>);

fn locate(shared: &Shared, viewer: &Viewer, path: &Path) -> Result<Located, (ErrorCode, String)> {
    let fs = shared.fs_for(path).ok_or_else(|| not_found(path))?;
    let above = above_mounts(&fs, viewer);
    Ok((fs, above))
}

fn above_mounts(fs: &Fs, viewer: &Viewer) -> HashMap<Id, bool> {
    let ix = fs.index.read().unwrap();
    let mounts: Vec<(Id, PathBuf)> = ix.tree.mounts().iter().map(|&m| (m, ix.tree.path(m))).collect();
    drop(ix);
    mounts
        .into_iter()
        .map(|(m, path)| {
            let ok = viewer.is_root()
                || path.ancestors().skip(1).all(|a| lstat(a).is_ok_and(|st| viewer.bits(st.mode, st.uid, st.gid) & 1 != 0));
            (m, ok)
        })
        .collect()
}

fn to_tree_node(tree: &Tree, p: &repertoire_core::Pruned, count: &mut usize) -> p::TreeNode {
    *count += 1;
    p::TreeNode {
        path: tree.path(p.id).into(),
        size: p.size,
        own: p.own,
        children: if *count > MAX_TREE_NODES { Vec::new() } else { p.children.iter().map(|c| to_tree_node(tree, c, count)).collect() },
    }
}

fn kind(k: Kind) -> p::Kind {
    match k {
        Kind::Dir => p::Kind::Dir,
        Kind::File => p::Kind::File,
        Kind::Symlink => p::Kind::Symlink,
        Kind::Other => p::Kind::Other,
    }
}

fn children(shared: &Shared, viewer: &Viewer, path: &Path, files: bool, sort: p::Sort, limit: usize, offset: usize) -> Answer {
    let (fs, above) = locate(shared, viewer, path)?;
    let sort = match sort {
        p::Sort::Name => Sort::Name,
        p::Sort::Size => Sort::Size,
        p::Sort::Mtime => Sort::Mtime,
    };
    let mut listing = {
        let ix = fs.index.read().unwrap();
        let tree = &ix.tree;
        let check = |m: Id| above.get(&m).copied().unwrap_or(false);
        let mut vis = Visibility::new(tree, viewer, &check);
        let id = tree.lookup(path).filter(|&id| tree.node(id).is_dir() && vis.reachable(id)).ok_or_else(|| not_found(path))?;
        let listable = vis.listable(id);
        let (dirs, file_ids) = tree.listing(id, sort, &mut vis);
        let total = tree.size(id);
        let dir_entry = |d: Id| {
            let n = tree.node(d);
            let x = tree.dir(d).unwrap();
            p::DirEntry {
                name: p::WireName::new(n.name.to_vec()),
                total: x.total,
                files: x.files,
                dirs: x.dirs,
                mtime: n.mtime,
                unreadable: tree.is_unreadable(d),
                mount: None,
            }
        };
        p::Listing {
            path: path.into(),
            total,
            hidden_bytes: if listable { 0 } else { total.saturating_sub(tree.node(id).bytes) },
            dir_count: dirs.len() as u32,
            file_count: if files { file_ids.len() as u32 } else { 0 },
            dirs: dirs.iter().map(|&d| dir_entry(d)).collect(),
            files: if files {
                file_ids
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|&f| {
                        let n = tree.node(f);
                        p::FileEntry { name: p::WireName::new(n.name.to_vec()), kind: kind(n.kind), bytes: tree.size(f), apparent: n.apparent, mtime: n.mtime }
                    })
                    .collect()
            } else {
                Vec::new()
            },
        }
    };
    // Filesystems mounted directly inside this folder show up as folders.
    if listing.hidden_bytes == 0 || viewer.is_root() {
        for other in shared.filesystems() {
            if Arc::ptr_eq(&other, &fs) {
                continue;
            }
            for mp in &other.fs.mount_points {
                if mp.parent() != Some(path) {
                    continue;
                }
                let ix = other.index.read().unwrap();
                if let Some(m) = ix.tree.lookup(mp) {
                    let x = ix.tree.dir(m).unwrap();
                    listing.dirs.push(p::DirEntry {
                        name: p::WireName::new(repertoire_core::sys::name_of(mp).to_vec()),
                        total: x.total,
                        files: x.files,
                        dirs: x.dirs,
                        mtime: ix.tree.node(m).mtime,
                        unreadable: false,
                        mount: Some(other.id().to_string()),
                    });
                }
            }
        }
        listing.dir_count = listing.dirs.len() as u32;
    }
    match sort {
        Sort::Name => listing.dirs.sort_by(|a, b| a.name.bytes().cmp(b.name.bytes())),
        Sort::Size => listing.dirs.sort_by_key(|d| std::cmp::Reverse(d.total)),
        Sort::Mtime => listing.dirs.sort_by_key(|d| std::cmp::Reverse(d.mtime)),
    }
    listing.dirs = listing.dirs.into_iter().skip(offset).take(limit).collect();
    Ok(Response::Children(listing))
}

fn search(shared: &Shared, viewer: &Viewer, query: &str, under: Option<PathBuf>, limit: usize, kinds: &[p::Kind]) -> Answer {
    let kinds: Vec<Kind> = kinds
        .iter()
        .map(|k| match k {
            p::Kind::Dir => Kind::Dir,
            p::Kind::File => Kind::File,
            p::Kind::Symlink => Kind::Symlink,
            p::Kind::Other => Kind::Other,
        })
        .collect();
    let under = under.map(|u| clean(&u)).transpose()?;
    // (rank, name length, -size, path) for a stable merge across filesystems.
    let mut hits: Vec<(Rank, usize, std::cmp::Reverse<u64>, PathBuf, p::Kind)> = Vec::new();
    for fs in shared.filesystems() {
        // Where to look in this filesystem: everything, one subtree, or the
        // mount points that are below `under`.
        let roots: Vec<Option<PathBuf>> = match &under {
            None => vec![None],
            Some(u) if fs.mount_for(u).is_some() && shared.fs_for(u).is_some_and(|f| Arc::ptr_eq(&f, &fs)) => vec![Some(u.clone())],
            Some(u) => fs.fs.mount_points.iter().filter(|m| m.starts_with(u)).map(|m| Some(m.clone())).collect(),
        };
        if roots.is_empty() {
            continue;
        }
        let above = above_mounts(&fs, viewer);
        let ix = fs.index.read().unwrap();
        let tree = &ix.tree;
        let check = |m: Id| above.get(&m).copied().unwrap_or(false);
        let mut vis = Visibility::new(tree, viewer, &check);
        for root in roots {
            let scope = match &root {
                None => None,
                Some(r) => match tree.lookup(r) {
                    Some(id) if vis.reachable(id) => Some(id),
                    _ => continue,
                },
            };
            for (id, rank) in tree.search(query, scope, &kinds, limit, &mut vis) {
                let n = tree.node(id);
                hits.push((rank, n.name.len(), std::cmp::Reverse(tree.size(id)), tree.path(id), kind(n.kind)));
            }
        }
    }
    hits.sort_by(|a, b| (a.0, a.1, a.2, &a.3).cmp(&(b.0, b.1, b.2, &b.3)));
    hits.truncate(limit);
    let results = hits
        .into_iter()
        .map(|(rank, _, size, path, kind)| p::SearchHit {
            path: path.into(),
            kind,
            bytes: size.0,
            rank: match rank {
                Rank::Exact => p::Rank::Exact,
                Rank::Prefix => p::Rank::Prefix,
                Rank::WordStart => p::Rank::Word,
                Rank::Substring => p::Rank::Substring,
            },
        })
        .collect();
    Ok(Response::Search { results })
}

/// `expected`: what the scan should end up seeing (the previous index's
/// total when there is one, else the filesystem's used space).
pub fn fs_state(fs: &Fs, phase: Phase, expected: u64) -> p::FsState {
    let seen = fs.scanner.progress.load(Ordering::Relaxed);
    let progress = if expected == 0 { 0.0 } else { (seen as f64 / expected as f64).min(1.0) as f32 };
    match phase {
        Phase::Scanning => p::FsState::Scanning { progress, bytes_seen: seen },
        Phase::Reconciling => p::FsState::Reconciling { progress, bytes_seen: seen },
        Phase::Stale => p::FsState::Stale { progress, bytes_seen: seen },
        Phase::Live => p::FsState::Live,
        Phase::Static => p::FsState::Static,
    }
}

pub fn fs_info(fs: &Fs) -> p::FsInfo {
    let (total, used) = repertoire_core::sys::space(&fs.fs.mount_points[0]).unwrap_or((0, 0));
    let ix = fs.index.read().unwrap();
    let root = ix.tree.dir(ROOT).cloned().unwrap_or_default();
    // A directory indexed on request doesn't own its volume's used space.
    let expected = match () {
        _ if root.total > 0 => root.total,
        _ if fs.fs.fs_type == "dir" => 0,
        _ => used,
    };
    p::FsInfo {
        id: fs.fs.id.clone(),
        device: fs.fs.device.clone(),
        fs_type: fs.fs.fs_type.clone(),
        mount_points: fs.fs.mount_points.iter().map(|m| p::MountPoint { path: m.as_path().into() }).collect(),
        total,
        used,
        indexed: root.total,
        files: root.files,
        // The mount nodes themselves are directories too.
        dirs: root.dirs,
        state: fs_state(fs, ix.phase, expected),
        last_full_scan: ix.last_full_scan.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64),
    }
}

/// Stream `Changed` for the watched folders and `FsState` for every
/// filesystem, each at most once a second. Returns when the client goes.
pub fn subscribe(shared: &Shared, viewer: &Viewer, paths: Vec<PathBuf>, mut send: impl FnMut(Response) -> io::Result<()>) -> io::Result<()> {
    let mut sent_totals: HashMap<PathBuf, u64> = HashMap::new();
    let mut sent_states: HashMap<String, p::FsState> = HashMap::new();
    let mut seen = u64::MAX;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let started = Instant::now();
        for path in &paths {
            let Ok(path) = clean(path) else { continue };
            let Some(fs) = shared.fs_for(&path) else { continue };
            let above = above_mounts(&fs, viewer);
            let total = {
                let ix = fs.index.read().unwrap();
                let check = |m: Id| above.get(&m).copied().unwrap_or(false);
                let mut vis = Visibility::new(&ix.tree, viewer, &check);
                ix.tree.lookup(&path).filter(|&id| vis.reachable(id)).map(|id| ix.tree.size(id))
            };
            if let Some(total) = total {
                if sent_totals.get(&path) != Some(&total) {
                    send(Response::Changed { path: path.as_path().into(), total_bytes: total })?;
                    sent_totals.insert(path, total);
                }
            }
        }
        for fs in shared.filesystems() {
            let info = fs_info(&fs);
            let changed = match (sent_states.get(fs.id()), &info.state) {
                (Some(old), new) => old != new,
                (None, _) => true,
            };
            if changed {
                send(Response::FsState { fs_id: fs.id().to_string(), state: info.state })?;
                sent_states.insert(fs.id().to_string(), info.state);
            }
        }
        // At most once a second; otherwise wait for a change. While a scan
        // runs, progress moves without changes: poll every second.
        std::thread::sleep(Duration::from_secs(1).saturating_sub(started.elapsed()));
        let scanning = sent_states.values().any(|s| !matches!(s, p::FsState::Live | p::FsState::Static));
        seen = shared.changes.wait(seen, Duration::from_secs(if scanning { 1 } else { 30 }));
    }
}
