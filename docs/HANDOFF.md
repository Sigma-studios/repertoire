# Hand-off

What Sigma needs to package répertoire on NixOS, what was measured, what's
open, and what to check on the real machine. Everything here was tested on
macOS (Apple Silicon) and in a Lima VM: Ubuntu 24.04, kernel 6.8, aarch64,
6 vCPUs, 8 GB RAM, ext4 on a virtio disk. Nothing has run on the target
(kernel 6.18, x86_64, ext4 on NVMe) yet.

## For the NixOS module

**Package.** A Cargo workspace; `cargo build --release` gives two binaries:

- `repertoired`, the daemon, for the service;
- `repertoire`, the CLI, for `environment.systemPackages`.

No build-time system libraries. `Cargo.lock` is committed. Runtime needs only
libc.

**Service.** Translate [`examples/repertoire.service`](../examples/repertoire.service).
It needs:

- `ExecStart = "${pkg}/bin/repertoired --foreground"`. Options worth
  exposing: `--exclude PATH`, `--exclude-name NAME` (both repeatable,
  none by default), `--verbose`.
- To run as **root** with `CapabilityBoundingSet = CAP_SYS_ADMIN
  CAP_DAC_READ_SEARCH`. `CAP_SYS_ADMIN` is for `fanotify_init` and
  filesystem marks, `CAP_DAC_READ_SEARCH` for `open_by_handle_at` and for
  reading every folder.
- `RuntimeDirectory = "repertoire"` (the socket,
  `/run/repertoire/repertoire.sock`, mode 0666) and `StateDirectory =
  "repertoire"` (snapshots, `/var/lib/repertoire/*.snap`, about 35 bytes per
  indexed entry).
- `KillSignal = SIGTERM`: snapshots are saved on the way out (a few hundred
  ms).
- The hardening in the example. Each directive was checked in the VM with the
  daemon live: `ProtectSystem=strict`, `ReadWritePaths=/var/lib/repertoire`,
  `ProtectHome=read-only`, `PrivateNetwork`, `PrivateDevices`,
  `NoNewPrivileges`, `Protect{KernelTunables,KernelModules,KernelLogs,ControlGroups,Clock,Hostname}`,
  `RestrictAddressFamilies=AF_UNIX`, `RestrictNamespaces`,
  `MemoryDenyWriteExecute`, and `SystemCallFilter=@system-service` plus
  `fanotify_init fanotify_mark name_to_handle_at open_by_handle_at
  ioprio_set`. `scripts/service-check.sh` installs the unit, writes a file
  under `/home` and checks it's counted.

**Clients.**

- sentinelle depends on `repertoire-proto` as a git dependency pinned by
  commit. With `rustPlatform.buildRustPackage` that needs
  `cargoLock.outputHashes."repertoire-proto-0.1.0" = "sha256-…";` in
  sentinelle's derivation.
- The Albert plugin is the folder `albert/repertoire`. Install it to
  `~/.local/share/albert/python/plugins/repertoire` (or the system plugin
  directory of the Albert package). It needs Albert's Python interface
  **5.0** (`md_iid = "5.0"`, Albert v35 and later; check the installed
  Albert's version). It uses only the Python standard library.
- `REPERTOIRE_SOCKET` overrides the socket path for every client, if the
  module ever moves it.

## Benchmarks

All in the VM unless noted. Release builds. `scripts/bench.sh` reproduces
them.

| what | result | target |
|---|---|---|
| `repertoire scan /` vs `du -sx --block-size=1 /` (174k entries, 6.0 GiB) | identical: 6 431 944 704 bytes both | within 1% |
| fixture trees (sparse, hard links in and across folders, unreadable folders, symlinks) vs `du` | every folder equal, macOS and Linux | equal |
| scan, 520 000 files (synthetic, warm cache, 6 threads) | 0.15 s | < 10 s |
| scan, 674 000 entries (VM root + synthetic, **cold** cache) | 1.45 s | |
| scan of a real macOS home, 12.5 M entries (APFS, 10 cores) | 91 s, 137 bytes per entry | |
| daemon RSS, 524k entries, after the first scan | 60 MB (peak 105 MB during the scan) | ~100 MB |
| same, restarted from its snapshot, after the reconcile scan | 84 MB (peak 182 MB while both trees exist) | |
| daemon RSS indexing the whole VM root (699k entries) | ~100 MB | |
| snapshot of 524k entries | 17.9 MB, loaded in 69 ms | |
| search over 524k names, client side (connect + JSON) | 8–18 ms (`repertoire search`), 10–13 ms (Albert plugin's client) | < 20 ms |
| Bevy debug build under the daemon (101 s, ~100k kernel events after merging) | **0.68 s CPU = 0.67% of one core**, RSS ~100 MB, no growth; `target/` matches `du` | < 1% |
| fanotify queue overflow (forced: no unlimited queue, reader paused, 18k events) | flagged stale, rescanned, equal to a fresh scan | recovers |

How the CPU budget was met (first measurement: 7.2% of a core): reading
events every 100 ms so the kernel merges repeated writes to the same file,
one refresh per merged event, keeping a batch's directories open, and
skipping the whole-index hard-link pass when a group is provably complete.

systemd's own accounting shows more memory than the RSS above ("memory
peak" 195–515 MB in the VM). That's because the cgroup is also charged for
the kernel's dentry and inode cache, which the scan fills. It's reclaimable.

## Tests

- `cargo test --workspace` on macOS and Linux (CI runs both, and the Linux
  job runs the suite again as root).
- `repertoire-core/tests/events.rs`: thousands of random operations
  (creates, writes, truncates, sparse growth, renames over files, folder
  moves, deletes, `rm -r`, hard links in and across folders, symlinks,
  chmod to 000/311/644/700, touch, moves in from and out to an unindexed
  folder), reported as fanotify would and applied late (content changes
  debounced across later renames). The tree must equal a fresh scan after
  every few batches. 3 seeds by default in CI, as user and root. Last
  sweep: 20 seeds × 3000 operations in the VM pass as user and as root
  (ext4, real file handles); on macOS 99 of 100 seeds pass. Seed 55 fails
  on APFS: right after an append APFS reported 176 KiB allocated for a
  119 KiB file, and later 120 KiB with no write in between, so no event
  ever said so. Not reproduced in isolation, and not investigated to the
  end (why the hard-link re-stat that follows didn't pick it up either).
  ext4 doesn't change allocation behind the application's back like this;
  XFS's speculative preallocation would. Worth a look if the daemon is
  ever used on APFS or XFS.
- `repertoire/tests/fanotify.rs` (root, Linux): the same operations against
  the real daemon and kernel, a forced overflow, and the multi-user check
  (`nobody` can't see names in a `0700` home; its owner and root can).
- `repertoire-linux/tests/kernel.rs`: what the running kernel reports. Run
  it on the target first; it prints every event.

## Open issues

- **Allocation that changes without an event** (APFS, XFS speculative
  preallocation) leaves a file's allocated size stale until it's touched
  again. See the seed 55 note above.

- **Hard links made to an already-indexed file** may count twice for up to
  10 s while the machine is busy (the pass that finds the other link runs at
  most every 10 s, or at once when idle). Cargo's own hard links don't
  trigger it.
- **Peak memory doubles during a reconcile**: the snapshot's tree is served
  while the new scan builds another (182 MB peak for 524k entries). The
  alternative, not serving numbers during the scan, seemed worse.
- **Mount changes** are polled every 30 s. `FAN_DELETE_SELF`/`FAN_MOVE_SELF`
  aren't requested: a mount root can't be removed while mounted, and asking
  for them would double the events of every deletion.
- **Not-mounted nested btrfs subvolumes** are skipped (they have their own
  device number). On btrfs, filesystem marks on subvolumes may also fail on
  older kernels; the target is ext4.
- **ACLs are ignored** by the permission filter (see PROTOCOL.md).
- **Aggregate sizes leak** across permissions: a user sees the total of a
  folder even if part of it is in someone else's private folder. Names never
  leak.
- **Configuration** is command-line only (no config file); excludes go in
  `ExecStart`.
- **Search** is substring/prefix/word ranking, no fuzzy matching.
- **Bind mounts of a subfolder elsewhere** (`/srv/data` also mounted at
  `/mnt/data`) are indexed as a second mount point of the same filesystem,
  so that space counts twice in the filesystem's `indexed` total, as in
  sentinelle's scanner. A folder bound onto its own path (what systemd's
  `ReadWritePaths=` and `ProtectHome=` do inside the service) is recognized
  and not counted again.

## Decisions taken (plan §9)

1. Name: répertoire (`repertoire` in code).
2. Every real filesystem by default (same list as sentinelle); `--root` for
   a subset.
3. The multi-user permission filter is implemented.
4. Memory: 60–100 MB steady for 500–700k entries, roughly twice that for a
   few seconds during a reconcile after a restart.

## To check on the target machine

1. **Kernel**: as root, `cargo test -p repertoire-linux --test kernel --
   --nocapture` on 6.18. It must pass and print a rename event with both
   ends.
2. **Everything is indexed**: `repertoire status` lists every real
   filesystem as `live`. For each, compare `indexed` with `sudo du -sx
   --block-size=1 <mount>` (should be equal or within a fraction of a
   percent; `used` will be higher by the filesystem's metadata).
3. **First scan time** of the 3.6 TiB NVMe (`journalctl -u repertoire`
   says `scanned N entries … in …`): expected well under 10 s for ~520k files
   warm, longer cold.
4. **Games don't stutter during a rescan**: start a game, run `repertoire
   rescan`, watch frame times. Scan threads are at nice 19 and idle I/O
   class.
5. **CPU during a big build**: `systemctl show repertoire -p CPUUsageNSec`
   before and after a large `cargo build`; divide by the build's wall time.
   Expect under 1%.
6. **Memory**: `ps -o rss -C repertoired` after the first scan and after a
   restart (snapshot + reconcile).
7. **Multi-user**: as another user, `repertoire search` something only in
   Sigma's home; nothing should come back if the home is `0700`.
8. **sentinelle**: the Storage header shows a green *live* badge; writing a
   big file moves the bars within a couple of seconds; `systemctl stop
   repertoire` makes it fall back to its own scanner (header back to
   "measured N ago"); starting the daemon again switches back within a
   minute.
9. **Albert**: the plugin loads (Settings → Plugins → Répertoire), `f
   steam` lists results; Enter opens, the other actions open the folder or
   copy the path.
10. **SystemCallFilter on x86_64**: nothing in `journalctl -u repertoire`
    about SIGSYS (it was only checked on aarch64).
11. **Steam libraries on other drives** and any other mount show up as
    their own filesystems, and `repertoire ls /` lists them as mounts.
