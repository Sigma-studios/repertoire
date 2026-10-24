//! repertoired: the indexing daemon.

use repertoire::{info, Config, Daemon};
use std::path::PathBuf;

const HELP: &str = "repertoired — keeps an index of disk usage and file names current

USAGE: repertoired [OPTIONS]

OPTIONS:
    --root <DIR>            Index this directory (repeatable) instead of every
                            real filesystem from the mount table
    --exclude <PATH>        Don't index this folder (repeatable)
    --exclude-name <NAME>   Don't index folders with this name (repeatable)
    --state-dir <DIR>       Where snapshots go (default /var/lib/repertoire)
    --no-state              Don't save or load snapshots
    --socket <PATH>         Listen here (default /run/repertoire/repertoire.sock)
    --no-watch              Scan only, then rescan every 20 minutes (no fanotify)
    --foreground            Accepted for compatibility; always in the foreground
    -v, --verbose           Log more
    -h, --help              Show this help

Watching needs root (CAP_SYS_ADMIN and CAP_DAC_READ_SEARCH). Without it the
daemon falls back to scan-only mode.";

fn main() {
    let mut config = Config::default();
    let mut args = std::env::args().skip(1);
    let need = |v: Option<String>, flag: &str| v.unwrap_or_else(|| usage_error(&format!("{flag} needs a value")));
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => config.roots.push(PathBuf::from(need(args.next(), "--root"))),
            "--exclude" => config.excludes.paths.push(PathBuf::from(need(args.next(), "--exclude"))),
            "--exclude-name" => {
                config.excludes.names.insert(need(args.next(), "--exclude-name").into_bytes().into());
            }
            "--state-dir" => config.state_dir = Some(PathBuf::from(need(args.next(), "--state-dir"))),
            "--no-state" => config.state_dir = None,
            "--socket" => config.socket = Some(PathBuf::from(need(args.next(), "--socket"))),
            "--no-watch" => config.watch = false,
            "--foreground" => {}
            "-v" | "--verbose" => config.verbose = true,
            "-h" | "--help" => {
                println!("{HELP}");
                return;
            }
            other => usage_error(&format!("unknown option {other}")),
        }
    }
    if let Some(s) = std::env::var_os("REPERTOIRE_SOCKET") {
        config.socket = Some(s.into());
    }

    // Handle SIGINT/SIGTERM in this thread only: block them before any
    // other thread exists, then wait for one.
    // SAFETY: plain signal-mask calls on a zeroed, then filled, set.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    let daemon = match Daemon::start(config) {
        Ok(d) => d,
        Err(e) => {
            info(format!("can't start: {e}"));
            std::process::exit(1);
        }
    };
    let mut sig: libc::c_int = 0;
    // SAFETY: `set` is initialized; sigwait writes the signal number.
    unsafe { libc::sigwait(&set, &mut sig) };
    info("stopping");
    daemon.shutdown();
}

fn usage_error(msg: &str) -> ! {
    eprintln!("repertoired: {msg}\n\n{HELP}");
    std::process::exit(2);
}
