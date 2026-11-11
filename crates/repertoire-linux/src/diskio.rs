//! How much is being read from disk, by everyone and by us, so background
//! scans can give way to everything else.
//!
//! I/O priority alone doesn't do it: NVMe drives usually run the `none`
//! scheduler, which ignores it, so an idle-class scan competes at full
//! strength with a login or a game loading.

/// Bytes read so far from physical disks, all of them.
///
/// Only devices backed by hardware (`/sys/block/<name>/device` exists) are
/// counted: a read through dm-crypt, LVM or md shows up both on the virtual
/// device and on the disk under it, and would otherwise count twice. Loop,
/// zram and ram devices aren't disk reads at all.
pub fn disk_read_bytes() -> Option<u64> {
    let stats = std::fs::read_to_string("/proc/diskstats").ok()?;
    let mut total = 0u64;
    for line in stats.lines() {
        // major minor name reads merged sectors-read ...
        let mut fields = line.split_whitespace().skip(2);
        let (Some(name), Some(sectors)) = (fields.next(), fields.nth(2)) else { continue };
        if !std::path::Path::new("/sys/block").join(name).join("device").exists() {
            continue;
        }
        // Always 512-byte units here, whatever the disk's sector size.
        total += sectors.parse::<u64>().unwrap_or(0) * 512;
    }
    Some(total)
}

/// Bytes this process has caused to be read from storage (every thread).
pub fn own_read_bytes() -> Option<u64> {
    let io = std::fs::read_to_string("/proc/self/io").ok()?;
    io.lines().find_map(|l| l.strip_prefix("read_bytes:")).and_then(|v| v.trim().parse().ok())
}
