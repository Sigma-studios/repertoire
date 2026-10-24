//! The filesystems worth indexing, from `/proc/self/mountinfo`.

use repertoire_core::mounts::parse_mountinfo;
use repertoire_core::sys::space;
use repertoire_core::Filesystem;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Real filesystems of at least 2 GiB (tiny boot partitions and the like
/// are left out, as in sentinelle), root first, then `/home`. Also returns
/// every mount point, so walks never enter one.
pub fn filesystems() -> (Vec<Filesystem>, HashSet<PathBuf>) {
    let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let (mut fss, all) = parse_mountinfo(&text, Path::is_dir);
    fss.retain(|fs| space(&fs.mount_points[0]).is_some_and(|(total, _)| total >= 2 << 30));
    fss.sort_by_key(|fs| {
        let p = &fs.mount_points[0];
        (p != Path::new("/"), !p.starts_with("/home"), p.clone())
    });
    (fss, all)
}
