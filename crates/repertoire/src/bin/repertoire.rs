//! repertoire: a thin command-line client for the daemon, plus `scan`, which
//! indexes a folder in-process (no daemon needed; what runs on macOS).

use repertoire_core::{Excludes, Scanner, Tree, Viewer, Visibility, ROOT};
use repertoire_proto::{self as p, Client, Response};
use std::path::{Path, PathBuf};
use std::time::Instant;

const HELP: &str = "repertoire — where disk space goes, and what files are called

USAGE: repertoire [--socket PATH] [--json] <COMMAND>

COMMANDS:
    status                       Daemon and filesystems
    tree [PATH] [--min SIZE] [--depth N]
                                 Folders of at least SIZE (default 1% of PATH)
    top [PATH] [-n N]            The N largest folders that don't contain one another
    ls [PATH] [-a] [--sort name|size|mtime] [--limit N]
                                 Subfolders (and files with -a)
    search QUERY [--under PATH] [--limit N] [--kind dir|file|symlink]
                                 Find files and folders by name
    rescan [FS_ID]               Rescan a filesystem (or all) in the background
    watch PATH...                Print size changes of these folders as they happen
    scan PATH [--top N] [--exclude PATH] [--exclude-name NAME]
                                 Index PATH here and now, without the daemon

PATH defaults to the current folder. SIZE accepts K, M, G, T suffixes.";

struct Opts {
    json: bool,
    socket: Option<PathBuf>,
}

fn main() {
    // Exit quietly when the reader of our output goes away (`| head`).
    // SAFETY: restoring the default disposition of one signal.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut opts = Opts { json: false, socket: None };
    while let Some(a) = args.first() {
        match a.as_str() {
            "--json" => opts.json = true,
            "--socket" => {
                args.remove(0);
                opts.socket = Some(PathBuf::from(args.first().cloned().unwrap_or_else(|| usage("--socket needs a path"))));
            }
            "-h" | "--help" => {
                println!("{HELP}");
                return;
            }
            _ => break,
        }
        args.remove(0);
    }
    let Some(cmd) = args.first().cloned() else { usage("missing command") };
    let rest = &args[1..];
    let r = match cmd.as_str() {
        "scan" => scan(rest),
        "status" => status(&opts),
        "tree" => tree(&opts, rest),
        "top" => top(&opts, rest),
        "ls" => ls(&opts, rest),
        "search" => search(&opts, rest),
        "rescan" => rescan(&opts, rest),
        "watch" => watch(&opts, rest),
        other => usage(&format!("unknown command {other}")),
    };
    if let Err(e) = r {
        eprintln!("repertoire: {e}");
        std::process::exit(1);
    }
}

fn usage(msg: &str) -> ! {
    eprintln!("repertoire: {msg}\n\n{HELP}");
    std::process::exit(2);
}

type Res = Result<(), Box<dyn std::error::Error>>;

fn connect(opts: &Opts) -> Result<Client, Box<dyn std::error::Error>> {
    let path = opts.socket.clone().unwrap_or_else(p::socket_path);
    Client::connect_to(&path).map_err(|e| format!("can't reach the daemon at {} ({e}). Is repertoired running?", path.display()).into())
}

/// Flags of the form `--name value` and positional arguments.
struct Args<'a> {
    rest: &'a [String],
}

impl<'a> Args<'a> {
    fn flag(&self, names: &[&str]) -> bool {
        self.rest.iter().any(|a| names.contains(&a.as_str()))
    }

    fn value(&self, names: &[&str]) -> Option<&'a str> {
        let i = self.rest.iter().position(|a| names.contains(&a.as_str()))?;
        Some(self.rest.get(i + 1).map(|s| s.as_str()).unwrap_or_else(|| usage(&format!("{} needs a value", names[0]))))
    }

    fn values(&self, name: &str) -> Vec<&'a str> {
        self.rest.windows(2).filter(|w| w[0] == name).map(|w| w[1].as_str()).collect()
    }

    /// Arguments that are neither flags nor flag values.
    fn positional(&self, with_values: &[&str]) -> Vec<&'a str> {
        let mut out = Vec::new();
        let mut skip = false;
        for a in self.rest {
            if skip {
                skip = false;
            } else if with_values.contains(&a.as_str()) {
                skip = true;
            } else if !a.starts_with('-') || a == "-" {
                out.push(a.as_str());
            }
        }
        out
    }
}

fn abs(path: Option<&str>) -> PathBuf {
    let p = PathBuf::from(path.unwrap_or("."));
    p.canonicalize().unwrap_or(p)
}

fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    let (num, mult) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        Some('T') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    (num.parse::<f64>().unwrap_or_else(|_| usage(&format!("bad size {s}"))) * mult as f64) as u64
}

fn size(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn json(r: &Response) {
    println!("{}", serde_json::to_string(&p::Envelope::new(r)).unwrap());
}

fn state(s: &p::FsState) -> String {
    match s {
        p::FsState::Scanning { progress, .. } => format!("scanning {:.0}%", progress * 100.0),
        p::FsState::Reconciling { progress, .. } => format!("reconciling {:.0}%", progress * 100.0),
        p::FsState::Stale { .. } => "stale (rescanning)".into(),
        p::FsState::Live => "live".into(),
        p::FsState::Static => "scanned (not watched)".into(),
    }
}

fn status(opts: &Opts) -> Res {
    let mut c = connect(opts)?;
    let hello = c.hello("repertoire-cli")?;
    let fss = c.filesystems()?;
    if opts.json {
        json(&Response::Hello(hello));
        json(&Response::Filesystems { filesystems: fss });
        return Ok(());
    }
    println!("{} (protocol {}), answering for uid {}", hello.daemon, hello.protocol, hello.uid);
    for fs in fss {
        let mounts: Vec<String> = fs.mount_points.iter().map(|m| m.path.lossy()).collect();
        println!();
        println!("{}  {} · {}", mounts.join("  "), fs.device, fs.fs_type);
        println!("  {} used of {}, {} indexed in {} files and {} folders", size(fs.used), size(fs.total), size(fs.indexed), fs.files, fs.dirs);
        if fs.used > fs.indexed && fs.fs_type != "dir" {
            println!("  {} not attributable (unreadable folders, filesystem metadata)", size(fs.used - fs.indexed));
        }
        let ago = fs.last_full_scan.map(|t| {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
            format!(", last full scan {} min ago", (now - t).max(0) / 60)
        });
        println!("  {}{}", state(&fs.state), ago.unwrap_or_default());
    }
    Ok(())
}

fn tree(opts: &Opts, rest: &[String]) -> Res {
    let a = Args { rest };
    let path = abs(a.positional(&["--min", "--depth"]).first().copied());
    let depth: u32 = a.value(&["--depth"]).map_or(64, |v| v.parse().unwrap_or_else(|_| usage("bad --depth")));
    let mut c = connect(opts)?;
    let min = match a.value(&["--min"]) {
        Some(v) => parse_size(v),
        None => c.tree(&path, u64::MAX, 0)?.size / 100,
    };
    let root = c.tree(&path, min, depth)?;
    if opts.json {
        json(&Response::Tree { root });
        return Ok(());
    }
    fn show(n: &p::TreeNode, depth: usize, total: u64) {
        let name = if depth == 0 { n.path.lossy() } else { n.path.to_path().file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default() };
        let pct = n.size as f64 / total.max(1) as f64 * 100.0;
        println!("{:>10}  {:>5.1}%  {}{}", size(n.size), pct, "  ".repeat(depth), name);
        for c in &n.children {
            show(c, depth + 1, total);
        }
    }
    show(&root, 0, root.size);
    Ok(())
}

fn top(opts: &Opts, rest: &[String]) -> Res {
    let a = Args { rest };
    let path = abs(a.positional(&["-n"]).first().copied());
    let n: u32 = a.value(&["-n"]).map_or(20, |v| v.parse().unwrap_or_else(|_| usage("bad -n")));
    let folders = connect(opts)?.top(&path, n)?;
    if opts.json {
        json(&Response::Top { folders });
        return Ok(());
    }
    for f in folders {
        println!("{:>10}  {}", size(f.total), f.path.lossy());
    }
    Ok(())
}

fn ls(opts: &Opts, rest: &[String]) -> Res {
    let a = Args { rest };
    let path = abs(a.positional(&["--sort", "--limit"]).first().copied());
    let sort = match a.value(&["--sort"]).unwrap_or("size") {
        "name" => p::Sort::Name,
        "size" => p::Sort::Size,
        "mtime" => p::Sort::Mtime,
        s => usage(&format!("bad --sort {s}")),
    };
    let limit: u32 = a.value(&["--limit"]).map_or(1000, |v| v.parse().unwrap_or_else(|_| usage("bad --limit")));
    let l = connect(opts)?.children(&path, a.flag(&["-a", "--files"]), sort, limit, 0)?;
    if opts.json {
        json(&Response::Children(l));
        return Ok(());
    }
    println!("{}  {}", size(l.total), l.path.lossy());
    for d in &l.dirs {
        let note = match (&d.mount, d.unreadable) {
            (Some(fs), _) => format!("  (mount: {fs})"),
            (None, true) => "  (unreadable)".into(),
            _ => String::new(),
        };
        println!("{:>10}  {}/{note}", size(d.total), d.name.lossy());
    }
    for f in &l.files {
        println!("{:>10}  {}", size(f.bytes), f.name.lossy());
    }
    if l.hidden_bytes > 0 {
        println!("{:>10}  (not visible to you)", size(l.hidden_bytes));
    }
    Ok(())
}

fn search(opts: &Opts, rest: &[String]) -> Res {
    let a = Args { rest };
    let pos = a.positional(&["--under", "--limit", "--kind"]);
    let query = pos.first().copied().unwrap_or_else(|| usage("search needs a query"));
    let under = a.value(&["--under"]).map(|u| abs(Some(u)));
    let limit: u32 = a.value(&["--limit"]).map_or(30, |v| v.parse().unwrap_or_else(|_| usage("bad --limit")));
    let kinds: Vec<p::Kind> = a
        .values("--kind")
        .iter()
        .map(|k| match *k {
            "dir" => p::Kind::Dir,
            "file" => p::Kind::File,
            "symlink" => p::Kind::Symlink,
            "other" => p::Kind::Other,
            k => usage(&format!("bad --kind {k}")),
        })
        .collect();
    let t = Instant::now();
    let results = connect(opts)?.search(query, under.as_deref(), limit, &kinds)?;
    if opts.json {
        json(&Response::Search { results });
        return Ok(());
    }
    for r in &results {
        let slash = if r.kind == p::Kind::Dir { "/" } else { "" };
        println!("{:>10}  {}{slash}", size(r.bytes), r.path.lossy());
    }
    eprintln!("{} results in {:.1?}", results.len(), t.elapsed());
    Ok(())
}

fn rescan(opts: &Opts, rest: &[String]) -> Res {
    let started = connect(opts)?.rescan(rest.first().map(|s| s.as_str()))?;
    if opts.json {
        json(&Response::Rescan { started });
        return Ok(());
    }
    for id in started {
        println!("rescanning {id}");
    }
    Ok(())
}

fn watch(opts: &Opts, rest: &[String]) -> Res {
    let paths: Vec<PathBuf> = if rest.is_empty() { vec![abs(None)] } else { rest.iter().map(|r| abs(Some(r))).collect() };
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    for ev in connect(opts)?.subscribe(&refs)? {
        let ev = ev?;
        if opts.json {
            json(&ev);
            continue;
        }
        match ev {
            Response::Changed { path, total_bytes } => println!("{:>10}  {}", size(total_bytes), path.lossy()),
            Response::FsState { fs_id, state: s } => println!("{:>10}  {fs_id}: {}", "", state(&s)),
            _ => {}
        }
    }
    Ok(())
}

/// Index a folder in this process and print what the daemon would say.
fn scan(rest: &[String]) -> Res {
    let a = Args { rest };
    let pos = a.positional(&["--top", "--exclude", "--exclude-name"]);
    let path = abs(Some(pos.first().copied().unwrap_or_else(|| usage("scan needs a path"))));
    let n: usize = a.value(&["--top"]).map_or(20, |v| v.parse().unwrap_or_else(|_| usage("bad --top")));
    let excludes = Excludes {
        paths: a.values("--exclude").iter().map(|p| abs(Some(p))).collect(),
        names: a.values("--exclude-name").iter().map(|n| n.as_bytes().into()).collect(),
    };
    repertoire_core::sys::raise_fd_limit();
    #[cfg(target_os = "linux")]
    let skip = repertoire_linux::mounts::filesystems().1.into_iter().filter(|m| m != &path).collect();
    #[cfg(not(target_os = "linux"))]
    let skip = Default::default();
    let scanner = Scanner { excludes, skip, ..Scanner::default() };
    let t = Instant::now();
    let scan = scanner.scan_root(&path)?;
    let walked = t.elapsed();
    let mut tree = Tree::new();
    let mount = tree.graft(ROOT, scan, false);
    let total = t.elapsed();
    let d = tree.dir(mount).unwrap();
    println!(
        "{}: {} ({} bytes) in {} files and {} folders",
        path.display(),
        size(d.total),
        d.total,
        d.files,
        d.dirs + 1
    );
    println!(
        "scanned in {:.2?} (walk {:.2?}, {} threads); index uses about {} ({} bytes per entry)",
        total,
        walked,
        rayon::current_num_threads(),
        size(tree.memory() as u64),
        tree.memory() / tree.len().max(1)
    );
    println!();
    let viewer = Viewer::root();
    let all = |_| true;
    let mut vis = Visibility::new(&tree, &viewer, &all);
    for id in tree.top(mount, n, &mut vis) {
        let pct = tree.size(id) as f64 / d.total.max(1) as f64 * 100.0;
        println!("{:>10}  {:>5.1}%  {}", size(tree.size(id)), pct, tree.path(id).display());
    }
    Ok(())
}
