//! The Unix socket: a blocking accept loop and one thread per client.
//! There are only ever a handful of local clients.
//!
//! The socket is world-writable; what a client may see is decided per
//! answer from its peer credentials (see `handlers`).

use crate::handlers;
use crate::index::Shared;
use crate::info;
use repertoire_core::Viewer;
use repertoire_proto::{Envelope, ErrorCode, Request, Response, PROTOCOL_VERSION};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Longest request line accepted.
const MAX_LINE: u64 = 1 << 20;

pub fn spawn(shared: Arc<Shared>, path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A socket left over from a crash; refuse to steal one that answers.
    if UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("{} is already served", path.display())));
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    info(format!("listening on {}", path.display()));
    std::thread::Builder::new().name("accept".into()).spawn(move || {
        for conn in listener.incoming() {
            if shared.stop.load(Ordering::Relaxed) {
                return;
            }
            let Ok(stream) = conn else { continue };
            let shared = shared.clone();
            let _ = std::thread::Builder::new().name("client".into()).spawn(move || {
                if let Err(e) = serve(&shared, stream) {
                    shared.debug(format!("client: {e}"));
                }
            });
        }
    })?;
    Ok(())
}

fn serve(shared: &Shared, stream: UnixStream) -> io::Result<()> {
    let viewer = peer(&stream)?;
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut reader).take(MAX_LINE).read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Envelope<Request>>(&line) {
            Err(e) => error(ErrorCode::BadRequest, format!("{e}")),
            Ok(env) if env.v != PROTOCOL_VERSION => {
                error(ErrorCode::Version, format!("protocol version {} not supported (this daemon speaks {PROTOCOL_VERSION})", env.v))
            }
            Ok(Envelope { body: Request::Subscribe { paths }, .. }) => {
                send(&mut out, &Response::Subscribed)?;
                let paths = paths.into_iter().map(|p| p.path.to_path()).collect();
                return handlers::subscribe(shared, &viewer, paths, |r| send(&mut out, &r));
            }
            Ok(env) => handlers::answer(shared, &viewer, env.body),
        };
        send(&mut out, &reply)?;
    }
}

fn error(code: ErrorCode, message: String) -> Response {
    Response::Error { code, message }
}

fn send(out: &mut UnixStream, r: &Response) -> io::Result<()> {
    let mut j = serde_json::to_vec(&Envelope::new(r)).map_err(io::Error::other)?;
    j.push(b'\n');
    out.write_all(&j)
}

/// Who is on the other end: uid and groups from the kernel, never from the
/// client.
fn peer(stream: &UnixStream) -> io::Result<Viewer> {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    #[cfg(target_os = "linux")]
    {
        // SAFETY: ucred is plain data; getsockopt fills at most `len` bytes.
        let cred = unsafe {
            let mut c: libc::ucred = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            if libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut c as *mut _ as *mut libc::c_void, &mut len) != 0 {
                return Err(io::Error::last_os_error());
            }
            c
        };
        let mut gids = vec![cred.gid];
        // Supplementary groups of the peer process.
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", cred.pid)) {
            if let Some(line) = status.lines().find_map(|l| l.strip_prefix("Groups:")) {
                gids.extend(line.split_whitespace().filter_map(|g| g.parse::<u32>().ok()));
            }
        }
        Ok(Viewer { uid: cred.uid, gids })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: plain out-parameters.
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Viewer { uid, gids: vec![gid] })
    }
}
