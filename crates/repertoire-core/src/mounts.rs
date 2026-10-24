//! Which filesystems to index, from `/proc/self/mountinfo` text.
//!
//! Same rules as sentinelle's storage view: real filesystem types only,
//! skipping snaps, container storage and runtime mounts, keeping the first
//! of several bind mounts of one directory, grouped by backing device.
//! Reading the file and the size filter live in the Linux crate; the
//! parsing is plain text and tested everywhere.

use std::collections::HashSet;
use std::path::PathBuf;

pub const REAL_FS: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "zfs", "bcachefs", "jfs", "reiserfs", "ntfs", "ntfs3", "fuseblk",
    "exfat", "vfat", "nilfs2",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filesystem {
    /// Stable identifier: the backing device (`/dev/nvme0n1p2`), or the root
    /// path for directories indexed on request.
    pub id: String,
    pub device: String,
    pub fs_type: String,
    /// Shortest first.
    pub mount_points: Vec<PathBuf>,
}

impl Filesystem {
    /// An arbitrary directory indexed as if it were a filesystem.
    pub fn from_root(path: PathBuf) -> Filesystem {
        let id = path.to_string_lossy().into_owned();
        Filesystem { device: id.clone(), id, fs_type: "dir".into(), mount_points: vec![path] }
    }
}

fn unescape(s: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// The real filesystems in a mountinfo text, and every mount point (so a
/// walk never enters one). `is_dir` filters out file bind mounts.
pub fn parse_mountinfo(text: &str, is_dir: impl Fn(&std::path::Path) -> bool) -> (Vec<Filesystem>, HashSet<PathBuf>) {
    let mut all = HashSet::new();
    let mut by_dev: Vec<Filesystem> = Vec::new();
    let mut seen_roots = HashSet::new();
    let mut mounts_by_dev: Vec<(String, PathBuf, PathBuf)> = Vec::new();
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let l: Vec<&str> = left.split(' ').collect();
        let r: Vec<&str> = right.split(' ').collect();
        if l.len() < 5 || r.len() < 2 {
            continue;
        }
        let mount = unescape(l[4]);
        all.insert(mount.clone());
        let (fs_type, source) = (r[0], unescape(r[1]).to_string_lossy().into_owned());
        if !REAL_FS.contains(&fs_type) {
            continue;
        }
        let m = mount.to_string_lossy();
        if m.starts_with("/snap/")
            || m.starts_with("/var/lib/docker")
            || m.starts_with("/var/lib/containers")
            || m.starts_with("/run/")
            || m.starts_with("/proc")
            || m.starts_with("/sys")
        {
            continue;
        }
        if !is_dir(&mount) {
            continue; // file bind mounts (containers)
        }
        // Bind mounts of the same directory: keep the first.
        if !seen_roots.insert((l[2].to_string(), l[3].to_string())) {
            continue;
        }
        // A directory bound onto its own path (systemd's ReadWritePaths=,
        // ProtectHome=) is already covered by the mount it lives in.
        let root = unescape(l[3]);
        let covered = mounts_by_dev.iter().any(|(dev, r0, m0): &(String, PathBuf, PathBuf)| {
            dev == l[2] && root.strip_prefix(r0).is_ok_and(|rest| m0.join(rest) == mount) && *m0 != mount
        });
        mounts_by_dev.push((l[2].to_string(), root, mount.clone()));
        if covered {
            continue;
        }
        if let Some(fs) = by_dev.iter_mut().find(|f| f.id == source) {
            fs.mount_points.push(mount);
        } else {
            by_dev.push(Filesystem {
                id: source.clone(),
                device: source.rsplit('/').next().unwrap_or(&source).to_string(),
                fs_type: fs_type.to_string(),
                mount_points: vec![mount],
            });
        }
    }
    for fs in &mut by_dev {
        fs.mount_points.sort_by_key(|p| p.as_os_str().len());
    }
    (by_dev, all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_groups() {
        let text = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
23 22 0:21 / /proc rw shared:2 - proc proc rw
24 22 259:3 / /home rw shared:3 - btrfs /dev/nvme0n1p3 rw,subvol=/home
25 22 259:3 /games /mnt/my\\040games rw shared:4 - btrfs /dev/nvme0n1p3 rw
26 22 259:3 /games /srv/games rw shared:5 - btrfs /dev/nvme0n1p3 rw
27 22 0:30 / /run/user/1000 rw - tmpfs tmpfs rw
28 22 7:1 / /snap/core/1 ro - squashfs /dev/loop1 ro
";
        let (fss, all) = parse_mountinfo(text, |_| true);
        assert_eq!(fss.len(), 2);
        assert_eq!(fss[0].id, "/dev/nvme0n1p2");
        assert_eq!(fss[1].mount_points, vec![PathBuf::from("/home"), PathBuf::from("/mnt/my games")]);
        assert_eq!(fss[1].device, "nvme0n1p3");
        assert!(all.contains(&PathBuf::from("/proc")));
        assert!(all.contains(&PathBuf::from("/srv/games")));
    }

    #[test]
    fn self_binds_are_not_new_mount_points() {
        let text = "\
22 1 259:2 / / ro,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
40 22 259:2 /var/lib/repertoire /var/lib/repertoire rw - ext4 /dev/nvme0n1p2 rw
41 22 259:2 /home /home ro - ext4 /dev/nvme0n1p2 rw
42 22 259:2 /srv/data /mnt/data rw - ext4 /dev/nvme0n1p2 rw
";
        let (fss, all) = parse_mountinfo(text, |_| true);
        assert_eq!(fss[0].mount_points, vec![PathBuf::from("/"), PathBuf::from("/mnt/data")]);
        assert!(all.contains(&PathBuf::from("/var/lib/repertoire")));
    }
}
